//! Wild's optional Unix-socket linker daemon.
//!
//! The daemon (`wild --serve <sock>`) holds a long-running process so
//! repeated link invocations from cargo skip exec(), libwild::init,
//! and SDK/TBD warmup. Each accept fork()s a child that runs the link
//! with the client's argv/env/cwd; the child exits, and the parent
//! sends the status back over the socket. Hot incremental links via
//! the daemon should be ~30–40 ms faster than fresh `wild` invocations
//! since we skip the dyld+rust-runtime startup tax on every call.
//!
//! Wire protocol — request (client → server):
//!   `[u32 argc][per arg: u32 len + bytes]`
//!   `[u32 envc][per (k,v): u32 klen + bytes + u32 vlen + bytes]`
//!   `[u32 cwd_len][cwd bytes]`
//!
//! Wire protocol — response (server → client):
//!   `[u32 stderr_len][stderr bytes]`
//!   `[u32 stdout_len][stdout bytes]`
//!   `[i32 exit_code]`
//!
//! All multi-byte integers are little-endian. Strings are not
//! null-terminated; the length prefix tells you how many bytes follow.
//!
//! Concurrency: v1 is a serial accept loop (one client at a time).
//! cargo's parallel link jobs queue up at the socket; that's fine
//! for the perf target since each link is sub-second. v2 can grow to
//! multi-child by tracking outstanding fork()s.

use crate::error::Context as _;
use crate::error::Result;
use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;

