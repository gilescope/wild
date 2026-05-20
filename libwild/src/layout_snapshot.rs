//! Tier-2 foundation + tier-3 contributors map: per-output section
//! layout snapshot.
//!
//! Captures, after `produce_layout` finishes, every output section's
//! `(name, alignment, file_offset, file_size, mem_offset, mem_size)`
//! AND the list of bundle-keyed input files that contributed bytes
//! to it. Persisted to `<output>.wild-layout`. Next link can:
//!
//! * Compare current vs previous layout to flag layout shifts (the tier-2 canary path).
//! * Combine the contributors map with the parse-skip dirty-input set (`.wild-hashes`) to compute a
//!   per-section *dirty bitmap*. Sections with no dirty contributors and unchanged layout are safe
//!   for tier 3 to mmap-copy from the previous output.
//!
//! This module is **capture + canary only** today — no behavioural
//! reuse yet. Shipping the storage layer first proves round-trip
//! correctness on real links (bevy-dylib) before tier 3 starts
//! trusting the snapshot for output construction.
//!
//! Format (schema v2):
//!
//! ```text
//! +------------------- Header (64 bytes) ----------------------+
//! | magic[8] = "WILDLO01"  schema u32  flags u32               |
//! | section_count u32  _pad u32  sections_off u64              |
//! | names_off u64  names_len u64                               |
//! | contributors_off u64  contributors_len u64                 |
//! +-- Sections (n × sizeof(SectionEntry) = 48 bytes each) -----+
//! | name_off u32  name_len u32  alignment u64                  |
//! | file_offset u64  file_size u64  mem_offset u64  mem_size u64
//! +-- Names blob ----------------------------------------------+
//! | concatenated section-name bytes                            |
//! +-- Contributors blob (one record per section, in id order) -+
//! | n_keys u32  _pad u32  keys: n_keys × [u8; 16]              |
//! +------------------------------------------------------------+
//! ```

use std::mem::size_of;
use std::path::Path;
use std::path::PathBuf;

const MAGIC: &[u8; 8] = b"WILDLO01";
const SCHEMA: u32 = 2;
const REQUIRED_ALIGN: usize = 8;

/// Per-input bundle key — same blake3-128 derivation as
/// [`crate::parsed_input_cache::bundle_key_for`]. Re-exported here so
/// the contributors map and the parse-skip cache speak the same key
/// and can be cross-referenced without conversion.
pub(crate) const KEY_LEN: usize = 16;
pub(crate) type ContributorKey = [u8; KEY_LEN];

#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    magic: [u8; 8],
    schema: u32,
    flags: u32,
    section_count: u32,
    _pad: u32,
    sections_off: u64,
    names_off: u64,
    names_len: u64,
    contributors_off: u64,
    contributors_len: u64,
}

const _: () = {
    // 8 + 4 + 4 + 4 + 4 + 8 + 8 + 8 + 8 + 8 = 64.
    assert!(size_of::<Header>() == 64);
    assert!(std::mem::align_of::<Header>() <= REQUIRED_ALIGN);
};

#[repr(C)]
#[derive(Clone, Copy)]
struct SectionEntry {
    name_off: u32,
    name_len: u32,
    alignment: u64,
    file_offset: u64,
    file_size: u64,
    mem_offset: u64,
    mem_size: u64,
}

const _: () = {
    // 4 + 4 + 8 + 8 + 8 + 8 + 8 = 48. Stays a multiple of 8 so an
    // 8-byte-aligned `sections_off` keeps every entry aligned.
    assert!(size_of::<SectionEntry>() == 48);
    assert!(std::mem::align_of::<SectionEntry>() <= REQUIRED_ALIGN);
};

/// One section's resolved layout. The `name` is captured eagerly so a
/// loaded snapshot stays meaningful even if the next link rearranges
/// section IDs. `contributors` is the set of bundle keys (one per
/// input file) whose loaded sections fed bytes into this output
/// section — empty for synthetic sections (prelude, epilogue,
/// LINKEDIT regions). Sorted + deduped so equality compare is
/// stable across links.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotSection {
    pub(crate) name: Vec<u8>,
    pub(crate) alignment: u64,
    pub(crate) file_offset: u64,
    pub(crate) file_size: u64,
    pub(crate) mem_offset: u64,
    pub(crate) mem_size: u64,
    pub(crate) contributors: Vec<ContributorKey>,
}

