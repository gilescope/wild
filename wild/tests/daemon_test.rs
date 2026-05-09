//! Daemon-mode end-to-end test.
//!
//! Spawns `wild --serve <sock>`, links a hello-world rust binary via
//! the daemon (using `WILD_SERVER=<sock>`), runs the resulting binary
//! and asserts its exit code. If the daemon's protocol or fork-and-
//! relay glue regresses (e.g. exit code lost, stderr framing wrong,
//! cwd not honoured), this catches it without needing a full bevy
//! benchmark fixture.
//!
//! Skips cleanly when cargo / rustc aren't available — keeps the
//! test suite portable across CI hosts that don't have a Rust
//! toolchain pre-installed.
#![cfg(target_os = "macos")]

use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

const HELLO_CARGO: &str = r#"
[workspace]

[package]
name = "daemon-hello"
version = "0.1.0"
edition = "2021"
"#;

/// Exits 17 so the test can distinguish "binary ran, prints right
/// thing" from "binary exists with default exit". Prints to stdout
/// because the daemon also forwards client stdout — exercising both
/// the exit-code and the stdout-relay paths in one run.
const HELLO_MAIN: &str = r#"
fn main() {
    println!("daemon-hello-ok");
    std::process::exit(17);
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

fn fixture_dir(tag: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(format!("target/daemon-test-{tag}"))
}

fn write_hello(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), HELLO_CARGO).unwrap();
    std::fs::write(root.join("src/main.rs"), HELLO_MAIN).unwrap();
}

struct DaemonGuard {
    child: Child,
    socket: PathBuf,
}

impl DaemonGuard {
    fn start(wild: &Path, socket: PathBuf) -> Option<Self> {
        Self::start_with_env(wild, socket, &[])
    }

    fn start_in_process(wild: &Path, socket: PathBuf) -> Option<Self> {
        Self::start_with_env(wild, socket, &[("WILD_DAEMON_INPROCESS", "1")])
    }

    fn start_with_env(wild: &Path, socket: PathBuf, extra_env: &[(&str, &str)]) -> Option<Self> {
        let _ = std::fs::remove_file(&socket);
        let mut cmd = Command::new(wild);
        cmd.arg("--serve").arg(&socket);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().ok()?;

        // Wait for socket to appear, up to ~3 s. The daemon prints a
        // banner to stderr before listen() — but since we don't
        // capture stderr (let it stream so failures are visible),
        // poll the filesystem instead.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if !socket.exists() {
            return None;
        }
        Some(Self { child, socket })
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Fork-based daemon: client → daemon → fork → link → return.
#[test]
fn daemon_links_hello_world() {
    run_link_via_daemon(false);
}

/// In-process daemon (`WILD_DAEMON_INPROCESS=1`): client → daemon →
/// in-process link in a scoped rayon pool → return. Catches the
/// re-entrancy regression where a global rayon-pool wedge from
/// link N would hang link N+1.
#[test]
fn in_process_daemon_links_hello_world() {
    run_link_via_daemon(true);
}

/// Burnin: send 5 sequential link requests to the same in-process
/// daemon. Pre-rayon-scope-fix, link #2 onwards would hang on
/// `verify_cached_inputs_unchanged`'s `par_iter` because the global
/// pool was wedged. This test bypasses cargo and speaks the daemon
/// protocol directly so the second request actually re-enters
/// libwild's link path (cargo would early-exit if rustflags were
/// unchanged across calls).
#[test]
fn in_process_daemon_handles_multiple_calls() {
    if Command::new("cargo").arg("--version").output().is_err() {
        eprintln!("cargo not available — skipping");
        return;
    }

    let wild = wild_binary_path();
    let root = fixture_dir("burnin");
    write_hello(&root);

    let socket = std::env::temp_dir().join(format!(
        "wild-daemon-test-{}-burnin.sock",
        std::process::id()
    ));
    let _guard = match DaemonGuard::start_in_process(&wild, socket.clone()) {
        Some(g) => g,
        None => {
            eprintln!("daemon failed to start at {} — skipping", socket.display());
            return;
        }
    };

    let rustflags = format!(
        "-C link-arg=-fuse-ld={} --cfg daemon_test_burnin",
        wild.display()
    );

    // 5 cargo invocations, each touching main.rs to invalidate cargo's
    // own incrementality so wild actually re-runs. Each iteration
    // changes the printed string + exit code so a stale binary would
    // be detected as a wrong assertion below.
    for iter in 0..5_u32 {
        let main = format!(
            r#"
fn main() {{
    println!("daemon-hello-ok-{iter}");
    std::process::exit(17 + {iter} as i32);
}}
"#
        );
        std::fs::write(root.join("src/main.rs"), main).unwrap();

        let out = Command::new("cargo")
            .current_dir(&root)
            .args(["build", "--release"])
            .env("RUSTFLAGS", &rustflags)
            .env("WILD_SERVER", _guard.socket.as_os_str())
            .output()
            .expect("invoke cargo");
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        assert!(
            out.status.success(),
            "iter {iter} build via in-process daemon failed:\n{text}"
        );

        let bin = root.join("target/release/daemon-hello");
        let run = Command::new(&bin).output().expect("run linked binary");
        let stdout = String::from_utf8_lossy(&run.stdout).trim().to_string();
        let code = run.status.code().expect("exit code");
        assert_eq!(stdout, format!("daemon-hello-ok-{iter}"));
        assert_eq!(code, 17 + iter as i32);
    }
}

fn run_link_via_daemon(in_process: bool) {
    if Command::new("cargo").arg("--version").output().is_err() {
        eprintln!("cargo not available — skipping daemon test");
        return;
    }

    // PID-suffixed + mode-suffixed socket path so parallel runs and
    // both modes don't collide on the same path.
    let mode_tag = if in_process { "inproc" } else { "fork" };
    let wild = wild_binary_path();
    let root = fixture_dir(mode_tag);
    write_hello(&root);
    let socket = std::env::temp_dir().join(format!(
        "wild-daemon-test-{}-{}.sock",
        std::process::id(),
        mode_tag
    ));
    let guard = if in_process {
        DaemonGuard::start_in_process(&wild, socket.clone())
    } else {
        DaemonGuard::start(&wild, socket.clone())
    };
    let _guard = match guard {
        Some(g) => g,
        None => {
            eprintln!("daemon failed to start at {} — skipping", socket.display());
            return;
        }
    };

    let rustflags = format!("-C link-arg=-fuse-ld={} --cfg daemon_test", wild.display());
    let out = Command::new("cargo")
        .current_dir(&root)
        .args(["build", "--release"])
        .env("RUSTFLAGS", &rustflags)
        .env("WILD_SERVER", _guard.socket.as_os_str())
        .output()
        .expect("invoke cargo");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "build via {mode_tag} daemon failed:\n{text}"
    );

    let bin = root.join("target/release/daemon-hello");
    let run = Command::new(&bin).output().expect("run linked binary");
    let stdout = String::from_utf8_lossy(&run.stdout).trim().to_string();
    let code = run.status.code().expect("exit code");
    assert_eq!(stdout, "daemon-hello-ok");
    assert_eq!(code, 17);
}