/// Marshalled client request — what the daemon needs to reproduce a
/// `wild` invocation on behalf of the caller.
pub struct Request {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// Daemon's reply to a client. `stderr_bytes` and `stdout_bytes` are
/// captured verbatim so the client can replay them to its own
/// terminals — that's the only way rustc's diagnostic forwarding
/// keeps working, since rustc reads the linker's stderr to surface
/// errors back to the user.
pub struct Response {
    pub stderr_bytes: Vec<u8>,
    pub stdout_bytes: Vec<u8>,
    pub exit_code: i32,
}

/// Default socket path. Per-UID under `$TMPDIR` (or `/tmp` fallback)
/// to avoid cross-user collisions. Mode is 0600 — only the owner can
/// connect, so other local users can't hijack the daemon to link as
/// us.
pub fn default_socket_path() -> PathBuf {
    let dir = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let uid = unsafe { libc::getuid() };
    dir.join(format!("wild-{uid}.sock"))
}

/// Read a length-prefixed byte string from `r`. Errors with a clear
/// message on truncated input so a malformed client doesn't look like
/// a generic I/O error. `cap` bounds the allowed length to keep a
/// hostile or buggy peer from making us alloc gigabytes on a u32
/// length read out of garbage bytes.
fn read_lp_bytes<R: Read>(r: &mut R, cap: usize, what: &str) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)
        .with_context(|| format!("daemon: short read on {what} length prefix"))?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > cap {
        crate::bail!("daemon: {what} length {len} exceeds cap {cap}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)
        .with_context(|| format!("daemon: short read on {what} body ({len} bytes)"))?;
    Ok(buf)
}

fn read_lp_string<R: Read>(r: &mut R, cap: usize, what: &str) -> Result<String> {
    let bytes = read_lp_bytes(r, cap, what)?;
    String::from_utf8(bytes)
        .with_context(|| format!("daemon: {what} is not valid UTF-8"))
}

fn write_lp_bytes<W: Write>(w: &mut W, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| crate::error!("daemon: payload exceeds u32::MAX bytes"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

const MAX_ARG_LEN: usize = 64 * 1024; // any single argv element
const MAX_ENV_LEN: usize = 64 * 1024;
const MAX_PATH_LEN: usize = 64 * 1024;
const MAX_ARGS: usize = 1 << 16;
const MAX_ENV_ENTRIES: usize = 1 << 16;
const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024; // safety cap for stderr/stdout

/// Read a [`Request`] from a connected stream.
pub fn read_request<R: Read>(r: &mut R) -> Result<Request> {
    let mut count_buf = [0u8; 4];
    r.read_exact(&mut count_buf)
        .context("daemon: short read on argc")?;
    let argc = u32::from_le_bytes(count_buf) as usize;
    if argc > MAX_ARGS {
        crate::bail!("daemon: argc {argc} exceeds cap {MAX_ARGS}");
    }
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        argv.push(read_lp_string(r, MAX_ARG_LEN, "argv entry")?);
    }

    r.read_exact(&mut count_buf)
        .context("daemon: short read on envc")?;
    let envc = u32::from_le_bytes(count_buf) as usize;
    if envc > MAX_ENV_ENTRIES {
        crate::bail!("daemon: envc {envc} exceeds cap {MAX_ENV_ENTRIES}");
    }
    let mut env = Vec::with_capacity(envc);
    for _ in 0..envc {
        let k = read_lp_string(r, MAX_ENV_LEN, "env key")?;
        let v = read_lp_string(r, MAX_ENV_LEN, "env value")?;
        env.push((k, v));
    }

    let cwd = read_lp_string(r, MAX_PATH_LEN, "cwd")?;
    Ok(Request {
        argv,
        env,
        cwd: PathBuf::from(cwd),
    })
}

/// Write a [`Request`] onto a connected stream.
pub fn write_request<W: Write>(w: &mut W, req: &Request) -> Result<()> {
    let argc = u32::try_from(req.argv.len())
        .map_err(|_| crate::error!("daemon: argv exceeds u32::MAX entries"))?;
    w.write_all(&argc.to_le_bytes())?;
    for arg in &req.argv {
        write_lp_bytes(w, arg.as_bytes())?;
    }
    let envc = u32::try_from(req.env.len())
        .map_err(|_| crate::error!("daemon: env exceeds u32::MAX entries"))?;
    w.write_all(&envc.to_le_bytes())?;
    for (k, v) in &req.env {
        write_lp_bytes(w, k.as_bytes())?;
        write_lp_bytes(w, v.as_bytes())?;
    }
    write_lp_bytes(w, req.cwd.as_os_str().as_encoded_bytes())?;
    w.flush()?;
    Ok(())
}

/// Read a [`Response`] from a connected stream.
pub fn read_response<R: Read>(r: &mut R) -> Result<Response> {
    let stderr_bytes = read_lp_bytes(r, MAX_STREAM_BYTES, "stderr")?;
    let stdout_bytes = read_lp_bytes(r, MAX_STREAM_BYTES, "stdout")?;
    let mut code_buf = [0u8; 4];
    r.read_exact(&mut code_buf)
        .context("daemon: short read on exit code")?;
    Ok(Response {
        stderr_bytes,
        stdout_bytes,
        exit_code: i32::from_le_bytes(code_buf),
    })
}

/// Write a [`Response`] onto a connected stream.
pub fn write_response<W: Write>(w: &mut W, resp: &Response) -> Result<()> {
    write_lp_bytes(w, &resp.stderr_bytes)?;
    write_lp_bytes(w, &resp.stdout_bytes)?;
    w.write_all(&resp.exit_code.to_le_bytes())?;
    w.flush()?;
    Ok(())
}

/// Bind a Unix socket at `socket_path` and serve linker requests in a
/// loop. Returns only on bind failure or fatal accept error — clean
/// shutdown is via SIGTERM/SIGINT (the OS removes the socket file
/// after we exit; we also unlink any stale file at startup).
///
/// Mode is 0600 so other local users can't connect.
pub fn serve(socket_path: &Path) -> Result<()> {
    // Stale-socket cleanup. If a previous daemon crashed, the socket
    // file lingers and `bind()` fails with EADDRINUSE. We try to
    // connect first — if that succeeds, another live daemon owns the
    // path and we refuse to clobber it.
    if socket_path.exists() {
        if UnixStream::connect(socket_path).is_ok() {
            crate::bail!(
                "daemon: another wild --serve appears to own {}; refusing to start",
                socket_path.display()
            );
        }
        let _ = std::fs::remove_file(socket_path);
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("daemon: bind {}", socket_path.display()))?;

    // Tighten permissions: only owner can connect.
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)
        .with_context(|| format!("daemon: chmod {}", socket_path.display()))?;

    eprintln!(
        "wild --serve listening on {} (pid {})",
        socket_path.display(),
        std::process::id()
    );

    loop {
        let (stream, _addr) = listener
            .accept()
            .context("daemon: accept failed")?;
        if let Err(e) = handle_one(stream) {
            // Per-connection errors are non-fatal — log and keep
            // serving so a buggy client can't take down the daemon.
            eprintln!("wild daemon: connection error: {e:?}");
        }
    }
}

/// Serve a single client connection: read request, fork a child that
/// performs the link with the client's context, write back the
/// response. Errors propagate to `serve` for logging.
fn handle_one(stream: UnixStream) -> Result<()> {
    let mut read_stream = stream
        .try_clone()
        .context("daemon: dup connection for read")?;
    let req = read_request(&mut read_stream)?;
    drop(read_stream);

    let resp = run_link_in_child(&req)?;
    let mut write_stream = stream;
    write_response(&mut write_stream, &resp)?;
    Ok(())
}

/// Fork a child that runs the linker with `req`'s argv/env/cwd, with
/// stderr+stdout redirected to pipes the parent reads after the child
/// exits.
///
/// Why fork rather than in-process: libwild reads `std::env::var`,
/// `std::env::current_dir`, and calls `std::process::exit` on some
/// error paths. None of those round-trip cleanly across multiple
/// in-process invocations; fork gives the child a clean
/// environment that's torn down on exit, sidestepping the entire
/// static-state problem. Trade-off: ~5–10 ms fork() cost on macOS.
/// v2 can refactor libwild to thread a context struct through and
/// drop fork.
fn run_link_in_child(req: &Request) -> Result<Response> {
    let mut stderr_pipe = [0; 2];
    let mut stdout_pipe = [0; 2];
    if unsafe { libc::pipe(stderr_pipe.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("daemon: stderr pipe");
    }
    if unsafe { libc::pipe(stdout_pipe.as_mut_ptr()) } != 0 {
        let _ = unsafe { libc::close(stderr_pipe[0]) };
        let _ = unsafe { libc::close(stderr_pipe[1]) };
        return Err(std::io::Error::last_os_error()).context("daemon: stdout pipe");
    }

    // Safety: serve() runs single-threaded (no other threads spawned
    // between accept and fork). The child only uses async-signal-safe
    // syscalls between fork() and the libwild call, then runs the
    // link normally.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let err = std::io::Error::last_os_error();
        for fd in [stderr_pipe[0], stderr_pipe[1], stdout_pipe[0], stdout_pipe[1]] {
            let _ = unsafe { libc::close(fd) };
        }
        return Err(err).context("daemon: fork failed");
    }

    if pid == 0 {
        // Child: redirect, exec, _exit.
        unsafe {
            libc::close(stderr_pipe[0]);
            libc::close(stdout_pipe[0]);
            libc::dup2(stderr_pipe[1], libc::STDERR_FILENO);
            libc::dup2(stdout_pipe[1], libc::STDOUT_FILENO);
            libc::close(stderr_pipe[1]);
            libc::close(stdout_pipe[1]);
        }
        let code = run_link_directly(req).unwrap_or_else(|e| {
            let _ = writeln!(std::io::stderr(), "{e:?}");
            1
        });
        unsafe { libc::_exit(code) };
    }

    // Parent: close write ends, drain reads concurrently while child runs.
    unsafe {
        libc::close(stderr_pipe[1]);
        libc::close(stdout_pipe[1]);
    }

    let stderr_bytes = drain_fd_in_thread(stderr_pipe[0]);
    let stdout_bytes = drain_fd_in_thread(stdout_pipe[0]);

    let mut status: libc::c_int = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    if waited < 0 {
        return Err(std::io::Error::last_os_error()).context("daemon: waitpid");
    }
    let exit_code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    };

    let stderr_bytes = stderr_bytes.join().unwrap_or_default();
    let stdout_bytes = stdout_bytes.join().unwrap_or_default();

    Ok(Response {
        stderr_bytes,
        stdout_bytes,
        exit_code,
    })
}