/// Owned snapshot of every output section's layout. Used by both the
/// writer (capturing fresh layout) and the canary path (comparing
/// fresh against on-disk).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LayoutSnapshot {
    pub(crate) sections: Vec<SnapshotSection>,
}

impl LayoutSnapshot {
    pub(crate) fn new() -> Self {
        Self {
            sections: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, s: SnapshotSection) {
        self.sections.push(s);
    }

    pub(crate) fn len(&self) -> usize {
        self.sections.len()
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Sort + dedup every section's contributor list. Idempotent.
    /// Must be called before any `PartialEq` compare or canary read,
    /// because input-walk order at capture time isn't stable across
    /// rayon-parallel layouts. `finish()` calls this internally for
    /// the on-disk byte format; callers that compare in-memory
    /// snapshots without serialising must invoke it explicitly.
    pub(crate) fn canonicalize(&mut self) {
        for s in &mut self.sections {
            s.contributors.sort_unstable();
            s.contributors.dedup();
        }
    }

    /// Phase-2b "wholesale prev → out copy" predicate: every section
    /// has matching layout AND no dirty contributors. Synthetic
    /// sections (empty contributors) are allowed because phase-2b
    /// copies the entire output file from prev, so even synthetic
    /// regions like the Mach-O header survive byte-equivalent.
    /// Returns `true` only when EVERY section is either reusable or
    /// purely synthetic.
    pub(crate) fn is_fully_reusable(
        prev: &LayoutSnapshot,
        cur: &LayoutSnapshot,
        clean_inputs: &hashbrown::HashSet<ContributorKey>,
    ) -> bool {
        if prev.sections.len() != cur.sections.len() {
            return false;
        }
        prev.sections.iter().zip(&cur.sections).all(|(a, b)| {
            a.name == b.name
                && a.file_offset == b.file_offset
                && a.file_size == b.file_size
                && a.mem_offset == b.mem_offset
                && a.mem_size == b.mem_size
                && a.contributors.iter().all(|k| clean_inputs.contains(k))
        })
    }

    /// Tier-3 reuse predicate: section indices where ALL of the
    /// following hold against `prev` (the snapshot from the previous
    /// link, loaded from disk):
    ///
    /// * Same `(name, file_offset, file_size, mem_offset, mem_size)` — the section hasn't moved or
    ///   grown, so the bytes a cold writer would emit live at the same spot they used to.
    /// * The section has at least one contributor — synthetic / writer-generated sections (Mach-O
    ///   header, LINKEDIT, codesign blob) have empty contributors and are never reusable for tier-3
    ///   purposes. The writer regenerates them every link with content that depends on the entire
    ///   output (e.g. CDHash) or with non-deterministic fields (LC_UUID, build-version timestamp).
    ///   Pre-filling them from prev would be silently overwritten by the writer; treating them as
    ///   reusable would also cause spurious canary divergences on the UUID drift.
    /// * Every contributor key (in `prev`'s contributor list for that section) is in `clean_inputs`
    ///   — none of the inputs feeding bytes here have changed since the previous link.
    ///
    /// Returns `Vec<usize>` of indices into `prev.sections` that are
    /// safe for tier 3's writer to mmap-copy from the previous
    /// output. Sections beyond `min(prev.len, cur.len)` are never
    /// reusable (the section count itself shifted).
    pub(crate) fn reusable_section_indices(
        prev: &LayoutSnapshot,
        cur: &LayoutSnapshot,
        clean_inputs: &hashbrown::HashSet<ContributorKey>,
    ) -> Vec<usize> {
        let n = prev.sections.len().min(cur.sections.len());
        let mut out = Vec::new();
        for i in 0..n {
            let a = &prev.sections[i];
            let b = &cur.sections[i];
            if a.name != b.name
                || a.file_offset != b.file_offset
                || a.file_size != b.file_size
                || a.mem_offset != b.mem_offset
                || a.mem_size != b.mem_size
            {
                continue;
            }
            if a.contributors.is_empty() {
                continue;
            }
            if a.contributors.iter().any(|k| !clean_inputs.contains(k)) {
                continue;
            }
            out.push(i);
        }
        out
    }

    /// Tier-3 helper: every section index whose contributor list
    /// contains at least one *dirty* (i.e. not present in
    /// `clean_inputs`) input. These sections cannot be reused from a
    /// previous output binary; the writer must re-emit them.
    ///
    /// Sections with empty contributor lists (synthetic / writer-
    /// generated) are *always* considered clean here — they have no
    /// input dependencies, so their content depends only on the
    /// resolved layout, which the layout snapshot already captures.
    /// Tier 3's writer integration will gate reuse on
    /// "section is clean here AND its `SnapshotSection` matches the
    /// previous link's snapshot byte-for-byte".
    pub(crate) fn dirty_section_indices(
        &self,
        clean_inputs: &hashbrown::HashSet<ContributorKey>,
    ) -> Vec<usize> {
        let mut out = Vec::new();
        for (i, s) in self.sections.iter().enumerate() {
            if s.contributors.iter().any(|k| !clean_inputs.contains(k)) {
                out.push(i);
            }
        }
        out
    }

    /// Encode to a stable on-disk representation. Sections are
    /// emitted in the order they were pushed; that order is the
    /// canonical output-section order (see
    /// `OutputSections::ids_with_info`) so it's already
    /// deterministic. Contributors are sorted before emit so two
    /// runs with the same logical input set produce byte-identical
    /// snapshots.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        // Belt-and-braces: callers SHOULD have called `canonicalize`
        // already, but `finish` repeats it so a forgetful call site
        // can't ship a non-deterministic on-disk snapshot.
        self.canonicalize();

        let n = self.sections.len();
        let header_size = size_of::<Header>();
        let sections_off = header_size;
        let sections_size = n * size_of::<SectionEntry>();
        let names_off = sections_off + sections_size;

        // Build name blob with offsets
        let mut names_blob: Vec<u8> = Vec::new();
        let mut entries: Vec<SectionEntry> = Vec::with_capacity(n);
        for s in &self.sections {
            let off = names_blob.len() as u32;
            let len = s.name.len() as u32;
            names_blob.extend_from_slice(&s.name);
            entries.push(SectionEntry {
                name_off: off,
                name_len: len,
                alignment: s.alignment,
                file_offset: s.file_offset,
                file_size: s.file_size,
                mem_offset: s.mem_offset,
                mem_size: s.mem_size,
            });
        }

        let names_len = names_blob.len();

        // Build contributors blob: per-section { n_keys u32, _pad u32,
        // keys: n × [u8;16] }. The pad keeps each section's record
        // 8-byte aligned.
        let contributors_off = names_off + names_len;
        let contributors_off_aligned = contributors_off.next_multiple_of(REQUIRED_ALIGN);
        let mut contributors_blob: Vec<u8> = Vec::new();
        for s in &self.sections {
            let n_keys = s.contributors.len() as u32;
            contributors_blob.extend_from_slice(&n_keys.to_le_bytes());
            contributors_blob.extend_from_slice(&[0u8; 4]); // _pad
            for k in &s.contributors {
                contributors_blob.extend_from_slice(k);
            }
        }
        let contributors_len = contributors_blob.len();
        let total = contributors_off_aligned + contributors_len;

        let mut out = vec![0u8; total];

        let header = Header {
            magic: *MAGIC,
            schema: SCHEMA,
            flags: 0,
            section_count: n as u32,
            _pad: 0,
            sections_off: sections_off as u64,
            names_off: names_off as u64,
            names_len: names_len as u64,
            contributors_off: contributors_off_aligned as u64,
            contributors_len: contributors_len as u64,
        };
        let hdr_bytes = unsafe {
            std::slice::from_raw_parts(&header as *const Header as *const u8, header_size)
        };
        out[..header_size].copy_from_slice(hdr_bytes);

        let entries_bytes =
            unsafe { std::slice::from_raw_parts(entries.as_ptr() as *const u8, sections_size) };
        out[sections_off..sections_off + sections_size].copy_from_slice(entries_bytes);
        out[names_off..names_off + names_len].copy_from_slice(&names_blob);
        out[contributors_off_aligned..contributors_off_aligned + contributors_len]
            .copy_from_slice(&contributors_blob);

        out
    }

    /// Decode from disk bytes. Returns `None` on any kind of
    /// corruption — callers fall through to "no snapshot" semantics.
    /// Never fails the link.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < size_of::<Header>() {
            return None;
        }
        if !(bytes.as_ptr() as usize).is_multiple_of(REQUIRED_ALIGN) {
            return None;
        }
        let header = unsafe { &*(bytes.as_ptr() as *const Header) };
        if &header.magic != MAGIC {
            return None;
        }
        if header.schema != SCHEMA {
            return None;
        }

