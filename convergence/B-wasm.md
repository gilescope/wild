# Cluster B: wasm reader + args

## wasm.rs

### 3-Column API Shape Comparison

| Concept | OURS (fork) | THEIRS (upstream d0914192) | Reshape action |
| ----------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| **`File<'data>` struct fields** | `data`, `symbols: Vec<WasmSymbol>`, `symbol_names: Vec<&'data [u8]>`, `sections`, `section_data`, `section_names` — all parallel vecs indexed by our contiguous section index | `data`, `version: u32`, `sections: Vec<SectionHeader>`, `standard_section_index: [Option<u32>; 13]`, `symbols`, `segments: Vec<WasmSegmentInfo>`, `reloc_sections: Vec<WasmRelocSection>`, `linking_version: Option<u32>`, `target_features_raw: Option<&'data [u8]>` | **Replace** our `File` with theirs. Drop `symbol_names`/`section_data`/`section_names` parallel vecs; upstream encodes names as byte ranges into `data` and accesses section payloads via `payload_range`. |
| **`SectionHeader` struct** | `index: usize`, `size: u64`, `is_code`, `is_data`, `is_comdat` — four booleans, no range | `id: u8`, `payload_range: Range<u32>`, `name_range: Option<Range<u32>>` — id + two byte-ranges, no booleans | **Replace** ours. The id + ranges model is strictly superior: all section semantics are derived at call time from `id`, not baked into construction. |
| **`WasmSymbol` struct** | `name_offset: u32`, `is_undefined`, `is_weak`, `is_local`, `is_hidden`, `value: u64`, `size: u64`, `section_index`, `is_func` — flat boolean fields, no kind | `kind: WasmSymbolKind`, `flags: u32` (raw bitset from `wasmparser::SymbolFlags`), `index: u32`, `offset: u32`, `size: u32`, `name_start: u32`, `name_len: u32` — explicit kind enum + raw flags | **Replace** ours. Theirs is `Copy + Default`, self-contained, and exposes the full 7-kind enum; ours loses kind fidelity (only `is_func`) and stores booleans redundantly with no raw flag bits for `ABSOLUTE`/`TLS`/`EXPORTED`. |
| **`WasmSymbolKind` enum** | Not present | `Null`, `Func`, `Data`, `Global`, `Section`, `Event`, `Table` (line 169–178) | **Add** this enum. Ours has no kind; the writer needs it. |
| **`WasmSegmentInfo<'data>`** | Not present | `name: &'data str`, `alignment: Alignment`, `flags: SegmentFlags` (line 217–222) | **Add** as a new struct; ours never parsed linking-section segment metadata at all. |
| **`WasmRelocSection` + `WasmRelocation`** | `RelocationList` carries a `Vec<WasmRelocation>` inline per call; `WasmRelocation` has `ty`, `offset`, `index`, `addend` | Identical shape: `WasmRelocSection { target_section_index: u32, entries: Vec<WasmRelocation> }`, `WasmRelocation { ty, offset, index, addend }` (lines 225–240) | **Converged** — our `WasmRelocation` shape matches theirs. The container differs: ours is `RelocationList(Vec<WasmRelocation>)` constructed at call-time; theirs stores them in `File::reloc_sections`. Adopt their storage location. |
| **Reloc encoding helpers** | `write_uleb128_5`, `write_sleb128_5`, `apply_relocation`, `slot_size`, `refers_to_symbol` — all present, matching theirs exactly | Same (lines 279–341) | **Identical** — no change needed. |
| **`platform::Symbol` trait impl** | Implemented on `WasmSymbol` at line 26, using boolean fields. Missing `is_absolute` (always `false`), `is_default_strippable`, `is_tls`, `with_hidden` | Implemented at line 754; uses `raw_flags()` + kind enum. Has `is_absolute` via `SymbolFlags::ABSOLUTE`, `is_tls` via `SymbolFlags::TLS`, `with_hidden` mutating `flags` (line 833). | After struct replacement, the trait impl follows automatically. Add missing `is_absolute` / `is_tls` / `with_hidden` impls. Drop manual bool fields. |
| **`platform::SectionHeader` trait** | Booleans `is_code`/`is_data` drive all methods; `is_alloc` = `code \| data`; `is_prog_bits` = `code \| data`; `is_group` = `is_comdat` | All methods return fixed values (alloc=true, executable/writable=false, is_group=false). Theirs says "all Wasm sections are conceptually loaded" (line 671–721). | **Upstream's model is simpler and correct** — section attributes (code vs data) belong to the symbol kind, not the section header. Replace our bool-field impl with theirs. |
| **`SectionAttributes` struct** | Has `is_code: bool`, `is_data: bool`; non-empty `is_null()` check | Empty struct `SectionAttributes {}`; `is_null` always false; all flag methods default/false (line 845–897) | **Replace** with theirs; booleans are redundant given section ids. |
| **`SegmentType` enum** | `Header`, `Module`, `Unused` variants with `ProgramSegmentDef { segment_type }` | Same three variants (line 911–921) with same `ProgramSegmentDef` | **Identical** — no change. |
| **`SECTION_DEFINITIONS` const array** | Present, populated per `osid::WASM_*` constants (lines 1005–1071) | Same array content, same constants (lines 1005–1071) | **Identical** — no change. |
| **`File::parse_bytes` / parsing strategy** | Uses `object::read::wasm::WasmFile` (the `object` crate) as the parser; has complex `is_shared_library` detection; has COMDAT hand-parsing (`parse_comdat_symbol_names`, ~250 lines) | Uses `wasmparser` directly: `Parser::new(0).parse_all(input)`; parses linking section via `wasmparser::KnownCustom::Linking`; no `object` crate dependency for parse path | **Significant divergence.** Theirs removes the `object` crate dependency for WASM parsing entirely. Ours retains it but adds extensive workarounds for object-crate bugs (fabricated File-kind symbols, empty-name globals). Migration path: adopt wasmparser-direct parsing, port COMDAT logic to `parse_linking_subsections` via `Linking::ComdatInfo` (upstream currently ignores `ComdatInfo` at line 1719 — a gap we must fill). |
| **`platform::ObjectFile::section_name` signature** | `fn section_name(&self, section_header: &'data SectionHeader) -> Result<&'data [u8]>` — takes a `&SectionHeader` | `fn section_name(&self, index: object::SectionIndex) -> Result<&'data [u8]>` — takes an index | **Trait signature mismatch** (platform trait differs between forks at this call site). Upstream changed the trait to take an index; ours still takes a header ref. Must track which platform.rs version we target and align. |
| **`unsafe` usage** | 6 unsafe blocks (lines 593, 602, 616, 628, 638, 649) — all extend lifetimes of `&WasmSymbol` / `&SectionHeader` to `'data` via raw pointer casts to work around the `'static + Copy` bound on `SymtabEntry` | 0 unsafe blocks — solved instead by making `WasmSymbol: Copy + Default` and storing name ranges as `(u32, u32)` within the struct | **Eliminate unsafe.** Adopt `WasmSymbol: Copy` + name-as-range pattern. The 6 unsafe blocks in our `File` impl exist solely because our `WasmSymbol` borrows `&'data [u8]`; theirs avoids the borrow entirely. This is the root cause of all our unsafe. |
| **`SymtabShndxEntry` assoc type** | `u32` (line 853) | `()` (line 1169) | Align to `()` when merging — Wasm has no extended section index table. |
| **`RelocationInfo` assoc type** | Not present (ours doesn't have this type) | `u32` (line 1160) | **Add** `type RelocationInfo = u32;` to our Platform impl. |
| **`default_symtab_entry()`** | Not present | `fn default_symtab_entry() -> WasmSymbol { WasmSymbol::default() }` (line 1604) | **Add** — requires `WasmSymbol: Default`, which theirs provides via `#[derive(Default)]`. |
| **`finalise_find_required_sections` signature** | Takes `_groups: &[GroupState<Self>]` — no return | Takes `groups: &mut [GroupState<Self>], symbol_db: &SymbolDb` — returns `Result` (line 1238–1241) | Signature drift between forks. Align to upstream's richer signature. |
| **`module` constants** | Not present (`section_id::*`, `reloc_type::*` modules, `WASM_MAGIC`, `WASM_VERSION`, `STANDARD_SECTION_LOOKUP_LEN`) | All present (lines 27–76) | **Add** all constants. The `reloc_type::*` constants are needed by our writer and ours already has equivalent numeric literals inline. |
| **`parse_wasm_module` / `parse_linking_subsections`** | Not present — parsing done in `parse_bytes` via object crate | `parse_wasm_module` (line 1614) + `parse_linking_subsections` (line 1689) + `wasm_symbol_from_info` (line 1726) | **Adopt** theirs. Port our COMDAT-parsing logic into `parse_linking_subsections` alongside their `Linking::ComdatInfo` arm. |

### Ordered Reshape Steps

1. **Step B-1 (types):** Add `section_id::*` consts, `reloc_type::*` consts, `WASM_MAGIC`, `WASM_VERSION`, `STANDARD_SECTION_LOOKUP_LEN`, `LINKING_SECTION_NAME`, `RELOC_SECTION_PREFIX`, `TARGET_FEATURES_SECTION_NAME` from upstream lines 27–77. These are purely additive.

2. **Step B-2 (WasmSymbol):** Replace our `WasmSymbol` struct + bool fields with upstream's `WasmSymbol { kind: WasmSymbolKind, flags: u32, index: u32, offset: u32, size: u32, name_start: u32, name_len: u32 }` + `WasmSymbolKind` enum (lines 157–213). Add `wasm_symbol_from_info` converter. Update `platform::Symbol` impl accordingly — the existing method body logic maps 1:1.

3. **Step B-3 (SectionHeader):** Replace our `SectionHeader { index, size, is_code, is_data, is_comdat }` with upstream's `SectionHeader { id: u8, payload_range: Range<u32>, name_range: Option<Range<u32>> }` + inherent methods `is_custom()`, `payload_range_usize()`, `standard_section_name()` (lines 113–152). Update `platform::SectionHeader` impl to remove the bool-driven logic.

4. **Step B-4 (File struct):** Replace our `File` fields — drop `symbol_names`, `section_data`, `section_names`; add `version`, `standard_section_index`, `segments`, `reloc_sections`, `linking_version`, `target_features_raw` as in upstream lines 82–110.

5. **Step B-5 (parsing):** Replace the `object`-crate-based `parse_bytes` with `parse_wasm_module` using `wasmparser` directly (upstream lines 1614–1687). Port our COMDAT extraction into a new `Linking::ComdatInfo` arm inside `parse_linking_subsections` (upstream ignores this arm at line 1719 — fill it in). Port `is_shared_library` detection; can keep our existing `quick_leb` logic or use wasmparser's section payloads.

6. **Step B-6 (unsafe elimination):** After steps B-2/B-3, the 6 `unsafe` blocks (lines 593–649) become unnecessary — `WasmSymbol: Copy` and `SectionHeader` no longer hold borrowed strings. Delete all 6 unsafe blocks; replace with direct index lookups.

7. **Step B-7 (ObjectFile trait):** Align `section_name` signature to upstream (`takes index` vs `takes &header`). Update `symbol_section` to use the `standard_section_index` lookup array. Update `symbol_offset_in_section` to use `WasmSymbolKind::Data` arm. Remove `section_by_name` stub (we now have the real impl from upstream lines 440–461).

8. **Step B-8 (Platform assoc types):** Change `type SymtabShndxEntry = u32` → `()`. Add `type RelocationInfo = u32`. Add `default_symtab_entry()`. Align `finalise_find_required_sections` signature.

9. **Step B-9 (WasmSegmentInfo + WasmRelocSection):** These are new types to add; `WasmRelocSection` stores relocated sections in `File` instead of constructing them ad hoc. Our `RelocationList` becomes a thin wrapper over a `&[WasmRelocation]` slice borrowed from `File`.

10. **Step B-10 (SectionAttributes / BuiltInSectionDetails):** Replace our `SectionAttributes { is_code, is_data }` with upstream's empty `SectionAttributes {}`. Upstream's `BuiltInSectionDetails { kind, target_segment_type }` is identical to ours — no change.

---

## args/wasm.rs

### 3-Column Comparison

| Item | OURS | THEIRS | Classification |
| ----------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------ | -------------------------------------------- |
| **Parser infrastructure** | Hand-rolled `match arg { ... }` loop (~500 lines); fully self-contained | `ArgumentParser<WasmArgs>` with `declare_with_param` / `declare_with_optional_param` builder API (lines 138–163) | **Structural clash** — different parsing architecture. Our impl is longer but complete; theirs uses a shared declarative parser infrastructure. |
| **`WasmArgs` struct — common fields** | `common: CommonArgs`, `output: Arc<Path>` | Same | Aligned |
| **`relocation_model: RelocationModel`** | Not in struct (hardcoded `NonRelocatable` in trait impl) | Present in struct (line 31), stored as `RelocationModel::NonRelocatable` default | Minor clash — theirs makes it a stored field; ours inlines it. Additive. |
| **`WASM_PAGE_ALIGNMENT`, `WASM_PAGE_SIZE`** | Not present (ours returns `Alignment { exponent: 0 }` from trait) | Present as module consts (lines 21–24); `loadable_segment_alignment` returns `WASM_PAGE_ALIGNMENT` (exponent 16) | **Value clash** — ours returns 1 byte alignment, theirs returns 64 KiB. Needs investigation: which is correct for wild's wasm output layout? |
| **`should_strip_debug`** | Implemented: checks `Strip::All \| Strip::Debug` | `todo!()` (line 65) | Ours is complete; theirs is a stub. **Ours wins.** |
| **`should_export_all_dynamic_symbols`** | Implemented: checks `no_export_dynamic` / `export_dynamic` / `export_all` / `is_shared` | `todo!()` (line 94) | Ours is complete. **Ours wins.** |
| **`should_export_dynamic`** | Returns `false` (correct stub) | `todo!()` (line 99) | Ours wins. |
| **`entry_symbol_name`** | Full impl: respects `no_entry`, `entry_symbol`, falls back `_start` | Hardcoded `b"_start"` (line 75) | Ours is complete. **Ours wins.** |
| **`lib_search_path`** | Returns `&self.lib_search_paths` | `todo!()` (line 79) | Ours is complete. **Ours wins.** |
| **`should_merge_sections`** | Returns `false` | Returns `true` with `// TODO` | **Value clash** — different defaults, both noted as TODO. Ours (`false`) is more correct for wasm (no section merging). |
| **`should_gc_sections`** | Implemented via `!self.no_gc_sections` | Not present in their `Args` impl | Ours adds this; theirs doesn't expose it yet. |
| **`wasm_opt_level`** | Implemented | Not present in theirs | Additive. |
| **`should_allow_object_undefined`** | Implemented | Not present in theirs | Additive. |
| **`allow_multiple_definitions` / `warn_multiple_definitions`** | Implemented | Not present in theirs | Additive. |
| **`force_undefined_symbol_names`** | Implemented | Not present in theirs | Additive. |
| **`force_export_symbol_names`** | Implemented | Not present in theirs | Additive. |
| **`should_output_partial_object`** | Implemented (`is_relocatable`) | Not present | Additive. |
| **`wasm_bitcode_lowering_enabled`** | Implemented (checks `WILD_DISABLE_WASM_LTO` env, probes `llc`) | Not present | Additive. |
| **`has_explicit_entry`** | Implemented | Not present | Additive. |

### Additive flags (ours only, no name conflict with theirs)

All 40+ fields on our `WasmArgs` beyond `common`, `output`, `relocation_model` are purely additive. Upstream's `WasmArgs` only has those three fields. There is no naming conflict between our extra fields and anything upstream has defined.

Additive fields (representative subset): `entry_symbol`, `lib_search_paths`, `no_entry`, `allow_undefined`, `export_dynamic`, `no_export_dynamic`, `no_gc_sections`, `exports`, `exports_if_defined`, `export_all`, `strip`, `keep_sections`, `is_relocatable`, `is_shared`, `initial_memory`, `max_memory`, `page_size`, `stack_size`, `stack_first`, `global_base`, `initial_heap`, `allow_multiple_definitions`, `warn_multiple_definitions`, `rpath`, `force_undefined`, `stub_unresolved_functions`, `shared_memory`, `import_memory`, `import_memory_name`, `export_memory_name`, `import_table`, `export_table`, `no_growable_memory`, `growable_table`, `compress_relocations`, `emit_relocs`, `memory64`, `is_pic`, `opt_level`, `lto`, `lld_compat`, `extra_features`, `trace_symbols`, `print_gc_sections`, `why_extract`, `wrap`, `import_undefined`, `trace_files`, `map_file`.

### Clashing items (need reconciliation)

| Flag/field | Ours | Theirs | Resolution |
| ---------------------------------------- | ------------------------------------------- | ----------------------------------------- | ----------------------------------------- |
| `WASM_PAGE_ALIGNMENT` const | Absent; trait returns exponent=0 (1 byte) | `exponent: 16` (64 KiB page) | Adopt theirs — wasm linear memory pages are 64 KiB. |
| Parsing infrastructure | Hand-rolled `match` loop | `ArgumentParser` builder | Keep ours for now (complete); plan to migrate to `ArgumentParser` in a follow-up once upstream extends its wasm parser. |
| `should_merge_sections` | `false` | `true` (TODO) | Keep `false` — Wasm sections are not merged in the ELF sense. |

---

## wasm_writer.rs

Upstream's `wasm_writer.rs` is a 21-line stub (lines 1–21). It writes only the 8-byte preamble (`WASM_MAGIC` + `WASM_VERSION` as little-endian u32) then immediately `bail!("Wasm section emission is not implemented yet")`. Our ~16 k-line writer is the complete implementation and **wins wholesale** — there is no upstream writer logic to merge.

Shared types that upstream's stub imports (and our writer must therefore continue to provide at the same paths):

- `crate::wasm::WASM_MAGIC` — already present in upstream's `wasm.rs` line 27.
- `crate::wasm::WASM_VERSION` — already present at line 30.
- `crate::wasm::Wasm` — the platform type.
- `crate::file_writer::SizedOutput` — not wasm-specific.
- `crate::layout::Layout<'data, Wasm>` — not wasm-specific.
- `crate::platform::Arch` — trait bound.

All of the above are shared infrastructure already present in both trees. No new exports are needed from `wasm.rs` to satisfy the upstream stub's import list.

---

## Risk Notes

| Risk | Detail | Mitigation |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| **`section_name` trait signature** | Ours (platform.rs line 899): `fn section_name(&self, section_header: &'data SectionHeader)`. Theirs (platform.rs line 829): `fn section_name(&self, index: object::SectionIndex)`. This is a platform-trait-level divergence, not just a wasm.rs divergence. | Track which platform.rs we are converging to; align wasm.rs after platform.rs is settled. |
| **`unsafe` blocks** | 6 uses at ours wasm.rs lines 593, 602, 616, 628, 638, 649 — all lifetime-extending pointer casts. Zero in upstream. | Eliminated in Step B-6; root cause is `symbol_names` parallel vec. |
| **COMDAT parsing** | Upstream's `parse_linking_subsections` ignores `Linking::ComdatInfo` (line 1719: `_ => {}`). Our `parse_comdat_symbol_names` / `parse_linking_for_comdat` (~220 lines) implements it hand-rolled. | Port our COMDAT logic into the `ComdatInfo` arm when adopting upstream's parser. Do not silently drop it. |
| **`object` crate dependency** | Our `parse_bytes` depends on `object::read::wasm::WasmFile`; upstream has removed this dependency entirely. | After adopting `parse_wasm_module`, audit `Cargo.toml` to see if the `object` crate can be made optional or feature-gated for the wasm path. |
| **`WASM_PAGE_ALIGNMENT` value** | Ours: exponent=0 (1 byte). Upstream: exponent=16 (64 KiB). This affects layout computations anywhere `loadable_segment_alignment` is called. | Audit callers; adopt upstream's 64 KiB value — it matches the wasm spec. |
| **`finalise_find_required_sections` signature** | Ours: `fn finalise_find_required_sections(_groups: &[GroupState<Self>])` no return. Theirs: takes `groups: &mut [GroupState<Self>], symbol_db: &SymbolDb` returns `Result`. | Align in Step B-8; currently both bodies are no-ops so no logic is lost. |
| **`SymtabShndxEntry = u32` vs `()`** | Our type is `u32`; upstream is `()`. Wasm has no SYMTAB_SHNDX section, so `()` is correct. Callers that read `SymtabShndxEntry` will need updating. | Change in Step B-8 and grep for any `SymtabShndxEntry`-typed call sites. |
| **`linking_version` / `target_features_raw`** | Not stored in our `File`; upstream stores both. Our writer may consume `target_features_raw` via ad hoc re-parsing today. | When adopting upstream's `File`, thread `target_features_raw` from parser through to writer to avoid re-parsing. |
| **`WasmArgs` parser infrastructure** | Our arg parser is a 500-line `match` loop vs upstream's `ArgumentParser` builder. Functionality is complete in ours; upstream's is a 2-field stub. | Keep ours unchanged during convergence; plan `ArgumentParser` migration as a separate step once upstream has a fuller wasm arg surface. |
