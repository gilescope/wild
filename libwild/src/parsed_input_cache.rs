//! Zero-copy on-disk cache for the parsed form of an input object.
//!
//! Tier-1 of wild's incremental-linking plan (see
//! `project_incremental_link_plan.md`) needs a fast path that skips
//! re-parsing a clean input's symbol table on every link. Postcard-
//! serialised caches would pay a fresh allocation + copy on every
//! deserialisation — wild already mmaps every input, so this module
//! stores the cached parse result in the same shape: a fixed-layout,
//! `repr(C)` blob that can be `mmap`ed and interpreted in place.
//!
//! # Format (schema v1)
//!
//! ```text
//! +------------------ CacheHeader (48 bytes) -----------------+
//! | magic [8]  schema u32  flags u32  n_symbols u64           |
//! | symbols_off u64  names_off u64  names_len u64             |
//! +--- symbols (n_symbols × sizeof(CachedSymbol) = 24 bytes) -+
//! | [name_off u32] [name_len u32] [hash u64] [flags u32]       |
//! | [kind u8] [_pad u8×3]                                      |
//! | …                                                          |
//! +-------------------- names blob -----------------------------+
//! | symbol name bytes, concatenated, NUL-separated optional    |
//! +------------------------------------------------------------+
//! ```
//!
//! The whole file is `8`-byte aligned so the symbol-array cast is
//! sound on every supported arch. On load we validate magic +
//! schema, then cast the symbol region straight to
//! `&[CachedSymbol]`. Name bytes are returned as slices into the
//! mmap'd buffer — zero copy, no lifetime juggling beyond the
//! borrow of the backing `&'data [u8]`.
//!
//! **Not yet hooked into the main loader.** Landing this module
//! first (with round-trip tests) gives the next session a green
//! foundation to slot a cache-lookup into `load_inputs`. Shipping
//! the wiring before the format is settled would be the same shape
//! of risk that bit us on the Mach-O umbrella regression — a
//! correctness-critical change fused with a storage-format churn.

use std::mem::size_of;
use std::path::Path;
use std::path::PathBuf;

/// 8-byte magic at the head of every per-blob cache file. Distinct from
/// `WILDIH01` (the `.wild-hashes` side-car magic) so mixing the two
/// fails loudly at `load`.
const MAGIC: &[u8; 8] = b"WILDPI01";

/// Schema is hand-bumped whenever `CacheHeader` or `CachedSymbol`
/// grows/shrinks a field. Cache files carrying an older schema are
/// rejected cleanly and the caller falls back to re-parsing.
const SCHEMA: u32 = 1;

/// Alignment requirement for the whole file: we cast the symbol
/// region to `&[CachedSymbol]` which must land on an 8-byte
/// boundary. Since we control layout (header is 56 bytes = 8×7,
/// symbols start immediately after) this is free, but we assert it
/// at load time to be safe.
const REQUIRED_ALIGN: usize = 8;

/// Symbol kind tag. `u8` so it packs into `CachedSymbol` without
/// bloat; exhaustive on purpose so new variants force a schema bump.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CachedSymbolKind {
    Undefined = 0,
    Local = 1,
    /// Non-local, defined. Covers the usual "global" + "weak defined"
    /// cases; wild's `load_symbols` differentiates further via
    /// `flags`.
    Defined = 2,
}

impl CachedSymbolKind {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Undefined),
            1 => Some(Self::Local),
            2 => Some(Self::Defined),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CacheHeader {
    magic: [u8; 8],
    schema: u32,
    flags: u32,
    n_symbols: u64,
    symbols_off: u64,
    names_off: u64,
    names_len: u64,
}

const _: () = {
    // Size must be stable (changes force schema bump). Field
    // accounting: magic(8) + schema(4) + flags(4) + n_symbols(8)
    // + symbols_off(8) + names_off(8) + names_len(8) = 48.
    assert!(size_of::<CacheHeader>() == 48);
    // Alignment must not exceed the whole-file guarantee.
    assert!(std::mem::align_of::<CacheHeader>() <= REQUIRED_ALIGN);
};

#[repr(C)]
#[derive(Clone, Copy)]
struct CachedSymbol {
    name_off: u32,
    name_len: u32,
    hash: u64,
    flags: u32,
    kind: u8,
    _pad: [u8; 3],
}

const _: () = {
    assert!(size_of::<CachedSymbol>() == 24);
    assert!(std::mem::align_of::<CachedSymbol>() <= REQUIRED_ALIGN);
};

