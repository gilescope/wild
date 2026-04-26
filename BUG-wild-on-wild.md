# Wild-on-wild self-link bug

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

## What likely is the cause

The pattern (some `t`-visibility symbols from the same `.o` file are kept while their `T`-visibility neighbours are demoted to `U`) points at an inconsistency between liveness tracking and symtab emission rather than archive extraction. `process_archive` in `libwild/src/input_data.rs` already loads every `.o` from each rlib up-front, so the `.o` files themselves are all in the symbol DB. The bug looks like a layout/output bug specific to archive-sourced external symbols whose function bodies wild decides to drop, where the dropped-symbol entry is left in the symtab as undefined external instead of being deleted alongside the body.

A targeted experiment that would confirm: in `libwild/src/macho_writer.rs`'s symtab emitter, find the path that emits `(undefined) external` symbols and check whether it's filtering out symbols that *did* have a definition in an archive but were stripped. If so, the fix is either to keep those bodies live (because *something* in the link still references the symbol — the U entry exists for a reason) or to delete the U entry when emitting.

## Workaround

Use the system linker (`ld`) when building wild itself. CI presumably already does this implicitly. The bug only matters when bootstrapping wild through a previous wild build.

## Test surface

A regression test could be: in `wild/tests/`, build a tiny rust binary (a hello-world that calls `eprintln!`) using wild as `-fuse-ld`, then run it and assert exit code 0. The smaller version of the bug — failing to resolve `__stderrp` correctly — manifests on any `eprintln!`-using rust binary linked by wild *that also uses an archive whose .o files contain the trigger pattern*. We don't yet know the minimal reproducer beyond "wild itself".
