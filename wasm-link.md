# wasm-link plan

Roadmap for closing the remaining gap to lld byte-for-byte parity in
wild's wasm linker. Snapshot date 2026-04-30 (updated post Phase 1).

Current state (after Phase 1 + Phase 3a + targeted wins):

- `lld_wasm_tests`: **144 passed** (was 122), 80 ignored, 0 failed.
- `wasm_regression_tests`: 2 passed.
- `--lld-compat` flag (mach-o `-ld64_compat` analog) enabled by the
  test runner; off by default for production users who want speed
  over byte equivalence.

The remaining ignored tests sort into roughly 5 buckets by effort
and risk. Phases run independently and can ship as separate commits.

## Status (2026-05-05 — small contained wins)

- ✅ **`allow-multiple-definition.s`**: split `--noinhibit-exec`
  from `--allow-multiple-definition` / `-z muldefs`. New
  `Args::warn_multiple_definitions` trait method (default false,
  overridden by wasm) gates the per-collision
  `warning: duplicate symbol: <name>` lld emits under
  `--noinhibit-exec`. Plain `--allow-multiple-definition` and
  `-z muldefs` stay silent (lld behaviour).
  Tally: **143 → 144**.
- 🟡 **`map-file.s` partial fix**: switched per-CODE-row and
  per-DATA-row offsets to lld's virtual stacking convention
  (first chunk Off = section_start + 1, Size = body_total;
  subsequent chunks stack via Off += prev.Size). Pulled segment
  names from `merged.data_segments[].name` instead of
  synthesising `.data.N`. Stored `function_origin` as full
  input path so `<path>:(<sym>)` matches lld's regex.
  Remaining: per-input data-segment attribution rows + BSS
  row synthesis for the BSS region not emitted to wasm DATA.
- ✅ **`__wasm_call_ctors` --export-dynamic suppression**:
  registered the synth ctor stub in `function_is_hidden` at
  insertion time so `comdats.ll`, `command-exports.s` and similar
  --export-dynamic fixtures stop seeing a phantom export. Explicit
  `--export=__wasm_call_ctors` still works.
- ✅ **`-l:NAME`**: route the binutils literal-filename library
  syntax to `InputSpec::Search` instead of `InputSpec::Lib`. Wild
  no longer searches for `liblibls.a.so` / `liblibls.a.a` for
  `-l:libls.a`. Pinned by `libsearch.s` (still failing on a
  deeper archive-extraction issue).

## Status (2026-05-04 — Phase 4a investigation)

