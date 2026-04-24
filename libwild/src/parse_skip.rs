//! Tier-1 incremental-linking plumbing around the [`SymbolSink`] trait.
//!
//! The symbol-load loop in [`symbol_db`](crate::symbol_db) produces a
//! stream of three operations per input file — [`SymbolSink::set_next`],
//! [`SymbolSink::add_non_versioned`], [`SymbolSink::add_versioned`].
//! This module provides the shims tier-1 needs:
//!
//! * [`TeeSink`] — forwards every op to an inner sink *and* records it
//!   into a [`CacheBuilder`]. The write path uses this to snapshot the
//!   parse of a clean input.
//! * [`CaptureSink`] — records the op stream into a
//!   `Vec<StreamOp>` without writing anywhere else. Used by the canary
//!   to diff the re-parse against the cache replay.
//! * [`replay_cached_symbols`] — reads a cached stream and replays it
//!   back into a sink, reproducing the original `(shard, outputs)`
//!   effects without re-iterating the object crate.
//!
//! ## What the v1 cache schema captures
//!
//! [`CachedSymbolKind::Undefined`] / [`CachedSymbolKind::Local`] /
//! [`CachedSymbolKind::Defined`] tags a symbol by its sink-op shape:
//!   * `Undefined` — only a `set_next(flags, UNDEF, file_id)`; no adds.
//!   * `Local`     — only a `set_next(flags, symbol_id, file_id)`; no adds.
//!   * `Defined`   — an `add_non_versioned(name)` followed by a
//!                   `set_next(flags, symbol_id, file_id)`.
//!
//! Mach-O `RawSymbolName::version_name()` always returns `None` and
//! `is_default()` always returns `true`, so the v1 schema is lossless
//! for Mach-O inputs (the canary validates this claim per-link).
//!
//! ELF *can* have versioned symbols (`add_versioned`) which the v1
//! schema doesn't capture. Under the canary, such inputs will surface
//! as a stream mismatch and the link will panic rather than silently
//! shipping a bad binary — by design. A follow-up schema bump adds
//! versioned-symbol support.

use crate::input_data::FileId;
use crate::parsed_input_cache::CacheBuilder;
use crate::parsed_input_cache::CacheView;
use crate::parsed_input_cache::CachedSymbolKind;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::PendingSymbol;
use crate::symbol_db::PendingVersionedSymbol;
use crate::symbol_db::SymbolId;
use crate::symbol_db::SymbolSink;
use crate::value_flags::ValueFlags;

/// Sink wrapper that forwards every op to `inner` and, when `cache` is
/// `Some`, also records it into a [`CacheBuilder`]. Ownership of the
/// cache buffer is parked inside the TeeSink; after parsing, callers
/// `take_cache()` to retrieve the built blob for disk persistence.
///
/// Takes `inner` as `&mut dyn SymbolSink<'data>` rather than a generic
/// so the write path can wrap an existing [`DefaultSymbolSink`] per
/// object without threading an extra type parameter through
/// `read_symbols_for_group`.
pub(crate) struct TeeSink<'a, 'data> {
    inner: &'a mut dyn SymbolSink<'data>,
    cache: Option<CacheBuilder>,
    /// State machine between `add_*` and `set_next`. A per-symbol `add`
    /// stashes the name here so the subsequent `set_next` can emit a
    /// single cache entry tagged `Defined`. For `Undefined` / `Local`
    /// symbols the slot is `None` at `set_next` time.
    pending_name: Option<PendingCachedName<'data>>,
}

#[derive(Clone, Copy)]
struct PendingCachedName<'data> {
    name: &'data [u8],
    hash: u64,
}

impl<'a, 'data> TeeSink<'a, 'data> {
    pub(crate) fn new(inner: &'a mut dyn SymbolSink<'data>, cache: Option<CacheBuilder>) -> Self {
        Self {
            inner,
            cache,
            pending_name: None,
        }
    }