        let n = header.section_count as usize;
        let sections_off = header.sections_off as usize;
        let sections_size = n.checked_mul(size_of::<SectionEntry>())?;
        let sections_end = sections_off.checked_add(sections_size)?;
        if sections_end > bytes.len() {
            return None;
        }
        if !sections_off.is_multiple_of(std::mem::align_of::<SectionEntry>()) {
            return None;
        }
        let names_off = header.names_off as usize;
        let names_len = header.names_len as usize;
        let names_end = names_off.checked_add(names_len)?;
        if names_end > bytes.len() {
            return None;
        }
        let contributors_off = header.contributors_off as usize;
        let contributors_len = header.contributors_len as usize;
        let contributors_end = contributors_off.checked_add(contributors_len)?;
        if contributors_end > bytes.len() {
            return None;
        }

        let entries: &[SectionEntry] = unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().add(sections_off) as *const SectionEntry, n)
        };
        let names = &bytes[names_off..names_end];
        let contributors_slice = &bytes[contributors_off..contributors_end];

        let mut sections = Vec::with_capacity(n);
        let mut cursor = 0usize;
        for e in entries {
            let off = e.name_off as usize;
            let len = e.name_len as usize;
            let end = off.checked_add(len)?;
            if end > names.len() {
                return None;
            }

            // Read this section's contributors record:
            // n_keys u32 + _pad u32 + n_keys × [u8; KEY_LEN]
            if cursor + 8 > contributors_slice.len() {
                return None;
            }
            let n_keys = u32::from_le_bytes(contributors_slice[cursor..cursor + 4].try_into().ok()?)
                as usize;
            cursor += 8; // skip n_keys + _pad
            let keys_bytes = n_keys.checked_mul(KEY_LEN)?;
            let keys_end = cursor.checked_add(keys_bytes)?;
            if keys_end > contributors_slice.len() {
                return None;
            }
            let mut contributors = Vec::with_capacity(n_keys);
            for i in 0..n_keys {
                let off = cursor + i * KEY_LEN;
                let mut k = [0u8; KEY_LEN];
                k.copy_from_slice(&contributors_slice[off..off + KEY_LEN]);
                contributors.push(k);
            }
            cursor = keys_end;

            sections.push(SnapshotSection {
                name: names[off..end].to_vec(),
                alignment: e.alignment,
                file_offset: e.file_offset,
                file_size: e.file_size,
                mem_offset: e.mem_offset,
                mem_size: e.mem_size,
                contributors,
            });
        }

        Some(Self { sections })
    }
}