/// Zero-copy view over a cache buffer. Holds the mmap'd bytes by
/// reference and yields iterator entries that also borrow into the
/// same buffer.
pub(crate) struct CacheView<'data> {
    bytes: &'data [u8],
    header: &'data CacheHeader,
    symbols: &'data [CachedSymbol],
    names: &'data [u8],
}

/// One entry out of the cache, fully resolved — `name` is a slice
/// into the cache mmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CachedEntry<'data> {
    pub(crate) name: &'data [u8],
    pub(crate) hash: u64,
    pub(crate) flags: u32,
    pub(crate) kind: CachedSymbolKind,
}

impl<'data> CacheView<'data> {
    /// Validate + construct a view. Returns `None` on any mismatch —
    /// callers fall back to the re-parse path. We never `panic!`
    /// here: a stale/corrupt cache must never prevent the link.
    pub(crate) fn from_bytes(bytes: &'data [u8]) -> Option<Self> {
        if bytes.len() < size_of::<CacheHeader>() {
            return None;
        }
        if !(bytes.as_ptr() as usize).is_multiple_of(REQUIRED_ALIGN) {
            // `mmap` always returns page-aligned pointers so this
            // only trips for in-memory tests on misaligned buffers.
            return None;
        }
        let header = unsafe { &*(bytes.as_ptr() as *const CacheHeader) };
        if &header.magic != MAGIC {
            return None;
        }
        if header.schema != SCHEMA {
            return None;
        }
        let n = header.n_symbols as usize;
        let sym_start = header.symbols_off as usize;
        let sym_end = sym_start.checked_add(n.checked_mul(size_of::<CachedSymbol>())?)?;
        if sym_end > bytes.len() {
            return None;
        }
        if !sym_start.is_multiple_of(std::mem::align_of::<CachedSymbol>()) {
            return None;
        }
        let names_start = header.names_off as usize;
        let names_end = names_start.checked_add(header.names_len as usize)?;
        if names_end > bytes.len() {
            return None;
        }
        let symbols = unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().add(sym_start) as *const CachedSymbol, n)
        };
        let names = &bytes[names_start..names_end];
        Some(Self {
            bytes,
            header,
            symbols,
            names,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.header.n_symbols as usize
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Resolve one entry. Returns `None` only on corruption
    /// (out-of-range name slice or unknown kind tag).
    pub(crate) fn get(&self, idx: usize) -> Option<CachedEntry<'data>> {
        let s = self.symbols.get(idx)?;
        let off = s.name_off as usize;
        let len = s.name_len as usize;
        let name = self.names.get(off..off.checked_add(len)?)?;
        let kind = CachedSymbolKind::from_u8(s.kind)?;
        Some(CachedEntry {
            name,
            hash: s.hash,
            flags: s.flags,
            kind,
        })
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = CachedEntry<'data>> + '_ {
        (0..self.len()).filter_map(move |i| self.get(i))
    }

    /// Raw bytes backing this view. The canary path uses this to
    /// byte-compare a freshly-built cache blob against the on-disk
    /// copy without reconstructing it.
    pub(crate) fn as_bytes(&self) -> &'data [u8] {
        self.bytes
    }
}

/// Builder for a fresh cache. Accepts entries one by one and emits
/// a single `Vec<u8>` ready for `write`. Callers are responsible
/// for atomically replacing the old cache file (write-to-tmp,
/// rename) to avoid torn reads under racing links.
#[derive(Clone)]
pub(crate) struct CacheBuilder {
    entries: Vec<CachedSymbol>,
    names: Vec<u8>,
    // Dedup identical names so two symbols with the same string
    // share the same name_off/name_len pair. Saves a little space
    // and matches how symbol tables usually look (weak/strong pairs
    // sharing a name).
    name_map: hashbrown::HashMap<Vec<u8>, (u32, u32), foldhash::fast::FixedState>,
}

impl Default for CacheBuilder {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            names: Vec::new(),
            name_map: hashbrown::HashMap::with_hasher(Default::default()),
        }
    }
}

impl CacheBuilder {
    pub(crate) fn add(&mut self, name: &[u8], hash: u64, flags: u32, kind: CachedSymbolKind) {
        let (name_off, name_len) = match self.name_map.get(name) {
            Some(&p) => p,
            None => {
                let off = self.names.len() as u32;
                let len = name.len() as u32;
                self.names.extend_from_slice(name);
                self.name_map.insert(name.to_vec(), (off, len));
                (off, len)
            }
        };
        self.entries.push(CachedSymbol {
            name_off,
            name_len,
            hash,
            flags,
            kind: kind as u8,
            _pad: [0; 3],
        });
    }

