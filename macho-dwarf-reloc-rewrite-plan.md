# Mach-O DWARF Relocation Rewriting After Subsection Reorder

Status: **planned**. Surfaced 2026-05-14 while debugging variable reads in
BugStalker on a Rust-built showcase binary linked with system `ld`.

## Problem

When LLVM/clang emits a Mach-O `.o` with
`MH_SUBSECTIONS_VIA_SYMBOLS` set, each function symbol becomes an atom
the linker may reorder independently. LLVM emits debug-info relocations
into the standard `__DWARF` sections (`__debug_info`,
`__debug_aranges`, `__debug_line`, …) in **section-relative** form:
`extern=False`, target = `(__TEXT,__text)` or `(__DATA,__const)`, stored
value = "offset N into that section".

After a subsection reorder, the symbol that used to be at the section's
offset 0 is no longer at the new section base. The stored relocation
still resolves to `section_base + 0`, but that address now belongs to a
*different* atom. Every DIE attribute that used the section-relative
form (`DW_AT_low_pc`, `DW_AT_high_pc`, `DW_AT_entry_pc`,
`DW_AT_ranges`, the line program's start address, etc.) becomes
arithmetically valid but semantically wrong.

The downstream effect: debuggers that walk the DWARF read a function
DIE whose claimed range encompasses physical code belonging to other
functions. Local-variable DWARF expressions evaluated at PCs inside
those foreign regions read the foreign function's stack frame and
return garbage.

`dsymutil` (Apple's DWARF linker) propagates the same broken
relocations into the dSYM bundle, so the lie persists end-to-end.

### Reproduction

Built BugStalker's `examples/showcase` with default rustc + system `ld`
on macOS arm64:

```text
$ nm examples/target/aarch64-apple-darwin/debug/showcase | grep "8showcase4main$"
0000000100008d80 t __RNvCs6b9yCPHURWH_8showcase4main

$ dwarfdump --debug-info examples/target/aarch64-apple-darwin/debug/showcase.dSYM \
    | grep -B 1 -A 2 '"_RNvCs6b9yCPHURWH_8showcase4main"' | grep -E "low_pc|high_pc|linkage_name"
                  DW_AT_low_pc    (0x00000001000083e8)   # WRONG — main is at 0x100008d80
                  DW_AT_high_pc   (0x0000000100009128)
                  DW_AT_linkage_name  ("_RNvCs6b9yCPHURWH_8showcase4main")
```

DWARF claims main spans `0x1000083e8..0x100009128`. nm puts main at
`0x100008d80`. The 0x998 byte prefix DWARF includes is physically other
functions' code (per nm: `core::array::iter::IntoIterator::into_iter`,
`hashbrown::HashMap::insert` monomorphizations, etc.).

The .o file's `__debug_info` relocation table confirms the cause —
all 27 entries are `extern=False, target=(__TEXT,__text)`, i.e.
section-relative.

## The fix

When wild applies the subsection-reorder layout to a Mach-O output,
walk every section-relative relocation that points into a reordered
section and rewrite the stored value so it resolves to the same atom
it referred to pre-link.

### What we already have

- `SubsectionTracking::input_to_output_offset` (see
  `subsections-via-symbols-plan.md`) — maps a pre-reorder offset into
  a section to its post-reorder offset. Currently only used for
  laying out section bytes.
- `MachOArgs::symbol_order` — the order-file priority map.
- Per-atom output offsets via `Platform::compute_atom_output_offsets`.

### What's missing

Apply the same input → output offset translation to debug-info
relocations during write-out. Pseudocode:

```text
for each relocation in __debug_info, __debug_aranges, __debug_line, …:
    if reloc.extern == False && reloc.target ∈ reordered_sections:
        original_offset = read_value_at(reloc.address)
        atom = atom_owning_offset(reloc.target_section, original_offset)
        new_offset = atom.new_section_offset
                   + (original_offset - atom.old_section_offset)
        write_value_at(reloc.address, new_offset)
```

The `apply_debug_relocations` ELF analogue lives in
`libwild/src/elf_writer.rs:2153` and is a useful shape reference —
the macho equivalent would call the existing
`input_to_output_offset` to do the work.

### Sections to cover

| Section          | Notes                                                              |
| ---------------- | ------------------------------------------------------------------ |
| `__debug_info`   | The main offender. Every subprogram's `low_pc`/`high_pc`.          |
| `__debug_aranges`| Coarse PC → CU index. Section-relative entries here too.           |
| `__debug_line`   | Line program's `set_address` opcodes use the same reloc shape.     |
| `__debug_ranges` | (DWARF 4) Cross-CU range lists.                                    |
| `__debug_loc`    | Location lists.                                                    |
| `__debug_frame`  | If present (it isn't for most macOS Rust; cmpct-unwind handles it).|

### Verification

- After implementing, link `examples/showcase` with wild and re-run
  the BugStalker variable-read test. Expected: variables read at
  any breakpoint inside main return correct values (no
  `force-unwind-tables` workaround needed).
- The `dwarfdump --debug-info` output for main should show
  `DW_AT_low_pc = 0x100008d80` (matching nm), not the wider
  pre-fix value.
- Existing wild macOS tests should still pass.

## Why this is wild's responsibility

LLVM emits section-relative relocations because that's what the Mach-O
ABI specifies for this case — `extern` relocations for symbols inside
the same translation unit are *more* fragile than section-relative
ones in many other contexts. But once the linker takes advantage of
`MH_SUBSECTIONS_VIA_SYMBOLS`, only the linker has both the original
symbol → offset map and the post-reorder layout to fix the references.
Asking the compiler to predict the linker's layout decisions in its
relocations doesn't compose.

dsymutil could in principle re-derive the right addresses by
re-scanning the .o files' symbol tables and the linker's output, but
that's a re-do of the linker's work. wild already has the data.

## Out of scope here

- Cross-platform DWARF version 5 changes (`debug_rnglists`,
  `debug_loclists`) — separate concern.
- DWARF *size* optimization (`dwarf-size-plan.md`) — orthogonal.
- The ELF side of `apply_debug_relocations` is already correct;
  no changes needed there.
