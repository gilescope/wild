# Wild fork → upstream convergence plan

**Goal:** reduce future merge pain with `wild-linker/wild` by reshaping our fork's
code to match upstream's structural shape *before* merging, en route to eventually
upstreaming our work. Alignment is structural (types, traits, signatures, naming,
module layout) — **no functionality or performance is sacrificed**.

## Situation (measured 2026-05-30)

| | ref | commits since base | files changed |
| --- | --- | --- | --- |
| merge base | `b3c508ed` (2026-04-06) | — | — |
| our `main` | `e641617f` | ~411 | ~893 |
| `upstream/main` | `d0914192` (2026-05-22) | 121 | ~243 |

Upstream's 121 commits: 27 `fix`, 25 `chore`, **15 `port`**, 13 `test`, 12 `feat`,
9 `build` (dep bumps), 6 `refactor`. The `port` commits are the collision engine:
**6 WASM**, **11 Mach-O** — both our flagship features, independently re-implemented
by upstream at the *same file paths*.

### The asymmetry that drives everything

| Layer | Ours | Upstream | Conflict shape |
| --- | --- | --- | --- |
| **Writers** (`wasm_writer.rs`, `macho_writer.rs`) | complete (16k / 11k lines) | stubs (21 / 970 lines) | **low** — ours wins wholesale |
| **Readers / object-model** (`wasm.rs`, `macho.rs`) | complete | mature (1787 / 1800 lines) | **high** — genuine shape clash |
| **Platform / args / file_kind** | complete | mature | **high** — both built abstractions |
| **Shared core** (`layout.rs`, `symbol_db.rs`, …) | extended | refactored (#1841/#1842/#1824/#1917/#1922/#1963) | **highest** — silent semantic conflicts |

Writers are safe; the reader/platform/shared layers are where alignment buys the most.

## Toolchain note

Workspace needs **rustc 1.94**. Default toolchain here was 1.91; `stable` is 1.94.
A directory override to `stable` is set so all cargo calls use 1.94. (No
`rust-toolchain.toml` is committed upstream; don't add one without consent.)

## Environment hazards (Phase 0)

- A build artifact named `main` and an `ld` symlink sit in the repo root and make
  the bareword `main` ambiguous to git. Always use `refs/heads/main` / SHA. These
  should be gitignored (they are currently tracked-or-untracked clutter — verify
  before deleting; they're build outputs, not source).
- Integration branch: `giles-merge-upstream` (created off `main`).
- Use **merge, not rebase** — 411 commits are pushed and a collaborator exists.

---

## Cluster findings (detail in A/B/C/D-*.md)

### A — platform / file_kind / module layout

- `Platform` gains `Default` supertrait + `RelocationInfo` assoc type (the latter
  changes `Arch::relocation_from_raw(u32)` → `RelocationInfo` across 6 arch impls).
- `SectionIterator` GAT lifetime `'data` → `'a` (relaxation; ripples to call sites).
- Many method-sig deltas (write_output_file `&mut`→`&`; finalise_object_sizes drops
  params; default_layout_rules `&'static[]`→`Vec` + `args`; genericise
  InternalSymbolsBuilder/InternalSymDefInfo over `<Self>`).
- Module renames: `wasm_arch`→`wasm_wasm32`, `elf_compress`→`compression`; add
  `thunks`, `input_section_id` modules.
- **Preserve (OURS-extra):** 3 Mach-O visibility consts, `output_file_size_at_layout`,
  `SymtabPrecount`, atom/subsection hooks, alignment hooks, `MachODylib` file-kind,
  bitcode-wrapper detection, the whole incremental-link + LTO + codesign + daemon
  module set.

### B — wasm reader + args

- Root mismatch: our `WasmSymbol` (flat bools + `symbol_names` sidetable, 6 `unsafe`
  blocks) vs upstream `{kind, flags, index, offset, name_start, name_len}` `Copy+Default`.
- Add `WasmSymbolKind` enum, `section_id`/`reloc_type` modules, `SectionHeader{id,
  payload_range, name_range}`.
- Migrate parser `object::WasmFile` → `wasmparser` (needs `wasmparser` in
  `libwild/Cargo.toml`; upstream uses 0.248 — **version skew risk**).
- **Real bug found:** our `build_output_order_and_program_segments` emits ELF section
  ids for WASM output. Fix to WASM ids.
- args: all 40+ of our flags are additive; two value clashes to adopt-theirs
  (`WASM_PAGE_ALIGNMENT` 64 KiB) / keep-ours (`should_merge_sections=false`).
- Writer: theirs is a stub — keep ours wholesale.

### C — macho reader + args + feature gating

- Ours is donor (4737/10931/2776 lines) vs upstream skeleton (~1800/970/170).
- `SectionHeader` `#[repr(transparent)]` newtype (ours, enables trait impls + ~30
  `unsafe` casts) vs plain `type` alias (theirs). Keep ours; guard with
  size/align asserts.
- `SegmentType` enum (ours, supports `__DATA_CONST`/ObjC/TLS) vs bool-pair
  `ProgramSegmentDef` (theirs). Keep ours; document for upstream.
- **Feature gate gap:** upstream has `macho = []` (off by default + runtime bail);
  ours has the runtime `cfg!(feature="macho")` guard but **no such feature declared**
  — latent foot-gun. Add `macho = []` and `wasm = []`.
- `relocation_from_raw` sig: theirs takes `RelocationInfo`, ours `u32` — reconcile.
- **Correctness fix:** PAGE21 missing `PageMask::SymbolPlusAddendAndPosition(PAGE_MASK_4KB)`.
- Upstream typo to flag upstream-side: `DyldChainedFixupsImporstFormat`.
- Adopt upstream's ~20 `pub type` aliases (FileHeader/SegmentCommand/…) — low-risk
  rename that aligns the API surface.

### D — cross-cutting refactors (the RED zone)

| Refactor | SHA | Rating | Note |
| --- | --- | --- | --- |
| PartId renumber (ours 38 vs theirs 54, +UNMAPPED/wasm/macho) | — | **RED** | ground zero; do first |
| global `InputSectionId` | #1841 | **RED** | absent from ours; threads `Vec<PartId>` + `section_id_range` everywhere |
| `Section` drops `index` (+ removes SubsectionTracking) | #1842 | **RED** | blocked by our atom-GC built on `section.index` |
| `InternalSymDefInfo<P>` | #1824 | **RED** | `is_hidden` field→method, ~43 sites, P-threading |
| `SymbolPlacement::Redirect` (+defsym repr) | #1917 | **RED** | collapses our Defsym variants; 8 match sites |
| `default_layout_rules(args)->Vec` | #1922 | **YELLOW** | mechanical sig change |
| `get_symbol_attributes` return type | #1963 | **YELLOW** | mechanical, after #1841+#1824 |

Also: `CommonArgs` `-flavor`/`has_flavor` (theirs) vs `incremental_cache`/`emit_patch`
(ours) — `Args::new` dispatch differs; reconcile. Debug-compression designs diverge
(ours: post-write pass, richer flags; theirs: pipeline-integrated `CompressionKind`)
— **field-name difference means git won't flag the conflict**; catch in review.

---

## Canonical sequencing (Phase 2)

Ordered low-risk → high-risk so the tree stays green and each step is an
independently-reviewable (and later independently-upstreamable) commit.

**2.0 Trivial/additive** (no behaviour change)

1. `Default` on `Elf`/`MachO`/`Wasm` marker structs.
2. `ThunkConfig` struct + `Arch` default methods.
3. `file_kind.rs`: `FatBinary`→`FatMachOObject`, display casing, wasm length guard,
   `unix` gate on gcc-bitcode. Keep `MachODylib` + bitcode-wrapper.
4. Add `macho = []`, `wasm = []` Cargo features.
5. `EnvFilter` → `WILD_LOG`.

**2.1 Module renames** — `wasm_arch`→`wasm_wasm32`, `elf_compress`→`compression`.

**2.2 Platform trait signatures** — method-sig alignment; preserve OURS-extra.
Hard subitems: `RelocationInfo` assoc type; `SectionIterator` GAT; `default_layout_rules`.

**2.3 wasm.rs reader** — adopt upstream object-model; kill 6 unsafe; wasmparser;
fix the section-id bug; keep writer + COMDAT + dylib detection.

**2.4 macho.rs reader** — reloc sig, PAGE21 PageMask fix, type aliases,
`symbol_offset_in_section` rename, multi-segment guard, is_default_strippable/with_hidden;
keep enum + all GC/atom/codesign machinery.

**2.5 cross-cutting RED** (do last, heavy testing):
PartId renumber → #1841 → (#1824 + #1917 together) → #1842 → #1922 → #1963.

Then **Phase 3** merge, **Phase 4** validate + draft PR on origin, **Phase 5** weekly
merge cadence + this FORK-DELTA map.

## FORK-DELTA (ongoing-merge map)

The hook points where our fork diverges from upstream — check these every merge:

- `libwild/src/platform.rs` — trait surface (the integration seam)
- `libwild/src/{wasm,macho}.rs` + `args/{wasm,macho}.rs` — dual implementations
- `libwild/src/{layout,symbol_db,resolution,elf_writer}.rs` — shared core both edit
- `libwild/src/part_id.rs` / `output_section_id.rs` — id numbering must stay reconciled
- `libwild/Cargo.toml` `[features]` + `[dependencies]` — ours is a superset
- OURS-only module set (incremental cache, lto/, daemon, macho_codesign, …) — additive