- 🟡 **Phase 4a metadata-table scaffolding** (commit `5082397`):
  added `MergedModule.function_export_pos: HashMap<name, (cmdline_rank,
  sym_pos)>`, populated during the parse pass for both canonical
  function names and aliases. Reserved `#[allow(dead_code)]` because
  wiring it into the EXPORT sort regressed two tests. Two findings
  worth recording for the follow-up:

  1. **Synth function collision at `(0, 0)`.** `__wasm_call_ctors`
     gets registered at `(rank=0, sym_pos=0)`; main-object `_start`
     gets `(rank=0, sym_pos=0)` too (it's typically the first sym).
     The kind tiebreak then orders FUNC before GLOBAL, pushing
     `_start` ahead of `__stack_pointer` (GLOBAL idx 0 → fallback
     `(0, 0)`). lld's actual order is `__wasm_call_ctors` →
     `__stack_pointer` → `_start` — there's some lld-internal
     synthesis-order key for synth FUNC vs synth GLOBAL that
     doesn't surface as a (rank, pos) tuple. mutable-global-exports.s
     pinned this regression.

  2. **Alias precedence at same output idx.** weak-alias.s's
     aux input has `direct_fn` at sym_pos 0 and BINDING_WEAK alias
     `alias_fn` at sym_pos 2 (both → output FUNC 1). With (rank,
     sym_pos) keying, `direct_fn` sorts before `alias_fn`. lld
     emits `alias_fn` first — possibly alphabetical name tiebreak,
     possibly an alias-first rule. sym_pos alone isn't the key.

  The right next step is a merged-function metadata table indexed
  by output func idx with explicit synth-source tagging, plus an
  alias relation, plus a per-emit-pass walker that interleaves
  FUNC/GLOBAL exports by lld's actual order rather than trying to
  compress everything into one sort key. Bigger than a single
  session.

- 🟡 **BINDING_LOCAL multi-def for `init-fini.ll`**: investigated.
  The bug: in `symbol_to_output_func`, `function_name_map.get(name)`
  resolves cross-input even for BINDING_LOCAL symbols, collapsing
  the second input's local `.Lcall_dtors.101` (output idx 17) onto
  the first input's (idx 9). Tried gating the lookup on `!is_local`
  to use `local_output_idx` instead — broke `init-fini-no-gc.ll` /
  `command-exports.s` because `local_output_idx` is the
  pre-synth-shift index, while `function_name_map` values get
  shifted +1 on `__wasm_call_ctors` insertion. The fix needs to
  apply the same shift to local indices (or skip name registration
  for BINDING_LOCAL entirely so the lookup returns
  `local_output_idx` AS-IF post-shift). Left as follow-up — the
  shift bookkeeping is intricate enough that I'd want a small
  unit test before committing.

## Status (2026-04-30 post-session)

- ✅ **Phase 1a — sig-mismatch trap stubs in exec mode**: shipped.
  Unlocked `signature-mismatch-export.ll`. The `.s` variant's exec
  arm passes too but its relocatable arm has a separate
  `write_relocatable` symbol-ordering bug (UNDEF symbols resolved
  by a later input drop their slot instead of reserving it; lld
  preserves source-order). Left as follow-up.
- ✅ **Phase 1b — `wasm-export-name` attribute**: shipped. Unlocked
  `export-name.ll`.
- ✅ **Phase 1c — `--wrap NAME` body rewrite**: shipped. Unlocked
  `wrap_import.s`. The sibling `wrap.s` pairs `-wrap` with
  `-emit-relocs` — a separate path, not unlocked.
- ✅ **Phase 1d — fixture probe**: tried 6 candidates, none pass
  incidentally. No additional wins from this bucket.
- 🟡 **Phase 2 — Map file**: shipped as infrastructure (commits
  752f083, c5a4897). Wild now emits a structurally-correct link map
  on `-M` / `-Map=PATH` / `-print-map` covering sections, GLOBAL
  sub-rows, CODE per-function rows with input attribution, and the
  DATA segment top row. Plus `R_WASM_TABLE_INDEX_I32` /`_I64` in
  data segments now register the target in the indirect function
  table and patch the resolved table index back into the segment
  bytes. The `map-file.s` fixture still fails on byte-level
  parity (function body sizes, `.data`/`.bss` segment naming, per-
  data-symbol sub-rows, name-section size), but the infra is in
  for users who want a debug map.
- ✅ **Phase 3a — init/fini wrappers**: shipped (commits 34001f1,
  c87e804, 85d2a80). `.Lcall_dtors.<P>` /
  `.Lregister_call_dtors.<P>` synthesis is a no-op for wild —
  LLVM lowers `llvm.global_dtors` at compile time so they appear
  as ordinary input functions. What wild adds: an
  `<entry>.command_export` wrapper, a sibling
  `<exported>.command_export` wrapper for every
  `WASM_SYM_EXPORTED` function (`.export_name foo, foo` style),
  and a Pass 2.7 init-func source-object pruner that approximates
  lld's two-pass archive resolution. Plus an existing-bug fix in
  the Pass 3 `table_entries` ctor-offset shift (was
  double-shifting Pass 2 entries).

  Unlocks `init-fini-no-gc.ll`, `ctor-no-gc.test`,
  `command-exports.s`, `command-exports-no-tors.s`. The sibling
  `init-fini.ll` needs BINDING_LOCAL multi-def handling for
  repeated `.Lcall_dtors.<P>` across inputs;
  `ctor-gc.test`'s WHOLEARCHIVE arm needs `--whole-archive`-aware
  prune (all members unconditionally alive); `weak-symbols.s` /
  `archive-export.test` need Phase 4a per-input EXPORT-emit-order
  tracking.

  Original analysis (kept for reference):

  **Archive-resolution dead-path issue.** Wild's archive resolution
  IS lazy (`is_optional()` in `grouping.rs`), but loads members
  based on any symbol reference, even from a function that's later
  GC'd. Concretely for `ctor-gc-setup.test`:
  - `setup.o` calls `lib_func` → `lib.o` loads.
  - `lib.o`'s `unused_lib_func` calls `def` → `ctor.o` loads.
  - `unused_lib_func` is unreachable from `_start` so it's GC'd.
  - But `ctor.o` is already loaded — its `init_array` entry for
    `test_ctor` ends up in `all_init_funcs`.

  Wild's current default behaviour (no `<entry>.command_export`
  wrapper) GCs `__wasm_call_ctors` because nothing references it,
  so the dead `init_funcs` chain unravels and `test_ctor` drops
  out — `ctor-gc-setup` happens to pass. But the moment a wrapper
  keeps `__wasm_call_ctors` alive (Phase 3a's goal), the chain
  re-roots and `test_ctor` survives, breaking the test.

  lld solves this with two-pass archive resolution: load greedily,
  GC, then re-evaluate object aliveness based on whether any of
  the object's symbols are used by post-GC live code. Wild would
  need similar two-pass semantics, OR a narrower fix: prune
  `all_init_funcs` whose source object has no other live symbols
  before building the `__wasm_call_ctors` body.
- 🟡 **Phase 4b — shared library `.so` understanding**: partial.
  Wild now recognises `.so` inputs (the wasm-ld dylink ABI marks
  these by emitting `dylink.0` as the very first custom section)
  and treats them as resolution-only contributions — exports become
  `env.<name>` imports in the output, code/data/types do NOT enter
  the merged module. The output's `dylink.0` `Needed` subsection
  lists the basename of each `.so` the link resolved against, so
  the dynamic linker knows which sibling modules to load before
  instantiating this one.

  Three pieces shipped together:
  1. `parse_wasm_sections` sets `is_shared_library = true` when
     section 0 is the `dylink.0` custom section.
  2. `merge_inputs` short-circuits on shared-library inputs: it
     captures their basenames into `dylink_needed` and their
     EXPORT-section entries into `dylink_exports` (with the
     function `FuncType` so the synth import gets the right
     SigIndex), then `continue`s — skipping the parse-loop body
     that would otherwise dump the .so's symbols / code / data /
     custom sections into the merged output.
  3. The `dylink.0` emit pass populates `Needed` from
     `merged.dylink_needed` instead of always-empty.

  Unlocks `shared-needed.s` (both the SO1 stand-alone-shared and
  the SO2 link-against-shared arms) and `no-shlib-sigcheck.s`.

  Follow-up: dylink.0 emit gate widened from `is_shared` only to
  `is_shared || (is_pic && !emit_relocs) || !dylink_needed.is_empty()`
  — covers `-pie` outputs and plain `-Bdynamic` exec links against a
  `.so`. The `--emit-relocs` arm of `-pie` is excluded because wild's
  reloc.CODE / reloc.DATA section-index bookkeeping doesn't yet
  account for the dylink.0 shift (`emit-relocs-fpic.s` pins the
  historical "no dylink.0" layout there).

  Remaining Phase 4b work is symbol-resolution-level —
  `symbol_db.rs`'s duplicate-strong-def check fires for `.so`
  inputs whose exports match a defined symbol in another input
  (e.g. `static-error.s` defines `_start` in both the `.o` and
  `.so`), and that path is platform-shared (elf/macho/wasm). A
  proper fix needs the layout to know about wasm shared libraries,
  which is a cross-cutting refactor. The other shared/dylink
  fixtures (`shared.s`, `shared-weak-symbols.s`, `pie.s`,
  `dylink*`, `stub-library*`) need additional pieces too:
  per-input encounter export ordering (Phase 4a tail), TABLE
  Limits widening for the indirect_function_table import
  reflecting the actual merged table_entries count, and various
  GOT routing details.

- 🟡 **Phase 4a — per-input EXPORT-emit-order**: partial. Three
  pieces shipped that lay the groundwork for the full per-input
  encounter walk:
  1. `merge_inputs` collects file refs and stable-sorts under
     `--lld-compat` so non-archive inputs come first. lld assigns
     function indices in "main object first, archive members later"
     order — without this, `archive-export.test`'s
     `_start = 0, foo = 1, bar = 2, archive2_symbol = 3` allocation
     doesn't match (wild's natural cmdline-order walk gives the
     archive members the lowest function indices).
  2. `function_cmdline_rank: Vec<u32>` tracks each function's source
     object's pre-sort cmdline position. The `EXPORT` section sort
     under `--lld-compat` uses it as the primary key for `EXPORT_FUNC`
     entries — which moves `_start` to its correct lld position
     (right after `__wasm_call_ctors`) regardless of its low merged
     `func_idx`. The `+1` ctors shift mirrors into `function_cmdline_rank`
     so the synth ctor stub gets rank 0.
  3. `--export-dynamic` now synthesises an immutable defined GLOBAL
     per non-hidden non-local data symbol whose init value is the
     symbol's output address (`weak-symbols.s`-style `weakGlobal`).
     A `global_export_pos` map carries the source's
     `(cmdline_rank, sym_pos)` so the EXPORT sort can place
     synth-from-data globals in the right per-input slot. The
     GlobalNames subsection in the `name` custom section excludes
     these (lld doesn't include them either — they're addressable
     via EXPORT). Gated to `!is_shared && !is_pic && !static_pic
     && !export_all && --export-dynamic`.

  Unlocks `archive-export.test`. Pulled `weak-undefined-pic` (Phase
  3b's win) into `KNOWN_PASSING` while we're here.

  Still pending: per-input encounter sym-position tracking for the
  EXPORT sort (`weak-symbols.s` needs `weakGlobal GLOBAL 1` to land
  *between* `exportWeak1` and `exportWeak2` per-input — wild's
  current sort lands it earlier). The straightforward approach
  (track `function_sym_pos: Vec<u32>` indexed by `func_idx`) is
  fragile because `__wasm_call_ctors`, sig-mismatch stubs, and
  `__cxa_atexit` decl-stubs interleave into the merged function-
  index space at points the parse pass doesn't see — keying by
  name was attempted but breaks `mutable-global-exports.s` /
  `weak-alias.s` / `export-name.s` (the `--export-dynamic` walk
  picks up alias names not present in the per-defined-function
  scan). Needs a more principled "merged-function metadata table
  with synth tracking" refactor.
  Also pending for `weak-symbols.s`: data-segment-name carry-
  through (wild emits `.data.0`, lld emits `.data` for the merged
  single-segment case).

- ✅ **Phase 3b — PIE PIC-base imports**: shipped. Three pieces
  landed together:
  1. New Pass 4a.5 emits PIC-base imports (`env.memory`,
     `env.__memory_base`, `env.__indirect_function_table`,
     `env.__table_base`, plus optional `env.__stack_pointer`)
     *before* the per-object input-import loop, matching lld's PIE
     section ordering (`weak-undefined-pic.s`'s
     `IMPORT-NEXT: env.foo → IMPORT-NEXT: GOT.func.foo` chain only
     matches when those bases sit at the lowest GLOBAL indices).
  2. `__stack_pointer` import is gated on whether any input
     actually references it under `-pie` (`-shared` always emits
     it).
  3. GlobalNames subsection now records the four PIC bases and
     falls back to using a PIC import's `field` (e.g. `foo` for
     `GOT.func.foo`) when the input has no matching kind=2
     symbol — llvm-mc emits these GOT imports on a `@GOT` reloc
     alone, with no kind=2 row to attach a name to.

  Unlocks `weak-undefined-pic.s`. Other PIE/`-shared` fixtures
  (`pie.s`, `shared.s`, `shared-needed.s`, `shared-weak-symbols.s`,
  `static-error.s`) still fail on `dylink.0` custom-section content
  and `.so`/dynamic-needed handling — that's Phase 4b territory.