    /// Retrieve the cache builder for persistence. The TeeSink is
    /// effectively drained after this — subsequent sink ops still
    /// forward to `inner` but stop writing to the cache.
    pub(crate) fn take_cache(&mut self) -> Option<CacheBuilder> {
        self.cache.take()
    }
}

impl<'a, 'data> SymbolSink<'data> for TeeSink<'a, 'data> {
    fn next_symbol_id(&self) -> SymbolId {
        self.inner.next_symbol_id()
    }

    fn set_next(&mut self, flags: ValueFlags, resolution: SymbolId, file_id: FileId) {
        if let Some(cache) = self.cache.as_mut() {
            let pending = self.pending_name.take();
            let flags_raw = flags.bits() as u32;
            if resolution.is_undefined() {
                cache.add(b"", 0, flags_raw, CachedSymbolKind::Undefined);
            } else if let Some(p) = pending {
                cache.add(p.name, p.hash, flags_raw, CachedSymbolKind::Defined);
            } else {
                // `set_next` for a non-undefined symbol with no preceding
                // `add_*` is a local — its name is anonymous from the
                // symbol-db's perspective (never indexed by name, so
                // nothing to store).
                cache.add(b"", 0, flags_raw, CachedSymbolKind::Local);
            }
        } else {
            // Cache already drained — just drop any stashed name.
            self.pending_name = None;
        }
        self.inner.set_next(flags, resolution, file_id);
    }

    fn add_non_versioned(&mut self, pending: PendingSymbol<'data>) {
        if self.cache.is_some() {
            let name = pending.name();
            self.pending_name = Some(PendingCachedName {
                name: name.bytes(),
                hash: name.hash(),
            });
        }
        self.inner.add_non_versioned(pending);
    }

    fn add_versioned(&mut self, pending: PendingVersionedSymbol<'data>) {
        // v1 schema doesn't carry version info. Canary catches any
        // divergence; until a schema bump lands, versioned inputs fall
        // through as-is — the cache will be written but replay of it
        // will diverge, so callers MUST keep the cache-read path
        // canary-gated.
        self.inner.add_versioned(pending);
    }
}

/// A single sink operation, captured verbatim. Used by the canary to
/// diff two parse paths for the same input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StreamOp<'data> {
    SetNext {
        flags: u32,
        resolution: SymbolId,
        file_id: FileId,
    },
    AddNonVersioned {
        name: &'data [u8],
        hash: u64,
        symbol_id: SymbolId,
    },
    AddVersioned {
        name: &'data [u8],
        hash: u64,
        symbol_id: SymbolId,
    },
}

/// Sink wrapper that records the op stream into an internal `Vec`
/// without forwarding. Intended for the canary's diff path, where
/// we want a second copy of the stream to compare but don't want
/// the replay to mutate real writer state.
pub(crate) struct CaptureSink<'data> {
    ops: Vec<StreamOp<'data>>,
    next: SymbolId,
}

impl<'data> CaptureSink<'data> {
    pub(crate) fn new(start: SymbolId) -> Self {
        Self {
            ops: Vec::new(),
            next: start,
        }
    }

    pub(crate) fn into_ops(self) -> Vec<StreamOp<'data>> {
        self.ops
    }
}

impl<'data> SymbolSink<'data> for CaptureSink<'data> {
    fn next_symbol_id(&self) -> SymbolId {
        self.next
    }

    fn set_next(&mut self, flags: ValueFlags, resolution: SymbolId, file_id: FileId) {
        self.ops.push(StreamOp::SetNext {
            flags: flags.bits() as u32,
            resolution,
            file_id,
        });
        self.next = self.next.next();
    }

    fn add_non_versioned(&mut self, pending: PendingSymbol<'data>) {
        let name = pending.name();
        self.ops.push(StreamOp::AddNonVersioned {
            name: name.bytes(),
            hash: name.hash(),
            symbol_id: pending.symbol_id(),
        });
    }

    fn add_versioned(&mut self, pending: PendingVersionedSymbol<'data>) {
        let name = pending.name();
        self.ops.push(StreamOp::AddVersioned {
            // `VersionedSymbolName::name` isn't directly exposed; for
            // canary purposes the prehash is enough to distinguish
            // symbols and we don't need the raw bytes.
            name: b"",
            hash: name.hash(),
            symbol_id: pending.symbol_id(),
        });
    }
}

