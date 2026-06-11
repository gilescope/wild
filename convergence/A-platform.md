# Cluster A: platform / file_kind / module-layout

Merge base: 2026-04-06. OURS = `giles-merge-upstream`, THEIRS = upstream `d0914192`.
All line numbers refer to the file at the root of each tree.

---

## platform.rs

### Trait-by-trait shape comparison

| OURS shape | THEIRS shape | Reshape action |
| ---------- | ------------ | -------------- |
| `trait Arch` (line 56) — no `ThunkConfig` dependency; `Arch` is self-contained | Upstream adds `struct ThunkConfig` (lines 56-68) **above** `Arch`; `Arch` gains `thunk_config() -> Option<ThunkConfig>` and `write_thunk()` default methods (lines 172-183) | **[d]** THEIRS-extra: add `ThunkConfig` struct and the two default methods to `Arch`. No existing functionality is displaced. |
| `Arch::relocation_from_raw(r_type: u32)` (line 71) | `Arch::relocation_from_raw(r_type: <Self::Platform as Platform>::RelocationInfo)` (line 86-88) | **[b] Hard**. Upstream generalised the raw relocation type from `u32` to an associated type `Platform::RelocationInfo`. Every `Arch` impl in `elf_x86_64.rs`, `elf_aarch64.rs`, `elf_riscv64.rs`, `elf_loongarch64.rs`, `macho_aarch64.rs`, `wasm_arch.rs` must be updated. All ELF impls will use `u32` for `RelocationInfo`; Mach-O likely uses a different type. |
| `Arch::collect_relaxation_deltas` returns `(Vec<(u64, i32)>, Option<u64>)` (line 116) | Returns `(Vec<(u64, u32)>, Option<u64>)` (line 133) — second tuple element is `u32` not `i32` | **[a]** Sign change on the delta element. Rename/cast in all `riscv64` impls; verify no negative deltas are produced. |
| `trait Platform: Copy + Send + Sync + Sized + Debug + 'static` (line 174) | `trait Platform: Copy + Send + Sync + Sized + Default + Debug + 'static` (line 206-207) — adds `Default` supertrait | **[a]** Add `Default` impl to `Elf`, `MachO`, `Wasm` platform zero-sized marker structs. Trivially `#[derive(Default)]`. |
| `Platform` has no `RelocationInfo` associated type | `Platform::RelocationInfo: Copy + Send + Sync + 'static` (line 221) | **[d]** THEIRS-extra — required by the `Arch::relocation_from_raw` change above. Add the associated type to `Platform` and all implementations. |
| `Platform::SectionIterator<'data>: Iterator<Item = &'data Self::SectionHeader>` (line 203, uses `'data`) | `SectionIterator<'a>: Iterator<Item = &'a Self::SectionHeader> where Self: 'a` (lines 238-240) — uses a **separate** lifetime `'a` with an explicit where-clause | **[b] Hard**. The upstream formulation is strictly more general (callers can borrow a section header for shorter than `'data`). Ours ties the header borrow to the data lifetime, which can sometimes cause over-constrained borrow-checker errors. Switching requires touching every `ObjectFile::section_iter` call site. `fn section_iter(&self) -> ...SectionIterator<'data>` → `fn section_iter<'a>(&'a self) -> ...SectionIterator<'a>`. |
| `Platform` has no `ALL_WEAK_MIN_VISIBILITY`, `PROMOTE_WEAK_TO_EXPORT_DYNAMIC`, `APPLY_HIDDEN_VIS_DOWNGRADE_TO_DEFS` constants | THREE const bool items (lines 261-243, checked in ours 230-243) | **[c]** OURS-extra: these three constants and their surrounding doc-comments were **added by our fork** as part of the Mach-O visibility work. They have NO upstream equivalent yet. Must be preserved; they should be proposed upstream as part of the Mach-O PR. |
| `Platform::write_output_file(output: &mut Output, ...)` (line 254, `&mut`) | `Platform::write_output_file(output: &Output, ...)` (line 267-270, `&` shared ref) | **[a]** Upstream removed the `mut` from the `output` parameter — the writer takes `&Output` not `&mut Output`. Adjust our ELF/Mach-O/Wasm `write_output_file` impls accordingly. |
| `Platform` has `output_file_size_at_layout`, `SymtabPrecount`, `precount_symtab` (lines 263-294) | These three items are **absent** from upstream | **[c]** OURS-extra: Mach-O-specific size-prediction hook and the `SymtabPrecount` machinery. Must be preserved; propose upstream alongside the Mach-O writer. |
| `Platform::maybe_compress_debug_sections` — absent in OURS | THEIRS adds `fn maybe_compress_debug_sections<'data, A>(_layout: &mut Layout<'data, Self>) -> Result` with default `Ok(())` (lines 272-276) | **[d]** THEIRS-extra: add the default method. No impl required initially. |
| `Platform::finalise_find_required_sections` signature: `fn(groups: &[GroupState<Self>])` (line 411) | Upstream signature: `fn(_groups: &mut [GroupState<Self>], _symbol_db: &SymbolDb<'data, Self>) -> Result` (lines 346-350) — adds `mut`, adds `symbol_db`, adds `Result` return | **[a/b]**. Upstream added `symbol_db` parameter and `Result` return. Moderate: update our ELF and Mach-O impls; the current body is logging-only so the extra params are `_`-ignorable for now. |
| `Platform::finalise_object_sizes` signature: `fn(object: &mut ObjectLayoutState, common: &mut CommonGroupState, output_sections: &OutputSections) -> Result` (line 431-435) | Upstream: `fn(object: &mut ObjectLayoutState, common: &mut CommonGroupState)` — **drops** `output_sections` param and returns `()` not `Result` (lines 371-374) | **[a]** Removed parameter and return type. Drop the `output_sections` param from our impl bodies, change `-> Result` to `-> ()`. |
| `Platform::align_load_segment_start` present in OURS (lines ~338-343, inherited from code not shown) | Also present in THEIRS (lines 336-343) | Same shape — no action. |
| `Platform::compute_subsection_padding_deltas`, `compute_atoms`, `compute_atom_output_offsets`, `scan_atom_relocations` (OURS lines 353-401) | Absent from upstream | **[c]** OURS-extra: Mach-O atom/subsection machinery. Preserve. |
| `Platform::allocate_header_sizes(prelude, sizes, header_info, output_sections)` (line 652) | Upstream: `fn(prelude, sizes, header_info, output_sections, args: &Self::Args, total_sizes: &OutputSectionPartMap<u64>)` — gains TWO extra params (lines 578-584) | **[a]** Additive signature change. Update our ELF/Mach-O impls to accept and use `args` + `total_sizes`; upstream uses them for tight Mach-O layout. |
| `Platform::adjust_output_section_alignments`, `adjust_alignments_after_sizing` (OURS lines 629-646) | Absent from upstream | **[c]** OURS-extra: two Mach-O alignment hooks. Preserve. |
| `Platform::build_output_order_and_program_segments(custom, output_kind, output_sections, secondary)` (line 756-762) | Upstream drops the `args: &Self::Args` parameter (lines 688-693) | **[a]** Upstream removed `args`. Strip the `args` parameter from our ELF/Mach-O/Wasm impls. |
| `Platform::apply_late_size_adjustments_epilogue(state, current_sizes, extra_sizes, dynamic_symbol_defs, args, symbol_db)` (line 584) | Upstream version lacks `symbol_db` parameter (lines 530-536) | **[a]** Remove `symbol_db` param from our implementations. |
| `Platform::last_part_size_to_extend` — absent in OURS | THEIRS lines 538-545 add this default method | **[d]** THEIRS-extra: add the default method stub. |
| `Platform::default_layout_rules() -> &'static [SectionRule<'static>]` (line 733) | Upstream: `fn default_layout_rules(args: &Self::Args) -> Vec<SectionRule<'static>>` (line 665) — gains `args` param, returns owned `Vec` not static slice | **[b] Hard**. Signature change in both parameter type and return type. All three impls (ELF, Mach-O, Wasm) must be updated. Upstream presumably needed `args` to choose rules; the return type change removes the `'static` constraint so runtime-constructed rule sets are possible. |
| `Platform::create_linker_defined_symbols(symbols: &mut InternalSymbolsBuilder, ...)` (line 517) — uses non-generic `InternalSymbolsBuilder` | Upstream: `InternalSymbolsBuilder<Self>` (line 463) — generic over `Self` | **[a]** Additive generics on the `InternalSymbolsBuilder` type. Update our ELF/Mach-O/Wasm impl signatures. |
| `Platform::allocate_internal_symbol(symbol_id, def_info: &InternalSymDefInfo, ...)` — non-generic `def_info` | Upstream: `def_info: &InternalSymDefInfo<Self>` (line 626) — generic over platform | **[a]** Same pattern as above. |
| `Platform::allocate_thunk_symbol_sizes` — absent in OURS | THEIRS lines 617-622 add a default no-op | **[d]** THEIRS-extra: add it. |
| `Platform::default_symtab_entry() -> Self::SymtabEntry` — absent in OURS | THEIRS line 704 | **[d]** THEIRS-extra: add a required method. All three impls must provide it. |
| `Symbol` trait: `is_default_strippable(name) -> bool` + `with_hidden(hidden: bool) -> Self` present in OURS (lines 979, 998) | Upstream `Symbol` (lines 953-999): lacks these two methods but adds `section_index() -> object::SectionIndex` (line 1069) | **[c+d]** `is_default_strippable` and `with_hidden` are OURS-extra for Mach-O. `section_index` is THEIRS-extra for upstream. Need to add `section_index` to our Symbol impls. |
| `SectionHeader` in OURS (line 988) lacks `merge_stride() -> Option<u32>` | Upstream adds `merge_stride` default method returning `None` (OURS lines 1008-1010) | Confusingly this is already in OURS. Check: OURS line 988 starts `SectionHeader` and line 1008 shows `merge_stride`. **No action.** |
| `ObjectFile::symbol_offset_in_section` — present in THEIRS (lines 769-773) | Absent from OURS `ObjectFile` | **[d]** THEIRS-extra required method. Add to all `ObjectFile` impls. |
| `ObjectFile::symbol_value_in_section` — absent from OURS | Present in THEIRS (lines 801, ours probably 869) | **[d]** THEIRS-extra default method: add it. |
| `ObjectFile::is_symbol_in_common_section` — absent in OURS (upstream lines 950-957) | Present in THEIRS, default `false` | **[d]** Add default method. |
| `ObjectFile` in OURS has `section_name(index) -> Result<&'data [u8]>` | THEIRS also has it (line 829) | Same. No action. |
| `ObjectFile::symbols_iter` return type in OURS: `impl Iterator<Item = &'data SymtabEntry>` | THEIRS: `impl Iterator<Item = &SymtabEntry>` (no explicit `'data` on the ref) | **[a]** Minor lifetime annotation difference. THEIRS is more permissive. May require adjusting call sites that assumed `'data` on the returned ref. |
| `ObjectFile::symbol` return type in OURS: `Result<&'data SymtabEntry>` | THEIRS: `Result<&SymtabEntry>` | Same as above. |
| `ObjectFile::enumerate_sections` iterator item in OURS: `&'data SectionHeader` | THEIRS: `&SectionHeader` (no `'data`) | Same lifetime relaxation pattern. |
| `ObjectFile::section` in OURS: `Result<&'data SectionHeader>` | THEIRS: `Result<&SectionHeader>` | Same. |
| `ObjectFile::section_by_name` in OURS: `Option<(SectionIndex, &'data SectionHeader)>` | THEIRS: `Option<(SectionIndex, &SectionHeader)>` | Same. |

