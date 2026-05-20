//! Global cache for SDK `.tbd` symbol sets.
//!
//! `args.parse` on macOS walks `<sysroot>/usr/lib/libSystem.tbd` and
//! every `.tbd` in `<sysroot>/usr/lib/system/` the first time any of
//! `-lSystem` / `-lc` / `-lm` / `-lpthread` is seen. That walk parses
//! ~100 text-stub-library records and populates a set of tens of
//! thousands of symbol names; measured cost on M-series aarch64 is
//! ~60 ms per cold link.
//!
//! Those bytes on disk change essentially never — the SDK ships
//! with the OS / Xcode Command Line Tools and rev-bumps on major
//! macOS releases. Caching the symbol set globally, keyed on the
//! sysroot path + `libSystem.tbd`'s `(size, mtime)`, lets every
//! `wild` invocation after the first skip the walk entirely.
//!
//! The cache is intentionally process-wide, not per-output: every
//! wild invocation targeting the same SDK benefits. A single
//! cache file per sysroot lives under
//! `$XDG_CACHE_HOME/wild/sdk-<hex>.bin` (or `~/.cache/wild/...` when
//! the env var is unset).
//!
//! Schema v1. The cache-header ends with the `(size, mtime_ns)` of
//! `libSystem.tbd` at the time of write; the loader rejects any
//! cache whose current `libSystem.tbd` stats have drifted, forcing
//! a fresh walk.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Read as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

const SDK_CACHE_MAGIC: &[u8; 8] = b"WILDSDK1";
const SDK_CACHE_SCHEMA: u32 = 1;

/// Process-resident in-memory cache of parsed TBD symbol sets, keyed
/// on the `.tbd` path. Saves the per-link disk-cache load on every
/// `link_framework` call.
///
/// On a bevy-class link the disk-backed cache costs ~1.5 ms per TBD
/// (open file + headers + symbol decode); 17 frameworks turns that
/// into the 26 ms `pending frameworks` stage observed in profiles of
/// the in-process daemon. Hoisting to a process-resident map drops
/// the second-and-subsequent invocations of `load_tbd_symbols` on
/// the same path to a single hash lookup + stat.
///
/// Why path-keyed (not (path, size, mtime)-keyed): the cache value
/// records `(size, mtime_ns)` of the source TBD at the time of the
/// most recent disk-cache load; on lookup we re-stat the source and
/// invalidate if either drifted. That preserves the same correctness
/// invariant `load_tbd_symbols` already enforces against the disk
/// cache, without forcing the caller to compute a key.
///
/// We store `Arc` rather than the bare value so concurrent callers
/// don't clone the symbol set (which is a HashSet<Vec<u8>> with ~10k
/// entries on macOS) — they share a frozen view.
struct TbdMemEntry {
    size: u64,
    mtime_ns: i128,
    install_name: Option<Vec<u8>>,
    symbols: Arc<crate::args::macho::DylibSymbols>,
}

static TBD_MEM_CACHE: Mutex<Option<HashMap<PathBuf, TbdMemEntry>>> = Mutex::new(None);

/// Same idea for the system-wide SDK symbol bundle. One sysroot per
/// machine in practice, but keep keying explicit so a daemon serving
/// multiple toolchains stays correct.
struct SdkMemEntry {
    libsystem_size: u64,
    libsystem_mtime_ns: i128,
    symbols: Arc<crate::args::macho::DylibSymbols>,
}

static SDK_MEM_CACHE: Mutex<Option<HashMap<PathBuf, SdkMemEntry>>> = Mutex::new(None);

fn stat_size_mtime(path: &Path) -> Option<(u64, i128)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime_ns = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    Some((md.len(), mtime_ns))
}

