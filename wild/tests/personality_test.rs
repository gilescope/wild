//! Regression test for the CIE-personality reloc-scan bug.
//!
//! Pre-fix: wild's `__eh_frame` reloc scan in
//! `MachO::load_object_section_relocations` only matched
//! `ARM64_RELOC_POINTER_TO_GOT` (r_type=7, r_length=2, pcrel,
//! extern). rustc emits the personality reference as a
//! SUBTRACTOR + UNSIGNED pair (`_rust_eh_personality - ltmp18`,
//! r_length=3, !pcrel) — neither half matched, so the personality
//! symbol got no resolution, the field was left with unrelocated
//! input bytes, and `is_plausible_got_vm` filtered out the bogus
//! value. Net effect: `__unwind_info` had no personality entry for
//! the affected FDEs and `catch_unwind` boundaries inside the
//! object couldn't run their personality routine.
//!
//! This test forces a `catch_unwind` across a wild-linked binary's
//! frame; failure mode pre-fix was either an abort during unwind or
//! a "panic propagated through a frame without personality" message.
//! Post-fix the panic is caught cleanly and the test exits 23.
#![cfg(target_os = "macos")]

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const FIXTURE_CARGO: &str = r#"
[workspace]

[package]
name = "personality-test-fixture"
version = "0.0.1"
edition = "2021"
"#;

/// `catch_unwind` requires the personality routine to run during
/// the unwind so the panic is captured rather than aborting the
/// process. If wild's CIE personality reloc handling is broken,
/// the panic either aborts or escapes the catch.
const FIXTURE_MAIN: &str = r#"
fn main() {
    let r = std::panic::catch_unwind(|| {
        panic!("expected panic — must be caught");
    });
    assert!(r.is_err(), "catch_unwind didn't catch the panic — \
                         personality unwind is broken");
    std::process::exit(23);
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
        .join("target/personality-test")
}

fn write_fixture(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), FIXTURE_CARGO).unwrap();
    std::fs::write(root.join("src/main.rs"), FIXTURE_MAIN).unwrap();
}

#[test]
fn cie_personality_reloc_resolves_correctly() {
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
    assert!(out.status.success(), "build via wild failed:\n{text}");

    let bin = root.join("target/release/personality-test-fixture");
    let run = Command::new(&bin).output().expect("run linked binary");
    let code = run.status.code().expect("exit code");
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    assert_eq!(
        code, 23,
        "linked binary exit code — expected 23, got {code}. \
         If non-23, wild's CIE personality reloc handling is broken: \
         either the SUBTRACTOR + UNSIGNED scan in \
         load_object_section_relocations regressed, or the writer \
         stopped applying the pair correctly. Stderr: {stderr:?}"
    );
}
