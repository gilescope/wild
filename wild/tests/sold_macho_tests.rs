//! Test runner for sold (bluewhalesystems/sold) Mach-O shell tests.
//!
//! These tests are adapted from the sold linker's Mach-O test suite (MIT License).
//!
//! Each test is a bash script that compiles C/C++ code, links with the linker
//! under test (via `--ld-path=./ld64`), and verifies the output.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn wild_binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wild"))
}

fn sold_tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sold-macho")
}

fn collect_tests(tests: &mut Vec<libtest_mimic::Trial>) {
    let wild_bin = wild_binary_path();
    let test_dir = sold_tests_dir();

    // Create a per-process working directory with ld64 symlink. Nextest runs each
    // test in its own process, all of which re-enter `collect_tests`; sharing one
    // path races on the symlink (`AlreadyExists`).
    let work_dir = std::env::temp_dir().join(format!("wild-sold-tests-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).unwrap();
    let ld64_link = work_dir.join("ld64");
    let _ = std::fs::remove_file(&ld64_link);
    std::os::unix::fs::symlink(&wild_bin, &ld64_link).unwrap();

    for entry in std::fs::read_dir(&test_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "sh") {
            continue;
        }

        let test_name = path.file_stem().unwrap().to_string_lossy().to_string();

        // Skip arch-incompatible tests at discovery time — they
        // would only ever show up as `ignored`, which conflates
        // "wild can't do this" with "the source can't even produce
        // the expected artifacts on this arch". `literals` is the
        // canonical example: ARM64 clang materialises double
        // constants via `MOVK` instead of emitting a `__literal8`
        // section, so the test's `objdump` grep would never match
        // regardless of linker.
        if !cfg!(target_arch = "x86_64") && is_x86_only(&test_name) {
            continue;
        }

        let test_path = path.clone();
        let wd = work_dir.clone();

        let ignored = should_ignore(&test_name);

        tests.push(
            libtest_mimic::Trial::test(format!("sold-macho/{test_name}"), move || {
                run_sold_test(&test_path, &wd).map_err(Into::into)
            })
            .with_ignored_flag(ignored),
        );
    }
}

fn is_x86_only(name: &str) -> bool {
    matches!(name, "literals")
}

fn should_ignore(name: &str) -> bool {
    // Tests that don't use --ld-path (invoke ./ld64 directly without cc)
    const DIRECT_LD64: &[&str] = &[];

    // Tests that use flags/features Wild doesn't support yet
    const UNSUPPORTED_FLAGS: &[&str] = &[
        // flat-namespace now passes (GOT for local globals + MH_TWOLEVEL removal)
        // undefined now passes (-flat_namespace + -undefined,warning)
        // U now passes (-U emits undefined symbol in output symtab)
        // umbrella now passes (LC_SUB_FRAMEWORK emission)
        // application-extension now passes (-application_extension + TBD flags)
        // application-extension2 now passes (MH_APP_EXTENSION_SAFE check)
        // exported-symbols-list now passes (export trie filtering via export_list)
        // unexported-symbols-list now passes (unexport_list filtering)
        // export-dynamic now passes (LTO support + EXPORT_DYNAMIC flag fix)
        // merge-scope now passes (weak_def_can_be_hidden visibility merging + trie default)
        // hidden-l now passes (archive symbols added to unexport list)
        // needed-l now passes (prefix link modifiers fall through to -l logic)
        // needed-framework now passes (dead_strip_dylibs + needed)
        // weak-l now passes (LC_LOAD_WEAK_DYLIB command value fix)
        // reexport-l now passes (recursive LC_REEXPORT_DYLIB chain tracing)
        // reexport-library now passes (symtab alignment + reexport_library)
        // install-name now passes (-install_name support)
        // install-name-executable-path now passes (@executable_path expansion)
        // install-name-loader-path now passes (@loader_path expansion)
        // install-name-rpath now passes (@rpath expansion in re-export resolution)
        // rpath now passes (-rpath → LC_RPATH)
        // search-paths-first now passes (default search order is paths-first)
        // search-dylibs-first now passes (pre-scan for global flags)
        // sectcreate now passes (-sectcreate data written to TEXT segment gap)
        // order-file now passes (atom-reorder via per-atom output offsets)
        /* stack-size now passes
                       * map now passes (link map file writer)
                       * dependency-info now passes
                       * print-dependencies now passes (--print-dependencies output)
                       * macos-version-min now passes
                       * platform-version now passes
                       * S now passes (stab debug symbol pass-through + -S strip)
                       * strip now passes (LINKEDIT packing + linker-signed codesign)
                       * no-function-starts now passes
                       * data-in-code-info now passes
                       * subsections-via-symbols now passes (signed SectionDeltas carries
                       *   alignment padding as insertion-direction entries) */
    ];

    // Tests requiring LTO
    // lto, object-path-lto, export-dynamic now pass (Mach-O LTO via libLTO.dylib)
    const LTO: &[&str] = &[];

    // Tests that need linking against a .dylib
    const NEEDS_DYLIB_INPUT: &[&str] = &[
        // dylib now passes (dylib input consumption)
        // tls-dylib now passes (cross-dylib TLV via GOT-bound TLV descriptor)
        /* data-reloc now passes
         * fixup-chains-addend now passes (implicit addend from data + import table
         * addend) fixup-chains-addend64 now passes
         * (DYLD_CHAINED_IMPORT_ADDEND64 format 3)
         * weak-def-dylib now passes
         * mark-dead-strippable-dylib now passes (MH_DEAD_STRIPPABLE_DYLIB +
         * auto-strip) */
    ];

    // Validation/correctness bugs in Wild to fix
    const WILD_BUGS: &[&str] = &[
        /* tls now passes (cross-dylib TLV via GOT-bound TLV descriptor)
         * tls-mismatch now passes (dylib_tls_symbols set + TLVP-on-non-TLS check)
         * tls-mismatch2 now passes (regular-GOT-on-TLS-target check)
         * cstring now passes (S_CSTRING_LITERALS merge enabled)
         * duplicate-error now passes (error format matches sold)
         * missing-error now passes (error format matches sold)
         * undef now passes (-u symbols kept alive as GC roots)
         * fixup-chains-unaligned-error now passes (test asm symbol prefix fix)
         * exception-in-static-initializer now passes (libc++ message wording fix)
         * indirect-symtab now passes (DYSYMTAB + indirect symbol table)
         * init-offsets now passes (__init_offsets section with S_INIT_FUNC_OFFSETS)
         * init-offsets-fixup-chains now passes (-fixup_chains implies -init_offsets)
         * libunwind/objc-selector now pass (TBD ObjC-key expansion,
         * data-section pass-through, 32-byte selector-loading stubs in
         * `__stubs` with inline methname strings, synthesised
         * `__DATA,__objc_selrefs` and `__DATA,__objc_imageinfo` sections so
         * dyld+objc canonicalise SELs at image load, and two-level-namespace
         * binds via `dylib_symbol_provenance`).
         * debuginfo now passes (SO/BNSYM/FUN/ENSYM stab synthesis for dsymutil) */
    ];

    // x86_64-specific tests — skipped on ARM64 because the source
    // wouldn't generate the expected section/encoding to begin with,
    // not because of a wild bug.
    const X86_ONLY: &[&str] = &[
        // ARM64 clang materialises double constants via four `MOVK`
        // instructions, so no `__literal8` section is emitted from
        // the input — no linker can synthesise one. Apple ld64 also
        // wouldn't pass this test on ARM64.
        "literals",
    ];

    // Tests that invoke ld64 directly (not through cc --ld-path)
    const NO_LD_PATH: &[&str] = &[];

    // .tbd parsing — all pass
    const TBD: &[&str] = &[];

    // Load command / output format checks
    const OUTPUT_FORMAT: &[&str] = &[
        // lc-build-version now passes (accepts tool 3)
        // uuid now passes (-final_output, -no_uuid, -random_uuid)
        // uuid2 now passes
        // version now passes (-v outputs Wild version)
        // w now passes (-w suppresses warnings)
        // Z now passes (-Z no default search paths)
        // adhoc-codesign now passes (linker-signed + no_adhoc_codesign flag)
        // dead-strip-dylibs now passes
        // dead-strip-dylibs2 now passes
    ];

    DIRECT_LD64.contains(&name)
        || UNSUPPORTED_FLAGS.contains(&name)
        || LTO.contains(&name)
        || WILD_BUGS.contains(&name)
        || X86_ONLY.contains(&name)
        || NO_LD_PATH.contains(&name)
        || NEEDS_DYLIB_INPUT.contains(&name)
        || TBD.contains(&name)
        || OUTPUT_FORMAT.contains(&name)
}

fn run_sold_test(test_path: &Path, work_dir: &Path) -> Result<(), String> {
    let output = Command::new("bash")
        .arg(test_path)
        .current_dir(work_dir)
        .env("WILD_VALIDATE_OUTPUT", "1")
        .output()
        .map_err(|e| format!("bash: {e}"))?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut msg = format!("Test failed with status {}\n", output.status);
        if !stdout.is_empty() {
            msg.push_str(&format!("stdout:\n{stdout}\n"));
        }
        if !stderr.is_empty() {
            msg.push_str(&format!("stderr:\n{stderr}\n"));
        }
        return Err(msg);
    }

    Ok(())
}

fn main() {
    if cfg!(not(target_os = "macos")) {
        eprintln!("sold MachO tests only run on macOS — skipping.");
        return;
    }
    let mut tests = Vec::new();
    collect_tests(&mut tests);
    let args = libtest_mimic::Arguments::from_args();
    libtest_mimic::run(&args, tests).exit();
}