/// Root directory for wild's on-disk caches. Respects
/// `$XDG_CACHE_HOME` on Linux/macOS; falls back to `~/.cache/wild/`
/// per the freedesktop spec.
pub(crate) fn cache_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        let p: PathBuf = PathBuf::from(xdg);
        if !p.as_os_str().is_empty() {
            return Some(p.join("wild"));
        }
    }
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".cache").join("wild"))
}

/// Filename for the SDK symbol cache belonging to a particular
/// sysroot. The 32-byte blake3 over the sysroot path bytes is
/// abbreviated to 16 hex chars — plenty of entropy for collision
/// resistance on a single machine's filesystem, short enough to
/// keep directory listings tidy.
pub(crate) fn sdk_cache_path(sysroot: &Path) -> Option<PathBuf> {
    let dir = cache_dir()?;
    let tag = blake3::hash(sysroot.as_os_str().as_encoded_bytes());
    let hex = &tag.to_hex().to_string()[..16];
    Some(dir.join(format!("sdk-{hex}.bin")))
}

/// Read the SDK symbol cache for a given sysroot, if present and
/// still valid. "Valid" means the cached `(size, mtime_ns)` of
/// `libSystem.tbd` matches what `stat` reports on disk right now.
/// Anything else (missing file, short read, schema drift, stat
/// drift) returns `None` and the caller falls through to the
/// full walk.
pub(crate) fn load_sdk_symbols(sysroot: &Path) -> Option<crate::args::macho::DylibSymbols> {
    // Process-resident fast path. As with `load_tbd_symbols`, the
    // disk-backed cache itself costs ~3 ms to deserialise on a
    // bevy-class link; for the daemon's in-process repeated links
    // we want zero file I/O.
    let libsystem = sysroot.join("usr/lib/libSystem.tbd");
    let now_stat = stat_size_mtime(&libsystem);
    if let Some((cur_size, cur_mtime)) = now_stat {
        let guard = SDK_MEM_CACHE.lock().expect("sdk mem cache poisoned");
        if let Some(map) = guard.as_ref()
            && let Some(entry) = map.get(sysroot)
            && entry.libsystem_size == cur_size
            && entry.libsystem_mtime_ns == cur_mtime
        {
            return Some((*entry.symbols).clone());
        }
    }

    let cache_path = sdk_cache_path(sysroot)?;
    let mut f = std::fs::File::open(&cache_path).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    if buf.len() < 8 + 4 + 8 + 16 + 4 {
        return None;
    }
    if &buf[0..8] != SDK_CACHE_MAGIC {
        return None;
    }
    let schema = u32::from_le_bytes(buf[8..12].try_into().ok()?);
    if schema != SDK_CACHE_SCHEMA {
        return None;
    }
    let cached_size = u64::from_le_bytes(buf[12..20].try_into().ok()?);
    let cached_mtime = i128::from_le_bytes(buf[20..36].try_into().ok()?);
    // Re-stat libSystem.tbd now — if anything drifted, reject.
    let libsystem = sysroot.join("usr/lib/libSystem.tbd");
    let md = std::fs::metadata(&libsystem).ok()?;
    let mtime = md.modified().ok()?;
    let cur_mtime_ns = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    if md.len() != cached_size || cur_mtime_ns != cached_mtime {
        return None;
    }
    let count = u32::from_le_bytes(buf[36..40].try_into().ok()?) as usize;
    let mut cursor = 40;
    let mut out: crate::args::macho::DylibSymbols =
        hashbrown::HashSet::with_capacity_and_hasher(count, Default::default());
    for _ in 0..count {
        if cursor + 2 > buf.len() {
            return None;
        }
        let len = u16::from_le_bytes(buf[cursor..cursor + 2].try_into().ok()?) as usize;
        cursor += 2;
        if cursor + len > buf.len() {
            return None;
        }
        out.insert(Arc::<[u8]>::from(&buf[cursor..cursor + len]));
        cursor += len;
    }

    // Populate the in-memory cache for the next link.
    if let Some((cur_size, cur_mtime)) = stat_size_mtime(&libsystem) {
        let mut guard = SDK_MEM_CACHE.lock().expect("sdk mem cache poisoned");
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(
            sysroot.to_path_buf(),
            SdkMemEntry {
                libsystem_size: cur_size,
                libsystem_mtime_ns: cur_mtime,
                symbols: Arc::new(out.clone()),
            },
        );
    }

    Some(out)
}