Net: +19 tests across sessions (122→141). Phases 1, 3a, 3b complete;
Phase 2 (infra) and Phases 4a/4b (partial) shipped. Phase 4b
infrastructure is now substantial: dylink.0 ImportInfo (for weak-
undef imports) and RuntimePath (for `-rpath`) subsections, table-
slot tracking under shared/PIE (R_WASM_TABLE_INDEX_I32/I64 +
GOT.func.*), conditional shared/PIE import gating, IMPORT-section
ordering matching lld, memory64-aware type widening for env.
__table_base / env.__indirect_function_table / GOT.*, `.so` symbol
skip at the platform-shared symbol_db, segment-name carry-through
(.rodata / .tdata / .data / .bss), and additional flag acceptance
(`--unresolved-symbols=import-dynamic`, `--noinhibit-exec`,
`-Bsymbolic`). Phase 4a's per-input sym-position key still remains;
the remaining shared/dylink fixtures need __wasm_apply_data_relocs +
__wasm_apply_global_relocs synth, __wasm_init_memory synth (shared
memory), weak-import-AND-export pattern (-shared weakdef), START
section synth in PIE, per-input GOT-import discovery ordering, and
demangle support.

Targeted wins outside the plan structure:

- ✅ **`__llvm_covfun` 8-byte alignment** (commit a5194fa): chunks
  of `__llvm_covfun` custom section are now padded to 8-byte
  alignment when concatenated across inputs. Unlocks
  `custom-section-align.s`.
