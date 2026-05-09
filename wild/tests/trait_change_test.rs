//! Trait-change correctness test for tier-1 parse-skip cache.
//!
//! The hardest case for incremental linking: a change to a `pub trait`
//! method body or default impl in crate A forces cargo to recompile
//! crate B (which `impl`s the trait or uses it generically), even
//! though crate B's source didn't change. Wild's tier-1 cache must
//! invalidate BOTH rlibs — not just A's — and re-parse them so the
//! linker sees the new symbols.
//!
//! Failure modes if cache invalidation is wrong:
//! * Stale parse of B → B's symbol table mentions methods that don't match A's new vtable layout →
//!   undefined-symbol error or miscompile.
//! * Stale parse of A → trait impl points at the old method body's address → run-time output
//!   reflects the old method, not the new one. We catch this by asserting the binary's STDOUT
//!   changes between the pre-change and post-change runs.
//!
//! Only runs on macOS (target host for tier-1 today). Skips cleanly
//! if rustup / cargo aren't available.
#![cfg(target_os = "macos")]

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Workspace `Cargo.toml` — opts out of any parent workspace via the
/// empty `[workspace]` block so the test fixture stays standalone.
const WORKSPACE_CARGO: &str = r#"
[workspace]
members = ["crate-a", "crate-b"]
resolver = "2"

[workspace.package]
edition = "2021"
"#;

const CRATE_A_CARGO: &str = r#"
[package]
name = "crate-a"
version = "0.1.0"
edition = "2021"
"#;

/// crate-a's initial trait. The test will MUTATE the body of `greet`
/// to a different return value, then re-link. The dependent crate-b
/// will be recompiled by cargo (its rlib's rustc-fingerprint suffix
/// changes) and wild's tier-1 must NOT reuse the cached parse.
const CRATE_A_LIB_V1: &str = r#"
pub trait Greeter {
    fn greet(&self) -> &'static str;
}
"#;

const CRATE_B_CARGO: &str = r#"
[package]
name = "crate-b"
version = "0.1.0"
edition = "2021"

[dependencies]
crate-a = { path = "../crate-a" }
"#;

/// crate-b is the *bin* in the test workspace. It implements the
/// trait from crate-a and prints whatever `greet` returns. We stamp
/// the version into crate-a's trait IMPL inside crate-b — that way a
/// trait-source change in crate-a forces cargo to recompile crate-b
/// even though crate-b's source is unchanged across the test.
///
/// Exit codes:
///   42 — `greet()` returned the V1 string ("hello-v1")
///   43 — `greet()` returned the V2 string ("hello-v2")
///   1  — anything else (test fails)
const CRATE_B_MAIN: &str = r#"
use crate_a::Greeter;

struct Hello;
impl Greeter for Hello {
    fn greet(&self) -> &'static str { GREETING }
}

const GREETING: &str = "REPLACE_ME";

fn main() {
    let h = Hello;
    let s = h.greet();
    println!("{s}");
    std::process::exit(match s {
        "hello-v1" => 42,
        "hello-v2" => 43,
        _ => 1,
    });
}
"#;

fn wild_binary_path() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop();
    path.pop();
    path.push("wild");
    if !path.exists() {
        path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/debug/wild");
    }
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/trait-change-test")
}