### Ordered reshape steps for platform.rs

1. **Add `struct ThunkConfig`** (THEIRS lines 56-68) above the `Arch` trait; add `thunk_config() -> Option<ThunkConfig>` and `write_thunk()` default methods to `Arch`.
2. **Add `Platform: Default`** to the `Platform` supertrait list (trivial `#[derive(Default)]` on marker structs).
3. **Add `type RelocationInfo: Copy + Send + Sync + 'static`** to `Platform`; set `type RelocationInfo = u32` in ELF and Wasm impls; Mach-O defines its own type.
4. **Update `Arch::relocation_from_raw`** to take `<Self::Platform as Platform>::RelocationInfo` instead of `u32`. Update all six arch impls.
5. **Fix `Arch::collect_relaxation_deltas` return type**: `i32` → `u32` on the delta element. Update riscv64 impl.
6. **Fix `Platform::SectionIterator` GAT**: add explicit `where Self: 'a` bound, switch to independent lifetime `'a`. Update `ObjectFile::section_iter` signature from `&self -> SectionIterator<'data>` to `fn section_iter<'a>(&'a self) -> SectionIterator<'a>`. Touch all call sites.
7. **Relax `ObjectFile` symbol/section return type lifetimes**: drop explicit `'data` annotations where THEIRS has generic short borrows (`symbol`, `symbol_iter`, `enumerate_sections`, `section`, `section_by_name`). This is a silent source-compat change — callers that already hold `'data` refs continue to work.
8. **Update `Platform::finalise_find_required_sections`**: add `groups: &mut [...]`, `symbol_db`, and `-> Result`. Body can be `Ok(())` for platforms that don't need it.
9. **Update `Platform::finalise_object_sizes`**: drop `output_sections` param and `-> Result`; return `()`.
10. **Update `Platform::allocate_header_sizes`**: add `args: &Self::Args` + `total_sizes: &OutputSectionPartMap<u64>`.
11. **Remove `args` param** from `Platform::build_output_order_and_program_segments`.
12. **Remove `symbol_db` param** from `Platform::apply_late_size_adjustments_epilogue`.
13. **Update `Platform::default_layout_rules`**: signature `fn(args: &Self::Args) -> Vec<SectionRule<'static>>`. Update ELF/Mach-O/Wasm impls; collect the static slice into a `Vec`.
14. **Genericise `InternalSymbolsBuilder`** and `InternalSymDefInfo` over `<Self>` in `create_linker_defined_symbols` and `allocate_internal_symbol`.
15. **Change `Platform::write_output_file`**: `&mut Output` → `&Output`.
16. **Add THEIRS-extra default methods**: `maybe_compress_debug_sections`, `last_part_size_to_extend`, `allocate_thunk_symbol_sizes`, `default_symtab_entry`.
17. **Add `Symbol::section_index() -> object::SectionIndex`** to `Symbol` trait; implement for all symbol types (ELF, Mach-O, Wasm).
18. **Add `ObjectFile::symbol_offset_in_section`** required method; implement for all `ObjectFile` impls.
19. **Add `ObjectFile::is_symbol_in_common_section`** default method (returns `false`); Mach-O overrides.
20. **Preserve OURS-extra** items: `ALL_WEAK_MIN_VISIBILITY`, `PROMOTE_WEAK_TO_EXPORT_DYNAMIC`, `APPLY_HIDDEN_VIS_DOWNGRADE_TO_DEFS` const bools; `output_file_size_at_layout`; `SymtabPrecount` associated type + `precount_symtab`; `compute_subsection_padding_deltas`, `compute_atoms`, `compute_atom_output_offsets`, `scan_atom_relocations`; `adjust_output_section_alignments`, `adjust_alignments_after_sizing`; `Symbol::is_default_strippable`, `Symbol::with_hidden`. These are candidates for upstreaming as part of the Mach-O PR.