- ✅ **`-t` / `--trace`** (commit 6dfd906): print loaded input file
  paths. Unlocks `trace.test`.
- ✅ **`-v` / `-V` / `--version`** (commit a46c700): print "LLD
  <version>". Unlocks `version.test`.

---

## Phase 1 — Cheap, contained wins

Target: +5 tests, ~3 days. Each item self-bisecting. Order doesn't
matter.

### 1a. Sig-mismatch stub in exec mode

**Tests unlocked:** `signature-mismatch.s`, `signature-mismatch-export.ll` (2)

**Status:** wild already builds the data structure. `compute_sig_mismatch_stubs(layout)`
runs at `wasm_writer.rs:186` in exec mode but only emits warnings
(`emit_sig_mismatch_warnings`). The relocatable path at line 1379 has
the actual stub-injection / name-renaming / reloc redirect logic.

**Plan:** lift the stub-injection block out of `write_relocatable`
into a helper, call it from both `write_relocatable` and exec-mode
`merge_inputs`. Each input that pulls in a stub gets its symbol
renamed to `signature_mismatch:<name>` (BINDING_LOCAL). The stub is
an `unreachable; end` trap function reserving FUNCTION 0 (or 1 if
`__wasm_call_ctors` is also synthesized).

**Files:** `libwild/src/wasm_writer.rs`.

