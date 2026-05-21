# Tier-1 parse-skip: integration plan

*Authored 2026-04-24. Picks up where `01f2236` + `f900907` leave off:
the storage layer (`libwild/src/parsed_input_cache.rs`) is ready,
nothing consumes it yet. This doc is the next session's spec.*

**Status as of 2026-04-26:** tiers 1, 1.5, 2, 3p1, 3p2, 3p2b, 3p3
all shipped. Productised via `--incremental-cache=off|write|
read-write` flag (commit `6ebc54b`). User-facing docs in
`INCREMENTAL.md` (commit `b265332`). Tier 3 phase 3 (partial-
section reuse via Mach-O writer pre-fill + per-section emit
filter) verified end-to-end on rust-hello with one fake-dirty
input: 6/54 sections pre-filled (16 KiB byte-identical), 48
sections re-emitted by writer, output runs.

**Bevy-dylib measurement (50 MB Rust binary, ~600 inputs)**:
real cargo dev-loop relink (touch `src/main.rs`, `cargo build
--release`) shows NO improvement from `--incremental-cache=
read-write` vs cold writer: ~1290 ms median both ways. Two
likely reasons:

1. rustc captures linker stderr and only forwards on failure,
   so we can't directly observe whether tier-3 fired through
   the cargo invocation. The mechanism is verified to fire on
   manual wild invocations (rust-hello fixture) but the bevy
   path may be hitting an edge case (e.g. all sections appear
   dirty when main.rs's contributing input changes, leaving
   nothing for the partial-reuse path).
2. Even if tier-3 fires, the writer's per-section emit may not
   be the dominant cost on bevy. The link is ~1.3 s of which
   probably 600 ms is wild itself; the writer is maybe 400 ms;
   skipping per-input-section iterations saves a fraction. The
   50 MB pre-fill costs ~30 ms which partially offsets.

Future work (not blocking current ship): instrument wild to
log via a side channel (file or named pipe) so the cargo path
can be observed; add a tier-3 mode flag that ALSO mmap-COWs
the prev output as the new output's initial state instead of
zero-init+memcpy (saves the 30 ms pre-fill cost on
bevy-class outputs).

## Status (2026-04-24) — SHIP-READY

Infrastructure shipped across 7 commits (`f0120e6` refactor →
`87ef10a` write → `bd38941` read + canary → `33a3b43`
wild-hashes gating → `ef750cd` parallel read → `0541d9a`
parallel writes + drop-rename).

**Canary sessions 1 + 2 + 3 of 3: green on all plan-specified
test sets (4 sets × 3 runs × 3 sessions = 36 clean runs):**

| input set       | cache entries | s1 | s2 | s3 |
| --------------- | ------------: | -- | -- | -- |
| rust-hello-world |           403 | ✓  | ✓  | ✓  |
| bevy-dylib      |          1649 | ✓  | ✓  | ✓  |
| ripgrep         |         (~300) | ✓  | ✓  | ✓  |
| rust-analyzer   |         (~700) | ✓  | ✓  | ✓  |

V1 schema is lossless for Mach-O symbol streams in practice —
plan's ship criterion met.

**Perf on bevy-dylib (total wall-clock):**

| path   | before tier-1 | after tier-1 | post-mmap | delta vs cold |
| ------ | -------------:| ------------:| ---------:| -------------:|
| fresh  |        400 ms |       400 ms |    345 ms |             0 |
| read   |             — |       420 ms |    370 ms |           +25 |
| canary |             — |       640 ms |         — |          +240 |
| write  |             — |       500 ms |         — |          +100 |

The read path's tax dropped from +20 ms (fs::read, arena memcpy,
triple validation) to +25 ms after the broader baseline shifted.
On rust-analyzer-incremental (229 inputs) the tax is +10 ms.
Total wall-clock is bounded by layout + write (~380 ms of
untouched work) — tier-1 moves only the symbol-read phase;
tier-2 + tier-3 exist to move the rest.

**Where the read-path floor lives** (probed via `WILD_PROBE`):
on bevy-dylib the prefetch sums to ~630 ms of CPU across 1649
inputs; rayon parallelism brings that to ~80 ms wall but it
overlaps with parse-side work elsewhere so the *visible* tax is
~25 ms. The hot CPU is `mmap` itself — 1649 syscalls each pay
the kernel's per-process VM-map lock. Within-group serial mmap
is *slower* (tested: 380 vs 370 ms) because we lose the
per-shard concurrency. **Tier-1.5 (landed 2026-04-25)** replaces
1649 cache files with one bundle at `<output>.wild-pi-cache`,
collapsing the syscall storm to a single mmap. Tax dropped from
+25 ms to ~0 ms — break-even on the worst case.

**Tier-2 foundation (landed 2026-04-25)** — capture + canary.
`<output>.wild-layout` stores a per-section snapshot
`(name, alignment, file_offset, file_size, mem_offset,
mem_size)` after `produce_layout` finishes. Snapshot was
2.8 KiB on bevy-dylib. `WILD_INCREMENTAL_LAYOUT_CANARY=1`
re-runs layout and panics on any divergence vs the previous
snapshot — proves layout determinism end-to-end.