impl Default for LayoutSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

/// Path to the layout snapshot for a given output binary. Sibling of
/// `<output>.wild-pi-cache` and `<output>.wild-hashes`.
pub(crate) fn snapshot_path_for_output(output: &Path) -> PathBuf {
    let mut p = output.to_path_buf();
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".wild-layout");
    p.set_file_name(name);
    p
}

/// Atomic write (tmp + rename) of `snapshot.finish()` to
/// `<output>.wild-layout`. Failures are swallowed — a missing
/// snapshot must never fail the link.
pub(crate) fn write_snapshot(output: &Path, snapshot: LayoutSnapshot) -> std::io::Result<()> {
    let path = snapshot_path_for_output(output);
    let bytes = snapshot.finish();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("wild-layout.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read + validate a snapshot for `output`. `None` when the file is
/// missing, unreadable, schema-mismatched, or truncated.
pub(crate) fn read_snapshot(output: &Path) -> Option<LayoutSnapshot> {
    let path = snapshot_path_for_output(output);
    let bytes = std::fs::read(&path).ok()?;
    LayoutSnapshot::from_bytes(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> LayoutSnapshot {
        let mut s = LayoutSnapshot::new();
        s.push(SnapshotSection {
            name: b"__text".to_vec(),
            alignment: 16,
            file_offset: 0x1000,
            file_size: 0x4000,
            mem_offset: 0x100001000,
            mem_size: 0x4000,
            contributors: vec![[1u8; KEY_LEN], [2u8; KEY_LEN]],
        });
        s.push(SnapshotSection {
            name: b"__cstring".to_vec(),
            alignment: 1,
            file_offset: 0x5000,
            file_size: 0x800,
            mem_offset: 0x100005000,
            mem_size: 0x800,
            contributors: vec![[3u8; KEY_LEN]],
        });
        s
    }

    fn aligned(bytes: &[u8]) -> Box<[u8]> {
        let layout = std::alloc::Layout::from_size_align(bytes.len().max(1), 8).unwrap();
        unsafe {
            let ptr = std::alloc::alloc(layout);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let slice = std::slice::from_raw_parts_mut(ptr, bytes.len());
            Box::from_raw(slice)
        }
    }

    #[test]
    fn round_trip_in_memory() {
        let s = fixture();
        let bytes = s.finish();
        let buf = aligned(&bytes);
        let parsed = LayoutSnapshot::from_bytes(&buf).expect("parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.sections[0].name, b"__text");
        assert_eq!(parsed.sections[0].file_offset, 0x1000);
        assert_eq!(parsed.sections[1].name, b"__cstring");
        assert_eq!(parsed.sections[1].mem_offset, 0x100005000);
    }

    #[test]
    fn rejects_bad_magic() {
        let s = fixture();
        let mut bytes = s.finish();
        bytes[0] ^= 1;
        let buf = aligned(&bytes);
        assert!(LayoutSnapshot::from_bytes(&buf).is_none());
    }

    #[test]
    fn rejects_bad_schema() {
        let s = fixture();
        let mut bytes = s.finish();
        bytes[8] = (SCHEMA + 1) as u8;
        let buf = aligned(&bytes);
        assert!(LayoutSnapshot::from_bytes(&buf).is_none());
    }

    #[test]
    fn rejects_truncated() {
        let s = fixture();
        let bytes = s.finish();
        let truncated = &bytes[..bytes.len() - 5];
        let buf = aligned(truncated);
        assert!(LayoutSnapshot::from_bytes(&buf).is_none());
    }

    #[test]
    fn rejects_misaligned() {
        let s = fixture();
        let bytes = s.finish();
        let mut padded = Vec::with_capacity(bytes.len() + 1);
        padded.push(0u8);
        padded.extend_from_slice(&bytes);
        assert!(LayoutSnapshot::from_bytes(&padded[1..]).is_none());
    }

    #[test]
    fn empty_round_trips() {
        let s = LayoutSnapshot::new();
        let bytes = s.finish();
        let buf = aligned(&bytes);
        let parsed = LayoutSnapshot::from_bytes(&buf).expect("parse");
        assert!(parsed.is_empty());
    }

    #[test]
    fn snapshot_path_appends_extension() {
        let p = snapshot_path_for_output(Path::new("/tmp/myapp"));
        assert_eq!(p, Path::new("/tmp/myapp.wild-layout"));
        let p = snapshot_path_for_output(Path::new("/tmp/myapp.dylib"));
        assert_eq!(p, Path::new("/tmp/myapp.dylib.wild-layout"));
    }

    // wasi's `std::env::temp_dir()` is a hard panic — skip filesystem-using tests there.
    #[cfg(not(target_os = "wasi"))]
    #[test]
    fn write_then_read_round_trip_on_disk() {
        let s = fixture();
        let tmp = std::env::temp_dir().join(format!("wild-layout-rt-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let snapshot_path = snapshot_path_for_output(&tmp);
        let _ = std::fs::remove_file(&snapshot_path);

        write_snapshot(&tmp, s).expect("write");
        let leftover = snapshot_path.with_extension("wild-layout.tmp");
        assert!(!leftover.exists(), "leftover tmp at {leftover:?}");

        let parsed = read_snapshot(&tmp).expect("read");
        assert_eq!(parsed.len(), 2);
        let _ = std::fs::remove_file(&snapshot_path);
    }

    #[test]
    fn equal_snapshots_compare_equal() {
        // The canary path (next session) byte-compares fresh vs
        // loaded; this test guards against silent drift in the eq
        // implementation.
        let a = fixture();
        let b = fixture();
        assert_eq!(a, b);
    }

    #[test]
    fn contributors_round_trip_through_disk_format() {
        let s = fixture();
        let bytes = s.finish();
        let buf = aligned(&bytes);
        let parsed = LayoutSnapshot::from_bytes(&buf).expect("parse");
        assert_eq!(parsed.sections[0].contributors.len(), 2);
        assert_eq!(parsed.sections[0].contributors[0], [1u8; KEY_LEN]);
        assert_eq!(parsed.sections[0].contributors[1], [2u8; KEY_LEN]);
        assert_eq!(parsed.sections[1].contributors.len(), 1);
        assert_eq!(parsed.sections[1].contributors[0], [3u8; KEY_LEN]);
    }

    #[test]
    fn contributors_sort_dedup_makes_byte_equality_stable() {
        // Two snapshots that differ only in contributor insertion
        // order must serialise to byte-identical blobs after the
        // sort+dedup the writer applies. Otherwise the canary's
        // byte-compare would false-positive on rayon-induced
        // shuffle.
        let mk = |order: &[u8; 3]| {
            let mut s = LayoutSnapshot::new();
            s.push(SnapshotSection {
                name: b"x".to_vec(),
                alignment: 1,
                file_offset: 0,
                file_size: 0,
                mem_offset: 0,
                mem_size: 0,
                contributors: order
                    .iter()
                    .map(|&b| [b; KEY_LEN])
                    .chain(std::iter::once([order[0]; KEY_LEN])) // dup of first
                    .collect(),
            });
            s.finish()
        };
        let a = mk(&[1, 2, 3]);
        let b = mk(&[3, 1, 2]);
        assert_eq!(a, b, "insertion-order should not affect on-disk bytes");
    }

    #[test]
    fn dirty_section_indices_flags_only_sections_with_dirty_contributors() {
        // s0 = clean inputs only → not dirty
        // s1 = mix of clean + dirty → dirty
        // s2 = synthetic (empty contributors) → not dirty
        let mut snap = LayoutSnapshot::new();
        let k1 = [1u8; KEY_LEN];
        let k2 = [2u8; KEY_LEN];
        let k3 = [3u8; KEY_LEN];
        snap.push(SnapshotSection {
            name: b"clean".to_vec(),
            alignment: 1,
            file_offset: 0,
            file_size: 0,
            mem_offset: 0,
            mem_size: 0,
            contributors: vec![k1, k2],
        });
        snap.push(SnapshotSection {
            name: b"dirty".to_vec(),
            alignment: 1,
            file_offset: 0,
            file_size: 0,
            mem_offset: 0,
            mem_size: 0,
            contributors: vec![k1, k3], // k3 not in clean set
        });
        snap.push(SnapshotSection {
            name: b"synth".to_vec(),
            alignment: 1,
            file_offset: 0,
            file_size: 0,
            mem_offset: 0,
            mem_size: 0,
            contributors: vec![],
        });

        let mut clean: hashbrown::HashSet<ContributorKey> = hashbrown::HashSet::new();
        clean.insert(k1);
        clean.insert(k2);
        let dirty = snap.dirty_section_indices(&clean);
        assert_eq!(dirty, vec![1]);
    }
}