**LOC:** ~80. **Risk:** low — algorithm is already proven.

---

### 1b. `wasm-export-name` attribute

**Tests unlocked:** `export-name.ll` (1)

**Status:** input `.o` files have an EXPORT section with custom names
(e.g. `wasm-export-name="bar"` on the `foo` function makes the EXPORT
say `bar`, not `foo`). Wild currently ignores the input EXPORT section.

**Plan:**

- Add `parsed.exports: Vec<InputExport>` to `parse_wasm_sections`.
  Fields: `(name: Vec<u8>, kind: u8, index: u32)`.
- During output EXPORT emit, when a function symbol carries
  WASM_SYM_EXPORTED, look up the override name from
  `parsed.exports` keyed by function index. Use the override name
  in the output EXPORT entry instead of the symbol name.
- Empty-string export names (`wasm-export-name=""`) pass through
  unchanged — lld emits an export with an empty Name field.

**Files:** `libwild/src/wasm_writer.rs`.

**LOC:** ~40. **Risk:** low — read-only addition.

---

### 1c. `--wrap NAME` body rewrite

**Tests unlocked:** `wrap_import.s` (1)

**Status:** flag already parses into `args.wrap: Vec<String>`.
Semantics not yet implemented.

**Plan:**

- Build `wrap_set` from `args.wrap`.
- For each wrap-target name, add a synth `env.__wrap_<name>`
  function import to `output_imports`.