fn drain_fd_in_thread(fd: libc::c_int) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe {
                libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        unsafe { libc::close(fd) };
        out
    })
}

/// Execute the link in this (forked-child) process using the client
/// supplied argv / env / cwd. We bypass `wild/src/main.rs` and call
/// libwild directly so we can drive [`Args`] from a custom argv
/// closure without hitting the process-level `std::env::args`.
fn run_link_directly(req: &Request) -> Result<i32> {
    // Replace process env with the client's view so libwild's
    // `std::env::var_os` calls (incremental cache flag aliases,
    // SDK overrides, sysroot probes) see what the client saw.
    // Safe in the child because we're single-threaded post-fork.
    for (k, _) in std::env::vars_os() {
        unsafe { std::env::remove_var(k) };
    }
    for (k, v) in &req.env {
        unsafe { std::env::set_var(k, v) };
    }

    if let Err(e) = std::env::set_current_dir(&req.cwd) {
        return Err(e).with_context(|| format!("daemon: chdir {}", req.cwd.display()));
    }

    crate::init_timing()?;

    let argv = req.argv.clone();
    let argv_iter = || argv.iter().cloned();
    let mut args = crate::Args::new(argv_iter)?;
    args.parse(argv_iter)?;
    crate::setup_tracing(&args)?;
    crate::run(args)?;
    Ok(0)
}