---

## file_kind.rs

### Variant comparison

| OURS variant | THEIRS variant | Reshape action |
| ------------ | -------------- | -------------- |
| `MachODylib` (line 22) | Absent from upstream | **[c]** OURS-extra: recognises Mach-O dylibs. Upstream only supports `MachOObject`. This is a significant feature gap — Mach-O dynamic linking requires this variant. Preserve; propose upstream. |
| `FatBinary` (line 23) | `FatMachOObject` (line 21) | **[a]** Rename: `FatBinary` → `FatMachOObject`. Update all match arms across the codebase. |
| `WasmObject` (line 28) | `WasmObject` but positioned at line 22 (before `Archive`) | Variant order differs; no semantic impact. Upstream places format variants before archive variants; minor style difference. Reorder for cosmetic alignment if desired. |

### Logic differences in `identify_bytes`

| Area | OURS behaviour | THEIRS behaviour | Action |
| ---- | -------------- | ---------------- | ------ |
| Mach-O dylib detection | Checks `MH_DYLIB`, `MH_DYLIB_STUB`, and `8` (MH_BUNDLE) → `MachODylib` (lines 75-79) | Only `MH_OBJECT` accepted; others return an error (lines 72-76) | **[c]** OURS-extra. Upstream lacks dylib support; must stay on our side. |
| Fat binary magic | Checks `FAT_MAGIC.to_be_bytes()` and `FAT_MAGIC_64.to_be_bytes()` using `bytes.len() >= 8` guard (lines 81-87) | Checks `FAT_MAGIC.as_bytes()` and `FAT_CIGAM.as_bytes()` (lines 81-84) — includes `FAT_CIGAM` (reversed endian) and uses the object crate's own constant form | **[a]** Adopt THEIRS' use of `FAT_CIGAM` and `.as_bytes()` form. |
| Wasm guard | No length guard before `\0asm` check | Adds `ensure!(bytes.len() >= 8, "Invalid Wasm file (too short)")` (lines 78-80) | **[d]** THEIRS-extra defensive check. Easy to adopt. |
| LLVM bitcode | Accepts both `"BC"` and `0x0B17C0DEu32.to_le_bytes()` (bitcode wrapper) (line 92-95) | Only `"BC"` prefix (line 87) | **[c]** OURS-extra. Our version handles the bitcode-wrapper format used by Clang on macOS with `-flto`. Preserve. |
| `is_gcc_bitcode` feature gate | `cfg!(feature = "plugins")` (line 114) | `cfg!(all(feature = "plugins", unix))` (line 107) — adds `unix` constraint | **[d]** THEIRS-extra: adds `unix` gate since GCC plugins are unix-only. Adopt. |
| Display string for `WasmObject` | `"WASM object"` (line 146) | `"Wasm object"` (line 132) | **[a]** Case change. Adopt THEIRS' capitalisation. |
| Display string for `FatBinary` | `"fat binary"` | `"Fat MachO"` (line 133) | **[a]** After renaming the variant, adopt the new display string. |

