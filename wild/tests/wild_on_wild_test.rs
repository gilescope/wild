//! Regression test for the wild-on-wild self-link bug fixed in v9.
//!
//! Pre-fix symptom: when `wild` linked a Rust binary that pulled in
//! `zstd-sys` (or any other static archive whose `.o` members had
//! undefined-external references to atoms wild's GC dropped), the
//! resulting binary's symtab held ~260 orphan `N_UNDF | N_EXT`
//! entries that dyld never bound. The strtab bytes for those
//! entries' names ended up bleeding into adjacent `__got` / `__DATA`
//! pages at link time and the binary segfaulted on first stderr or
//! mutex op (`pthread_mutex_lock` dereferencing a fragment of a
//! symbol-name string).
//!
//! This test reproduces the trigger and asserts the linked binary
//! runs cleanly. It's deliberately small — a hello-world that
//! depends on `zstd` (which transitively pulls in the C archive).
//! On a buggy wild build this test SIGSEGVs at startup; the fix is
//! the post-filter in `write_exe_symtab` that drops orphan
//! `N_UNDF|N_EXT` entries with no chained-fixup binding.
#![cfg(target_os = "macos")]

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// `[workspace]` opt-out keeps the test fixture independent of the
/// surrounding wild workspace; otherwise cargo refuses to build it.
const FIXTURE_CARGO: &str = r#"
[workspace]

[package]
name = "wild-on-wild-trigger"
version = "0.0.1"
edition = "2021"

[dependencies]
zstd = "0.13"
"#;

/// `eprintln!` goes through `std::io::stdio::Stderr::lock` —
/// historically the first crash site under the wild-on-wild bug
/// because libstd's stderr Mutex's pthread_mutex pointer landed on
/// strtab bytes. `zstd::stream::raw::Encoder::new(0)` forces the
/// link to keep zstd-sys atoms (the bug's source archive) alive.
const FIXTURE_MAIN: &str = r#"
fn main() {
    eprintln!("wild-on-wild-test alive");
    let _enc = zstd::stream::raw::Encoder::new(0).expect("zstd init");
    std::process::exit(31);
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
            .join("target/release/wild");
    }
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/wild-on-wild-test")
}

fn write_fixture(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), FIXTURE_CARGO).unwrap();
    std::fs::write(root.join("src/main.rs"), FIXTURE_MAIN).unwrap();
}

#[test]
fn wild_links_zstd_dependent_binary_cleanly() {
    if Command::new("cargo").arg("--version").output().is_err() {
        eprintln!("cargo not available — skipping");
        return;
    }

    let wild = wild_binary_path();
    let root = fixture_dir();
    write_fixture(&root);

    let rustflags = format!("-C link-arg=-fuse-ld={}", wild.display());
    let out = Command::new("cargo")
        .current_dir(&root)
        .args(["build", "--release"])
        .env("RUSTFLAGS", &rustflags)
        .output()
        .expect("invoke cargo");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "build via wild failed:\n{text}"
    );

    let bin = root.join("target/release/wild-on-wild-trigger");
    let run = Command::new(&bin).output().expect("run linked binary");
    let code = run.status.code().expect("exit code");
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    assert_eq!(
        code, 31,
        "linked binary exit code — expected 31, got {code}. \
         If non-31, wild emitted orphan N_UNDF|N_EXT entries in the \
         symtab that landed on top of GOT/DATA bytes; the binary \
         likely segfaulted on stderr's first use. Check
         macho_writer.rs::write_exe_symtab's `symtab: filter orphan \
         N_UNDF_EXT` step. Stderr from the run: {stderr:?}"
    );
}
