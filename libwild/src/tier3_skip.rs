//! Tier-3 phase 3 — partial writer-skip state.
//!
//! Set once by `lib.rs` before calling `P::write_output_file`; read
//! by the Mach-O writer at two points:
//!
//! 1. **Pre-fill** — at the top of `write_direct_inner` we copy the previous output's bytes into
//!    the new output buffer for every "reusable" section (file ranges that are unchanged since the
//!    last link). This gives those sections their final content *before* the platform writer runs.
//!
//! 2. **Per-section emit filter** — `split_output_for_objects` drops contributions whose target
//!    output section is reusable so the writer never iterates over them. Saves the
//!    per-input-section work (memcpy + reloc apply) proportional to how much of the output is
//!    reusable.
//!
//! Cleared by `lib.rs` after the writer returns. The legacy "all
//! reusable" speculative-skip (tier-3 phase 2b) still wins when 100 %
//! of sections are reusable; phase 3 covers the partial-reuse case
//! (some inputs dirty, most sections still reusable).

use crate::output_section_id::OutputSectionId;
use hashbrown::HashSet;
use std::sync::Mutex;

/// Active tier-3 phase 3 state, populated when the user opts in via
/// `--incremental-cache=read-write` AND there's a layout snapshot
/// from the previous link AND there's at least one reusable section
/// (and at least one dirty section — when *all* sections are
/// reusable, phase 2b's whole-output bypass wins instead).
pub(crate) struct State {
    /// Output sections whose contributing inputs are all clean AND
    /// whose layout matches the previous link's snapshot
    /// byte-for-byte. The writer must NOT touch these — their bytes
    /// come from the pre-fill below.
    pub(crate) reusable_ids: HashSet<OutputSectionId>,
    /// File-offset ranges for the reusable sections, derived from
    /// the snapshot's `(file_offset, file_size)` pairs. Pre-filled
    /// from `prev_bytes` at the top of the writer's main closure.
    /// One range per reusable section, in snapshot order; ordering
    /// doesn't affect correctness, only diagnostics.
    pub(crate) ranges: Vec<(usize, usize)>,
    /// mmap of the previous output binary, leaked for process
    /// lifetime by `lib.rs` so its bytes outlive every borrow we
    /// hand out. Indexed by `(file_offset, file_size)` from
    /// `ranges`.
    pub(crate) prev_bytes: &'static [u8],
    /// When `true`, `file_writer` opens the output path
    /// `UpdateInPlace` (no rename, no truncate), preserving prev's
    /// bytes on disk. The writer then overwrites only the changed
    /// sections — the reusable bytes never leave the file. Saves
    /// the per-link pre-fill memcpy (50 MB on bevy-class outputs
    /// = ~17 ms wall) AND avoids re-writing the unchanged blocks
    /// to disk.
    ///
    /// Tradeoff: not atomic. A writer crash mid-write leaves the
    /// file at `<output>` partially-modified. Cold-link retry
    /// recovers (whole-link skip rejects, full link rebuilds).
    pub(crate) use_in_place: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Install partial-skip state. Call from `lib.rs` immediately
/// before `P::write_output_file`. Pass `None` to clear (also done
/// implicitly after writer return).
pub(crate) fn set(s: Option<State>) {
    *STATE.lock().expect("tier3_skip mutex poisoned") = s;
}

/// `true` if tier-3 partial-skip is active AND `id` is in the
/// reusable set. Per-section writer hot path; locks once per check
/// — measured at <100 ns under contention so not worth a thread-
/// local cache today.
pub(crate) fn contains(id: OutputSectionId) -> bool {
    let guard = STATE.lock().expect("tier3_skip mutex poisoned");
    guard.as_ref().is_some_and(|s| s.reusable_ids.contains(&id))
}

/// Run `f` with a borrowed view of the current state, if any. Used
/// by the writer's pre-fill step where we need both `prev_bytes`
/// and `ranges` together — taking the lock once for both is
/// cheaper than two `contains`-style probes.
pub(crate) fn with<R>(f: impl FnOnce(Option<&State>) -> R) -> R {
    let guard = STATE.lock().expect("tier3_skip mutex poisoned");
    f(guard.as_ref())
}

/// `true` if tier-3 has installed state that wants `file_writer` to
/// open the output `UpdateInPlace` (no rename, no truncate). The
/// existing bytes ARE the pre-fill — saves a 50 MB memcpy on
/// bevy-class outputs. Queried by `file_writer::set_size`.
pub(crate) fn wants_in_place() -> bool {
    let guard = STATE.lock().expect("tier3_skip mutex poisoned");
    guard.as_ref().is_some_and(|s| s.use_in_place)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_section_id::TEXT;

    #[test]
    fn contains_returns_false_when_unset() {
        // Reset to a known-clean baseline. Other tests may have left
        // state behind — Mutex<Option<State>> means the global is
        // shared across the whole test binary.
        set(None);
        assert!(!contains(TEXT));
    }

    #[test]
    fn contains_reflects_installed_state() {
        let mut reusable = HashSet::new();
        reusable.insert(TEXT);
        set(Some(State {
            reusable_ids: reusable,
            ranges: vec![(0x1000, 0x4000)],
            prev_bytes: &[],
            use_in_place: false,
        }));
        assert!(contains(TEXT));
        set(None);
        assert!(!contains(TEXT));
    }
}