### Ordered reshape steps for file_kind.rs

1. Rename `FatBinary` → `FatMachOObject`; update Display impl and all match arms.
2. Adopt `FAT_CIGAM` detection in `identify_bytes`; use `.as_bytes()` form.
3. Add Wasm length guard (`ensure!(bytes.len() >= 8, ...)`).
4. Change `is_gcc_bitcode` feature gate to `cfg!(all(feature = "plugins", unix))`.
5. Fix capitalisation: `"WASM object"` → `"Wasm object"`, `"fat binary"` → `"Fat MachO"`.
6. Preserve `MachODylib` variant and its detection logic (Mach-O dylib, stub, bundle); upstream does not support this yet.
7. Preserve bitcode-wrapper detection (`0x0B17C0DEu32.to_le_bytes()`).

---

## lib.rs / module layout

### How upstream organises modules vs OURS

Both trees declare modules flat at the crate root in `lib.rs`. There is NO nested submodule tree — every module (`elf`, `macho`, `wasm`, `platform`, `arch`, etc.) is a top-level `mod` declaration.

**Upstream `lib.rs` (THEIRS) module inventory (relevant subset):**

- `pub(crate) mod arch` (line 3)
- `pub(crate) mod elf` (line 11)
- `pub(crate) mod elf_aarch64` (line 12)
- `pub(crate) mod elf_loongarch64` (line 13)
- `pub(crate) mod elf_riscv64` (line 14)
- `pub(crate) mod elf_writer` (line 15)
- `pub(crate) mod elf_x86_64` (line 16)
- `pub(crate) mod macho` (line 37)
- `pub(crate) mod macho_aarch64` (line 38)
- `pub(crate) mod macho_writer` (line 39)
- `pub(crate) mod platform` (line 61)
- `pub(crate) mod thunks` (line 75) — **THEIRS-extra**
- `pub(crate) mod timing` (line 78)
- `pub(crate) mod wasm` (line 83)
- `pub(crate) mod wasm_wasm32` (line 84) — named `wasm_wasm32` not `wasm_arch`
- `pub(crate) mod wasm_writer` (line 85)
- `pub(crate) mod input_section_id` (line 28) — **THEIRS-extra**
- `pub(crate) mod compression` (line 6) — **THEIRS-extra** (ours has `elf_compress`)

