#[cfg(feature = "mimalloc")]
#[global_allocator]
static MIMALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() {
    // `wild --serve <socket>` enters daemon mode: bind a Unix socket
    // and serve link requests in a fork-per-connection loop. We
    // dispatch *before* libwild::init_timing / Args::parse so the
    // server's own startup isn't billed to the first client.
    #[cfg(unix)]
    if let Some(socket_path) = parse_serve_arg() {
        if let Err(error) = libwild::daemon::serve(&socket_path) {
            libwild::error::report_error_and_exit(&error);
        }
        return;
    }

    // `WILD_SERVER=<socket>` enters client mode: ship our argv/env/cwd
    // to the daemon and replay its captured stderr/stdout. On socket
    // failure we fall through to a direct in-process link so a stale
    // env var (e.g. daemon stopped between cargo invocations) doesn't
    // break the build.
    #[cfg(unix)]
    if let Some(socket_path) = std::env::var_os("WILD_SERVER") {
        let path = std::path::PathBuf::from(socket_path);
        match libwild::daemon::dispatch_to_daemon(&path) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!(
                    "wild: WILD_SERVER={} unreachable ({:?}); falling back to direct link",
                    path.display(),
                    e
                );
                // Drop into the normal direct-link path below.
            }
        }
    }

    if let Err(error) = run() {
        libwild::error::report_error_and_exit(&error)
    }
}

/// The current Wild version as written by build.rs.
const VERSION: &str = include_str!(concat!(env!("OUT_DIR"), "/version.txt"));

/// Returns `Some(path)` when argv contains `--serve <path>` or
/// `--serve=<path>`. We scan manually rather than going through
/// `Args::parse` so daemon mode doesn't pay the cost of full link-arg
/// parsing — and so flags like `-L`, which depend on a real link
/// context, can't error out a daemon-startup invocation.
#[cfg(unix)]
fn parse_serve_arg() -> Option<std::path::PathBuf> {
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix("--serve=") {
            return Some(std::path::PathBuf::from(rest));
        }
        if arg == "--serve" {
            return iter.next().map(std::path::PathBuf::from);
        }
    }
    None
}

fn run() -> libwild::error::Result {
    #[cfg(feature = "dhat")]
    let _profiler = dhat::Profiler::new_heap();

    libwild::init_timing()?;

    // Incremental whole-link skip — runs BEFORE `Args::parse` so
    // we skip the ~274 ms spent walking `-L`, resolving `-l`, and
    // probing the SDK. If the cache side-car alongside the output
    // agrees on (argv hash, wild version, per-input fingerprints,
    // output size), the existing output binary is already the
    // correct link result.
    //
    // Gated on `WILD_INCREMENTAL_DEBUG=1`; no-op when unset.
    if let Some(output) = libwild::try_early_skip_from_argv() {
        libwild::bump_output_path_mtime(&output);
        return Ok(());
    }

    let mut args = libwild::Args::new(std::env::args)?;
    args.set_version(VERSION);
    args.parse(std::env::args)?;

    if libwild::should_fork(&args) {
        // Safety: We haven't spawned any threads yet.
        unsafe { libwild::run_in_subprocess(args) };
    } else {
        // Run the linker in this process without forking.

        // Note, we need to setup tracing before worker, otherwise the threads won't contribute to
        // counters such as --time=cycles,instructions etc.
        libwild::setup_tracing(&args)?;

        libwild::run(args)
    }
}
