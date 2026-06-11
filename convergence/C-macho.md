# Cluster C: macho reader + args + feature gating

Reference commit: upstream `d0914192`.  Line numbers are from the live file reads.
All paths below are relative to `libwild/src/` unless stated otherwise.

---

## macho.rs

### File sizes

| Tree   | Lines |
| ------ | ----- |
| OURS   | 4 737 |
| THEIRS |   970 |

OURS is the superset. THEIRS is still a partially-populated skeleton;
OURS has the full working implementation.

---

### 3-column comparison (key types)

| Symbol | OURS (line) | THEIRS (line) | Status |
| ------ | ----------- | ------------- | ------ |
| `struct MachO` | derives `Copy, Clone, Default` (l 18) | same derives (l 59) | agree-but-differ: OURS has `Default` in derive list explicitly; THEIRS omits it but has `#[derive(Debug, Copy, Clone, Default)]` — identical |
| `const LE` | l 20 | l 61 | identical |
| `const MACHO_START_MEM_ADDRESS: u64 = 0x1_0000_0000` | absent in OURS root (defined inline in `start_memory_address`) | l 65 | **theirs-extra** — OURS bakes the same value inline; trivial to adopt |
| `const MACHO_COMMAND_ALIGNMENT: usize = 8` | absent | l 68 | **theirs-extra** |
| `const DYLINKER_PATH` | absent (writer embeds it) | l 72 | **theirs-extra** |
| `const DEFAULT_SEGMENT_COUNT: usize = 4` | absent | l 73 | **theirs-extra** |
| `const CHAINED_FIXUP_TABLE_SIZE: u64` | absent | l 74–75 | **theirs-extra** |
| `type SectionHeader` | OURS: `#[repr(transparent)] struct SectionHeader(pub macho::Section64<Endianness>)` (l 49) | THEIRS: `type SectionHeader = Section64<…>` (l 77, a type alias) | **fundamentally differ** — see §Reloc Model below |
| `type SectionTable<'data>` | identical concept; OURS uses it | identical | agree |
| `type SymbolTable<'data>` | identical | identical | agree |
| `type SymtabEntry` | `pub(crate)` (l 44) | module-private `type` alias (l 80) | agree-but-differ: visibility |
| `type Relocation` | absent (OURS uses `&[macho::Relocation<Endianness>]` directly) | `type Relocation = object::macho::Relocation<Endianness>` (l 81) | **theirs-extra** |
| `pub(crate) type FileHeader` | absent | l 83 | **theirs-extra** |
| `pub(crate) type SegmentCommand` | absent | l 84 | **theirs-extra** |
| `pub(crate) type SectionEntry` | absent | l 85 | **theirs-extra** |
| `pub(crate) type EntryPointCommand` | absent | l 86 | **theirs-extra** |
| `pub(crate) type DylinkerCommand` | absent | l 87 | **theirs-extra** |
| `pub(crate) type CodeSignatureCommand` | absent | l 88 | **theirs-extra** |
| `pub(crate) type DyldChainedFixupsCommand` | absent | l 89 | **theirs-extra** |
| `pub(crate) type ChainedFixupsHeader = DyldChainedFixupsHeader` | absent | l 90 | **theirs-extra** |
| `pub(crate) type SymtabCommand` | absent | l 91 | **theirs-extra** |
| `enum DyldChainedFixupsImporstFormat` | absent | l 96–102 | **theirs-extra** |
| `struct DyldChainedFixupsHeader` (zerocopy) | absent | l 106–122 | **theirs-extra** |
| `struct CodeSignatureSuperBlob` | absent (OURS has it in macho_writer.rs) | l 138–150 | **theirs-extra in macho.rs**, **ours-extra in macho_writer.rs** — same concept, different home |
| `struct CodeSignatureBlobIndex` | same story | l 152–162 | ditto |
| `struct CodeSignatureCodeDirectory` | same story | l 164–223 | ditto |
| `CS_*` constants | OURS in macho.rs (l ~20–50) | THEIRS in macho.rs (l 225–247) | agree-but-differ: OURS also exports `OBJC_STUB_SLOT_BYTES`, `OBJC_STUB_CODE_BYTES`, `OBJC_STUB_MAX_SELECTOR` which THEIRS lacks |
| `fn code_signature_identifier` | absent | l 249–254 | **theirs-extra** (OURS computes in writer) |
| `fn code_signature_padded_identifier_size` | absent | l 256–258 | **theirs-extra** |
| `struct File<'data>` | l 177 (with `flags: u32`) | l 261 | identical shape |
| `impl ObjectFile for File` | OURS: fully implemented, l 1938–2268 | THEIRS: mostly `todo!()`, l 271–566 | **ours-extra** — OURS has the real impl |
| `struct ObjectLayoutStateExt` | OURS: rich struct with `lsda_map: OnceLock<LsdaMap>`, `compact_unwind_scanned: AtomicBool` (l 197–207) | THEIRS: `type ObjectLayoutStateExt<'data> = ()` (l 1029 of THEIRS' Platform impl) | **ours-extra** |
| `type LsdaMap` | OURS: l 214 | absent | **ours-extra** |
| `struct SectionHeader` | OURS: `#[repr(transparent)] struct SectionHeader(pub(crate) macho::Section64<…>)` (l 49) | THEIRS: plain `type` alias (l 77) | **fundamentally differ** |
| `struct SectionType` | OURS: `SectionType(u32)` with real impl (l 2417) | THEIRS: `struct SectionType {}` stub (l 621) | agree-but-differ |
| `struct SectionFlags` | OURS: `SectionFlags(u32)` with `from_header()`; `SectionFlags::from_u32()` | THEIRS: `SectionFlags(u32)` with `empty()`, `from_u32()`, `raw()` (l 642–660) | agree-but-differ: OURS adds `from_header` helper |
| `struct SectionAttributes` | OURS: `{ flags: u32, segname: [u8;16] }` (l 2593) | THEIRS: `{ flags: SectionFlags }` (l 758) | **fundamentally differ**: OURS carries `segname` for segment-aware writability queries |
| `enum SegmentType` | OURS: `{ Text, Data, LinkedEdit, DataConst, Custom, … }` (l ~2890) | THEIRS: `{ Text, LoadCommands, TextSections, DataSections, DataConstSections, LinkeditSections, Unused }` (l 822–833) | **fundamentally differ**: OURS is dynamic/segment-driven; THEIRS uses a flat enum with fixed-slot layout |
| `struct ProgramSegmentDef` | OURS: `{ writable: bool, executable: bool }` | THEIRS: `{ segment_type: SegmentType }` (l 836) | **fundamentally differ** |
| `struct BuiltInSectionDetails` | OURS: empty struct (l 3027) | THEIRS: `{ kind, section_flags, min_alignment, target_segment_type }` (l 916–921) | **fundamentally differ**: OURS builds section infos dynamically via `built_in_section_infos()`; THEIRS uses a static array `SECTION_DEFINITIONS` |
| `const SECTION_DEFINITIONS` | absent | l 1558–1659 | **theirs-extra** |
| `const DEFAULT_SECTION_RULES` | OURS: `MACHO_SECTION_RULES` (l 4417), 37 entries with full routing | THEIRS: 3 entries, stub (l 1662–1668) | **ours-extra** |
| `const PROGRAM_SEGMENT_DEFS` | OURS: `MACHO_SEGMENT_DEFS` built dynamically | THEIRS: static slice (l 1670–1688) | agree-but-differ |
| `fn has_active_segment` | absent | l 1691–1697 | **theirs-extra** |
| `fn count_sections_for_segment_type` | absent | l 1699–1710 | **theirs-extra** |
| `struct SegmentSectionsInfo<'data>` | absent (OURS has equivalent in macho_writer.rs) | l 1712–1716 | **theirs-extra in macho.rs** |
| `fn get_segment_sections` | absent | l 1718–1764 | **theirs-extra in macho.rs** |
| `fn process_relocation<A>` | l 1767–1798 (THEIRS only processes extern relocs, does not apply them) | absent | **theirs-extra** |
| `struct MachOResolutionExt` | OURS: `{ got_address, plt_address, selref_address }` (l 3033) | absent (`type ResolutionExt = ()`) | **ours-extra** |
| `struct NonAddressableIndexes` | OURS: struct with real `new` impl | THEIRS: l 811 empty struct | agree-but-differ |
| `fn estimate_unwind_info_entries` | OURS: l 63–145 | absent | **ours-extra** |
| `fn unwind_info_reserved_bytes` | OURS: l 162–174 | absent | **ours-extra** |
| `fn compact_atom_managed_sections` | OURS: l 268–468 | absent | **ours-extra** |
| `fn build_lsda_map` | OURS: l 832–1065 | absent | **ours-extra** |
| `fn decode_non_extern_section_offset` | OURS: l 1076–1105 | absent | **ours-extra** |
| `fn is_private_local_label` | OURS: l 1113–1115 | absent | **ours-extra** |
| `fn decode_pending_page21` | OURS: l 1128–1161 | absent | **ours-extra** |
| `fn decode_pageoff12_reloc` | OURS: l 1170–1190 | absent | **ours-extra** |
| `fn decode_non_extern_target` | OURS: l 1313–1363 | absent | **ours-extra** |
| `fn scan_reloc_range_for_atom` | OURS: l 1382–1393 | absent | **ours-extra** |
| `fn scan_extern_relocs_only` | OURS: l 1409–1425 | absent | **ours-extra** |
| `fn scan_reloc_range_for_atom_impl` | OURS: l 1468–very long | absent | **ours-extra** |
| `fn compute_atoms` | OURS: l 600–808 | absent | **ours-extra** |
| `fn build_atoms_for_section` (internal) | OURS: internal | absent | **ours-extra** |
| `fn compute_atom_output_offsets` | OURS: l ~1760 | absent | **ours-extra** |
| `fn compute_subsection_padding_deltas` | OURS: l 1852–1936 | absent | **ours-extra** |
| `fn trim_nul` | OURS: l 4493 | absent explicitly (used inline) | **ours-extra** (exported) |
| `impl platform::Platform for MachO` | OURS: l 3123–4414 | THEIRS: l 996–1800 | **ours is superset**: OURS has ~60 impl methods, THEIRS has same set but most `todo!()` |

---

### Relocation model (#1869)

**THEIRS** models a relocation as:

- `type Relocation = object::macho::Relocation<Endianness>` (l 81)
- `struct RelocationList<'data> { relocations: &'data [Relocation] }` (l 938)
- `fn process_relocation<A>` (l 1767): delegates to `A::relocation_from_raw(rel_info)` taking a `RelocationInfo` (the decoded form, not the raw `Relocation`)

**OURS** models it the same way structurally (`RelocationList<'data>` containing `&'data [macho::Relocation<Endianness>]`, l 3052–3060) but uses:

- `object::macho::RelocationInfo` for decoded values throughout (via `.info(LE)`)
- The full `scan_reloc_range_for_atom_impl` machinery with `r_extern` / `r_type` / `r_length` / `r_symbolnum` fields
- Additional per-type decode helpers (`decode_pending_page21`, `decode_pageoff12_reloc`, `decode_non_extern_target`)

**The key structural delta** is the `SectionHeader` newtype vs type-alias.  THEIRS uses:

```rust
type SectionHeader = Section64<crate::macho::Endianness>;  // l 77
```

OURS uses:

```rust
#[repr(transparent)]
pub(crate) struct SectionHeader(pub(crate) macho::Section64<Endianness>);  // l 49
```

This newtype is pervasive in OURS — it lets trait impls be attached and unsafe pointer casts (`as *const SectionHeader`) flow from the section table.  Merging to upstream's type-alias form would require removing the newtype wrapper and adjusting every `.0` field access and pointer cast site (roughly 30–40 callsites in macho.rs alone).

---

### Symbol table model (#1888)

**THEIRS** (`impl platform::Symbol for SymtabEntry`, l 669–754): minimal stubs — `size()` returns 0 (TODO), `is_tls()` returns false (TODO), most predicates are thin.  `is_default_strippable` strips `ltmp*` prefixes.

**OURS** (l 2453–2588): full impl with:

- `as_common()` — decodes `N_UNDF|N_EXT` with `n_value > 0` as a common symbol with `GET_COMM_ALIGN` alignment from `n_desc[8..11]` (l 2453–2476)
- `is_undefined()` — excludes common symbols from the "undefined" predicate (l 2478–2485)
- `is_local()` — properly excludes stabs (l 2487–2491)
- `visibility()` — handles `N_PEXT`, `.weak_def_can_be_hidden` (N_WEAK_DEF|N_WEAK_REF) (l 2501–2524)
- `section_index()`, `has_name()`, `debug_string()` — all real impls
- `is_symbol_in_common_section()` — checks `__common` section name (l 2210–2224)

**Status**: agree on structure; OURS is the complete version.

---

### Segment writer shape (#1821)

**THEIRS** (`impl platform::ProgramSegmentDef`, l 847–913):

- Uses a fixed `SegmentType` enum with seven variants
- `always_keep()` keeps Text, LoadCommands, TextSections, LinkeditSections
- `should_include_section()` dispatches on `section_id` via a big `match` to pick the right `SegmentType`
- Hard-coded section ordering in `build_output_order_and_program_segments` (l 1474–1509): 17 sections in a fixed list

**OURS** (`impl platform::ProgramSegmentDef`, l 2940–3025):

- `ProgramSegmentDef { writable: bool, executable: bool }` — property-driven rather than named-variant-driven
- `should_include_section()` queries `section_info.section_attributes` for writability/executability
- `build_output_order_and_program_segments` (l 4336–4414): adds custom exec/ro/data/bss sections, handles `__DATA_CONST`/`__DATA` split via `RELRO_PADDING`, adds ObjC, TLS, eh_frame sections — far more complete

**Status**: **fundamentally differ**.  OURS has grown well beyond THEIRS in section routing and cannot be trivially replaced.  To upstream we would need to contribute the property-driven `ProgramSegmentDef` approach (or adapt OURS to THEIRS' enum-based approach while keeping OURS' richer section routing).

---

### Code signature (#1919)

**THEIRS**: `CodeSignatureSuperBlob`, `CodeSignatureBlobIndex`, `CodeSignatureCodeDirectory`, and all `CS_*` constants live in `macho.rs` (l 132–247).  `fn code_signature_identifier`, `fn code_signature_padded_identifier_size` also in `macho.rs` (l 249–258).

**OURS**: same structs/constants live in `macho.rs` (scattered; `CS_*` near l 20–50); the writing logic is in `macho_writer.rs`.  The key difference: OURS also has `OBJC_STUB_SLOT_BYTES` / `OBJC_STUB_CODE_BYTES` / `OBJC_STUB_MAX_SELECTOR` constants (l 37–40) used in the ObjC stub allocator.

**Status**: agree-but-differ on location; trivially reconcilable by adopting THEIRS' `macho.rs` home for the CS structs (already done in OURS, just a slightly different layout).

---

## macho_aarch64.rs

### File sizes

| Tree   | Lines |
| ------ | ----- |
| OURS   |   250 |
| THEIRS |   162 |

---

### 3-column comparison

| Symbol | OURS (line) | THEIRS (line) | Status |
| ------ | ----------- | ------------- | ------ |
| `struct MachOAArch64` | identical | identical | agree |
| `struct Relaxation` | OURS: full impl with no-op bodies (l 121–151) | THEIRS: all `todo!()` (l 17–40) | **ours-extra** |
| `fn relocation_from_raw(r_type: u32)` | OURS takes `r_type: u32` (l 200); maps 11 ARM64 types to `RelocationKindInfo` | THEIRS takes `rel: object::macho::RelocationInfo` (l 63); uses `rel.r_pcrel` / `rel.r_length` | **fundamentally differ** on signature |
| Coverage of `relocation_from_raw` | OURS: 11 types including `GOT_LOAD_PAGE21`, `GOT_LOAD_PAGEOFF12`, `POINTER_TO_GOT`, `TLVP_*`, `ADDEND` | THEIRS: 4 types (`UNSIGNED`, `BRANCH26`, `PAGE21`, `PAGEOFF12`); rest `bail!` | **ours-extra** |
| PAGE21 handling | OURS: `RelocationKind::Relative`, uses `RelocationSize::bit_mask_aarch64(12, 33, AArch64Instruction::Adr)` (l 33–37) | THEIRS: same params but adds `mask: Some(PageMask::SymbolPlusAddendAndPosition(PAGE_MASK_4KB))` (l 91–97) | **differs**: THEIRS has a `PageMask`; OURS has `mask: None` |
| PAGEOFF12 handling | OURS: `RelocationKind::AbsoluteLowPart`, `AArch64Instruction::Add` (l 39–44) | THEIRS: `RelocationKind::AbsoluteLowPart`, `AArch64Instruction::MachOLow12` (l 99–106) | **differs**: OURS uses generic `Add`; THEIRS uses a Mach-O-specific instruction variant |
| `fn rel_type_to_string` | OURS: full mapping (l 104–119) | THEIRS: `todo!()` | **ours-extra** |
| `fn write_plt_entry` | OURS: full ADRP/LDR/BR stub (l 165–198) | THEIRS: `todo!()` | **ours-extra** |
| `fn tp_offset_start` | OURS: returns 0 | THEIRS: `todo!()` | **ours-extra** |
| `fn get_property_class` | OURS: returns `None` | THEIRS: `todo!()` | **ours-extra** |
| `fn merge_eflags` | OURS: returns `Ok(0)` | THEIRS: `todo!()` | **ours-extra** |
| `fn high_part_relocations` | OURS: returns `&[]` | THEIRS: `todo!()` | **ours-extra** |
| `fn get_source_info` | OURS: returns `Ok(SourceInfo(None))` | THEIRS: `todo!()` | **ours-extra** |
| `fn new_relaxation` | OURS: returns `None` | THEIRS: `todo!()` | **ours-extra** |

---

### Ordered convergence steps for macho_aarch64.rs

1. **Adopt `PageMask` in PAGE21** — THEIRS adds `mask: Some(PageMask::SymbolPlusAddendAndPosition(PAGE_MASK_4KB))` which correctly gates the low 12 bits of the relocation to the page.  OURS omits this; if upstream's generic reloc machinery changed to require it, OURS would misbehave.  **Risk: medium** — affects GOT/data reachability and PC-relative addressing correctness.
2. **Adopt `AArch64Instruction::MachOLow12` for PAGEOFF12** — THEIRS uses a dedicated variant that presumably handles the load-size-specific scaling (byte/word/dword) that the PAGEOFF12 encoding requires.  OURS uses `AArch64Instruction::Add` which may or may not work identically.  **Risk: low-medium**.
3. **Adopt upstream's `relocation_from_raw(r_type: u32)` signature** — THEIRS takes a `u32` directly; OURS takes `object::macho::RelocationInfo` (the decoded struct).  The platform trait may have diverged on which form the method accepts.  Reconcile before upstreaming.
4. Fill remaining `todo!()` stubs in THEIRS from OURS' working implementations.

---

## args/macho.rs

### File sizes

| Tree   | Lines |
| ------ | ----- |
| OURS   | 2 776 |
| THEIRS |   170 |

---

### 3-column comparison

| Symbol | OURS (line) | THEIRS (line) | Status |
| ------ | ----------- | ------------- | ------ |
| `struct MachOArgs` | 50+ fields (l 63–241) | 3 fields: `common`, `output`, `relocation_model` (l 15–20) | OURS is the superset; THEIRS is a minimal stub |
| `pub(crate) type DylibSymbols` | OURS (l 30) | absent | **ours-extra** |
| `enum DylibLoadKind` | OURS (l 33–38) | absent | **ours-extra** |
| `enum UndefinedTreatment` | OURS (l 41–47) | absent | **ours-extra** |
| `enum MultiplyDefinedTreatment` | OURS (l 56–61) | absent | **ours-extra** |
| `impl Default for MachOArgs` | OURS: 40-field struct literal (l 259–336) | THEIRS: 3-field (l 31–41) | **ours-extra** |
| `fn MachOArgs::new` | both: delegates to `CommonArgs::from_env()` | identical concept | agree |
| `impl platform::Args` — `parse` | OURS: full 2200-line `parse_one_arg` + pre-scan + response-file + per-arg timing | THEIRS: 20-line stub using `ArgumentParser` (l 43–106) | **fundamentally differ** on parser architecture — OURS uses a hand-rolled loop; THEIRS uses a `setup_argument_parser()` declarative builder |
| `should_strip_debug` | OURS: real impl checking `Strip::All\|Debug` | THEIRS: `todo!()` | **ours-extra** |
| `should_strip_all` | OURS: `!self.is_relocatable && …` | THEIRS: returns `false` | agree-but-differ |
| `entry_symbol_name` | OURS: respects `self.entry_symbol` override, explicit entry flag | THEIRS: hard-coded `b"_main"` | **ours-extra** |
| `has_explicit_entry` | OURS: l 360 | absent in THEIRS | **ours-extra** |
| `lib_search_path` | OURS: returns `&self.lib_search_paths` | THEIRS: `todo!()` | **ours-extra** |
| `should_export_all_dynamic_symbols` | OURS: `self.export_dynamic` | THEIRS: `todo!()` | **ours-extra** |
| `should_export_dynamic` | OURS: returns `false` | THEIRS: `todo!()` | agree |
| `should_gc_sections` | OURS: `self.gc_sections` (l 384) | absent in THEIRS | **ours-extra** |
| `export_list_path` / `unexport_list_path` | OURS (l 388–395) | absent | **ours-extra** |
| `should_allow_object_undefined` | OURS (l 396) | absent | **ours-extra** |
| `loadable_segment_alignment` | OURS: 16 KB (`exponent: 14`) | THEIRS: `MACHO_PAGE_ALIGNMENT` (l 89) | agree-but-differ: THEIRS names the constant in alignment module |
| `should_output_partial_object` | OURS: `self.is_relocatable` (l 452) | absent | **ours-extra** |
| `lto_library_path`, `object_path_lto`, `dylib_symbols`, etc. | OURS | absent | **ours-extra** |
| Argument flags parsed | OURS: ~100+ flags (-L, -l, -F, -framework, -dead_strip, -dylib, -rpath, -syslibroot, -order_file, -platform_version, -stack_size, -sectcreate, -headerpad, -no_compact_unwind, …) | THEIRS: `-o`, `--time`, `--validate-output`, `--update-in-place` | OURS covers the full ld64 API surface; THEIRS is barely a placeholder |
| **Additive vs conflicting flags** | No flag in THEIRS conflicts with OURS' flags — THEIRS simply has 4 flags vs OURS' ~100 | — | No conflicts; pure addition |

**Parser architecture note**: THEIRS introduces an `ArgumentParser` builder pattern (via `setup_argument_parser`).  OURS uses a hand-rolled match loop in `parse_one_arg`.  To converge, OURS' flag handlers would need to be migrated into the declarative `ArgumentParser` style, or the parser abstraction extended to handle the complex multi-token cases (response files, `-Wl,` expansion, per-arg timing, deferred TBD/framework processing) that OURS already has.

---

## macho_writer.rs

### File sizes

| Tree   | Lines |
| ------ | ----- |
| OURS   | 10 931 |
| THEIRS |    970 |

OURS is again the complete implementation; THEIRS is a 970-line working
first cut covering the basic executable case (segments, load commands,
code signature, symbol table for the simple case).

### Shared types THEIRS introduces (required for compatibility)

THEIRS' writer directly imports 20+ types from `macho.rs` (l 1–103):

| Type | Source in THEIRS' `macho.rs` | Notes |
| ---- | ---- | ----- |
| `FileHeader` | l 83 | OURS must export this if writer moves to THEIRS' structure |
| `SegmentCommand` | l 84 | |
| `SectionEntry` | l 85 | |
| `EntryPointCommand` | l 86 | |
| `DylinkerCommand` | l 87 | |
| `CodeSignatureCommand` | l 88 | |
| `DyldChainedFixupsCommand` | l 89 | |
| `ChainedFixupsHeader` | l 90 | |
| `SymtabCommand` | l 91 | |
| `DyldChainedFixupsImporstFormat` (note: typo "Imporst" in THEIRS) | l 96 | |
| `DyldChainedFixupsHeader` | l 106 | |
| `CodeSignatureSuperBlob`, `CodeSignatureBlobIndex`, `CodeSignatureCodeDirectory` | l 138–223 | |
| `CS_*` constants | l 225–247 | |
| `fn code_signature_identifier`, `fn code_signature_padded_identifier_size` | l 249–258 | |
| `fn get_segment_sections`, `struct SegmentSectionsInfo` | l 1718–1764 | |
| `SegmentType`, `MachO`, `SectionFlags` | throughout | |

**Verdict**: THEIRS' writer is a clean ~970-line implementation of a simple executable writer (no dylib, no fat binary, no ObjC stubs, no TLS, no `__unwind_info`).  OURS' 10 931-line writer is the full production implementation.  The approach to converge is NOT to replace OURS' writer with THEIRS' — OURS is the donor.  Instead, adopt the `pub type` aliases from THEIRS' `macho.rs` so OURS' writer uses the same re-exported names rather than importing from `object::macho` directly.  This is a rename/re-export exercise, not a rewrite.

### Key structural observation

THEIRS' writer calls into `macho.rs` for `get_segment_sections` / `SegmentSectionsInfo` (l 49–54 of writer).  OURS embeds equivalent logic inside the writer itself.  Moving OURS' equivalent helpers to `macho.rs` (as THEIRS does) would be a low-risk cleanup that makes the code more modular.

---

## Feature gating

### THEIRS (upstream, `libwild/Cargo.toml` l 84–85)

```toml
# Experimental support for linking Mach-O. Currently incomplete.
macho = []
```

The `macho` feature is declared but has **no dependencies** (pure compile-time gate).  It is **not in `[features] default`** — disabled by default.

In source the gate is a **runtime check** at `link_for_arch` entry (upstream `macho.rs` l 1038–1042):

```rust
if !cfg!(feature = "macho") {
    crate::bail!(
        "Mach-O support is still experimental. Rebuild with `--features macho` to enable it."
    );
}
```

No `#[cfg(feature = "macho")]` attribute guards appear anywhere in THEIRS' source — all Mach-O modules compile unconditionally; the feature only affects the runtime bail.

The workspace root `Cargo.toml` has no mention of the `macho` feature (no propagation to the `wild` binary crate via `libwild/features?/macho`).

### OURS (fork, `libwild/Cargo.toml`)

The `macho` feature **does not exist** in OURS' `Cargo.toml`.  The `[features]` block (l 95–136) lists: `default = ["wilt"]`, `fork`, `plugins`, `macho-lto`, `llvm`, `wilt`, `wasm-opt`, `wasm-addr64`, `perfetto`, `wip`.  No `macho` gate.

The only Mach-O feature gate in OURS is `macho-lto = ["libloading"]` (l 105), which guards LTO via `libLTO.dylib` — orthogonal to the Mach-O linker path itself.

In OURS' source, `cfg(feature = "macho")` never appears.  Mach-O code is always compiled and the linker path is always available.

### Recommended approach for convergence

1. **Add `macho = []` to OURS' `libwild/Cargo.toml`** (matching THEIRS exactly) and expose it from the workspace root / `wild` binary crate if needed.
2. **Replace the always-on path in OURS' `Platform::link_for_arch`** with the same runtime bail guard as THEIRS (l 1038–1042 of THEIRS).  This means users of the upstream fork do not accidentally activate OURS' production Mach-O path without opting in — which matters because OURS makes the path unconditionally available today.
3. **Do not introduce `#[cfg(feature = "macho")]` attribute gates** on individual types/fns — THEIRS deliberately avoids them to keep the compilation model simple; compile-time exclusion would require far more scaffolding and is inconsistent with THEIRS' approach.
4. OURS' `macho-lto` feature can be preserved as-is; it is orthogonal.

---

## Risk notes

1. **`SectionHeader` newtype vs type alias** — OURS has 30–40 `unsafe` pointer cast sites (`as *const SectionHeader`) that rely on `#[repr(transparent)]`.  Removing the newtype to match THEIRS' `type` alias would eliminate the `unsafe` blocks but requires rewiring every site where `SectionHeader` traits (`is_alloc`, `is_writable`, etc.) are dispatched.  **Risk: high if attempted as a big-bang change; low if done incrementally.**

2. **`SegmentType` enum mismatch** — THEIRS uses a flat 7-variant enum with a static `SECTION_DEFINITIONS` array.  OURS has a property-driven `ProgramSegmentDef { writable, executable }` that dynamically assigns sections.  These are architecturally incompatible.  To upstream, OURS would need to contribute its richer section-routing approach as a new design (or persuade upstream to adopt the property-based model).

3. **`relocation_from_raw` signature** — THEIRS' `Arch::relocation_from_raw` takes `object::macho::RelocationInfo` (the decoded struct); OURS' (l 200 of OURS' `macho_aarch64.rs`) takes `r_type: u32`.  These are different trait method signatures.  Merging requires the platform trait to agree on which form is canonical.

4. **`PAGE21` `PageMask` gap** — OURS omits `mask: Some(PageMask::SymbolPlusAddendAndPosition(PAGE_MASK_4KB))` for the PAGE21 relocation.  If upstream's relocation applier uses the `mask` field to gate the low bits, OURS may silently produce wrong addresses when a PAGE21 target is at a sub-page offset.  **Risk: medium — could cause data/GOT pointer corruption on certain inputs without a hard failure.**

5. **Parser architecture** — OURS' hand-rolled argument parser is functionally complete but structurally incompatible with THEIRS' `ArgumentParser` builder.  Migrating OURS to the declarative builder would be a 1 000-line refactor with no functional change — worthwhile for upstreaming but risky in terms of regression surface.

6. **`DylibSymbols` type alias** (`hashbrown::HashSet<Arc<[u8]>, foldhash::fast::FixedState>`) in OURS' `args/macho.rs` (l 30) replaces `std::HashSet` — a perf optimization.  THEIRS does not have this.  It is purely additive and should be contributed upstream.

7. **`ObjectLayoutStateExt`** — OURS has a real struct with `lsda_map: OnceLock<LsdaMap>` and `compact_unwind_scanned: AtomicBool`; THEIRS maps this to `()`.  This is the spine of OURS' GC/subsection implementation.  It must remain in any upstream contribution.

8. **`MachOResolutionExt`** — OURS adds `{ got_address, plt_address, selref_address }` to the resolution record.  THEIRS uses `type ResolutionExt = ()`.  This is required for OURS' GOT/PLT/ObjC stub emitter.  Cannot be removed.

9. **`SectionAttributes.segname` field** — OURS adds `segname: [u8; 16]` to `SectionAttributes` (l 2595) so `is_writable()` can check the segment name at layout time.  THEIRS only stores `flags: SectionFlags`.  This field is required for correct `__DATA` vs `__TEXT` section placement and the `__DATA_CONST`/`__DATA` split.

10. **THEIRS has a typo**: `DyldChainedFixupsImporstFormat` (note "Imporst") at upstream `macho.rs` l 96.  OURS does not have this symbol; the writer imports it from THEIRS as `DyldChainedFixupsImporstFormat` (l 40 of THEIRS' writer).  When contributing upstream, fix the typo (`ImportsFormat`) — or preserve it to avoid a breaking change until a major version bump.