**OURS (`lib.rs`) extras not in THEIRS:**

- `pub(crate) mod elf_abbrev_dedup` (line 16) — OURS-extra
- `pub(crate) mod elf_compress` (line 17) — OURS-extra (renamed to `compression` in THEIRS)
- `pub(crate) mod elf_line_v5` (line 18) — OURS-extra
- `pub(crate) mod lto` (line 49) — OURS-extra (LTO driver module)
- `pub(crate) mod macho_codesign` (line 52) — OURS-extra
- `pub(crate) mod macho_lto` (line 57, cfg-gated) — OURS-extra
- `pub(crate) mod wasm_arch` (line 105) — renamed to `wasm_wasm32` in THEIRS
- `pub(crate) mod sdk_cache` (line 38) — OURS-extra
- `pub(crate) mod suffix_share` (line 39) — OURS-extra
- `pub(crate) mod incremental_cache`, `layout_snapshot`, `parsed_input_cache`, `parse_skip`, `tier3_skip` — OURS-extra incremental-link machinery (not even listed as `mod` items in THEIRS)
- `pub(crate) mod save_dir` and `output_trace` — also OURS-extra
- `pub(crate) mod gc_stats`, `glob_match`, `hash`, `sharding`, `validation`, `verification`, `perf`, `sframe`, `diff`, `diagnostics`, `dwarf_address_info`, `symbol`, `expression_eval`, `export_list`, `version_script`, `string_merging` — present in BOTH; no action.
- `#[cfg(unix)] pub mod daemon` + `pub mod daemon_protocol` — OURS-extra
- `pub(crate) mod llvm_tools` — OURS-extra
- `pub mod error` — both; THEIRS also exports `make_executable` from `fs` (`pub use fs::make_executable` line 101).