/// Client side: connect to `socket_path`, send the current process's
/// argv/env/cwd as a [`Request`], read back the [`Response`], replay
/// stderr/stdout, and return the exit code the caller should pass to
/// `std::process::exit`. Returns `Err` on socket failure — caller
/// should fall back to a direct in-process link rather than failing.
pub fn dispatch_to_daemon(socket_path: &Path) -> Result<i32> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("daemon: connect {}", socket_path.display()))?;

    let argv: Vec<String> = std::env::args().collect();
    let env: Vec<(String, String)> = std::env::vars().collect();
    let cwd = std::env::current_dir().context("daemon: getcwd")?;
    let req = Request { argv, env, cwd };
    write_request(&mut stream, &req)?;

    let resp = read_response(&mut stream)?;
    if !resp.stderr_bytes.is_empty() {
        let _ = std::io::stderr().write_all(&resp.stderr_bytes);
    }
    if !resp.stdout_bytes.is_empty() {
        let _ = std::io::stdout().write_all(&resp.stdout_bytes);
    }
    Ok(resp.exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_round_trip() {
        let req = Request {
            argv: vec!["wild".into(), "-o".into(), "/tmp/out".into()],
            env: vec![("A".into(), "1".into()), ("B".into(), "two".into())],
            cwd: PathBuf::from("/proj"),
        };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let mut c = Cursor::new(&buf);
        let r = read_request(&mut c).unwrap();
        assert_eq!(r.argv, req.argv);
        assert_eq!(r.env, req.env);
        assert_eq!(r.cwd, req.cwd);
    }

    #[test]
    fn response_round_trip() {
        let resp = Response {
            stderr_bytes: b"oops\n".to_vec(),
            stdout_bytes: b"ok\n".to_vec(),
            exit_code: 7,
        };
        let mut buf = Vec::new();
        write_response(&mut buf, &resp).unwrap();
        let mut c = Cursor::new(&buf);
        let r = read_response(&mut c).unwrap();
        assert_eq!(r.stderr_bytes, resp.stderr_bytes);
        assert_eq!(r.stdout_bytes, resp.stdout_bytes);
        assert_eq!(r.exit_code, resp.exit_code);
    }

    #[test]
    fn rejects_argc_overflow() {
        let mut buf = Vec::new();
        // huge argc but no actual data — read_request must reject.
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut c = Cursor::new(&buf);
        assert!(read_request(&mut c).is_err());
    }
}