/// Replay a cached op stream back into `sink`. `file_id` is the file
/// being replayed — same value the original parse passed to every
/// `set_next`.
///
/// Returns the number of entries replayed. The caller is responsible
/// for ensuring the name slices in `view` live at least as long as
/// the `'data` lifetime on `sink`.
pub(crate) fn replay_cached_symbols<'data, S: SymbolSink<'data>>(
    view: &CacheView<'data>,
    file_id: FileId,
    sink: &mut S,
) -> usize {
    let mut n = 0usize;
    for entry in view.iter() {
        let flags = ValueFlags::from_bits_retain(entry.flags as u16);
        let symbol_id = sink.next_symbol_id();
        match entry.kind {
            CachedSymbolKind::Undefined => {
                sink.set_next(flags, SymbolId::undefined(), file_id);
            }
            CachedSymbolKind::Local => {
                sink.set_next(flags, symbol_id, file_id);
            }
            CachedSymbolKind::Defined => {
                let prehashed = crate::hash::PreHashed::new(
                    UnversionedSymbolName::new(entry.name),
                    entry.hash,
                );
                sink.add_non_versioned(PendingSymbol::from_prehashed(symbol_id, prehashed));
                sink.set_next(flags, symbol_id, file_id);
            }
        }
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsed_input_cache::CacheBuilder;
    use crate::parsed_input_cache::CacheView;

    /// Oracle sink for tests — just records every op verbatim, no
    /// forwarding; lets us inspect what the loader would have done.
    struct Oracle<'data> {
        ops: Vec<StreamOp<'data>>,
        next: SymbolId,
    }

    impl<'data> Oracle<'data> {
        fn new() -> Self {
            Self {
                ops: Vec::new(),
                next: SymbolId::undefined().next(),
            }
        }
    }

    impl<'data> SymbolSink<'data> for Oracle<'data> {
        fn next_symbol_id(&self) -> SymbolId {
            self.next
        }
        fn set_next(&mut self, flags: ValueFlags, resolution: SymbolId, file_id: FileId) {
            self.ops.push(StreamOp::SetNext {
                flags: flags.bits() as u32,
                resolution,
                file_id,
            });
            self.next = self.next.next();
        }
        fn add_non_versioned(&mut self, pending: PendingSymbol<'data>) {
            let name = pending.name();
            self.ops.push(StreamOp::AddNonVersioned {
                name: name.bytes(),
                hash: name.hash(),
                symbol_id: pending.symbol_id(),
            });
        }
        fn add_versioned(&mut self, pending: PendingVersionedSymbol<'data>) {
            let name = pending.name();
            self.ops.push(StreamOp::AddVersioned {
                name: b"",
                hash: name.hash(),
                symbol_id: pending.symbol_id(),
            });
        }
    }

    fn file_id_one() -> FileId {
        // Any non-prelude FileId. The actual value doesn't matter for
        // the sink-level round-trip — the replay preserves it.
        crate::input_data::FileId::from_encoded(1)
    }

    /// Force an 8-byte aligned backing buffer so CacheView can ingest
    /// the bytes produced by CacheBuilder — in real use mmap takes
    /// care of alignment, but a heap Vec is only 8-byte-aligned by
    /// coincidence.
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
    fn tee_captures_defined_symbol() {
        let mut inner = Oracle::new();
        let mut tee = TeeSink::new(&mut inner, Some(CacheBuilder::default()));

        let sid = tee.next_symbol_id();
        let name = UnversionedSymbolName::prehashed(b"_foo");
        tee.add_non_versioned(PendingSymbol::from_prehashed(sid, name));
        tee.set_next(ValueFlags::ABSOLUTE, sid, file_id_one());

        let cache = tee.take_cache().expect("cache still present");
        let bytes = cache.finish();
        let buf = aligned(&bytes);
        let view = CacheView::from_bytes(&buf).expect("view");
        assert_eq!(view.len(), 1);
        let e = view.get(0).unwrap();
        assert_eq!(e.name, b"_foo");
        assert_eq!(e.kind, CachedSymbolKind::Defined);
    }

    #[test]
    fn tee_captures_undefined_and_local() {
        let mut inner = Oracle::new();
        let mut tee = TeeSink::new(&mut inner, Some(CacheBuilder::default()));

        // Undefined: set_next with UNDEF resolution, no add.
        tee.set_next(
            ValueFlags::ABSOLUTE,
            SymbolId::undefined(),
            file_id_one(),
        );
        // Local: set_next with a real resolution but no preceding add.
        let sid = tee.next_symbol_id();
        tee.set_next(ValueFlags::empty(), sid, file_id_one());

        let cache = tee.take_cache().unwrap();
        let bytes = cache.finish();
        let buf = aligned(&bytes);
        let view = CacheView::from_bytes(&buf).unwrap();
        assert_eq!(view.len(), 2);
        assert_eq!(view.get(0).unwrap().kind, CachedSymbolKind::Undefined);
        assert_eq!(view.get(1).unwrap().kind, CachedSymbolKind::Local);
    }

    #[test]
    fn replay_reproduces_tee_stream() {
        // Feed a canonical stream through a TeeSink, then replay the
        // cache into a fresh Oracle and compare against a second
        // direct-parse into another Oracle.
        let reference: Vec<StreamOp<'_>> = {
            let mut o = Oracle::new();
            let sid0 = o.next_symbol_id();
            o.set_next(ValueFlags::ABSOLUTE, SymbolId::undefined(), file_id_one());
            let sid1 = o.next_symbol_id();
            let name = UnversionedSymbolName::prehashed(b"_main");
            o.add_non_versioned(PendingSymbol::from_prehashed(sid1, name));
            o.set_next(ValueFlags::empty(), sid1, file_id_one());
            let sid2 = o.next_symbol_id();
            o.set_next(ValueFlags::NON_INTERPOSABLE, sid2, file_id_one());
            let _ = sid0;
            o.ops
        };

        // Drive the same stream through TeeSink to capture a cache.
        let bytes = {
            let mut drain = Oracle::new();
            let mut tee = TeeSink::new(&mut drain, Some(CacheBuilder::default()));
            tee.set_next(ValueFlags::ABSOLUTE, SymbolId::undefined(), file_id_one());
            let sid1 = tee.next_symbol_id();
            let name = UnversionedSymbolName::prehashed(b"_main");
            tee.add_non_versioned(PendingSymbol::from_prehashed(sid1, name));
            tee.set_next(ValueFlags::empty(), sid1, file_id_one());
            let sid2 = tee.next_symbol_id();
            tee.set_next(ValueFlags::NON_INTERPOSABLE, sid2, file_id_one());
            tee.take_cache().unwrap().finish()
        };

        let buf = aligned(&bytes);
        let view = CacheView::from_bytes(&buf).expect("view");
        let mut replayed = Oracle::new();
        let n = replay_cached_symbols(&view, file_id_one(), &mut replayed);
        assert_eq!(n, 3);
        assert_eq!(replayed.ops, reference);
    }

    #[test]
    fn capture_sink_increments_next_per_set() {
        let mut c = CaptureSink::new(SymbolId::undefined().next());
        let s0 = c.next_symbol_id();
        c.set_next(ValueFlags::empty(), s0, file_id_one());
        let s1 = c.next_symbol_id();
        assert_ne!(s0, s1, "next_symbol_id did not advance after set_next");
    }
}