- Track `wrap_redirects: HashMap<(input_idx, sym_idx), import_idx>`.
- During `symbol_to_output_func` population, redirect wrapped sym
  indices to the wrap-import index (instead of the local-defined
  function's index).
- Refs to `__real_<name>` resolve to the original `<name>` (no rewrite
  for these — the `__real_` symbol is already absent from
  `function_name_map`, so look up the bare `<name>` and use that).

**Files:** `libwild/src/wasm_writer.rs`, `libwild/src/args/wasm.rs`.

**LOC:** ~60. **Risk:** medium — touches function-index bookkeeping.

---

### 1d. Test-runner fixture pulls

**Tests unlocked:** ~1 (probabilistic)

**Status:** several tests are skipped via broad content patterns
(`comdat`, `--fatal-warnings`, `llvm-objdump`, `llvm-nm`). Some pass
incidentally now after the `lld_compat`, mutable-globals, and trace-
symbol work.

**Plan:** quick probe — temporarily flip each broad pattern off, run
the suite, record which previously-skipped tests pass. Add their
stems to `KNOWN_PASSING`. Restore the broad patterns.

**Files:** `wild/tests/lld_wasm_tests.rs`.

**LOC:** ~10. **Risk:** zero — only enables tests.

---

## Phase 2 — Map file (high debug value)

Target: +1 test, ~3 days. Pays off beyond its test count.

### 2. `-M` / `-Map=PATH` / `-print-map`

**Tests unlocked:** `map-file.s` (1)

**Why it's worth more than 1 test:** pairs with `-y` and
`--print-gc-sections` to make wild's wasm output as debuggable as
lld's. Useful for "what's actually in this output and where?" — both
for users and for our own diagnostic work.

**Plan:**

- Wrap `write_section` in a recording helper that captures
  `(section_id, file_offset, size, name)`.
- Per CODE function: track each body's offset within CODE (already
  computed for relocs at `body_data_starts`).
- Per data segment: track `(memory_offset, file_offset, size, name,
  source_input_basename)`.
- Format with fixed-width columns: `Addr=8 Off=8 Size=8 Out In Symbol`.
  Use `-` for sections without a memory address.
- Match lld's input-attribution syntax: `<basename>:(<funcname>)`
  for code, `<basename>:(<section_name>)` for data.
- Output destination: `--Map=PATH` writes to file, `-M` /
  `-print-map` writes to stdout.

**Files:** `libwild/src/wasm_writer.rs`, `libwild/src/args/wasm.rs`.

**LOC:** ~120. **Risk:** low — output-only, no behavioural change.

---

## Phase 3 — Cluster B & PIE

Target: +5 tests, ~5 days. Each commits independently.

### 3a. Init/fini ctor/dtor wrappers

**Tests unlocked:** `init-fini.ll`, `init-fini-no-gc.ll`, `command-exports.s` (3)

**Status:** wild has `__wasm_call_ctors` synth. Missing: the dtor-
registration wrappers and the command-export wrapper.