/// Persist the SDK symbol set after a successful walk. Best-effort:
/// failures (cache dir not writable, quota, etc.) are logged to
/// stderr under `WILD_INCREMENTAL_DEBUG` but never fail the link —
/// a skipped write just means the next invocation walks again.
pub(crate) fn save_sdk_symbols(sysroot: &Path, symbols: &crate::args::macho::DylibSymbols) {
    let Some(cache_path) = sdk_cache_path(sysroot) else {
        return;
    };
    let libsystem = sysroot.join("usr/lib/libSystem.tbd");
    let Ok(md) = std::fs::metadata(&libsystem) else {
        return;
    };
    let size = md.len();
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i128);
    // Sorted symbols → determinism. Byte-equal cache across runs
    // makes CI diff-tooling possible and enables future content-hash
    // caching of the cache itself.
    let mut sorted: Vec<&[u8]> = symbols.iter().map(|s| s.as_ref()).collect();
    sorted.sort();
    let mut buf = Vec::with_capacity(40 + symbols.iter().map(|s| s.len() + 2).sum::<usize>());
    buf.extend_from_slice(SDK_CACHE_MAGIC);
    buf.extend_from_slice(&SDK_CACHE_SCHEMA.to_le_bytes());
    buf.extend_from_slice(&size.to_le_bytes());
    buf.extend_from_slice(&mtime_ns.to_le_bytes());
    buf.extend_from_slice(&(symbols.len() as u32).to_le_bytes());
    for sym in &sorted {
        if sym.len() > u16::MAX as usize {
            continue; // pathological
        }
        buf.extend_from_slice(&(sym.len() as u16).to_le_bytes());
        buf.extend_from_slice(sym);
    }
    // Atomic create via <file>.tmp + rename. Concurrent wild
    // invocations (cargo builds with -j > 1) won't observe a torn
    // file — the rename is atomic on POSIX filesystems.
    let tmp = cache_path.with_extension("bin.tmp");
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all().ok();
        std::fs::rename(&tmp, &cache_path)?;
        Ok(())
    })();
    if let Err(e) = write_result {
        if std::env::var_os("WILD_INCREMENTAL_DEBUG").is_some() {
            eprintln!(
                "wild sdk-cache: failed to persist {}: {}",
                cache_path.display(),
                e
            );
        }
        let _ = std::fs::remove_file(&tmp);
    }
}

// ---------------------------------------------------------------------------
// Per-file TBD cache — same idea as the SDK cache, but keyed on an
// arbitrary `.tbd` path. Used for framework TBDs (CoreFoundation,
// CoreServices, …) and any other `.tbd` file whose symbol set would
// otherwise be re-parsed each link via yaml_rust. Profile on rust-
// analyzer showed YAML scanning dominating wild's hot path at 85%
// of leaf samples; each framework TBD is ~100 KiB of text-stub-
// library YAML that takes ~5-15 ms to parse.
// ---------------------------------------------------------------------------

const TBD_CACHE_MAGIC: &[u8; 8] = b"WILDTBD1";
const TBD_CACHE_SCHEMA: u32 = 1;

/// Filename for a per-file TBD cache. Hashing the TBD path (rather
/// than its basename) avoids collisions when e.g. two SDKs supply
/// different `Foundation.tbd` versions side by side on the same
/// machine.
fn tbd_cache_path(tbd_path: &Path) -> Option<PathBuf> {
    let dir = cache_dir()?.join("tbd");
    let tag = blake3::hash(tbd_path.as_os_str().as_encoded_bytes());
    let hex = &tag.to_hex().to_string()[..16];
    Some(dir.join(format!("{hex}.bin")))
}

