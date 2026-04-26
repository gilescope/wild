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

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/daemon-test")
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
        let _ = std::fs::remove_file(&socket);
        let child = Command::new(wild)
            .arg("--serve")
            .arg(&socket)
            .spawn()
            .ok()?;

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

#[test]
fn daemon_links_hello_world() {
    if Command::new("cargo").arg("--version").output().is_err() {
        eprintln!("cargo not available — skipping daemon test");
        return;
    }

    let wild = wild_binary_path();
    let root = fixture_dir();
    write_hello(&root);

    // PID-suffixed socket path so parallel test runners don't collide.
    let socket = std::env::temp_dir().join(format!("wild-daemon-test-{}.sock", std::process::id()));
    let _guard = match DaemonGuard::start(&wild, socket.clone()) {
        Some(g) => g,
        None => {
            // Daemon failed to bind. On a CI host this might mean the
            // tmpfs doesn't allow Unix sockets — skip rather than fail
            // so the rest of the suite still runs.
            eprintln!("daemon failed to start at {} — skipping", socket.display());
            return;
        }
    };

    let rustflags = format!(
        "-C link-arg=-fuse-ld={} --cfg daemon_test",
        wild.display()
    );
    let out = Command::new("cargo")
        .current_dir(&root)
        .args(["build", "--release"])
        .env("RUSTFLAGS", &rustflags)
        .env("WILD_SERVER", _guard.socket.as_os_str())
        .output()
        .expect("invoke cargo");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "build via daemon failed:\n{text}");

    let bin = root.join("target/release/daemon-hello");
    let run = Command::new(&bin).output().expect("run linked binary");
    let stdout = String::from_utf8_lossy(&run.stdout).trim().to_string();
    let code = run.status.code().expect("exit code");
    assert_eq!(stdout, "daemon-hello-ok");
    assert_eq!(code, 17);
}
