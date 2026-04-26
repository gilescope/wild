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

use crate::daemon_protocol::Request;
use crate::daemon_protocol::Response;
use crate::daemon_protocol::read_request;
use crate::daemon_protocol::read_response;
use crate::daemon_protocol::write_request;
use crate::daemon_protocol::write_response;
use crate::error::Context as _;
use crate::error::Result;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;

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

/// Serve a single client connection: read the request, dispatch to
/// either the in-process or fork-per-request worker depending on the
/// `WILD_DAEMON_INPROCESS` env var, and write back the response.
fn handle_one(stream: UnixStream) -> Result<()> {
    let mut read_stream = stream
        .try_clone()
        .context("daemon: dup connection for read")?;
    let req = read_request(&mut read_stream)?;
    drop(read_stream);

    // `WILD_DAEMON_INPROCESS=1` keeps each link in the daemon's own
    // process. Each request gets a freshly-built rayon ThreadPool
    // (avoiding the global-pool wedge that would otherwise hang
    // call 2). Stderr/stdout are dup2'd over a pipe pair and read
    // back into the response. Default stays fork-based for
    // belt-and-braces isolation; the in-process path is now
    // correct under burnin (40+ consecutive calls) but still pays
    // a per-link rayon-pool spin-up that fork avoids implicitly.
    let in_process = std::env::var_os("WILD_DAEMON_INPROCESS")
        .as_deref()
        == Some(std::ffi::OsStr::new("1"));
    let resp = if in_process {
        run_link_in_process(&req)?
    } else {
        run_link_in_child(&req)?
    };
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

    // `init_timing` / `setup_tracing` are one-shot inits — they
    // install global subscribers that can't be torn down. In daemon
    // in-process mode the second+ link sees `AlreadyInitialised`,
    // which is not a real failure for our purposes: the original
    // subscriber from link #1 keeps working. Silently ignore so the
    // daemon survives sustained load.
    let _ = crate::init_timing();

    let argv = req.argv.clone();
    let argv_iter = || argv.iter().cloned();
    let mut args = crate::Args::new(argv_iter)?;
    args.parse(argv_iter)?;
    let _ = crate::setup_tracing(&args);

    // In-process daemon mode builds a fresh rayon ThreadPool for
    // every request and runs the link inside `pool.install`. The
    // pool drops on return so each request gets clean rayon state.
    //
    // We tried sharing one pool across requests for ~1 ms savings;
    // it re-introduces the same multi-call wedge that v3 fixed —
    // the burnin test (`in_process_daemon_handles_multiple_calls`)
    // hangs on iteration 2. Per-request build is cheap enough
    // (~1 ms) that the safety is worth it.
    let n_threads = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(n_threads)
        .build()
        .map_err(|e| crate::error!("daemon: rayon pool build failed: {e}"))?;

    let result: crate::error::Result<()> = pool.install(|| crate::run(args));
    result?;
    Ok(0)
}

/// In-process worker. Runs the link in the daemon's own process,
/// capturing stderr/stdout via dup2'd pipes that the same thread
/// drains after the link returns. Wraps the libwild call in
/// [`std::panic::catch_unwind`] so a panic from a buggy link bubbles
/// up as exit code 101 instead of taking down the daemon.
///
/// Re-entrancy: between calls we reset env+cwd and re-parse args. We
/// rely on libwild's own per-link state reset (`parse_skip::reset_stats`
/// in `link_for_arch`, `tier3_skip::set(None)` after writer return)
/// for the dynamic state. Any static state that's *valid* to share
/// across links (SDK root, parsed TBD bundle, sysroot probe results)
/// stays warm — that's the whole point of in-process mode.
///
/// Capture trick: we save STDERR/STDOUT FD copies, dup2 our pipe
/// write-ends over them, run the link, restore on return, then read
/// the pipes drained sync. Because the daemon's accept loop is
/// serial, no other thread is using the std handles during the
/// redirect — this is safe.
fn run_link_in_process(req: &Request) -> Result<Response> {
    let mut stderr_pipe = [0; 2];
    let mut stdout_pipe = [0; 2];
    if unsafe { libc::pipe(stderr_pipe.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("daemon-inproc: stderr pipe");
    }
    if unsafe { libc::pipe(stdout_pipe.as_mut_ptr()) } != 0 {
        let _ = unsafe { libc::close(stderr_pipe[0]) };
        let _ = unsafe { libc::close(stderr_pipe[1]) };
        return Err(std::io::Error::last_os_error()).context("daemon-inproc: stdout pipe");
    }

    // Save the daemon's own stderr/stdout so we can restore them.
    let saved_stderr = unsafe { libc::dup(libc::STDERR_FILENO) };
    let saved_stdout = unsafe { libc::dup(libc::STDOUT_FILENO) };

    // Make the pipe read ends non-blocking so the post-link drain
    // doesn't deadlock waiting for an EOF that never comes (the
    // write end stays open via STDERR_FILENO/STDOUT_FILENO until we
    // restore it after the link returns).
    unsafe {
        libc::dup2(stderr_pipe[1], libc::STDERR_FILENO);
        libc::dup2(stdout_pipe[1], libc::STDOUT_FILENO);
        libc::close(stderr_pipe[1]);
        libc::close(stdout_pipe[1]);
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_link_directly(req)
    }));

    // Restore daemon's stderr/stdout BEFORE draining so any
    // diagnostics from `drain_pipe` itself aren't lost into our
    // captured buffers.
    unsafe {
        libc::dup2(saved_stderr, libc::STDERR_FILENO);
        libc::dup2(saved_stdout, libc::STDOUT_FILENO);
        libc::close(saved_stderr);
        libc::close(saved_stdout);
    }

    // Drain the pipes with a short timeout so a background fd holder
    // (notably tracing's global subscriber from a prior link, which
    // caches a `dup(2)` of the redirected stderr) can't deadlock us
    // by keeping the pipe write end open indefinitely.
    let stderr_bytes = drain_fd_with_timeout(stderr_pipe[0]);
    let stdout_bytes = drain_fd_with_timeout(stdout_pipe[0]);

    let exit_code = match result {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => {
            // libwild error — surface the message in stderr so the
            // caller (rustc → cargo) can show it, then return 1.
            let _ = std::io::stderr().write_all(format!("{e:?}\n").as_bytes());
            1
        }
        Err(_) => 101, // panic
    };

    Ok(Response {
        stderr_bytes,
        stdout_bytes,
        exit_code,
    })
}

/// Drain a pipe read-fd until either EOF or 50 ms of no data. The
/// timeout is the safety net for the hang we hit in earlier in-process
/// burnins: tracing's global subscriber, installed during link N,
/// holds an internal `dup(2)` of the then-redirected stderr fd. After
/// we restore the daemon's terminal stderr, tracing still holds a
/// reference to the pipe write end — `read()` on the pipe never sees
/// EOF and a naive blocking drain hangs indefinitely. With a poll
/// timeout, we settle for "drained whatever was produced + a short
/// quiescence period" instead.
fn drain_fd_with_timeout(fd: libc::c_int) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, 50) };
        if r <= 0 {
            // 0 = timeout, <0 = error. Either way, stop draining.
            break;
        }
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    unsafe { libc::close(fd) };
    out
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
