//! End-to-end smoke tests for the tier-1 parse-skip gates
//! (`WILD_INCREMENTAL_PARSE_SKIP_WRITE`,
//! `..._CANARY`, `..._READ`).
//!
//! These build a trivial C program, link it through wild under each
//! gate in turn, and assert:
//!
//! 1. The write pass populates the on-disk cache without blowing up
//!    or corrupting the output binary.
//! 2. The canary pass (which builds a fresh cache AND compares it
//!    against the one just written) does not panic. A lossy schema
//!    or stale cache would panic here via
//!    `symbol_db::panic_canary_diff`.
//! 3. The read pass reproduces a runnable binary whose exit code
//!    matches the fresh-parse baseline.
//!
//! Only runs on macOS. If `clang` isn't available the test is
//! skipped — same policy as the existing macho integration harness.

#![cfg(target_os = "macos")]

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Minimal C fixture — `main` returns 42, matching the exit-code
/// convention used by the broader macho integration tests so any
/// future reuse stays consistent.
const HELLO_C: &str = "int main() { return 42; }\n";

fn wild_binary_path() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // remove test binary name
    path.pop(); // remove `deps/`
    path.push("wild");
    if !path.exists() {
        path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/debug/wild");
    }
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn build_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/parse-skip-tests")
}

