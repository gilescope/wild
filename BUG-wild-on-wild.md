# Wild-on-wild self-link bug — FIXED 2026-04-27

**Status: fixed.** Root cause and patch summary at the top; the rest of this file preserves the (sometimes wrong) intermediate diagnoses for history.

The fix lives in `libwild/src/macho_writer.rs::write_exe_symtab`'s "symtab: filter orphan N_UNDF_EXT" step. The existing N_UNDF|N_EXT collection mirrored every undefined-external from every input `.o` into the output symtab. Many of those `U` references pointed at symbols whose definitions lived in archive members or atoms wild's GC dropped — leaving the `U` entry orphaned: no chained-fixup import, no `-U`-list entry, no defined-locally fallback. The added post-filter drops those orphans before the symtab is written.

Diagnosis path that worked: peppered the writer with `debug_assert`s ("invariants we expect to hold if our mental model is right"), built wild in debug mode, used it as the linker for a release rebuild. The orphan-detector assert tripped immediately with 262 bad symbols and a clear stack trace. The earlier hypotheses ("GOT-cursor spill", "chained-fixup encoder bug") were both disproven by other asserts that did NOT trip.

Regression test: `wild/tests/wild_on_wild_test.rs` builds a tiny rust binary depending on `zstd` (the original trigger archive), links via wild, runs it. Pre-fix this SIGSEGVs at startup; post-fix it exits 31 cleanly.

---

When the `wild` linker is used to link itself (`cargo build --release -p wild-linker` with `RUSTFLAGS=-C link-arg=-fuse-ld=<wild>`), the resulting `wild` binary is corrupt and segfaults on startup.

## Reproduction

```bash
# 1. Build a known-good wild with the system linker.
unset RUSTFLAGS
cargo build --release -p wild-linker
cp target/release/wild /tmp/wild-good

# 2. Use that wild to relink wild itself.
touch wild/src/main.rs
RUSTFLAGS="-C link-arg=-fuse-ld=/tmp/wild-good" \
    cargo build --release -p wild-linker
cp target/release/wild /tmp/wild-broken

# 3. /tmp/wild-broken segfaults immediately:
/tmp/wild-broken --serve /tmp/sock
# → "deadlock in SIGSEGV handler" + EXC_BAD_ACCESS
```

## Crash signature

Under lldb the crash always lands at the same site:

```text
EXC_BAD_ACCESS (code=1, address=0x7272657274735f00)
frame #0: libsystem_pthread.dylib`pthread_mutex_lock + 12
    ldr  x8, [x0]                ; x0 is the mutex pointer — invalid