/// Load the cached `(install_name, symbols)` pair for a `.tbd` file
/// if the on-disk file's `(size, mtime_ns)` still matches what was
/// recorded.
///
/// Returns the symbols as an `Arc<DylibSymbols>` so the in-process
/// daemon's mem-cache fast path can hand back a refcount-bumped
/// share rather than a full HashSet clone. Disk-cache hits wrap a
/// freshly-deserialised set in a fresh `Arc`, so the caller's API
/// stays uniform across the two paths.
pub(crate) fn load_tbd_symbols(
    tbd_path: &Path,
) -> Option<(Option<Vec<u8>>, Arc<crate::args::macho::DylibSymbols>)> {
    // Process-resident fast path: short-circuits the disk-backed
    // cache load entirely when the same daemon already decoded this
    // TBD on a previous link AND the source `.tbd` hasn't moved
    // since (re-stat below catches any drift). On a bevy-class link
    // this turns the 26 ms `pending frameworks` stage into ~1 ms.
    let now_stat = stat_size_mtime(tbd_path);
    if let Some((cur_size, cur_mtime)) = now_stat {
        let guard = TBD_MEM_CACHE.lock().expect("tbd mem cache poisoned");
        if let Some(map) = guard.as_ref()
            && let Some(entry) = map.get(tbd_path)
            && entry.size == cur_size
            && entry.mtime_ns == cur_mtime
        {
            // Refcount bump only — no HashSet clone. The caller
            // iterates into its own `dylib_symbols` set.
            return Some((entry.install_name.clone(), Arc::clone(&entry.symbols)));
        }
        drop(guard);
    }

    let cache_path = tbd_cache_path(tbd_path)?;
    let mut f = std::fs::File::open(&cache_path).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    if buf.len() < 8 + 4 + 8 + 16 + 2 + 4 {
        return None;
    }
    if &buf[0..8] != TBD_CACHE_MAGIC {
        return None;
    }
    let schema = u32::from_le_bytes(buf[8..12].try_into().ok()?);
    if schema != TBD_CACHE_SCHEMA {
        return None;
    }
    let cached_size = u64::from_le_bytes(buf[12..20].try_into().ok()?);
    let cached_mtime = i128::from_le_bytes(buf[20..36].try_into().ok()?);
    let md = std::fs::metadata(tbd_path).ok()?;
    let mtime_ns = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    if md.len() != cached_size || mtime_ns != cached_mtime {
        return None;
    }
    let inst_len = u16::from_le_bytes(buf[36..38].try_into().ok()?) as usize;
    let mut cursor = 38;
    let install_name = if inst_len == 0 {
        None
    } else {
        if cursor + inst_len > buf.len() {
            return None;
        }
        let v = buf[cursor..cursor + inst_len].to_vec();
        cursor += inst_len;
        Some(v)
    };
    if cursor + 4 > buf.len() {
        return None;
    }
    let count = u32::from_le_bytes(buf[cursor..cursor + 4].try_into().ok()?) as usize;
    cursor += 4;
    let mut symbols: crate::args::macho::DylibSymbols =
        hashbrown::HashSet::with_capacity_and_hasher(count, Default::default());
    for _ in 0..count {
        if cursor + 2 > buf.len() {
            return None;
        }
        let len = u16::from_le_bytes(buf[cursor..cursor + 2].try_into().ok()?) as usize;
        cursor += 2;
        if cursor + len > buf.len() {
            return None;
        }
        symbols.insert(Arc::<[u8]>::from(&buf[cursor..cursor + len]));
        cursor += len;
    }

    // Wrap in `Arc` and populate the in-memory cache. The same `Arc`
    // is shared with the caller and stored — no extra HashSet copy.
    let symbols_arc = Arc::new(symbols);
    if let Some((cur_size, cur_mtime)) = stat_size_mtime(tbd_path) {
        let mut guard = TBD_MEM_CACHE.lock().expect("tbd mem cache poisoned");
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(
            tbd_path.to_path_buf(),
            TbdMemEntry {
                size: cur_size,
                mtime_ns: cur_mtime,
                install_name: install_name.clone(),
                symbols: Arc::clone(&symbols_arc),
            },
        );
    }

    Some((install_name, symbols_arc))
}

