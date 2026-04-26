# Incremental linking (experimental)

Wild can cache the parsed form of every input plus the resolved
section layout next to your output binary, so the next link can
replay them instead of redoing the work.

## Quick start

Pass `--incremental-cache=read-write` to wild. From Cargo:

```sh
RUSTFLAGS="-C link-arg=-fuse-ld=wild \
           -C link-arg=--incremental-cache=read-write" \
  cargo build --release
```

The first link is cold and writes the cache. The second and later
links replay the parse and skip the writer when every section is
reusable.

## Modes

| Mode         | Aliases            | Behaviour                            |
| ------------ | ------------------ | ------------------------------------ |
| `off`        | `false`, `0`       | Cold link. Default. No sidecars.     |
| `write`      |                    | Tee parse + layout into sidecars.    |
| `read-write` | `rw`, `on`, `true` | Replay + speculative writer-skip.    |

## Sidecars

Three files are produced next to `<output>`:

- `<output>.wild-pi-cache` — parsed-input bundle. One mmap on read
  replaces N file opens; size scales with input symbol count.
- `<output>.wild-layout` — per-section snapshot
  `(name, alignment, file_offset, file_size, mem_offset, mem_size)`
  plus the bundle keys of contributing inputs.
- `<output>.wild-hashes` — per-input fingerprint side-car. Used to
  decide whether each input is clean since the last link.

Removing the sidecars (or running with `--incremental-cache=off`)
returns to a cold link.

## Implementation tiers

Tiers landed:

- **Tier 1** — parse-skip cache. Replays the per-input `load_symbols`
  pass instead of re-iterating each input's symbols.
- **Tier 1.5** — single per-output cache bundle. Collapses N mmap
  syscalls into one. Bevy-class workloads went from a +25 ms tax
  (per-input files) to ~0 ms (one bundle).
- **Tier 2** — layout snapshot. Records every output section's
  resolved layout. Foundation for tier 3.
- **Tier 3 phase 1** — per-section contributors map.
  `LayoutSnapshot::dirty_section_indices` flags sections whose
  contributors include any dirty input.
- **Tier 3 phase 2** — byte-equivalence canary. Empirical proof
  that "reusable" predicate matches what the writer would emit.
- **Tier 3 phase 2b** — speculative writer-skip. When ALL sections
  are reusable, bypass the platform writer and `memcpy` prev to
  out. Saves the entire writer phase (~280 ms on bevy-dylib-class
  outputs).

Not yet shipped: partial-section reuse where some inputs change
and only a few sections need re-emit. Requires Mach-O writer
modification; tracked as tier 3 phase 3.

## Caveats

- Wild's writer has pre-existing non-determinism in `LC_UUID` and
  build-version timestamp regions. Two cold links of the same
  fixture aren't byte-identical. The speculative writer-skip
  preserves the previous link's UUID — functionally equivalent
  (loads, runs, codesigns) but cosmetically distinct from a fresh
  cold link.
- The cache format is versioned and self-validating. A
  schema-mismatched sidecar is silently rejected and the link
  proceeds cold.

## Power-user env vars

The legacy `WILD_INCREMENTAL_*` env vars used during tier
development are still honoured. An env var override always wins
over the flag. See the implementation in
`libwild/src/parsed_input_cache.rs` and `libwild/src/lib.rs` for
the full list. Notable ones:

- `WILD_INCREMENTAL_TIER3_CANARY=1` — runs the byte-equivalence
  canary even when not skipping the writer. Useful for CI to
  catch cache-invariant regressions.
- `WILD_INCREMENTAL_NO_POST_LOAD_SKIP=1` — opts out of the
  whole-link skip so the per-section tier-3 mechanism can be
  exercised for benchmarking.
- `WILD_INCREMENTAL_DEBUG=1` — enables verbose stderr lines
  describing which incremental fast paths fired.