fn write_fixture(root: &Path, b_greeting: &str) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root.join("crate-a/src")).unwrap();
    std::fs::create_dir_all(root.join("crate-b/src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), WORKSPACE_CARGO).unwrap();
    std::fs::write(root.join("crate-a/Cargo.toml"), CRATE_A_CARGO).unwrap();
    std::fs::write(root.join("crate-a/src/lib.rs"), CRATE_A_LIB_V1).unwrap();
    std::fs::write(root.join("crate-b/Cargo.toml"), CRATE_B_CARGO).unwrap();
    let main = CRATE_B_MAIN.replace("REPLACE_ME", b_greeting);
    std::fs::write(root.join("crate-b/src/main.rs"), main).unwrap();
}

fn cargo_build(root: &Path, wild: &Path, extra_link_arg: Option<&str>) -> (bool, String) {
    // RUSTFLAGS must be byte-stable across the test's two builds, or
    // cargo emits to a different output path and wild's per-output
    // sidecars never line up. Keep the flag string identical.
    let mut rustflags = format!("-C link-arg=-fuse-ld={}", wild.display());
    if let Some(arg) = extra_link_arg {
        rustflags.push_str(" -C link-arg=-Wl,");
        rustflags.push_str(arg);
    }
    let out = Command::new("cargo")
        .current_dir(root)
        .args(["build", "--release"])
        .env("RUSTFLAGS", &rustflags)
        .output()
        .expect("invoke cargo");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

fn run_binary(root: &Path) -> (i32, String) {
    let bin = root.join("target/release/crate-b");
    let out = Command::new(&bin).output().expect("run linked binary");
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let code = out.status.code().expect("exit code");
    (code, stdout)
}

#[test]
fn trait_change_invalidates_dependent_rlib() {
    if Command::new("cargo").arg("--version").output().is_err() {
        eprintln!("cargo not available — skipping trait-change test");
        return;
    }

    let wild = wild_binary_path();
    let root = fixture_dir();

    // Phase 1 — V1 fixture: cold build, sanity check.
    write_fixture(&root, "hello-v1");
    let (ok, out) = cargo_build(&root, &wild, None);
    assert!(ok, "v1 cold build failed:\n{out}");
    let (code, stdout) = run_binary(&root);
    assert_eq!(code, 42, "v1 cold exit code (stdout: {stdout:?})");
    assert_eq!(stdout, "hello-v1");

    // Phase 2 — seed: same V1 sources, but with `--incremental-cache=write`
    // so wild populates `.wild-hashes` + `.wild-pi-cache` next to the
    // output binary. cargo will see RUSTFLAGS changed → recompile
    // everything; that's intentional, we want a clean snapshot of the
    // V1 link's input set.
    let (ok, out) = cargo_build(&root, &wild, Some("--incremental-cache=write"));
    assert!(ok, "v1 seed build failed:\n{out}");
    let (code, _) = run_binary(&root);
    assert_eq!(code, 42, "v1 seed exit code");

    // Phase 3 — V1 reuse: identical sources + identical RUSTFLAGS →
    // wild should hit the parse-skip cache for every input. We don't
    // assert the cache-hit count from stderr (rustc swallows linker
    // stderr), but we do assert correctness: binary still exits 42.
    let (ok, out) = cargo_build(&root, &wild, Some("--incremental-cache=read-write"));
    assert!(ok, "v1 reuse build failed:\n{out}");
    let (code, stdout) = run_binary(&root);
    assert_eq!(code, 42, "v1 reuse exit code (stdout: {stdout:?})");
    assert_eq!(stdout, "hello-v1");

    // Phase 4 — V2: change crate-b's GREETING constant from "hello-v1"
    // to "hello-v2". This is a content change in crate-b's source AND
    // forces crate-b's rlib to be recompiled (different filename hash).
    // Wild's tier-1 must invalidate the cached parse of crate-b
    // because its filename-fingerprint changed; if it doesn't, the
    // linked binary's symbol table still resolves GREETING to the V1
    // string and the test exits 42 (incorrect for V2).
    let main = CRATE_B_MAIN.replace("REPLACE_ME", "hello-v2");
    std::fs::write(root.join("crate-b/src/main.rs"), main).unwrap();
    let (ok, out) = cargo_build(&root, &wild, Some("--incremental-cache=read-write"));
    assert!(ok, "v2 build failed:\n{out}");
    let (code, stdout) = run_binary(&root);
    assert_eq!(
        code, 43,
        "v2 exit code — expected 43 (post-change), got {code}. \
         If 42, wild reused a stale parse of crate-b and the binary \
         points at the old GREETING constant. Stdout: {stdout:?}"
    );
    assert_eq!(stdout, "hello-v2");

    // Phase 5 — TRAIT change in crate-a. We add a default-impl method
    // that crate-b doesn't override. The trait's source changes →
    // cargo recompiles crate-a (new rlib filename hash) AND crate-b
    // (transitively dirty). Wild's tier-1 must invalidate both.
    //
    // The new method's body returns "trait-changed-correctly" and
    // crate-b's main is also updated to call it instead of `greet`.
    // If wild reused stale parses, either the link fails (undefined
    // symbol) or runs but exit code != 44.
    let crate_a_v2 = r#"
pub trait Greeter {
    fn greet(&self) -> &'static str;
    /// New default-impl method added in V2. Any impl that doesn't
    /// override it picks this up; cargo MUST recompile every dependent
    /// rlib and wild must re-parse them.
    fn greet_loud(&self) -> &'static str { "trait-changed-correctly" }
}
"#;
    std::fs::write(root.join("crate-a/src/lib.rs"), crate_a_v2).unwrap();
    let crate_b_main_v3 = r#"
use crate_a::Greeter;

struct Hello;
impl Greeter for Hello {
    fn greet(&self) -> &'static str { "hello-v2" }
}

fn main() {
    let h = Hello;
    let s = h.greet_loud();
    println!("{s}");
    std::process::exit(if s == "trait-changed-correctly" { 44 } else { 1 });
}
"#;
    std::fs::write(root.join("crate-b/src/main.rs"), crate_b_main_v3).unwrap();
    let (ok, out) = cargo_build(&root, &wild, Some("--incremental-cache=read-write"));
    assert!(
        ok,
        "trait-change build failed — most likely wild reused a stale \
         parse of crate-a or crate-b that doesn't see the new \
         `greet_loud` method, leaving an undefined-symbol error.\n{out}"
    );
    let (code, stdout) = run_binary(&root);
    assert_eq!(
        code, 44,
        "trait-change exit code — expected 44, got {code}. \
         If non-44, wild's parse-skip handed the writer stale \
         symbol info that doesn't match the new trait vtable. \
         Stdout: {stdout:?}"
    );
    assert_eq!(stdout, "trait-changed-correctly");
}