**THEIRS-extra not in OURS:**

- `pub(crate) mod thunks` — range-extension thunk machinery, needed by the AArch64 thunk support
- `pub(crate) mod input_section_id` — upstream introduced a separate module for input section IDs
- `pub(crate) mod compression` — replaces `elf_compress` with a format-neutral name

**Naming divergences:**

| OURS name | THEIRS name | Action |
| --------- | ----------- | ------ |
| `wasm_arch` | `wasm_wasm32` | Rename file and module. The `_wasm32` suffix mirrors the `elf_x86_64`/`elf_aarch64` pattern — arch-specific impl named `<format>_<arch>`. |
| `elf_compress` | `compression` | Rename file and module. Upstream generalised it beyond ELF. |
| `linker_plugins` path: `lto/elf_gold.rs` or `lto/elf_gold_disabled.rs` (OURS) | `linker_plugins_disabled.rs` (fallback, THEIRS) | OURS reorganised plugin code under an `lto/` subdirectory. Upstream keeps it flat. This is a structural divergence but does not affect public API. Low risk to leave as-is until the LTO PR. |

**`write_output_file` call convention change in lib.rs:**

- OURS calls `P::write_output_file::<A>(&mut output, &layout)` (line 910)
- THEIRS calls `P::write_output_file::<A>(&output, &layout)` (line 349) — drops `mut`