**Tier-3 phase 1 — contributors map (landed 2026-04-25)**:
schema bumped v1 → v2; each `SnapshotSection` now carries the
list of bundle keys for every input that contributed loaded
sections to it. Contributors are sorted+deduped via
`canonicalize()` so byte-equality of two snapshots reflects
logical equality. Snapshot grew to 83 KiB on bevy-dylib (still
trivial vs the 22 MiB parse cache).
`LayoutSnapshot::dirty_section_indices(&clean_inputs)` returns
section ordinals that have at least one dirty contributor —
the predicate tier 3's writer will use to decide reuse.
`WILD_INCREMENTAL_TIER3_PROBE=1` runs the dry-run intersection
on a real link and reports `N/M sections reusable, B/T bytes
(P%)`. On bevy-dylib all-clean: **55/55 sections, 100% bytes
reusable** — confirms the dirty-bitmap mechanic on a real
workload before writer integration. Cold path unchanged.

**Tier-3 phase 2 — byte-equivalence canary (landed 2026-04-25)**:
the reuse predicate is strengthened with a layout-stability
check (same name + offset + size + memory address as the
previous link, *plus* all contributors clean) via
`LayoutSnapshot::reusable_section_indices(&prev, &cur,
&clean_inputs)`. `WILD_INCREMENTAL_TIER3_CANARY=1` mmaps the
*previous* output binary before `produce_layout` triggers its
rename-and-recreate (so the inode pages stay valid via the
mmap reference even after the file is replaced), runs the
writer cold, then byte-compares prev_mmap[off..off+size]
against the freshly-written output for every reusable
section. Reports `M/N sections byte-identical, X bytes
verified safe to reuse`; emits a `first divergence at
section #i` line if any reusable verdict disagreed with
byte-equality. On the C-hello fixture: **51/51 sections,
904 bytes byte-identical**.

**Tier-3 phase 2b — speculative writer-skip (landed 2026-04-26)**:
when `WILD_INCREMENTAL_TIER3_SKIP=1` AND every section is
reusable AND prev_output_mmap is available, bypass the
platform writer entirely and `memcpy` prev → out. The output
gets the previous link's bytes wholesale; the canary path
already proved this is byte-equivalent (per-section) to a
cold writer's output. Codesign verification still passes
because Apple's CDHash is computed over file content and the
new file's content equals the prev file's content.

`WILD_INCREMENTAL_NO_POST_LOAD_SKIP=1` opt-out for the
post-load whole-link skip so tier-3's narrower section-level
skip can be exercised on workloads where whole-link-skip
would otherwise win the race.

Verified on `bench-fixtures/saved-rust-hello` (459 KiB Rust
binary, 406 inputs):

| path | wall-clock | size | sections verified |
|---|---|---|---|
| Cold | ~82 ms | 459 KB | — |
| Tier-3 skip | ~79 ms | 459 KB | 54/54, 347 KB |

Modest (3 ms) win on this small fixture — process startup
dominates and the writer's actual work is only a few
milliseconds. On bevy-dylib-class outputs (38 MB, 1649
inputs) the writer takes ~280 ms cold while skip would take
~10-15 ms (mmap + memcpy 38 MB), so the win there is
expected to be ~250 ms.

End-to-end test phase 6 asserts:

* skip path fires (stderr contains `wild tier-3 skip:
  bypassed writer`),
* output size matches prev exactly,
* binary still runs and exits with the expected code.

**Caveat**: wild's writer has pre-existing non-determinism in
LC_UUID / build-version timestamp regions, so two cold runs
of the same fixture aren't byte-identical. The tier-3 skip
preserves the *previous* link's UUID rather than minting a
fresh one — which is functionally equivalent (the binary
loads + runs + codesign-validates) but cosmetically distinct
from a fresh cold link. The canary's *per-section*
byte-equality holds regardless.

## What's landed

* Zero-copy on-disk format (`repr(C)` header + symbol array + names
  blob), mmap-compatible, schema v1.
* `CacheView<'data>` reader + `CacheBuilder` writer with
  name-dedup.
* `CacheBuilder::write_to(&Path)` (atomic tmp-and-rename).
* `cache_path_for_input(&Path)` — `$XDG_CACHE_HOME/wild/parsed-inputs/<blake3>.wildpi`.
* 12 unit tests: round-trip, name-dedup, bad-magic/schema, truncated,
  misaligned, unknown-kind, empty, zero-copy assertion, atomic write,
  path collision-freeness.

## What ships tier-1

A `load_symbols` fast path that, for an input whose fingerprint is
clean, replays a cached symbol stream instead of iterating the
object crate. Measured target on bevy-dylib: −50 to −150 ms off the
370 ms cold link when the dev-loop touched only a few crates.

## The refactor

### `SymbolSink` trait