/// Persist the parsed `(install_name, symbols)` pair for a `.tbd`
/// file. Best-effort; failures are only visible under
/// `WILD_INCREMENTAL_DEBUG`.
pub(crate) fn save_tbd_symbols(
    tbd_path: &Path,
    install_name: Option<&[u8]>,
    symbols: &crate::args::macho::DylibSymbols,
) {
    let Some(cache_path) = tbd_cache_path(tbd_path) else {
        return;
    };
    let Ok(md) = std::fs::metadata(tbd_path) else {
        return;
    };
    let size = md.len();
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i128);
    let mut sorted: Vec<&[u8]> = symbols.iter().map(|s| s.as_ref()).collect();
    sorted.sort();
    let inst_bytes = install_name.unwrap_or(&[]);
    if inst_bytes.len() > u16::MAX as usize {
        return;
    }
    let mut buf = Vec::with_capacity(
        38 + inst_bytes.len() + 4 + symbols.iter().map(|s| s.len() + 2).sum::<usize>(),
    );
    buf.extend_from_slice(TBD_CACHE_MAGIC);
    buf.extend_from_slice(&TBD_CACHE_SCHEMA.to_le_bytes());
    buf.extend_from_slice(&size.to_le_bytes());
    buf.extend_from_slice(&mtime_ns.to_le_bytes());
    buf.extend_from_slice(&(inst_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(inst_bytes);
    buf.extend_from_slice(&(symbols.len() as u32).to_le_bytes());
    for sym in &sorted {
        if sym.len() > u16::MAX as usize {
            continue;
        }
        buf.extend_from_slice(&(sym.len() as u16).to_le_bytes());
        buf.extend_from_slice(sym);
    }
    let tmp = cache_path.with_extension("bin.tmp");
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all().ok();
        std::fs::rename(&tmp, &cache_path)?;
        Ok(())
    })();
    if let Err(e) = res {
        if std::env::var_os("WILD_INCREMENTAL_DEBUG").is_some() {
            eprintln!(
                "wild tbd-cache: failed to write {}: {}",
                cache_path.display(),
                e
            );
        }
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialises tests that read or mutate `XDG_CACHE_HOME` (process-global).
    // Without this, `cache_dir_respects_xdg` can shift the env mid-flight of
    // `sdk_cache_path_stable_per_sysroot` and the two reads disagree.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn cache_dir_respects_xdg() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Preserve state across tests.
        let old_xdg = std::env::var_os("XDG_CACHE_HOME");
        // SAFETY: ENV_LOCK serialises all env mutation in this module's tests.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", "/custom/cache");
        }
        let got = cache_dir();
        assert_eq!(got, Some(PathBuf::from("/custom/cache/wild")));
        // Restore.
        unsafe {
            match old_xdg {
                Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
        }
    }

    #[test]
    fn sdk_cache_path_stable_per_sysroot() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let a = sdk_cache_path(Path::new("/Library/Developer/SDK/A.sdk"));
        let a2 = sdk_cache_path(Path::new("/Library/Developer/SDK/A.sdk"));
        let b = sdk_cache_path(Path::new("/Library/Developer/SDK/B.sdk"));
        assert_eq!(a, a2);
        assert_ne!(a, b);
    }

    #[test]
    fn load_returns_none_for_missing() {
        let nonexistent = PathBuf::from("/nonexistent-sysroot-for-test");
        assert!(load_sdk_symbols(&nonexistent).is_none());
    }
}