**`layout_rules_builder.build` signature:**

- OURS: `layout_rules_builder.build::<P>()` — no args
- THEIRS: `layout_rules_builder.build::<P>(args)` — takes `args`
Consistent with the `default_layout_rules(args)` change noted above.

**`symbol_db.add_inputs` signature:**

- OURS: `add_inputs(..., loaded, clean_input_paths, bundle)` — extra incremental args
- THEIRS: `add_inputs(..., loaded)` — simpler

### Ordered reshape steps for lib.rs / module layout

1. Add `pub(crate) mod thunks;` declaration.
2. Add `pub(crate) mod input_section_id;` declaration.
3. Rename `wasm_arch.rs` → `wasm_wasm32.rs`; update `mod wasm_arch` → `mod wasm_wasm32` in `lib.rs` and all `use crate::wasm_arch::` imports.
4. Rename `elf_compress.rs` → `compression.rs`; update `mod elf_compress` → `mod compression`; update all `use crate::elf_compress::` imports.
5. Export `pub use fs::make_executable;` (THEIRS line 101).
6. Change `EnvFilter::from_default_env()` → `EnvFilter::from_env("WILD_LOG")` in `setup_tracing`.
7. Update `write_output_file` call to take `&output` not `&mut output`.
8. Update `layout_rules_builder.build::<P>()` → `build::<P>(args)`.
9. Preserve all OURS-extra modules (incremental cache tiers, daemon, LTO, Mach-O codesign, etc.) — do not remove them.

---

## Risk notes

1. **`Arch::relocation_from_raw` signature change** (step 4) is the riskiest single item: every architecture's relocation parsing is affected. The change is mechanical (ELF arches all use `u32`; Mach-O likely needs a newtype), but any mis-mapping of the `RelocationInfo` type will silently break relocation handling at link time. Needs per-arch test coverage.

2. **`SectionIterator` GAT lifetime change** (step 6) is likely to cause widespread borrow-checker errors in `layout.rs` and `parsing.rs` where section headers are iterated. The change is structurally correct but requires touching potentially dozens of call sites. Bisect: make the change in `platform.rs` first, let the compiler guide the fix sites.

3. **`default_layout_rules` return type change** (`&'static [...]` → `Vec<...>`) will affect performance if called in a hot path. Check call frequency; if called once per link during rules construction, the allocation is negligible.

4. **`Platform::write_output_file` `&mut` → `&` change** implies that upstream's `Output` type uses interior mutability (likely a `Mutex<...>` or `AtomicCell<...>`) for the mmap resize operation. Verify the `file_writer::Output` type in both trees before mechanically dropping `mut`, otherwise the change will not compile.

5. **OURS-extra incremental link machinery** in `lib.rs` (`load_inputs_and_link` is ~800 lines in OURS vs ~100 lines in THEIRS). This is a fundamental divergence in the link driver. For upstream contribution, this logic should be gated behind a Cargo feature flag so upstream consumers who don't want incremental caching don't pay the code complexity cost.

6. **`MachODylib` variant** in `file_kind.rs` is load-bearing for all Mach-O dynamic linking. Upstream lacks this entirely. Removing it would break dylib input processing. It must stay until upstream adds dynamic Mach-O support.

7. **`Symbol::with_hidden` and `is_default_strippable`** are OURS-extra methods that affect Mach-O symbol visibility. ELF impls can provide trivial stubs (`is_default_strippable` returns `false`, `with_hidden(self, _) -> Self { self }`).

8. **`lto/` submodule tree** (OURS organises LTO under `lto/elf_gold.rs`) diverges from THEIRS' flat layout. If upstream eventually merges LTO, the module path will conflict. Consider whether to flatten back to `linker_plugins.rs` now or defer.
