# Cluster D: cross-cutting refactors + args core

Reference: OURS = `/Users/gilescope/git/gilescope/rec/linker/`
           THEIRS = `/Users/gilescope/git/gilescope/rec/linker-upstream/` (d0914192)

---

## Cross-cutting refactors

| Refactor | Upstream new shape | Does OURS build on old shape? | Rating | Reshape action |
| ------------------------------------------ | ---------------------------------------------------------------------------------------------------- | ----------------------------- | ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **#1841 — Introduce global `InputSectionId`** | New file `input_section_id.rs` defines `InputSectionId(u32)` + `SectionIdRange`. Every resolved object/script carries a `section_id_range: SectionIdRange`; `SymbolDb` gains `next_input_section_id` + `section_part_ids: Vec<PartId>`. `UnloadedSection` drops its `part_id` field; `UnloadedDebugInfo` becomes a unit variant. | OURS has none of this. We still hold `part_id` inside `UnloadedSection` (resolution.rs:613), `UnloadedDebugInfo(PartId)` (resolution.rs:599), and `Section { index, part_id, … }` (layout.rs:1284-1291). No `SectionIdRange` anywhere in our tree. | **RED** | Add `input_section_id.rs` verbatim. Then: (a) add `section_id_range` to `ResolvedObject`/`ResolvedLinkerScript`/`NotLoaded` in resolution.rs; (b) add `next_input_section_id` + `section_part_ids` to `SymbolDb`; (c) strip `part_id` from `UnloadedSection` and make `UnloadedDebugInfo` a unit variant; (d) replace every per-slot `part_id` access with `symbol_db.section_part_ids[section_id_range + section_index]` pattern shown in upstream layout.rs:4244-4250. Touches: symbol_db.rs, resolution.rs, grouping.rs, layout.rs, string_merging.rs, elf_writer.rs. |
| **#1842 — Remove `section_index` from `layout::Section`** | `Section { size, flags }` — two fields only (layout.rs:1230-1235). `subsection_tracking` HashMap gone from both `ObjectLayoutState` and `ObjectLayout`. Section index now derived at point-of-use from caller context or `SectionIdRange`. | OURS `Section` has `{ index: object::SectionIndex, part_id, size, flags }` (layout.rs:1284). `subsection_tracking: HashMap<usize, SubsectionTracking>` present in both `ObjectLayoutState` (layout.rs:1169) and `ObjectLayout` (layout.rs:755). `section_key = section.index.0` used at layout.rs:4065. `SubsectionTracking` is a 36-usage struct unique to OURS (Mach-O atom GC). | **RED** | `Section.index` removal is **blocked** by our Mach-O atom-GC feature (`SubsectionTracking`, `triggering_offset` in `SectionLoadRequest`). Upstream has no `Atoms`/`SubsectionTracking` at all. Before merge: either (a) extract the atom-index bookkeeping into a side-table keyed by `InputSectionId` (aligns with #1841), eliminating `section.index`; or (b) negotiate with upstream to accept the atom-GC data structure before applying #1842. Removing `part_id` from `Section` is intertwined — do both together in one PR to upstream. |
| **#1824 — Remove ELF-specific stuff from `InternalSymDefInfo`** | `InternalSymDefInfo<'data, P: Platform>` gains `symbol: P::SymtabEntry` (parsing.rs:67). `elf_symbol_type: SymbolType` and `is_hidden: bool` fields removed. `Platform` trait gains `default_symtab_entry() -> Self::SymtabEntry` (platform.rs:704) and `Symbol` trait gains `with_hidden(self, hidden: bool) -> Self` (platform.rs:998). `hide()` / `set_hidden()` now delegate to `self.symbol.with_hidden(…)`. | OURS `InternalSymDefInfo<'data>` (no platform param) has `elf_symbol_type: SymbolType` + `is_hidden: bool` (parsing.rs:67-74). Used in 43 places. `elf_symbol_type.raw()` called at elf_writer.rs:4108/4444/4455; `is_hidden` read at elf_writer.rs:4467, layout.rs:3486, symbol_db.rs:1981/2287/2467, elf.rs:1521, wasm.rs:49/93. We also carry `ImportDynamicSymbol` placement variant (parsing.rs:106) and `DefsymAbsolute`/`DefsymSymbol` variants that upstream collapsed into `Redirect`. | **RED** | (a) Add `default_symtab_entry()` to `Platform` trait and implement for Elf/MachO/Wasm. (b) Add `with_hidden()` to `Symbol` trait. (c) Add `P` type param to `InternalSymDefInfo`, replace `elf_symbol_type`/`is_hidden` with `symbol: P::SymtabEntry`. (d) Remove `ImportDynamicSymbol` variant — upstream now handles `GLIBC_ABI_DT_RELR` via `load_glibc_abi_dt_relr_version()` (elf.rs:2008). (e) Keep `DefsymAbsolute`/`DefsymSymbol` vs upstream's `Redirect` divergence as a separate concern (see #1917 row). |
| **#1922 — `default_layout_rules` takes `Args` and returns `Vec`** | `fn default_layout_rules(args: &Self::Args) -> Vec<SectionRule<'static>>` on `Platform` trait (platform.rs:665). ELF impl builds a `Vec` and appends sframe rule conditioned on `args.experimental_sframe` (elf.rs:1704-1719). Call site uses `&P::default_layout_rules(args)`. | OURS `fn default_layout_rules() -> &'static [SectionRule<'static>]` — returns a static slice, no `args` (platform.rs:733). Implementations: elf.rs:1676, macho.rs:4332, wasm.rs:1260. 6 call sites. | **YELLOW** | Straightforward signature change. Add `args: &Self::Args` param; change return from `&'static [SectionRule]` to `Vec<SectionRule<'static>>`; update 3 impl sites (elf, macho, wasm) and 2 call sites in layout_rules.rs. Elf impl gains sframe conditional (upstream elf.rs:1705-1718). No logic conflict — OURS handles sframe separately. |
| **#1917 — Change internal representation of `--sym-def` symbols** | `SymbolPlacement::DefsymAbsolute` and `SymbolPlacement::DefsymSymbol` replaced by single `Redirect(Redirect<'data>)` variant. `Redirect` holds `kind: RedirectKind` (DefSym or Script), `expression: Expression<'data>`, `loc: SymbolLoc`. `defsym` field on `ElfArgs` becomes `Vec<(String, String)>` (raw string, not pre-parsed `DefsymValue`). `Platform` trait `defsym()` returns `&[(String, String)]`. Parsing deferred to `Prelude::new()` via `linker_script::parse_expression`. | OURS has `DefsymValue { Value(u64), SymbolWithOffset(String, i64) }` enum in args.rs:690. `defsym` field: `Vec<(String, DefsymValue)>`. `platform::Args::defsym()` returns `&[(String, DefsymValue)]`. `DefsymAbsolute(u64)` and `DefsymSymbol(&'data str, i64)` are live variants (parsing.rs:96-100). Used in parsing.rs:248-250, elf_writer.rs, symbol_db.rs. `ImportDynamicSymbol` variant also unique to OURS. Also OURS lacks `SegmentStart(SegmentName, u64)` variant (upstream parsing.rs:101) and `SegmentName` enum. | **RED** | This is a three-part change that must be coordinated: (1) Remove `DefsymValue` from args.rs, change `ElfArgs::defsym` to `Vec<(String, String)>`; update `parse_defsym_expression` in args/elf.rs to not pre-parse. (2) Replace `DefsymAbsolute` + `DefsymSymbol` variants with `Redirect`. (3) Add upstream's `SegmentStart(SegmentName, u64)` variant + `SegmentName` enum + `segment_start_override()` on ElfArgs + `ttext`/`tdata`/`tbss` fields (these are new upstream features OURS lacks). Also remove `ImportDynamicSymbol` (refactor to `load_glibc_abi_dt_relr_version` style). Interacts with #1824 (both touch `SymbolPlacement`). |
| **#1963 — Simplify `elf_writer::get_symbol_attributes`** | Return type changed from `(u32, u8)` to `(SymbolSection, u8)` where `SymbolSection` is an enum (`Raw(u16)` / `Index(u32)`) defined in upstream elf_writer.rs:166. Object branch: uses `obj.section_part_id(section_index, &layout.symbol_db.section_part_ids)` to find part_id. Prelude branch: `def_info.symbol.st_type()` replaces `def_info.elf_symbol_type.raw()`. Linker-script branch: uses `script.internal_symbols.symbol_definitions[local_index.0]`. `get_defsym_attributes` split out (upstream elf_writer.rs:4400). | OURS returns `(u32, u8)` (elf_writer.rs:4036). Uses `def_info.elf_symbol_type.raw()` at line 4108. Accesses sections via `obj_layout.sections.get(section_index.0)` pattern (line 4050-4062) without `section_part_ids`. | **YELLOW** | Blocked partially by #1841 (needs `section_part_ids`) and #1824 (needs `def_info.symbol.st_type()`). Once those are done, the simplification is mechanical: introduce `SymbolSection` enum, change return type, split out `get_defsym_attributes`, adopt `section_part_id()` helper call. Low independent risk but cannot land before #1841+#1824. |