    /// Serialise the cache WITHOUT consuming the builder. Used by the
    /// canary path, which needs to hold on to the bytes for an
    /// in-memory compare AND the bytes to persist afterwards. Has the
    /// same complexity as `finish()` — just clones out the inner
    /// vectors first. Not a hot path.
    pub(crate) fn clone_bytes(&self) -> Vec<u8> {
        self.clone().finish()
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        let header_size = size_of::<CacheHeader>();
        let sym_bytes = self.entries.len() * size_of::<CachedSymbol>();
        // Names go right after the symbol region. Pad symbol region
        // to 8 bytes (already aligned by construction — CachedSymbol
        // is 24 bytes, any multiple of 24 is also a multiple of 8).
        let symbols_off = header_size;
        let names_off = symbols_off + sym_bytes;
        let names_len = self.names.len();
        let total = names_off + names_len;

        let mut out = Vec::with_capacity(total);
        let header = CacheHeader {
            magic: *MAGIC,
            schema: SCHEMA,
            flags: 0,
            n_symbols: self.entries.len() as u64,
            symbols_off: symbols_off as u64,
            names_off: names_off as u64,
            names_len: names_len as u64,
        };
        let hdr_bytes = unsafe {
            std::slice::from_raw_parts(&header as *const CacheHeader as *const u8, header_size)
        };
        out.extend_from_slice(hdr_bytes);
        let sym_raw =
            unsafe { std::slice::from_raw_parts(self.entries.as_ptr() as *const u8, sym_bytes) };
        out.extend_from_slice(sym_raw);
        out.extend_from_slice(&self.names);
        out
    }
}

// =====================================================================
// Tier-1.5 — single-bundle cache format (schema v2 of the cache layer)
// =====================================================================
//
// The per-input file scheme above pays one mmap syscall per cached
// input. On bevy-dylib (1649 inputs) those syscalls dominate the
// read-path tax — the kernel's per-process VM-map lock serialises
// what looks like cheap parallel I/O.
//
// The bundle format below stores ALL of a link's parsed-input caches
// inside a single sidecar file at `<output>.wild-pi-cache`. One mmap
// gets the whole thing; an in-memory `HashMap<key, blob_slice>` does
// O(1) per-input lookup. Each blob inside the bundle is a complete
// v1 [`CacheView`] payload (with its own magic/schema), so the
// existing reader is reused unchanged.

/// Bundle file magic. Distinct from `WILDPI01` (per-input) and
/// `WILDIH01` (`.wild-hashes`) so accidentally feeding one to another
/// fails loudly.
const BUNDLE_MAGIC: &[u8; 8] = b"WILDPB02";

/// Bundle schema version. Bumped whenever `BundleHeader` /
/// `BundleTocEntry` change shape, or whenever the meaning of fields
/// changes. v1 of the cache layer stayed in `WILDPI01` files; the
/// bundle is `WILDPB02` from the start to make the version distinct.
const BUNDLE_SCHEMA: u32 = 2;

/// Per-blob alignment inside a bundle. v1 [`CacheView`] requires its
/// bytes to be 8-byte aligned for the symbol-array cast; we honour
/// that for every blob.
const BUNDLE_BLOB_ALIGN: usize = 8;

/// Length of the per-input key used as a TOC primary key — blake3 of
/// `(input_path, entry_id)` truncated to 16 bytes. 128 bits gives a
/// collision probability of ~1.5e-30 for 1k inputs; well below the
/// "ignore it" threshold.
pub(crate) const BUNDLE_KEY_LEN: usize = 16;

/// Header at the start of every bundle file. Field order matches the
/// disk layout exactly; `repr(C)` + 8-byte alignment is asserted at
/// build time.
#[repr(C)]
#[derive(Clone, Copy)]
struct BundleHeader {
    magic: [u8; 8],
    schema: u32,
    flags: u32,
    n_entries: u32,
    _pad: u32,
    toc_off: u64,
    blobs_off: u64,
    blobs_len: u64,
}

const _: () = {
    // Header layout: magic(8) + schema(4) + flags(4) + n_entries(4)
    // + _pad(4) + toc_off(8) + blobs_off(8) + blobs_len(8) = 48.
    assert!(size_of::<BundleHeader>() == 48);
    assert!(std::mem::align_of::<BundleHeader>() <= REQUIRED_ALIGN);
};