/// Compile `src` to an object file in `build_dir`. Returns the
/// resulting `.o` path. Panics on compile failure — there's no
/// recovery path for a broken fixture.
fn compile(src_path: &Path, build: &Path) -> PathBuf {
    let obj = build.join("hello.o");
    let result = Command::new("clang")
        .arg("-c")
        .arg(src_path)
        .arg("-o")
        .arg(&obj)
        .output()
        .expect("invoke clang");
    assert!(
        result.status.success(),
        "clang compile failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    obj
}

/// Link `obj` with wild into `output`, optionally with env vars set.
/// Returns stdout+stderr for the caller to assert on.
fn link_with_wild(obj: &Path, output: &Path, envs: &[(&str, &str)]) -> (bool, String) {
    link_with_wild_extra(obj, output, envs, &[])
}

/// Variant of `link_with_wild` that also passes extra args directly
/// to wild (via `-Wl,…` so clang forwards them). Used to exercise
/// the `--incremental-cache` flag without going through env vars.
fn link_with_wild_extra(
    obj: &Path,
    output: &Path,
    envs: &[(&str, &str)],
    wild_args: &[&str],
) -> (bool, String) {
    let wild = wild_binary_path();
    let mut cmd = Command::new("clang");
    cmd.arg(format!("-fuse-ld={}", wild.display()))
        .arg(obj)
        .arg("-o")
        .arg(output);
    for arg in wild_args {
        // `-Wl,foo` tells clang to pass `foo` to the linker.
        cmd.arg(format!("-Wl,{arg}"));
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let result = cmd.output().expect("invoke clang driver");
    let mut out = String::from_utf8_lossy(&result.stdout).into_owned();
    out.push_str(&String::from_utf8_lossy(&result.stderr));
    (result.status.success(), out)
}

/// Run the just-linked binary and return its exit code. Fails the
/// test if the binary can't be invoked at all (e.g. wild produced
/// an unloadable Mach-O).
fn run_exit_code(bin: &Path) -> i32 {
    Command::new(bin)
        .output()
        .expect("run linked binary")
        .status
        .code()
        .expect("process terminated without exit code")
}

/// End-to-end: WRITE → CANARY → READ, asserting correctness after
/// each phase. One test rather than three to keep the write / cache
/// state in a consistent lock-step; splitting would require either
/// shared fixtures across tests (fragile under parallel cargo test)
/// or redundant rebuilds.
#[test]
fn parse_skip_gates_round_trip() {
    // If clang isn't available, skip (matches macho_integration_tests
    // behaviour — some CI shards don't have a C toolchain).
    if Command::new("clang").arg("--version").output().is_err() {
        eprintln!("clang not available — skipping parse-skip round-trip test");
        return;
    }

    let build = build_dir();
    std::fs::create_dir_all(&build).expect("mkdir build");
    let src = build.join("hello.c");
    std::fs::write(&src, HELLO_C).expect("write fixture");
    let obj = compile(&src, &build);

    // Baseline: fresh link (no gate), confirm fixture works.
    let baseline = build.join("baseline");
    let (ok, out) = link_with_wild(&obj, &baseline, &[]);
    assert!(ok, "baseline link failed:\n{out}");
    assert_eq!(run_exit_code(&baseline), 42, "baseline exit code");

    // Phase 1: WRITE — tee parse into the on-disk cache.
    let write_out = build.join("write-out");
    let (ok, out) = link_with_wild(
        &obj,
        &write_out,
        &[("WILD_INCREMENTAL_PARSE_SKIP_WRITE", "1")],
    );
    assert!(ok, "write-gate link failed:\n{out}");
    assert_eq!(run_exit_code(&write_out), 42, "write-gate exit code");

    // Phase 2: populate .wild-hashes so canary / read can gate on
    // clean-input status. Without it, clean_input_paths is empty and
    // canary / read never engage (safe fall-through to parse).
    let _ = std::fs::remove_file(write_out.with_extension("wild-hashes"));
    let (ok, out) = link_with_wild(
        &obj,
        &write_out,
        &[
            ("WILD_INCREMENTAL_DEBUG", "1"),
            ("WILD_INCREMENTAL_PARSE_SKIP_WRITE", "1"),
        ],
    );
    assert!(ok, "wild-hashes population link failed:\n{out}");

    // Phase 3: CANARY — fresh parse + compare against on-disk cache.
    // A lossy schema or stale cache would cause
    // `panic_canary_diff` to fire; clang would surface that as a
    // nonzero linker exit with a panic message in stderr.
    let canary_out = build.join("canary-out");
    // Output path and its wild-hashes must line up — reuse the
    // existing output so its side-car still matches.
    let _ = std::fs::remove_file(&canary_out);
    let (ok, out) = link_with_wild(
        &obj,
        &write_out, // reuse — wild-hashes already keyed to this path
        &[("WILD_INCREMENTAL_PARSE_SKIP_CANARY", "1")],
    );
    assert!(
        ok,
        "canary-gate link panicked or failed — schema / cache drift:\n{out}"
    );
    assert!(
        !out.contains("canary mismatch"),
        "canary emitted a mismatch message (binary was produced but a \
         divergence was logged):\n{out}"
    );
    assert_eq!(run_exit_code(&write_out), 42, "canary-gate exit code");
    let _ = canary_out;

    // Phase 4: READ — replay the cache, skipping the parse. Output
    // must still be a valid Mach-O that runs and returns the
    // expected exit code.
    let (ok, out) = link_with_wild(
        &obj,
        &write_out,
        &[("WILD_INCREMENTAL_PARSE_SKIP_READ", "1")],
    );
    assert!(ok, "read-gate link failed:\n{out}");
    assert_eq!(
        run_exit_code(&write_out),
        42,
        "read-gate exit code — cache replay produced a broken binary"
    );

    // Phase 5: TIER-3 CANARY — byte-compare reusable sections in the
    // freshly-written output against the previous output's bytes.
    // Two consecutive links capture the snapshot and then verify
    // tier-3 reuse is empirically safe. A divergence message in
    // stderr means the dirty-bitmap predicate would let tier 3
    // reuse a section whose bytes actually differ — fails this
    // test before phase 2b's writer-skip is allowed to ship.
    //
    // First link: write a layout snapshot so the second link has
    // a `prev` to compare against.
    let (ok, out) = link_with_wild(
        &obj,
        &write_out,
        &[
            ("WILD_INCREMENTAL_LAYOUT_CANARY", "1"),
            ("WILD_INCREMENTAL_PARSE_SKIP_READ", "1"),
        ],
    );
    assert!(ok, "tier-3 canary seed link failed:\n{out}");

    let (ok, out) = link_with_wild(
        &obj,
        &write_out,
        &[
            ("WILD_INCREMENTAL_TIER3_CANARY", "1"),
            ("WILD_INCREMENTAL_PARSE_SKIP_READ", "1"),
        ],
    );
    assert!(ok, "tier-3 canary link failed:\n{out}");
    // The tier-3 canary line is "wild tier-3 canary: M/N sections
    // byte-identical, X bytes verified safe to reuse". A first-
    // divergence line only appears when M != N. Asserting the
    // absence of "first divergence" is the canary's "all reusable
    // sections checked out" pass criterion.
    assert!(
        !out.contains("first divergence"),
        "tier-3 canary reported a section whose 'reusable' verdict \
         disagreed with byte-equality — phase 2b reuse would corrupt:\n{out}"
    );
    assert_eq!(
        run_exit_code(&write_out),
        42,
        "tier-3 canary exit code — output corrupted by inspection?"
    );

    // Phase 6: TIER-3 SKIP — actual writer bypass. With every
    // section reusable AND a prev_output mmap available, wild
    // skips the platform writer entirely and copies prev bytes
    // wholesale into the new output. Asserts:
    //   * stderr names the skip path,
    //   * the resulting binary runs and exits 42 (functional
    //     correctness of the byte-copy + codesign preservation).
    // Two cold runs of wild aren't necessarily byte-identical
    // (wild's writer has pre-existing non-determinism in
    // LC_UUID / timestamp regions), so we can't assert byte-
    // equivalence vs a fresh cold link — but the canary above
    // proved per-section bytes match, and the run + exit code
    // here proves codesign + load commands stayed valid.
    let prev_size = std::fs::metadata(&write_out)
        .map(|m| m.len())
        .unwrap_or(0);
    let (ok, out) = link_with_wild(
        &obj,
        &write_out,
        &[
            ("WILD_INCREMENTAL_TIER3_SKIP", "1"),
            ("WILD_INCREMENTAL_DEBUG", "1"),
            ("WILD_INCREMENTAL_NO_EARLY_SKIP", "1"),
            ("WILD_INCREMENTAL_PRE_LOAD_SKIP", "0"),
            ("WILD_INCREMENTAL_NO_POST_LOAD_SKIP", "1"),
        ],
    );
    assert!(ok, "tier-3 skip link failed:\n{out}");
    assert!(
        out.contains("wild tier-3 skip: bypassed writer"),
        "tier-3 skip path didn't fire — expected `wild tier-3 skip: \
         bypassed writer …` in stderr but got:\n{out}"
    );
    let new_size = std::fs::metadata(&write_out)
        .map(|m| m.len())
        .unwrap_or(0);
    assert_eq!(
        prev_size, new_size,
        "tier-3 skip output size shifted — speculative copy is supposed to \
         preserve the previous binary's size byte-for-byte"
    );
    assert_eq!(
        run_exit_code(&write_out),
        42,
        "tier-3 skip exit code — bypassed writer produced an unrunnable binary"
    );

    // Phase 7: --incremental-cache=read-write FLAG
    // Productisation gate. The flag must reach `Args::parse` and
    // translate into the same env-var gates the older tests used.
    // Two consecutive links: first seeds with `=write`, second
    // fires the read-side skip via `=read-write`. We pass the flag
    // through clang via `-Wl,…` so the integration is end-to-end
    // (driver → linker), matching how `cargo build` would emit it
    // when wild is wired in via RUSTFLAGS.
    let flag_out = build.join("flag-out");
    let _ = std::fs::remove_file(&flag_out);
    let _ = std::fs::remove_file(flag_out.with_extension("wild-hashes"));
    let _ = std::fs::remove_file(flag_out.with_extension("wild-layout"));
    let _ = std::fs::remove_file(flag_out.with_extension("wild-pi-cache"));
    let (ok, out) = link_with_wild_extra(
        &obj,
        &flag_out,
        &[("WILD_INCREMENTAL_DEBUG", "1")],
        &["--incremental-cache=write"],
    );
    assert!(ok, "--incremental-cache=write seed link failed:\n{out}");
    assert!(
        flag_out.with_extension("wild-pi-cache").exists(),
        "--incremental-cache=write didn't produce a parse-skip bundle"
    );
    assert!(
        flag_out.with_extension("wild-layout").exists(),
        "--incremental-cache=write didn't produce a layout snapshot"
    );

    let (ok, out) = link_with_wild_extra(
        &obj,
        &flag_out,
        &[
            ("WILD_INCREMENTAL_NO_EARLY_SKIP", "1"),
            ("WILD_INCREMENTAL_PRE_LOAD_SKIP", "0"),
            ("WILD_INCREMENTAL_NO_POST_LOAD_SKIP", "1"),
        ],
        &["--incremental-cache=read-write"],
    );
    assert!(ok, "--incremental-cache=read-write link failed:\n{out}");
    assert!(
        out.contains("wild tier-3 skip: bypassed writer"),
        "--incremental-cache=read-write didn't fire the writer-skip path:\n{out}"
    );
    assert_eq!(
        run_exit_code(&flag_out),
        42,
        "exit code via --incremental-cache=read-write"
    );
}
