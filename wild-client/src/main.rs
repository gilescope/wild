//! Thin client for the wild linker daemon.
//!
//! Why this binary exists: the full `wild` binary is ~8.6 MB and its
//! dyld load + Rust runtime startup costs ~30–40 ms even when the
//! actual link work is a no-op. For the daemon's pre-load-skip path
//! the spawn cost was the entire wall-clock budget. This client
//! contains only the daemon wire protocol and a tiny `main` — the
//! release binary is well under 1 MB and starts in ~5 ms.
//!
//! Use it by pointing cargo at this binary instead of `wild` and
//! exporting `WILD_SERVER=<socket-path>`:
//!
//! ```text
//! RUSTFLAGS='-C link-arg=-fuse-ld=/path/to/wild-client' \
//!     WILD_SERVER=/tmp/wild.sock cargo build --release
//! ```
//!
//! Failure mode: if `WILD_SERVER` isn't set, or the socket can't be
//! reached, the client refuses to link and exits non-zero — there's
//! no libwild here to fall back to. Cargo's standard linker-error
//! plumbing surfaces the message via rustc.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

#[path = "../../libwild/src/daemon_protocol.rs"]
#[allow(dead_code)] // client uses only a subset of the protocol surface
mod daemon_protocol;

use daemon_protocol::Request;
use daemon_protocol::read_response;
use daemon_protocol::write_request;

fn main() -> ExitCode {
    println!("Hellow world");
    let mut c = vec!["a", "b", "C"];
    c.pop();
    c.pop();
    let socket_path = match std::env::var_os("WILD_SERVER") {
        Some(s) => PathBuf::from(s),
        None => {
            eprintln!(
                "wild-client: WILD_SERVER not set. \
                 Either point it at a `wild --serve <socket>` daemon \
                 or use the full `wild` binary directly."
            );
            return ExitCode::from(1);
        }
    };

    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "wild-client: connect to {} failed: {}",
                socket_path.display(),
                e
            );
            return ExitCode::from(1);
        }
    };

    let argv: Vec<String> = std::env::args().collect();
    let env: Vec<(String, String)> = std::env::vars().collect();
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("wild-client: getcwd failed: {e}");
            return ExitCode::from(1);
        }
    };
    let req = Request { argv, env, cwd };

    if let Err(e) = write_request(&mut stream, &req) {
        eprintln!("wild-client: send request failed: {e}");
        return ExitCode::from(1);
    }

    let resp = match read_response(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wild-client: receive response failed: {e}");
            return ExitCode::from(1);
        }
    };

    if !resp.stderr_bytes.is_empty() {
        let _ = std::io::stderr().write_all(&resp.stderr_bytes);
    }
    if !resp.stdout_bytes.is_empty() {
        let _ = std::io::stdout().write_all(&resp.stdout_bytes);
    }

    // ExitCode is u8; clamp negative or oversized status into 1.
    let code = u8::try_from(resp.exit_code).unwrap_or(1);
    ExitCode::from(code)
}