/// One TOC slot. `key` is opaque (blake3-128 of input + entry id);
/// `blob_off` is bytes from the start of the file; `blob_len` is the
/// blob's payload length (the next blob starts at the next 8-byte
/// boundary after `blob_off + blob_len`).
#[repr(C)]
#[derive(Clone, Copy)]
struct BundleTocEntry {
    key: [u8; BUNDLE_KEY_LEN],
    blob_off: u64,
    blob_len: u64,
}

const _: () = {
    // 16 + 8 + 8 = 32. Multiple of 8 so a TOC array placed on an
    // 8-byte boundary stays aligned for every entry.
    assert!(size_of::<BundleTocEntry>() == 32);
    assert!(std::mem::align_of::<BundleTocEntry>() <= REQUIRED_ALIGN);
};

/// Derive the bundle key for one input. Length-prefixed so
/// `(path="ab", entry="c")` and `(path="a", entry="bc")` can't alias.
pub(crate) fn bundle_key_for(input: &Path, entry_id: Option<&[u8]>) -> [u8; BUNDLE_KEY_LEN] {
    let mut h = blake3::Hasher::new();
    h.update(input.as_os_str().as_encoded_bytes());
    if let Some(id) = entry_id {
        h.update(&(id.len() as u64).to_le_bytes());
        h.update(id);
    } else {
        h.update(&0u64.to_le_bytes());
    }
    let full = h.finalize();
    let mut out = [0u8; BUNDLE_KEY_LEN];
    out.copy_from_slice(&full.as_bytes()[..BUNDLE_KEY_LEN]);
    out
}

/// Path to the bundle for a given output binary. The bundle lives
/// next to the output (not in `$XDG_CACHE_HOME`) because it's
/// per-output, not per-input — co-located with `.wild-hashes` for
/// the same output.
pub(crate) fn bundle_path_for_output(output: &Path) -> PathBuf {
    let mut p = output.to_path_buf();
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".wild-pi-cache");
    p.set_file_name(name);
    p
}

/// Read-only mmap-backed view of a bundle. The `HashMap` is built
/// once at load time so per-input lookups are O(1); blob slices
/// borrow into the leaked mmap and downcast trivially to any per-link
/// `'data` lifetime.
pub(crate) struct BundleView<'data> {
    #[allow(dead_code)]
    bytes: &'data [u8],
    index: hashbrown::HashMap<[u8; BUNDLE_KEY_LEN], &'data [u8], foldhash::fast::FixedState>,
}

impl<'data> BundleView<'data> {
    fn from_bytes(bytes: &'data [u8]) -> Option<Self> {
        if bytes.len() < size_of::<BundleHeader>() {
            return None;
        }
        if !(bytes.as_ptr() as usize).is_multiple_of(REQUIRED_ALIGN) {
            return None;
        }
        let header = unsafe { &*(bytes.as_ptr() as *const BundleHeader) };
        if &header.magic != BUNDLE_MAGIC {
            return None;
        }
        if header.schema != BUNDLE_SCHEMA {
            return None;
        }
        let n = header.n_entries as usize;
        let toc_off = header.toc_off as usize;
        let toc_end = toc_off.checked_add(n.checked_mul(size_of::<BundleTocEntry>())?)?;
        if toc_end > bytes.len() {
            return None;
        }
        if !toc_off.is_multiple_of(std::mem::align_of::<BundleTocEntry>()) {
            return None;
        }
        let blobs_off = header.blobs_off as usize;
        let blobs_end = blobs_off.checked_add(header.blobs_len as usize)?;
        if blobs_end > bytes.len() {
            return None;
        }
        let toc = unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().add(toc_off) as *const BundleTocEntry, n)
        };
        let mut index = hashbrown::HashMap::with_capacity_and_hasher(n, Default::default());
        for entry in toc {
            let off = entry.blob_off as usize;
            let len = entry.blob_len as usize;
            // Bounds-check each blob against the file. A bad TOC
            // entry rejects the WHOLE bundle so we never hand out a
            // partial / inconsistent view.
            let end = off.checked_add(len)?;
            if end > bytes.len() {
                return None;
            }
            let blob = &bytes[off..end];
            // Each blob must independently parse as a v1 CacheView —
            // this is the canary against silent format drift between
            // bundle writer and per-blob writer.
            CacheView::from_bytes(blob)?;
            index.insert(entry.key, blob);
        }
        Some(Self { bytes, index })
    }

    /// O(1) lookup for one input's cached blob, ready to feed
    /// [`CacheView::from_bytes`].
    pub(crate) fn lookup(&self, key: &[u8; BUNDLE_KEY_LEN]) -> Option<&'data [u8]> {
        self.index.get(key).copied()
    }

    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn raw_bytes(&self) -> &'data [u8] {
        self.bytes
    }
}

