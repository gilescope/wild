//! Tier-2 foundation: per-output section-layout snapshot.
//!
//! Captures every output section's `(name, alignment, file_offset,
//! file_size, mem_offset, mem_size)` after `produce_layout` finishes.
//! Persisted to `<output>.wild-layout`. On the next link the snapshot
//! is loaded; tier 3 will use it to decide which sections of the
//! previous output binary can be mmap-preserved instead of re-emitted.
//!
//! This module is **capture + canary only** — no behavioural reuse
//! yet. Shipping the storage layer first lets us prove round-trip
//! correctness against real links (rust-analyzer, bevy-dylib) before
//! anything starts trusting the snapshot for output construction.
//!
//! Format (schema v1):
//!
//! ```text
//! +------------------- Header (48 bytes) ----------------------+
//! | magic[8] = "WILDLO01"  schema u32  flags u32               |
//! | section_count u32  _pad u32  sections_off u64              |
//! | names_off u64  names_len u64                               |
//! +-- Sections (n × sizeof(SectionEntry) = 48 bytes each) -----+
//! | name_off u32  name_len u32  alignment u64                  |
//! | file_offset u64  file_size u64  mem_offset u64  mem_size u64
//! +-- Names blob ----------------------------------------------+
//! | concatenated section-name bytes                            |
//! +------------------------------------------------------------+
//! ```

use std::mem::size_of;
use std::path::Path;
use std::path::PathBuf;

const MAGIC: &[u8; 8] = b"WILDLO01";
const SCHEMA: u32 = 1;
const REQUIRED_ALIGN: usize = 8;

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
}

const _: () = {
    // 8 + 4 + 4 + 4 + 4 + 8 + 8 + 8 = 48.
    assert!(size_of::<Header>() == 48);
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
/// section IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotSection {
    pub(crate) name: Vec<u8>,
    pub(crate) alignment: u64,
    pub(crate) file_offset: u64,
    pub(crate) file_size: u64,
    pub(crate) mem_offset: u64,
    pub(crate) mem_size: u64,
}

/// Owned snapshot of every output section's layout. Used by both the
/// writer (capturing fresh layout) and the canary path (comparing
/// fresh against on-disk).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LayoutSnapshot {
    pub(crate) sections: Vec<SnapshotSection>,
}

impl LayoutSnapshot {
    pub(crate) fn new() -> Self {
        Self { sections: Vec::new() }
    }

    pub(crate) fn push(&mut self, s: SnapshotSection) {
        self.sections.push(s);
    }

    pub(crate) fn len(&self) -> usize {
        self.sections.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Encode to a stable on-disk representation. Sections are
    /// emitted in the order they were pushed; that order is the
    /// canonical output-section order (see
    /// `OutputSections::ids_with_info`) so it's already
    /// deterministic.
    pub(crate) fn finish(self) -> Vec<u8> {
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
        let total = names_off + names_len;
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
        };
        let hdr_bytes = unsafe {
            std::slice::from_raw_parts(&header as *const Header as *const u8, header_size)
        };
        out[..header_size].copy_from_slice(hdr_bytes);

        let entries_bytes = unsafe {
            std::slice::from_raw_parts(entries.as_ptr() as *const u8, sections_size)
        };
        out[sections_off..sections_off + sections_size].copy_from_slice(entries_bytes);
        out[names_off..names_off + names_len].copy_from_slice(&names_blob);

        out
    }

    /// Decode from disk bytes. Returns `None` on any kind of
    /// corruption — callers fall through to "no snapshot" semantics.
    /// Never fails the link.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < size_of::<Header>() {
            return None;
        }
        if bytes.as_ptr() as usize % REQUIRED_ALIGN != 0 {
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
        if sections_off % std::mem::align_of::<SectionEntry>() != 0 {
            return None;
        }
        let names_off = header.names_off as usize;
        let names_len = header.names_len as usize;
        let names_end = names_off.checked_add(names_len)?;
        if names_end > bytes.len() {
            return None;
        }

        let entries: &[SectionEntry] = unsafe {
            std::slice::from_raw_parts(
                bytes.as_ptr().add(sections_off) as *const SectionEntry,
                n,
            )
        };
        let names = &bytes[names_off..names_end];

        let mut sections = Vec::with_capacity(n);
        for e in entries {
            let off = e.name_off as usize;
            let len = e.name_len as usize;
            let end = off.checked_add(len)?;
            if end > names.len() {
                return None;
            }
            sections.push(SnapshotSection {
                name: names[off..end].to_vec(),
                alignment: e.alignment,
                file_offset: e.file_offset,
                file_size: e.file_size,
                mem_offset: e.mem_offset,
                mem_size: e.mem_size,
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
        });
        s.push(SnapshotSection {
            name: b"__cstring".to_vec(),
            alignment: 1,
            file_offset: 0x5000,
            file_size: 0x800,
            mem_offset: 0x100005000,
            mem_size: 0x800,
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

    #[test]
    fn write_then_read_round_trip_on_disk() {
        let s = fixture();
        let tmp =
            std::env::temp_dir().join(format!("wild-layout-rt-{}.bin", std::process::id()));
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
}