frame #1: wild-broken`std::io::stdio::Stderr::lock
```

The bytes at `0x7272657274735f00` (low → high) decode to `\0\0_strerr` — a fragment that looks like the symbol-name string `__stderrp` instead of the address of `__stderrp`'s entry in `__DATA_CONST,__got`.

## Root signature

`nm /tmp/wild-broken | wc -l` shows **432 undefined symbols** vs the system-linker build's **172** — **261 extra externals** that wild left unresolved despite their definitions being available in the input archives. Sample of what's missing:

```text
___assert_rtn                       ; libSystem stub
___isOSVersionAtLeast               ; libSystem stub
___stderrp                          ; libSystem (the crash site)
__ZN12regex_syntax3hir7literal...   ; regex_syntax rlib
__ZN14unsafe_libyaml3api...         ; unsafe_libyaml rlib
_ZSTD_compress2                     ; libzstd_sys-*.rlib
_ZSTD_compressBegin_usingCDict...   ; libzstd_sys-*.rlib
…
```

Of the 261 extras, 59 trace to `libzstd_sys-*.rlib`. The rest span at least: `rand_chacha`, `rand`, `rand_core`, `regex_syntax`, `unsafe_libyaml`, `anyhow`, `blake3`, plus some libSystem and compiler-builtins stubs. Wild emits these symbols as `(undefined) external` in the output's symtab, but the binary's `LC_DYLD_CHAINED_FIXUPS` import table doesn't list them at all — so dyld doesn't even attempt to bind them. References in code presumably crash on first use; but for `__stderrp` the bytes that *should* be the resolved address are never written, leaving leftover string-pool data in the GOT slot. That's how the Mutex pointer ends up looking like `_strerr…`.

## What's NOT the cause

- **Not the v1–v7 daemon work.** Reproduces at commit `4a064bf` (pre-daemon, "tier-4 padding") with identical symptoms.
- **Not chained fixups in general.** Wild's bevy-dylib output also uses `LC_DYLD_CHAINED_FIXUPS` and runs cleanly.
- **Not `-dead_strip`.** Stripping the flag from the link line doesn't change the missing-symbol count (432 vs 433).

## Investigation update (2026-04-26)

**Theory that turned out wrong:** I suspected GOT-cursor spill —
`create_resolution` over-allocating past `__got`'s reserved size.
Confirmed false: tracing every `create_resolution` GOT allocation
via the new `WILD_DEBUG_GOT_SPILL=1` env var shows exactly 252
allocations on a wild self-link, all within `__got`'s 2016-byte
reservation (highest address `0x1006c07d8`, end is `0x1006c07e0`).
So the GOT allocator is fine.

**What's confirmed:**

- Trace inside `write_macho`'s chained-fixup encoder shows the
  encoder writes the *correct* bytes (with `next_stride=2`) at
  `__got` slot 0. A read-back immediately after the loop confirms
  the slot has the encoded value.
- The final binary's `__got` slot 0 has a **different** value — the
  pre-encoder bytes that `write_got_entries` wrote (raw absolute
  address with no `next_stride`). Something between the encoder
  and the file flush overwrites `__got`.
- `dyld_info -fixup_chains /tmp/wild-broken` shows seg[2]
  (`__DATA_CONST,__got`) with a single chain start at offset 0
  and only `next=0` everywhere. That makes sense: most slots have
  the *raw absolute* bytes (untouched chained-fixup format would
  be a rebase with target = absolute - image_base, and `next` set
  by the encoder; we see neither).
- The crash address `0x7272657274735f00` decodes to the bytes
  `\0_strerr...` which match what's at file offset `0x6b4b70`
  in the binary — the `__LINKEDIT` strtab containing the symbol
  name string `_strerror_r\0`. So the runtime got that string-
  table address as the value of some GOT slot, dereferenced it as
  a Mutex pointer, and SIGSEGV'd.
- 261 symbols (zstd internals + Rust-mangled names from
  `regex_syntax` / `unsafe_libyaml` / `anyhow` / etc.) appear in
  the binary's symtab as undefined external but NOT in the
  chained-fixups imports table — dyld doesn't try to bind them.

**Open question:** what writes to the `__got` slots *after* the
chained-fixup encoder runs (line ~1947 of `macho_writer.rs`) but
before the file is flushed? The post-encoder snapshot shows correct
encoded bytes; the on-disk file shows raw absolute addresses. Either
a second pass through `write_got_entries` runs (didn't see one in
the call graph), or a tier-3 / mmap path overlays previous-run
bytes back on top.

A targeted next step: instrument `sized_output.flush()` and
`crate::macho_codesign::sign_in_place` to log what they see at
`file_off=0x6c0000` on entry; whichever shows the encoded bytes
*not* present is the layer that overwrote them. The post-encoder
snapshot already confirms the encoder put them there; it's the
later layer that matters.

## (Disproven "GOT spill" hypothesis below — kept as history)

> **WRONG**: This section blamed `create_resolution` over-allocating
> past `__got`'s reserved size. Disproven by `WILD_DEBUG_GOT_SPILL=1`
> tracing — the allocator runs exactly 252 times on a self-link and
> all addresses stay within the reserved 2016 bytes. The real bug is
> in the chained-fixup write-back step, not the GOT allocator.

Tracing wild's chained-fixup writer (`libwild/src/macho_writer.rs`,
fn `write_macho`) reveals the bug isn't in the symtab emitter at all
— it's in **GOT-slot allocation**. Each external symbol that needs a
GOT entry calls `MachO::create_resolution`, which advances a cursor
inside `memory_offsets[part_id::GOT]`. The size-estimation pass
(`MachO::allocate_resolution` driven by `finalise_symbol_sizes` in
`libwild/src/layout.rs`) is *supposed* to count exactly the same
symbols and pre-reserve `GOT_size = needs_got_count × 8` bytes.

For wild's self-link the two passes disagree massively. The
allocated `__got` section is **2016 bytes** (252 entries) — that's
what `dyld_info -segments /tmp/wild-broken` shows — yet
`create_resolution` runs against the cursor for **6,627 fixups**
(per a chained-fixup count trace). The cursor spills past `__got`'s
end (`0x1006C07E0`) into `__DATA`, then past `__DATA`, finally into
`__LINKEDIT` where the symtab string pool lives. `_strerror_r`'s
"got_address" comes back as `0x10074a850`, which decodes as a file
offset deep in the strtab — the bytes there happen to spell
fragments of Rust mangled names. dyld faithfully writes these
fragment-bytes into the slot at runtime; when the binary's stderr
init code dereferences that slot it gets a Mutex pointer of
`0x7272657274735f00` (`\0_strerr` ASCII) and crashes.

Confirmation evidence:

- A trace at the chained-fixup encoder shows it writes **correct**
  bytes to `__got` slot 0 with `next_stride=2`. The post-encoder
  read-back confirms.
- `dyld_info -fixup_chains` on the broken binary shows seg[2]
  (`__DATA_CONST`, where `__got` lives) has only `start[0]: 0x0000`
  — a one-entry chain — but inspecting the raw bytes shows the
  chain was emitted with `next=0` everywhere, terminating after the
  first entry. dyld processes only that one slot; the rest stay as
  their pre-encoder bytes (raw absolute addresses written by
  `write_got_entries`).
- The `_strerror_r` resolution has both PLT and GOT addresses
  populated; the GOT address is well outside the bounds the layout
  reserved.

So actually two related bugs:

1. **GOT-cursor spill.** `create_resolution` allocates GOT slots at
   addresses past `__got`'s end. Every spilled symbol points into a
   neighbouring section.
2. **Chain encoding partial-overwrite.** The chained-fixup encoder
   only links chains up to the SIZE the layout reserved for `__got`.
   The 6,627 - 252 spilled fixups were never linked into a chain;
   their slots stay at whatever bytes `apply_relocations` /
   `write_got_entries` wrote. (The first slot looks correctly
   chained because `dyld_info` reports `start[0]: 0x0000` and
   `next=0` — a chain of one — which is technically valid encoding,
   just incomplete.)

The PRIMARY fix is bug 1: align `finalise_symbol_sizes`'s symbol
iteration with `finalise_symbol_resolution`'s so the same set of
symbols is counted as is allocated. Likely root: the size pass uses
`is_canonical(symbol_id)` to filter (see `layout.rs:810`), while the
resolution pass uses a different predicate. The two predicates need
to converge.

A guard rail to prevent silent corruption (NOT a fix) ships in this
commit: setting `WILD_DEBUG_GOT_SPILL=1` traces every
`create_resolution` GOT allocation to `/tmp/wild-got-spill.log`,
making it easy to spot when the cursor crosses `__got`'s reserved
end.

## Workaround

Use the system linker (`ld`) when building wild itself. CI presumably already does this implicitly. The bug only matters when bootstrapping wild through a previous wild build.

## Test surface

A regression test could be: in `wild/tests/`, build a tiny rust binary (a hello-world that calls `eprintln!`) using wild as `-fuse-ld`, then run it and assert exit code 0. The smaller version of the bug — failing to resolve `__stderrp` correctly — manifests on any `eprintln!`-using rust binary linked by wild *that also uses an archive whose .o files contain the trigger pattern*. We don't yet know the minimal reproducer beyond "wild itself".