/// Try to mmap and validate the bundle for `output`. Returns a
/// leaked `&'static BundleView<'static>` so callers can share it
/// across rayon workers and `'data` lifetimes without lifetime
/// gymnastics. wild's a one-shot CLI — kernel reclaims the mapping
/// at process exit.
///
/// Any I/O / validation failure → `None`; callers fall through to
/// the re-parse path. Never returns an error: a stale or corrupt
/// bundle MUST NOT prevent linking.
pub(crate) fn try_load_bundle_view_mmap(output: &Path) -> Option<&'static BundleView<'static>> {
    let path = bundle_path_for_output(output);
    let file = std::fs::File::open(&path).ok()?;
    // SAFETY: opened read-only and immediately leaked, so the mapping
    // outlives every borrow we hand out and can't be mutated under
    // our feet.
    let mmap = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let leaked: &'static memmap2::Mmap = Box::leak(Box::new(mmap));
    let bytes: &'static [u8] = leaked.as_ref();
    let view = BundleView::from_bytes(bytes)?;
    Some(Box::leak(Box::new(view)))
}

/// Builder for a fresh bundle. Accepts (key, blob_bytes) pairs, then
/// emits the full bundle as one byte vec ready to write.
pub(crate) struct BundleBuilder {
    entries: Vec<([u8; BUNDLE_KEY_LEN], Vec<u8>)>,
}