---

## args.rs / args/elf.rs shape comparison

### `CommonArgs` struct

Both sides are nearly identical. OURS adds two fields absent from upstream:

- `incremental_cache: IncrementalCacheMode` (args.rs:92) — our incremental linking machinery (`IncrementalCacheMode` enum at args.rs:325; `IncrementalCache` module is entirely OURS-only, no upstream equivalent).
- `emit_patch: Option<PathBuf>` (args.rs:103) — our macOS live-patch file feature.

Upstream adds one field absent from OURS:

- `has_flavor: bool` (upstream args.rs:97) — supports LLVM `ld.lld`-style `-flavor gnu|darwin|link` dispatch. Upstream `Args::new()` logic is restructured around this (upstream args.rs:119-143).

### `ElfArgs` struct

Upstream-only fields (OURS lacks all of these):

| Field | Upstream location | Semantics |
| ----------------------------------------- | -------------------- | --------------------------------- |
| `ttext: Option<u64>` | args/elf.rs:93 | `-Ttext <addr>` override |
| `tdata: Option<u64>` | args/elf.rs:94 | `-Tdata <addr>` override |
| `tbss: Option<u64>` | args/elf.rs:95 | `-Tbss <addr>` override |
| `use_android_relr_tags: bool` | args/elf.rs:123 | `--use-android-relr-tags` |
| `nmagic: bool` | args/elf.rs:129 | `--nmagic` (disables RELRO + page alignment) |
| `experimental_sframe: bool` | args/elf.rs:133 | `--experimental-sframe` (gates sframe rule in `default_layout_rules`) |
| `debug_compression_kind: Option<CompressionKind>` | args/elf.rs:135 | `--compress-debug-sections=zlib\|zstd\|none` (upstream's simpler type, 2 variants) |

OURS-only fields:

| Field | Our location | Semantics |
| ----------------------------------------- | -------------------- | ------------------------------------------------- |
| `compress_debug_sections: DebugCompression` | args/elf.rs:134 | Our richer type: `None\|Zstd` (default `None`) |
| `upgrade_debug_line: DebugLineUpgrade` | args/elf.rs:143 | `--upgrade-debug-line=v5` DWARF 4→5 rewrite |
| `dedup_debug_abbrev: bool` | args/elf.rs:154 | `--dedup-debug-abbrev` cross-CU dedup |
| `opt_level: u8` | args/elf.rs:168 | `-O<N>` gate for debug-opt passes |

### `defsym` type conflict (additive vs structural)

OURS: `defsym: Vec<(String, DefsymValue)>` where `DefsymValue` is pre-parsed.
Upstream: `defsym: Vec<(String, String)>` — raw string pair, parsed lazily in `Prelude::new()`.

This is a **structural conflict**: the type of the second tuple element differs. Any upstream code that calls `args.defsym()` and pattern-matches on `DefsymValue` must be rewritten when adopting upstream's shape.

### `compression` approach conflict

OURS has three dedicated modules (`elf_compress.rs`, `elf_abbrev_dedup.rs`, `elf_line_v5.rs`) running as post-write passes over the already-written buffer, controlled by OURS-only fields. Upstream's `compression.rs` integrates zstd compression into the layout/writing pipeline using `CompressionKind`. These are parallel but architecturally divergent implementations of the same feature. Upstreaming OURS's richer feature set will require design negotiation.

### `PartId` numeric assignment divergence

OURS `NUM_SINGLE_PART_SECTIONS = 38` (part_id.rs:69). Upstream = 54.
Upstream adds `UNMAPPED = PartId(0)` (shifting all others +1) plus Wasm sections `WASM_TYPE`–`WASM_DATA` (PartId 42-53) and Mach-O `DYLD_CHAINED_FIXUPS`, `CHAINED_FIXUP_TABLE`, `SYMTAB_COMMAND`, `CODE_SIGNATURE_COMMAND`, `CODE_SIGNATURE` (PartId 37-41).

OURS adds `OBJC_SELREFS = PartId(36)` and `OBJC_IMAGEINFO = PartId(37)` after `ENTRY_POINT = PartId(35)`.

**This is a silent integer-constant conflict**: any serialized data (incremental cache, layout snapshots) keyed by raw `PartId` values will silently misinterpret sections if mixed. The numbering must be reconciled before any merge. Upstream's `UNMAPPED = 0` sentinel is architecturally load-bearing for the `section_part_ids` global array.

---

## Merge-ordering implications

### Must be reshaped BEFORE (or during) merge

1. **`PartId` renumbering** — arithmetic identity of every `PartId` constant changes. Do this first; it is a prerequisite for all other refactors. OURS must: accept `UNMAPPED=0` (shifting FILE_HEADER from 0 → 1), reconcile Mach-O-specific IDs (`OBJC_SELREFS`/`OBJC_IMAGEINFO` vs upstream's `DYLD_CHAINED_FIXUPS` block), and accept upstream's Wasm IDs.

2. **#1841 `InputSectionId` introduction** — prerequisite for #1842 (removes `Section.index`), #1963 (uses `section_part_id()` helper), and the `string_merging.rs` upstream path. Large surface: symbol_db, resolution, grouping, layout, string_merging.

3. **#1824 `InternalSymDefInfo<P>` generification** — prerequisite for #1963. Also prerequisite for `SymbolPlacement` refactoring (#1917). Requires removing `elf_symbol_type`/`is_hidden` inline fields from 43 call sites.

4. **#1917 `SymbolPlacement` / `defsym` repr** — prerequisite for correct `section_id()` impl on upstream's `InternalSymDefInfo`. Must also add `SegmentStart` + `SegmentName` + `ttext`/`tdata`/`tbss` (new upstream features we lack).

5. **`-flavor` / `has_flavor` in `CommonArgs`** — upstream refactored `Args::new()` dispatch around this. Our `new()` is structurally different. Must reconcile before any args-touching PR.

### Can be resolved during merge (lower dependency)

1. **#1922 `default_layout_rules` signature** — self-contained, no deep prerequisite, mechanical change.

2. **#1842 `Section.index` removal** — blocked by #1841 and by our `SubsectionTracking`/atom-GC feature. Needs design discussion with upstream before upstreaming; atom-GC may itself need to land first as a new feature, then #1842 can follow.

3. **#1963 `get_symbol_attributes` simplification** — purely internal to `elf_writer.rs`, mechanical once #1841+#1824 done.

### OURS-only features to carry forward (not blocked but need homes)

- `IncrementalCacheMode` + `emit_patch` in `CommonArgs`
- `SubsectionTracking` / `Atom` / `atom_output_offsets` (Mach-O atom GC + `-order_file`)
- `triggering_offset` in `SectionLoadRequest`
- `compress_debug_sections` / `upgrade_debug_line` / `dedup_debug_abbrev` / `opt_level` ElfArgs fields + three post-write pass modules (`elf_compress.rs`, `elf_abbrev_dedup.rs`, `elf_line_v5.rs`)
- `unexport_list` in `SymbolDb`
- `OBJC_SELREFS` / `OBJC_IMAGEINFO` PartId constants

---

## Risk notes

- **`PartId` divergence (part_id.rs:19-69 ours vs :21-81 upstream)**: `NUM_SINGLE_PART_SECTIONS=38` (ours) vs `54` (upstream). Every `PartId` integer constant after `ENTRY_POINT` has a different numeric value. Silent data corruption in any test that serialises `PartId` raw values.
- **`layout::Section` struct (layout.rs:1284 ours vs :1230 upstream)**: We have 4 fields; upstream has 2. All code that reads `section.index` (e.g. layout.rs:4065) or `section.part_id` directly must change to the `section_part_id(section_index, &symbol_db.section_part_ids)` indirection.
- **`SectionSlot::UnloadedDebugInfo(PartId)` (resolution.rs:599 ours) vs `UnloadedDebugInfo` unit (upstream:685)**: Pattern-match exhaustion will silently fail to carry the part_id if the tuple-variant is dropped before the global array is wired up.
- **`InternalSymDefInfo` type parameter (parsing.rs:67 ours vs :66 upstream)**: Upstream adds `<P: Platform>`; ours does not. Every generic bound that mentions `InternalSymDefInfo` gains a `P` param — layout.rs `InternalSymbols<'data>` becomes `InternalSymbols<'data, P>` (upstream layout.rs:812).
- **`get_symbol_attributes` return type (elf_writer.rs:4036 ours `(u32, u8)` vs :4334 upstream `(SymbolSection, u8)`)**: Callers at elf_writer.rs:4206 and :4418 destructure the result differently. Must update both call sites.
- **`defsym` field type (`Vec<(String, DefsymValue)>` ours vs `Vec<(String, String)>` upstream, elf.rs:87 / platform.rs:1402 ours vs :1238 upstream)**: Type-level conflict; any `match` on `DefsymValue` variants in our parsing.rs:248-250 must be removed.
- **`default_layout_rules` trait fn (platform.rs:733 ours vs :665 upstream)**: Missing `args` param and wrong return type; wasm.rs:1260, elf.rs:1676, macho.rs:4332 all need updating.