**Plan (per recon agent's read of `lld/wasm/Writer.cpp::createCommandExportWrapper`):**

- For each priority N in `llvm.global_dtors`, synth `.Lcall_dtors.N`
  and `.Lregister_call_dtors.N` functions. Bodies call
  `__cxa_atexit(<dtor>, …)`.
- Synth `_start.command_export` wrapper:
  `[locals=0] call __wasm_call_ctors; <user_start_args>; call _start;
  call __wasm_call_dtors; end`.
- Original `_start` gets VISIBILITY_HIDDEN; the wrapper takes its
  EXPORT slot.
- Detect `__cxa_atexit` presence as the gate for dtor synthesis.
- Function-index bookkeeping: wrappers go after the `__wasm_call_ctors`
  shift, before per-input defs. Recon estimated ~90 LOC; verify.

**Files:** `libwild/src/wasm_writer.rs`.

**LOC:** ~90. **Risk:** medium — function-idx shifts compound with
the existing ctors-at-0 shift. Test in isolation, then verify all
currently-passing tests against the shift bookkeeping.

---

### 3b. PIE PIC-base imports

**Tests unlocked:** `pie.s`, `weak-undefined-pic.s` fully; partial
progress on `shared-needed.s` and `dylink*` (unblocks but won't
fully pass)

**Status:** wild's `is_shared` path emits PIC-base imports
(`__memory_base` / `__stack_pointer` / `__indirect_function_table` /
`__table_base`). The `is_pic` (non-shared) path doesn't, even though
PIE needs them. We already disabled GOT.func internalisation under
`is_pic` (commit `c4fc1a3`), so the imports stay through Pass 4 —
but the PIC-base imports themselves still need adding.

**Plan:**

- Extend the `if layout.symbol_db.args.is_shared` block at line 9145
  to fire for `is_pic` too. Both modes need the same import set.
- Skip the local memory section (line 442) under `is_pic` as well as
  `is_shared` / `import_memory`.
- Skip the local `__stack_pointer` synth global (line 6275) under
  `is_pic` — it becomes an import.
- Verify the `merged.is_static_pic` auto-detection still works (the
  field gates wild's existing PIC-aware behaviour for inputs that use
  PIC features without explicit `--experimental-pic`).

**Files:** `libwild/src/wasm_writer.rs`.

**LOC:** ~60. **Risk:** medium — memory section gating, GC interplay
with the imported `__stack_pointer`. Several currently-passing
tests touch the `is_shared` block; the extension to `is_pic` could
inadvertently widen their scope.

---

## Phase 4 — Foundational refactors

Target: +5–7 tests, ~10 days, but with high regression risk and
diminishing test ratio. Defer until you actually need the byte
parity.

### 4a. Per-input EXPORT-emit-order tracking

**Tests unlocked:** `weak-symbols.s`, `shared-weak-symbols.s`,
`archive-export.test` (3); probably also fixes coincidental wins
in currently-passing tests.

**Why it's hard:** lld's EXPORT order varies per test. Three rules
are mutually contradictory under any simple sort:

- `stack-first.test`: by index, FUNC-first on tie.
- `weak-symbols.s`: all FUNCTIONs (by idx), THEN all GLOBALs (by idx).
- `visibility-hidden.ll`: GLOBAL-first when at lower idx.

The actual rule is per-input encounter order with synth globals
sorted by where they were synthesized (which differs across tests).

**Plan:**

- Refactor exports collection. Add `merged.export_order: Vec<EmitEntry>`
  populated as symbols are walked, preserving the order each export
  was decided.
- Drop the post-collection `sort_by` (or limit it to memory/table
  head pinning).
- Migrate the `--lld-compat --export-all` bespoke order to live in
  the same mechanism (currently uses a separate `lld_export_rank`
  table).

**Files:** `libwild/src/wasm_writer.rs`.

**LOC:** substantial — refactors the existing 200-line emit.
**Risk:** high — every currently-passing test with an EXPORT CHECK
touches this. Bisect via the suite at every step.

---

### 4b. Shared library `.so` understanding

**Tests unlocked:** `shared.s`, `shared-needed.s`, `stub-library*`
(`-library`, `-archive`), `dylink*` (`-non-pie`, base), `static-error.s`,
`no-shlib-sigcheck.s` (5–7); partial wins on `tls-export.s`.

**Why it's hard:** new infrastructure. wild currently treats `.so`
inputs as another `.o`, which produces invalid wasm output (the
`out of order section type: 0` error from `no-shlib-sigcheck.s`).

**Plan:**

- Recognise `.so` inputs as dynamic libraries (check for `dylink.0`
  custom section).
- Parse the `.so`'s exported function table.
- Resolve undef refs against the `.so`'s exports at link time;
  emit imports for resolved names and a `Needed` entry in the
  output's `dylink.0` section.
- Don't extract code/data from the `.so` — only the export table
  and the dependency chain.
- Static-link error path: if main object has unresolved refs after
  `.so` scan and `-static` is set, error like `static-error.s`
  expects.

**Files:** `libwild/src/wasm_writer.rs`, possibly `libwild/src/wasm.rs`
for input recognition.

**LOC:** ~300. **Risk:** high — new code path with dependencies on
input loading, symbol resolution, error reporting.

---

## Phase 5 — Skip (low ROI / niche / unknown)

- **Compact-imports proposal** (`compact-imports.s`): wasm extension
  that reorders imports to share name strings. Niche; one test.
  Unproven outside lld.
- **`debuginfo.test`**: DWARF section integrity. Could be a one-line
  fix or a multi-week DWARF rewrite — unknown until triage. Worth a
  30-minute probe (run `llvm-dwarfdump` on wild's output, compare to
  lld's, look for missing DIEs). Don't commit more without that.
- **`-shared` arm of why-extract**: errors on unrelated grounds
  (`/` is a directory, not openable for write). Would need shared-
  library understanding (Phase 4b) AND the `cannot open` error
  format polish.

---

## Recommended order

1. **Phase 1** first — +5 tests in ~3 days, mostly contained risk,
   each commit ships independently. Run the full suite after each.
2. **Phase 2** if linker-debug tooling is on the wishlist; otherwise
   defer. The map file pays back across every wasm bug for the next
   year.
3. **Phase 3** if PIE / init-fini are load-bearing for any actual
   user (rust-wasm builds use init-fini for static initializers).
4. **Phase 4** only if byte-for-byte shared-library parity is a
   stated requirement. Otherwise, this is open-ended.
5. **Phase 5**: leave alone unless a real user complaint surfaces.

---

## Test count projections

Cumulative `lld_wasm_tests` passing if each phase ships:

| After                | Passing | Ignored | Notes                          |
| -------------------- | ------- | ------- | ------------------------------ |
| (start)              | 122     | 102     | shipped 2026-04-30             |
| Phase 1 (actual)     | 125     | 99      | 1a/1b/1c shipped, 1d 0 wins    |
| Phase 2              | 126     | 98      | + map file                     |
| Phase 3              | 131     | 93      | + ctor wrappers, PIE           |
| Phase 4              | 136–139 | 85–88   | + exports order, .so handling  |
| (lld parity ceiling) | ~143    | ~82     | minus the niche/unknown bucket |

The tail (~80 ignored) is a mix of tests that exercise wasm features
wild doesn't implement (multi-table reference types, custom-page-size
imports outside the basic case, full LTO with the bitcode reader),
tests that need the assembler/runtime tools we don't fully integrate
(yaml2obj, llvm-readobj specific output formats), and tests for
wasm proposals that aren't on wild's roadmap.

Stretch byte parity beyond ~141 is diminishing returns — at that
point most wasm-ld features wild needs for real users (Rust
toolchain output, midnight-node-style host imports, basic shared
libraries) are already covered.