impl BundleBuilder {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, key: [u8; BUNDLE_KEY_LEN], blob: Vec<u8>) {
        self.entries.push((key, blob));
    }

    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialise into bundle bytes. Sorts by key so the on-disk order
    /// is deterministic — useful for diffing two bundles bit-for-bit
    /// in correctness tests.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.entries.sort_by_key(|(k, _)| *k);
        let n = self.entries.len();
        let toc_off = size_of::<BundleHeader>();
        let toc_size = n * size_of::<BundleTocEntry>();
        let blobs_off_unaligned = toc_off + toc_size;
        let blobs_off = blobs_off_unaligned.next_multiple_of(BUNDLE_BLOB_ALIGN);

        // First pass: lay out blobs to determine total size and
        // per-entry offsets.
        let mut blob_layout: Vec<(u64, u64)> = Vec::with_capacity(n);
        let mut cursor = blobs_off;
        for (_, blob) in &self.entries {
            let off = cursor;
            let len = blob.len();
            blob_layout.push((off as u64, len as u64));
            cursor = (off + len).next_multiple_of(BUNDLE_BLOB_ALIGN);
        }
        let blobs_len = cursor - blobs_off;
        let total = blobs_off + blobs_len;

        let mut out = vec![0u8; total];

        // Header.
        let header = BundleHeader {
            magic: *BUNDLE_MAGIC,
            schema: BUNDLE_SCHEMA,
            flags: 0,
            n_entries: n as u32,
            _pad: 0,
            toc_off: toc_off as u64,
            blobs_off: blobs_off as u64,
            blobs_len: blobs_len as u64,
        };
        let hdr_bytes = unsafe {
            std::slice::from_raw_parts(
                &header as *const BundleHeader as *const u8,
                size_of::<BundleHeader>(),
            )
        };
        out[..size_of::<BundleHeader>()].copy_from_slice(hdr_bytes);

        // TOC.
        for (i, ((key, _), (off, len))) in self.entries.iter().zip(blob_layout.iter()).enumerate() {
            let entry = BundleTocEntry {
                key: *key,
                blob_off: *off,
                blob_len: *len,
            };
            let entry_bytes = unsafe {
                std::slice::from_raw_parts(
                    &entry as *const BundleTocEntry as *const u8,
                    size_of::<BundleTocEntry>(),
                )
            };
            let dst_off = toc_off + i * size_of::<BundleTocEntry>();
            out[dst_off..dst_off + size_of::<BundleTocEntry>()].copy_from_slice(entry_bytes);
        }

        // Blobs (with inter-blob 8-byte padding zeroed by the initial
        // `vec![0u8; total]` so we don't have to write padding bytes).
        for ((_, blob), (off, len)) in self.entries.iter().zip(blob_layout.iter()) {
            let off = *off as usize;
            let len = *len as usize;
            out[off..off + len].copy_from_slice(blob);
        }
        out
    }

    /// Atomically write to `path` (tmp + rename). Single-file write
    /// is small enough to do safely; the previous fan-out scheme had
    /// to skip rename for perf, but the bundle is one file so we
    /// keep the strict-atomic write.
    pub(crate) fn write_to(self, path: &Path) -> std::io::Result<()> {
        let bytes = self.finish();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("wild-pi-cache.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

impl Default for BundleBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Force an 8-byte aligned backing buffer so `from_bytes` accepts
    /// the test slice. On real use the mmap'd page is always
    /// page-aligned.
    #[repr(C, align(8))]
    #[allow(dead_code)]
    struct Aligned<const N: usize>([u8; N]);

    fn aligned(bytes: &[u8]) -> Box<[u8]> {
        // Copy into an over-aligned Vec. We ensure alignment by using
        // an 8-byte-aligned ZST prefix via `Box::from`.
        let layout = std::alloc::Layout::from_size_align(bytes.len().max(1), 8).unwrap();
        unsafe {
            let ptr = std::alloc::alloc(layout);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let slice = std::slice::from_raw_parts_mut(ptr, bytes.len());
            Box::from_raw(slice)
        }
    }

    #[test]
    fn roundtrip_single_symbol() {
        let mut b = CacheBuilder::default();
        b.add(b"_main", 0xdead_beef, 0x11, CachedSymbolKind::Defined);
        let bytes = b.finish();
        let buf = aligned(&bytes);
        let view = CacheView::from_bytes(&buf).expect("view");
        assert_eq!(view.len(), 1);
        let e = view.get(0).unwrap();
        assert_eq!(e.name, b"_main");
        assert_eq!(e.hash, 0xdead_beef);
        assert_eq!(e.flags, 0x11);
        assert_eq!(e.kind, CachedSymbolKind::Defined);
    }

    #[test]
    fn roundtrip_many_and_iter_order_preserved() {
        let mut b = CacheBuilder::default();
        let fixtures: &[(&[u8], u64, u32, CachedSymbolKind)] = &[
            (b"_start", 1, 0, CachedSymbolKind::Local),
            (b"_main", 2, 0x11, CachedSymbolKind::Defined),
            (b"_printf", 3, 0, CachedSymbolKind::Undefined),
            (b"", 4, 0, CachedSymbolKind::Defined), // empty name is valid
        ];
        for &(n, h, f, k) in fixtures {
            b.add(n, h, f, k);
        }
        let bytes = b.finish();
        let buf = aligned(&bytes);
        let view = CacheView::from_bytes(&buf).unwrap();
        let got: Vec<_> = view.iter().collect();
        assert_eq!(got.len(), fixtures.len());
        for (g, &(n, h, f, k)) in got.iter().zip(fixtures) {
            assert_eq!(g.name, n);
            assert_eq!(g.hash, h);
            assert_eq!(g.flags, f);
            assert_eq!(g.kind, k);
        }
    }

    #[test]
    fn name_dedup_shares_bytes() {
        // Two symbols with the same name should share the names
        // region — confirms the builder actually dedups.
        let mut b = CacheBuilder::default();
        b.add(b"_shared", 1, 0, CachedSymbolKind::Defined);
        b.add(b"_shared", 2, 0, CachedSymbolKind::Local);
        let bytes = b.finish();
        let buf = aligned(&bytes);
        let view = CacheView::from_bytes(&buf).unwrap();
        assert_eq!(view.len(), 2);
        // Names region should contain "_shared" exactly once.
        let names_off = view.header.names_off as usize;
        let names_len = view.header.names_len as usize;
        assert_eq!(names_len, b"_shared".len());
        assert_eq!(&buf[names_off..names_off + names_len], b"_shared");
    }

    #[test]
    fn rejects_bad_magic() {
        let mut b = CacheBuilder::default();
        b.add(b"_x", 1, 0, CachedSymbolKind::Defined);
        let mut bytes = b.finish();
        // Flip one magic byte.
        bytes[0] ^= 1;
        let buf = aligned(&bytes);
        assert!(CacheView::from_bytes(&buf).is_none());
    }

    #[test]
    fn rejects_bad_schema() {
        let mut b = CacheBuilder::default();
        b.add(b"_x", 1, 0, CachedSymbolKind::Defined);
        let mut bytes = b.finish();
        // Bump schema field past known value. Layout: magic[8] then
        // schema u32 little-endian at offset 8.
        bytes[8] = (SCHEMA + 1) as u8;
        let buf = aligned(&bytes);
        assert!(CacheView::from_bytes(&buf).is_none());
    }

    #[test]
    fn rejects_truncated_buffer() {
        let mut b = CacheBuilder::default();
        b.add(b"_truncated", 1, 0, CachedSymbolKind::Defined);
        let bytes = b.finish();
        // Drop the last 5 bytes of the names region.
        let truncated = &bytes[..bytes.len() - 5];
        let buf = aligned(truncated);
        assert!(CacheView::from_bytes(&buf).is_none());
    }

    #[test]
    fn rejects_misaligned_buffer() {
        // Force a buffer that begins at an odd address. We copy the
        // cache into a Vec and then view it starting one byte in —
        // guaranteed misaligned.
        let mut b = CacheBuilder::default();
        b.add(b"_x", 1, 0, CachedSymbolKind::Defined);
        let bytes = b.finish();
        let mut padded = Vec::with_capacity(bytes.len() + 1);
        padded.push(0u8);
        padded.extend_from_slice(&bytes);
        // The real cache starts at padded[1..]; its ptr is
        // padded.as_ptr() + 1, which is odd-aligned.
        let view_bytes = &padded[1..];
        assert!(CacheView::from_bytes(view_bytes).is_none());
    }

    #[test]
    fn unknown_kind_tag_returns_none_from_get() {
        // Build a valid cache, then poke an invalid kind byte.
        let mut b = CacheBuilder::default();
        b.add(b"_x", 1, 0, CachedSymbolKind::Defined);
        let mut bytes = b.finish();
        // Find the symbol region and overwrite the kind byte with 99.
        let hdr_size = size_of::<CacheHeader>();
        // CachedSymbol layout: name_off(4) name_len(4) hash(8) flags(4) kind(1)
        let kind_off = hdr_size + 4 + 4 + 8 + 4;
        bytes[kind_off] = 99;
        let buf = aligned(&bytes);
        let view = CacheView::from_bytes(&buf).expect("structure still valid");
        assert_eq!(view.len(), 1);
        // Per-entry `get` reports None rather than panicking.
        assert!(view.get(0).is_none());
    }

    #[test]
    fn empty_cache_roundtrips() {
        let b = CacheBuilder::default();
        let bytes = b.finish();
        let buf = aligned(&bytes);
        let view = CacheView::from_bytes(&buf).unwrap();
        assert!(view.is_empty());
        assert_eq!(view.iter().count(), 0);
    }

    #[test]
    fn bundle_round_trip_persists_and_reloads() {
        // End-to-end: build two blobs, push them into a bundle, write
        // it to disk, read back via `BundleView::from_bytes`, look
        // each up by key, replay through `CacheView::from_bytes` and
        // confirm the symbol records survived intact.
        let blob_a = {
            let mut b = CacheBuilder::default();
            b.add(b"_a", 1, 0, CachedSymbolKind::Defined);
            b.finish()
        };
        let blob_b = {
            let mut b = CacheBuilder::default();
            b.add(b"_b", 2, 0x10, CachedSymbolKind::Undefined);
            b.finish()
        };
        let key_a = bundle_key_for(Path::new("/fixture/a.o"), None);
        let key_b = bundle_key_for(Path::new("/fixture/b.o"), None);
        let mut bundle = BundleBuilder::new();
        bundle.push(key_a, blob_a);
        bundle.push(key_b, blob_b);

        // Write to a unique temp output path; bundle_path_for_output
        // appends `.wild-pi-cache`.
        let tmp_out =
            std::env::temp_dir().join(format!("wild-bundle-rt-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&tmp_out);
        let bundle_path = bundle_path_for_output(&tmp_out);
        let _ = std::fs::remove_file(&bundle_path);
        bundle.write_to(&bundle_path).expect("bundle write");

        // tmp+rename atomicity: no .tmp left behind on success.
        let leftover = bundle_path.with_extension("wild-pi-cache.tmp");
        assert!(!leftover.exists(), "leftover tmp at {leftover:?}");

        let bytes = std::fs::read(&bundle_path).unwrap();
        let buf = aligned(&bytes);
        let view = BundleView::from_bytes(&buf).expect("view");
        assert_eq!(view.len(), 2);

        for (key, expected_name) in [(key_a, &b"_a"[..]), (key_b, &b"_b"[..])] {
            let blob = view.lookup(&key).expect("lookup");
            let cv = CacheView::from_bytes(blob).expect("blob view");
            assert_eq!(cv.len(), 1);
            assert_eq!(cv.get(0).unwrap().name, expected_name);
        }
        let _ = std::fs::remove_file(&bundle_path);
    }

    #[test]
    fn bundle_rejects_bad_magic() {
        let mut bundle = BundleBuilder::new();
        let blob = {
            let mut b = CacheBuilder::default();
            b.add(b"_x", 1, 0, CachedSymbolKind::Defined);
            b.finish()
        };
        bundle.push(bundle_key_for(Path::new("/x"), None), blob);
        let mut bytes = bundle.finish();
        bytes[0] ^= 1;
        let buf = aligned(&bytes);
        assert!(BundleView::from_bytes(&buf).is_none());
    }

    #[test]
    fn bundle_rejects_truncated() {
        let mut bundle = BundleBuilder::new();
        let blob = {
            let mut b = CacheBuilder::default();
            b.add(b"_x", 1, 0, CachedSymbolKind::Defined);
            b.finish()
        };
        bundle.push(bundle_key_for(Path::new("/x"), None), blob);
        let bytes = bundle.finish();
        let truncated = &bytes[..bytes.len() - 5];
        let buf = aligned(truncated);
        assert!(BundleView::from_bytes(&buf).is_none());
    }

    #[test]
    fn bundle_path_appends_extension() {
        let p = bundle_path_for_output(Path::new("/tmp/myapp"));
        assert_eq!(p, Path::new("/tmp/myapp.wild-pi-cache"));
        let p = bundle_path_for_output(Path::new("/tmp/myapp.dylib"));
        assert_eq!(p, Path::new("/tmp/myapp.dylib.wild-pi-cache"));
    }

    #[test]
    fn bundle_lookup_misses_unknown_key() {
        let mut bundle = BundleBuilder::new();
        let blob = {
            let mut b = CacheBuilder::default();
            b.add(b"_x", 1, 0, CachedSymbolKind::Defined);
            b.finish()
        };
        let known = bundle_key_for(Path::new("/known"), None);
        bundle.push(known, blob);
        let bytes = bundle.finish();
        let buf = aligned(&bytes);
        let view = BundleView::from_bytes(&buf).unwrap();
        assert!(view.lookup(&known).is_some());
        let absent = bundle_key_for(Path::new("/absent"), None);
        assert!(view.lookup(&absent).is_none());
    }

    #[test]
    fn bundle_key_is_collision_free_for_same_basename() {
        // The `libfoo-<hash>.rlib` cargo convention means multiple
        // inputs with the same basename live in different dirs.
        // bundle_key_for must disambiguate via full-path hashing.
        let a = bundle_key_for(Path::new("/tmp/build-a/libfoo-abc.rlib"), None);
        let b = bundle_key_for(Path::new("/tmp/build-b/libfoo-abc.rlib"), None);
        assert_ne!(
            a, b,
            "same-basename inputs from different dirs produced the same bundle key"
        );
        // Same input twice → same key.
        let a2 = bundle_key_for(Path::new("/tmp/build-a/libfoo-abc.rlib"), None);
        assert_eq!(a, a2, "identical input path produced different bundle keys");
        // Same archive file, different member entries → distinct keys.
        let m1 = bundle_key_for(Path::new("/tmp/foo.rlib"), Some(b"first.o"));
        let m2 = bundle_key_for(Path::new("/tmp/foo.rlib"), Some(b"second.o"));
        assert_ne!(
            m1, m2,
            "archive-entry-disambiguated bundle keys unexpectedly collided"
        );
        // Member vs. whole-archive disambiguation: the archive-entry
        // case must also differ from the no-entry case.
        let whole = bundle_key_for(Path::new("/tmp/foo.rlib"), None);
        assert_ne!(whole, m1, "entry-absent bundle key collided with a member");
        // Length-prefix discipline: ("ab", c) and ("a", bc) must NOT
        // alias even though their concatenation does.
        let p1 = bundle_key_for(Path::new("/ab"), Some(b"c"));
        let p2 = bundle_key_for(Path::new("/a"), Some(b"bc"));
        assert_ne!(p1, p2, "length-prefix bundle keys aliased on concatenation");
    }

    #[test]
    fn names_are_zero_copy_into_backing_buffer() {
        // Stress the zero-copy property: the name slice returned by
        // `get` must point inside the cache bytes, not into some
        // heap-allocated String.
        let mut b = CacheBuilder::default();
        b.add(b"_zcopy", 0, 0, CachedSymbolKind::Defined);
        let bytes = b.finish();
        let buf = aligned(&bytes);
        let view = CacheView::from_bytes(&buf).unwrap();
        let entry = view.get(0).unwrap();
        let name_ptr = entry.name.as_ptr() as usize;
        let buf_start = buf.as_ptr() as usize;
        let buf_end = buf_start + buf.len();
        assert!(
            (buf_start..buf_end).contains(&name_ptr),
            "name slice at {name_ptr:#x} outside buffer [{buf_start:#x}..{buf_end:#x})"
        );
    }
}