`libwild/src/symbol_db.rs` currently writes parsed symbols into two
places in `load_symbols_from_file`:

```rust
outputs.add_non_versioned(pending);
outputs.add_versioned(pending);
symbols_out.set_next(flags, resolution, file_id);
```

Extract a trait:

```rust
trait SymbolSink<'data> {
    fn set_next(&mut self, flags: ValueFlags, resolution: SymbolId, file_id: FileId);
    fn add_non_versioned(&mut self, p: PendingSymbol<'data>);
    fn add_versioned(&mut self, p: PendingVersionedSymbol<'data>);
}
```

Existing code becomes the default `SymbolSink` impl on the pair
`(&mut SymbolWriterShard, &mut SymbolLoadOutputs)`.

### Teeing impl

```rust
struct TeeSink<'a, 'data, S: SymbolSink<'data>> {
    inner: S,
    cache: Option<&'a mut CacheBuilder>,
}
```

When `cache: Some(b)`, every `set_next` / `add_*` duplicates into `b`.
This captures the exact symbol stream — no schema drift, no
replicated flag-computation logic.

### Cache-replay path

Add to `load_symbols_from_file` (before dispatching to
`RegularObjectSymbolLoader`/`DynamicObjectSymbolLoader`):

```rust
if let Some(cache_bytes) = try_load_cache(s.parsed.input.path()) {
    if let Some(view) = CacheView::from_bytes(&cache_bytes) {
        return replay_cached_symbols(view, s.file_id, sink);
    }
}
```

`replay_cached_symbols` iterates `CachedEntry` → `SymbolSink::add_*`.

### Gate

Under `WILD_INCREMENTAL_DEBUG=1`:

* Write path: `TeeSink` wraps the default sink, `CacheBuilder`
  captures the parse output, `write_to(cache_path_for_input(input))`
  at end.
* Read path: only consume a cache file when the `.wild-hashes`
  side-car reports the input clean. Otherwise fall through to
  re-parse (and refresh the cache from that parse).

Default off until the canary below is green for a session.

## The canary

Before flipping `WILD_INCREMENTAL_DEBUG` default, a second env var
`WILD_INCREMENTAL_PARSE_SKIP_CANARY=1` runs BOTH paths per input:

1. Parse via object crate into a scratch `SymbolWriterShard +
   SymbolLoadOutputs`.
2. If a cache exists, replay into a second scratch pair.
3. Compare structurally — same symbol count per bucket, same
   `(name, hash, flags, kind, resolution)` in insertion order.
4. Panic with a clear diff on mismatch.

Ship once a bevy-dylib + rust-analyzer + ripgrep run under
`CANARY=1` is clean across 3 consecutive sessions.

## Lifetime contract

`CachedEntry<'data>` borrows from the cache mmap. Pushing into
`pending_symbols_by_bucket` is fine — those structs hold
`UnversionedSymbolName<'data>` which accepts any `&[u8]`
with the link's 'data lifetime. The cache mmap needs to live at
least as long as the rest of the input mmaps.

Plumb the cache mmap through `FileLoader` alongside the input
mmap (same arena, same lifetime) so Rust's borrow checker sees
them as equivalent.

## File layout the next session touches

* `libwild/src/symbol_db.rs` — trait extraction + teeing sink.
* `libwild/src/platform.rs` — default sink impl on the pair.
* `libwild/src/input_data.rs` — mmap-hold for cache files.
* `libwild/src/lib.rs` — gate, canary wiring.
* `libwild/src/parsed_input_cache.rs` — maybe extend with
  `try_load_cache(&Path) -> Option<Mmap>`.
* `libwild/tests/incremental_parse_skip.rs` (new) — canary
  integration test.

## Measurement script

```sh
export WILD_INCREMENTAL_DEBUG=1
# First link: writes caches.
time /tmp/wild-saves-macho/bevy-dylib/run-with $WILD
ls ~/.cache/wild/parsed-inputs/ | wc -l   # should match input count
# Second link: consumes caches.
time /tmp/wild-saves-macho/bevy-dylib/run-with $WILD
# Target: ≥100 ms shaved from the 370 ms cold.
```

## Risks the canary should catch

1. Symbol-version metadata lost (weak version string).
2. COMDAT group selector lost.
3. `N_ARM_THUMB_DEF` / `N_NO_DEAD_STRIP` bits lost.
4. TLS flags lost.
5. Hidden/protected visibility lost (Mach-O N_PEXT).
6. Local symbol ordering changed (some callers rely on order).

Any of these is a subtle miscompile. The canary runs BOTH loaders
and compares, so a divergence panics the link rather than shipping
a bad binary.

## After tier-1

* Tier 2 (sticky layout) is the next beat — persist section
  offsets + symbol addresses, reuse for clean-input subsets.
* Tier 3 (per-section memcpy skip) builds on tier-1's clean-input
  bit to skip content-addressed sections like `__cstring`.
* Both need tier 1's per-input clean/dirty verdict to mean what
  this module says it means. Ship that foundation first.
