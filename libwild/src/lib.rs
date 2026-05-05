pub(crate) mod alignment;
pub use args::Args;
pub(crate) mod arch;
pub(crate) mod archive;
pub mod args;
#[cfg(unix)]
pub mod daemon;
pub mod daemon_protocol;
pub(crate) mod debug_trace;
pub(crate) mod diagnostics;
pub(crate) mod diff;
pub(crate) mod dwarf_address_info;
pub(crate) mod eh_frame;
pub(crate) mod elf;
pub(crate) mod elf_aarch64;
pub(crate) mod elf_abbrev_dedup;
pub(crate) mod elf_compress;
pub(crate) mod elf_line_v5;
pub(crate) mod elf_loongarch64;
pub(crate) mod elf_riscv64;
pub(crate) mod elf_writer;
pub(crate) mod elf_x86_64;
pub mod error;
pub(crate) mod export_list;
pub(crate) mod expression_eval;
pub(crate) mod file_kind;
pub(crate) mod file_writer;
pub(crate) mod fs;
pub(crate) mod gc_stats;
pub(crate) mod glob_match;
pub(crate) mod grouping;
pub(crate) mod hash;
pub(crate) mod incremental_cache;
pub(crate) mod input_data;
pub(crate) mod layout;
pub(crate) mod layout_rules;
pub(crate) mod layout_snapshot;
pub(crate) mod sdk_cache;
pub(crate) mod suffix_share;
// The ELF Gold-plugin LTO code lives physically under `lto/` as part
// of the LtoDriver family (see `wild-lto-plan.md`). The `mod
// linker_plugins` alias is kept so existing callers continue to use
// `crate::linker_plugins::…`; a follow-up commit mass-renames them.
#[cfg_attr(feature = "plugins", path = "lto/elf_gold.rs")]
#[cfg_attr(not(feature = "plugins"), path = "lto/elf_gold_disabled.rs")]
mod linker_plugins;
pub(crate) mod linker_script;
pub mod llvm_tools;
pub(crate) mod lto;
pub(crate) mod macho;
pub(crate) mod macho_aarch64;
pub(crate) mod macho_codesign;
#[cfg(feature = "macho-lto")]
pub(crate) mod macho_lto;
pub(crate) mod macho_writer;
pub(crate) mod output_kind;
pub(crate) mod output_section_id;
pub(crate) mod output_section_map;
pub(crate) mod output_section_part_map;
pub(crate) mod output_trace;
pub(crate) mod parse_skip;
pub(crate) mod parsed_input_cache;
pub(crate) mod tier3_skip;
pub(crate) mod parsing;
pub(crate) mod part_id;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) mod perf;
#[cfg(any(
    not(target_os = "linux"),
    all(
        target_os = "linux",
        any(target_arch = "riscv64", target_arch = "loongarch64")
    )
))]
#[path = "perf_unsupported.rs"]
pub(crate) mod perf;
pub(crate) mod platform;
pub(crate) mod program_segments;
pub(crate) mod resolution;
pub(crate) mod save_dir;
pub(crate) mod sframe;
pub(crate) mod sharding;
pub(crate) mod string_merging;
#[cfg(all(feature = "fork", unix))]
pub(crate) mod subprocess;
#[cfg(not(all(feature = "fork", unix)))]
#[path = "subprocess_unsupported.rs"]
pub(crate) mod subprocess;
pub(crate) mod symbol;
pub(crate) mod symbol_db;
#[cfg(all(test, not(target_family = "wasm")))]
mod tidy_tests;
pub(crate) mod timing;
pub(crate) mod validation;
pub(crate) mod value_flags;
pub(crate) mod verification;
pub(crate) mod version_script;
pub(crate) mod wasm;
pub(crate) mod wasm_arch;
pub(crate) mod wasm_writer;

use crate::elf::Elf;
use crate::error::Context;
use crate::error::Result;
use crate::layout_rules::LayoutRulesBuilder;
use crate::macho::MachO;
use crate::output_kind::OutputKind;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::Platform;
use crate::value_flags::PerSymbolFlags;
use crate::version_script::VersionScript;
use colosseum::sync::Arena;
use crossbeam_utils::atomic::AtomicCell;
use error::AlreadyInitialised;
use input_data::FileLoader;
use input_data::InputFile;
use input_data::InputLinkerScript;
use layout_rules::LayoutRules;
use output_section_id::OutputSections;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
pub use subprocess::run_in_subprocess;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Runs the linker and cleans up associated resources. Only use this function if you've OK with
/// waiting for cleanup.
pub fn run(mut args: Args) -> error::Result {
    let thread_pool = args.common_mut().activate_thread_pool()?;
    let linker = Linker::new();
    linker.run(&args, &thread_pool)?;
    drop(linker);
    timing::finalise_perfetto_trace()?;
    Ok(())
}

/// Super-early skip check — called from `main()` BEFORE
/// `Args::parse` and BEFORE the fork dispatch. Only reads `argv`
/// and the cache side-car; does no library-path resolution.
///
/// On a rust-analyzer link the full arg parser takes ~274 ms
/// (walks `-L`, resolves every `-l`, probes the SDK). This
/// function deliberately bypasses all of that — on a cache hit
/// the skip cost is dominated by the 229-path fingerprint verify,
/// not by arg parsing.
///
/// Gated on `WILD_INCREMENTAL_DEBUG=1`; with the env var unset,
/// returns `false` without reading any files.
pub fn try_early_skip_from_argv() -> Option<std::path::PathBuf> {
    if std::env::var_os("WILD_INCREMENTAL_DEBUG").is_none() {
        return None;
    }
    if std::env::var_os("WILD_INCREMENTAL_NO_EARLY_SKIP").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        return None;
    }
    let argv: Vec<String> = std::env::args().collect();
    let output = incremental_cache::extract_output_path(&argv);
    let args_hash = incremental_cache::compute_args_hash(&argv);
    let hashes_path = incremental_cache::hashes_path_for_output(&output);
    let Some(cached) = incremental_cache::read_link_cache(&hashes_path) else {
        return None;
    };
    if cached.wild_version != incremental_cache::WILD_VERSION || cached.args_hash != args_hash {
        return None;
    }
    if incremental_cache::verify_cached_inputs_unchanged(&cached.inputs).is_none() {
        return None;
    }
    match std::fs::metadata(&output) {
        Ok(m) if m.len() == cached.output_size => {
            eprintln!(
                "wild incremental: EARLY SKIP (pre-argparse) — output at {} \
                 reused",
                output.display()
            );
            Some(output)
        }
        _ => None,
    }
}

/// Keep the old signature-check for callers that already have a
/// parsed [`Args`] — used by the post-load defence-in-depth path.
pub fn try_early_skip(args: &Args) -> bool {
    if std::env::var_os("WILD_INCREMENTAL_DEBUG").is_none() {
        return false;
    }
    if std::env::var_os("WILD_INCREMENTAL_NO_EARLY_SKIP").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        return false;
    }
    early_skip_impl(args)
}

/// Update the output's mtime so build systems (cargo, make) that
/// look at timestamps see the file as freshly produced by this
/// invocation. Equivalent to `touch -c -a -m <output>`.
///
/// Non-fatal — failure is silently swallowed. If the mtime doesn't
/// update, the next build may see the output as stale and trigger
/// a real relink, which falls through to the cold path. That's a
/// correctness-preserving downgrade (we'd re-link unnecessarily),
/// not a correctness bug.
pub fn bump_output_mtime(args: &Args) {
    bump_output_path_mtime(args.output_path());
}

/// Path-taking variant of [`bump_output_mtime`] — used from the
/// pre-argparse skip where we only have an output path, no parsed
/// `Args`.
pub fn bump_output_path_mtime(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        // SAFETY: `cpath` is a valid nul-terminated C string;
        // `utimensat(…, NULL, 0)` is a POSIX-defined way to set
        // atime+mtime to "now" with no side effects on failure.
        unsafe {
            libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), std::ptr::null(), 0);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn early_skip_impl(args: &Args) -> bool {
    let argv: Vec<String> = std::env::args().collect();
    let args_hash = incremental_cache::compute_args_hash(&argv);
    let hashes_path = incremental_cache::hashes_path_for_output(args.output_path());
    let Some(cached) = incremental_cache::read_link_cache(&hashes_path) else {
        eprintln!(
            "wild incremental: early skip: no cache at {}",
            hashes_path.display()
        );
        return false;
    };
    if cached.wild_version != incremental_cache::WILD_VERSION {
        eprintln!(
            "wild incremental: early skip: wild version mismatch (cached {} vs {})",
            cached.wild_version,
            incremental_cache::WILD_VERSION
        );
        return false;
    }
    if cached.args_hash != args_hash {
        eprintln!("wild incremental: early skip: args_hash mismatch");
        return false;
    }
    if incremental_cache::verify_cached_inputs_unchanged(&cached.inputs).is_none() {
        eprintln!("wild incremental: early skip: input fingerprint mismatch");
        return false;
    }
    match std::fs::metadata(args.output_path()) {
        Ok(m) if m.len() == cached.output_size => {
            eprintln!(
                "wild incremental: EARLY SKIP — output at {} reused, \
                 thread pool / linker arenas bypassed",
                args.output_path().display()
            );
            true
        }
        Ok(m) => {
            eprintln!(
                "wild incremental: early skip: output size mismatch ({} vs cached {})",
                m.len(),
                cached.output_size
            );
            false
        }
        Err(e) => {
            eprintln!("wild incremental: early skip: output stat failed: {e}");
            false
        }
    }
}

/// Sets up whatever tracing, if any, is indicated by the supplied arguments. This can only be
/// called once and only if nothing else has already set the global tracing dispatcher. Calling this
/// is optional. If it isn't called, no tracing-based features will function. e.g. --time.
pub fn setup_tracing(args: &Args) -> Result<(), AlreadyInitialised> {
    if let Some(opts) = args.common().time_phase_options.as_ref() {
        timing::init_tracing(opts)
    } else if args.common().print_allocations.is_some() {
        debug_trace::init()
    } else {
        tracing_subscriber::registry()
            .with(fmt::layer())
            .with(EnvFilter::from_default_env())
            .try_init()
            .map_err(|_| AlreadyInitialised)
    }
}

/// This is effectively a data store for use while linking. It takes ownership of all the input data
/// that we read, which allows the linking stages to borrow that data. Dropping this struct might be
/// expensive, so the caller of the linker might want to think about when best to drop it - probably
/// together with the `LinkerOutput`. Note, calling `exit` without dropping this struct is an
/// option, but likely won't save any time, since the bulk of the work done during drop (unmapping
/// pages) will still happen anyway.
pub struct Linker {
    /// We store our input files here once we've read them.
    inputs_arena: Arena<InputFile>,

    linker_plugin_arena: Arena<linker_plugins::LoadedPlugin>,

    /// Anything that doesn't need a custom Drop implementation can go in here. In practice, it's
    /// mostly just the decompressed copy of compressed string-merge sections.
    herd: bumpalo_herd::Herd,

    /// We'll fill this in when we're done linking and start shutting down. Once this is dropped,
    /// that signals the end of shutdown for the purposes of timing measurement.
    #[allow(dyn_drop)]
    shutdown_scope: AtomicCell<Vec<Box<dyn Drop>>>,

    /// A timing scope that exists for the whole time we're linking.
    #[allow(dyn_drop)]
    _link_scope: Vec<Box<dyn Drop>>,
}

pub struct LinkerOutput<'layout_inputs> {
    #[allow(dyn_drop)]
    /// This is just here so that we defer its destruction. This allows us to (a) measure how long
    /// it takes to drop and (b) if we forked, signal our parent that we're done, then drop it in
    /// the background.
    layout: Option<Box<dyn Drop + 'layout_inputs>>,
}

impl Linker {
    pub fn new() -> Self {
        let (guard_a, guard_b) = timing_guard!("Link");

        Self {
            inputs_arena: Arena::new(),
            linker_plugin_arena: Arena::new(),
            herd: Default::default(),
            shutdown_scope: Default::default(),
            _link_scope: vec![Box::new(guard_a), Box::new(guard_b)],
        }
    }

    /// Runs the linker. The returned value isn't useful for anything, but is somewhat expensive to
    /// drop, so we leave it up to the caller to decide when to drop it. At the point at which we
    /// return, the output file should be usable.
    pub fn run<'layout_inputs>(
        &'layout_inputs self,
        args: &'layout_inputs Args,
        // We don't actually use this, but take it as an argument to ensure that the caller has
        // created it. We may decide to actually use it in future, if we stop using rayon's global
        // thread pool.
        _thread_pool: &crate::args::ThreadPool,
    ) -> error::Result<LinkerOutput<'layout_inputs>> {
        let identity = args.common().linker_identity();
        match args.common().version_mode {
            args::VersionMode::ExitAfterPrint => {
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "{identity}")?;
                return Ok(LinkerOutput { layout: None });
            }
            args::VersionMode::Verbose => {
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "{identity}")?;
                // Continue linking
            }
            args::VersionMode::None => {
                // Don't print version
            }
        }

        match args {
            Args::Elf(elf_args) => Elf::link_for_arch(self, elf_args),
            Args::MachO(macho_args) => MachO::link_for_arch(self, macho_args),
            Args::Wasm(wasm_args) => wasm::Wasm::link_for_arch(self, wasm_args),
        }
    }

    fn link_for_arch<'data, P: Platform, A: Arch<Platform = P>>(
        &'data self,
        args: &'data P::Args,
    ) -> error::Result<LinkerOutput<'data>> {
        let mut file_loader = input_data::FileLoader::new(&self.inputs_arena);

        // Zero the tier-1 parse-skip counters so the end-of-link
        // summary reflects only this link. Callers that drive
        // `Linker::run` multiple times in-process get per-link
        // granularity rather than cumulative totals.
        parse_skip::reset_stats();

        // Note, we propagate errors from `link_with_input_data` after we've checked if any files
        // changed. We want inputs-changed errors to take precedence over all other errors.
        let result = self.load_inputs_and_link::<P, A>(&mut file_loader, args);

        file_loader.verify_inputs_unchanged()?;

        // Incremental link — persist the signature + input hashes
        // for the next link. Fires whenever the user opted into a
        // cache-writing mode (`--incremental-cache=write|read-write`,
        // or the legacy `WILD_INCREMENTAL_DEBUG=1` env var). On
        // skip-paths the prior cache is already current, so we only
        // persist on a full-link path.
        let should_persist = result.is_ok()
            && (args.common().incremental_cache.writes_cache()
                || std::env::var_os("WILD_INCREMENTAL_DEBUG").is_some());
        if should_persist {
            persist_link_cache::<P>(&file_loader, args);
        }

        // Tier-1 parse-skip telemetry: terse one-liner summarising
        // how many inputs replayed vs re-parsed vs wrote cache
        // entries. Gated on WILD_INCREMENTAL_DEBUG=1 so normal
        // links stay silent.
        parse_skip::maybe_report();

        // Write the dependency file and inputs trace after successful linking.
        if result.is_ok() {
            if let Some(dep_file_path) = &args.dependency_file() {
                write_dependency_file(dep_file_path, args.output(), &file_loader.loaded_files)
                    .with_context(|| {
                        format!(
                            "Failed to write dependency file `{}`",
                            dep_file_path.display()
                        )
                    })?;
            }
            if args.should_write_trace_file() {
                let mut buf = BufWriter::new(std::io::stdout());
                for input in &file_loader.loaded_files {
                    writeln!(buf, "{}", input.filename.display())?;
                }
            }
        }

        result
    }

    fn load_inputs_and_link<'data, P: Platform, A: Arch<Platform = P>>(
        &'data self,
        file_loader: &mut FileLoader<'data>,
        args: &'data P::Args,
    ) -> error::Result<LinkerOutput<'data>> {
        // Incremental *pre-load* skip — fires before `load_inputs`
        // even opens a file. If the cache's args_hash + wild_version
        // + per-input fingerprints + output_size all match what's
        // on disk right now, we can short-circuit with zero mmap,
        // zero archive extraction, zero symbol parsing.
        //
        // Differs from `try_whole_link_skip` (which runs after
        // load_inputs) in WHERE it fires; both produce the same
        // verdict under a valid cache. Keeping the post-load version
        // as defence-in-depth for paths where the pre-load check
        // can't run (first link, cache v-mismatch, explicit
        // WILD_INCREMENTAL_PRE_LOAD_SKIP=0 opt-out).
        let pre_load_skip_active = args.common().incremental_cache.reads_cache()
            || std::env::var_os("WILD_INCREMENTAL_DEBUG").is_some();
        if pre_load_skip_active
            && std::env::var_os("WILD_INCREMENTAL_PRE_LOAD_SKIP").as_deref()
                != Some(std::ffi::OsStr::new("0"))
            && try_pre_load_skip::<P>(args)
        {
            return Ok(LinkerOutput { layout: None });
        }

        let mut plugin = P::maybe_init_linker_plugin(args, &self.linker_plugin_arena, &self.herd)?;

        let loaded = file_loader.load_inputs::<P>(&args.common().inputs, args, &mut plugin);

        args.common().save_dir.finish(file_loader, args)?;

        let loaded = loaded?;

        // Post-load fallback: same signature check, but after inputs
        // are fully resolved. Catches cases where argv-level pre-load
        // couldn't see the real input set (e.g. `-l` dylib lookup
        // that resolved to a different dylib since last link).
        // `WILD_INCREMENTAL_NO_POST_LOAD_SKIP=1` opts out so tier 3's
        // narrower section-level skip can be exercised on workloads
        // where whole-link-skip would otherwise win the race.
        if std::env::var_os("WILD_INCREMENTAL_DEBUG").is_some()
            && std::env::var_os("WILD_INCREMENTAL_NO_POST_LOAD_SKIP").is_none()
            && try_whole_link_skip::<P>(file_loader, args)
        {
            return Ok(LinkerOutput { layout: None });
        }

        let output_kind = OutputKind::new(args, file_loader);

        let mut output = file_writer::Output::new(args, output_kind);

        let mut output_sections =
            OutputSections::with_base_address(P::start_memory_address(output_kind));

        let mut layout_rules_builder = LayoutRulesBuilder::default();

        let auxiliary = input_data::AuxiliaryFiles::new(args, &self.inputs_arena)?;

        let mut symbol_db = symbol_db::SymbolDb::new(args, output_kind, &auxiliary, &self.herd)?;
        let mut per_symbol_flags = PerSymbolFlags::new();

        // Tier-1 parse-skip gating: classify each input as clean (its
        // on-disk bytes fingerprint matches the last link's
        // .wild-hashes side-car) so the replay / canary paths only
        // engage for inputs that are actually reusable. Built once
        // per link and threaded through add_inputs → read_symbols →
        // run_object_parse_skip; None when no parse-skip gate is
        // active so the hashing cost vanishes on normal links.
        let clean_input_paths = compute_clean_input_paths::<P>(file_loader, args);

        // Tier-1.5: load the per-output cache bundle once. The bundle
        // is `&'static` (leaked mmap) and shared across rayon workers
        // for the read path. `None` on first link, schema-mismatch, or
        // any other failure — callers fall through to the re-parse +
        // re-write path.
        let bundle = crate::parsed_input_cache::try_load_bundle_view_mmap(args.output());

        // Tier-3 canary: mmap the PREVIOUS output binary BEFORE
        // `produce_layout` triggers its rename-and-recreate. The mmap
        // holds an inode reference, so even after the file is renamed
        // to `<output>.delete` and unlinked, our pages stay valid
        // until process exit. Used for the post-write byte-
        // equivalence check that proves the dirty-bitmap predicate is
        // correctly conservative — if every "reusable" section has
        // bytes byte-identical to those a cold writer just emitted,
        // tier-3 phase 2b's actual section-skip is empirically safe.
        // Gated on WILD_INCREMENTAL_TIER3_CANARY=1 so cold/normal
        // links stay zero-overhead.
        let prev_output_mmap: Option<&'static [u8]> =
            if std::env::var_os("WILD_INCREMENTAL_TIER3_CANARY").is_some()
                || std::env::var_os("WILD_INCREMENTAL_TIER3_SKIP").is_some()
            {
                std::fs::File::open(args.output())
                    .ok()
                    .and_then(|f| unsafe { memmap2::Mmap::map(&f) }.ok())
                    .map(|m| {
                        let leaked: &'static memmap2::Mmap = Box::leak(Box::new(m));
                        leaked.as_ref() as &'static [u8]
                    })
            } else {
                None
            };

        symbol_db.add_inputs(
            &mut per_symbol_flags,
            &mut output_sections,
            &mut layout_rules_builder,
            loaded,
            clean_input_paths.as_ref(),
            bundle,
        )?;

        // TODO: Doing this here means that we can't wrap symbols produced by the linker plugin.
        // Moving it earlier or later however requires some rethought as to how this works.
        symbol_db.apply_wrapped_symbol_overrides();

        let mut resolver = resolution::Resolver::default();

        resolver
            .resolve_symbols_and_select_archive_entries(&mut symbol_db, &mut per_symbol_flags)?;

        // Now that we know which archive entries are being loaded, we can resolve alternative
        // symbol definitions.
        crate::symbol_db::resolve_alternative_symbol_definitions(
            &mut symbol_db,
            &mut per_symbol_flags,
            &resolver.resolved_groups,
        )?;

        if let Some(plugin) = plugin.as_mut()
            && plugin.is_initialised()
        {
            P::plugin_all_symbols_read(
                plugin,
                &mut symbol_db,
                &mut resolver,
                file_loader,
                &mut per_symbol_flags,
                &mut output_sections,
                &mut layout_rules_builder,
            )?;
        }

        // If it's a rust version script, apply the global symbol visibility now.
        // We previously downgraded all symbols to local visibility.
        if let VersionScript::Rust(rust_vscript) = &symbol_db.version_script {
            symbol_db.handle_rust_version_script(rust_vscript, &mut per_symbol_flags);
        }

        let layout_rules = layout_rules_builder.build::<P>();

        let resolved = resolver.resolve_sections_and_canonicalise_undefined(
            &mut symbol_db,
            &mut per_symbol_flags,
            &mut output_sections,
            &layout_rules,
        )?;

        let layout = layout::compute::<P, A>(
            symbol_db,
            per_symbol_flags,
            resolved,
            output_sections,
            &mut output,
        )?;

        // Tier-2 capture: snapshot the resolved section layout to
        // `<output>.wild-layout` so the next link can consume it
        // (tier 3 will use it to mmap-preserve unchanged sections of
        // the previous output binary). Best-effort — any IO failure
        // is silent. Gated on the same parse-skip env vars as tier 1
        // so cold/non-incremental links pay no overhead.
        let layout_snapshot_active = std::env::var_os("WILD_INCREMENTAL_DEBUG").is_some()
            || std::env::var_os("WILD_INCREMENTAL_PARSE_SKIP_WRITE").is_some()
            || std::env::var_os("WILD_INCREMENTAL_PARSE_SKIP").is_some()
            || std::env::var_os("WILD_INCREMENTAL_PARSE_SKIP_READ").is_some()
            || std::env::var_os("WILD_INCREMENTAL_PARSE_SKIP_CANARY").is_some()
            || std::env::var_os("WILD_INCREMENTAL_LAYOUT_CANARY").is_some()
            || std::env::var_os("WILD_INCREMENTAL_TIER3_CANARY").is_some()
            || std::env::var_os("WILD_INCREMENTAL_TIER3_SKIP").is_some();
        // Tier-3 phase 2 canary state: if set, contains
        // `(reusable_indices, current_snapshot)` so the post-write
        // path can byte-compare prev_output_mmap vs the freshly-
        // written output for every "would-be reusable" section.
        let mut tier3_canary_state: Option<(
            Vec<usize>,
            layout_snapshot::LayoutSnapshot,
        )> = None;
        // Phase 2b "wholesale prev → out copy" predicate: true iff
        // every section's layout matches AND no contributor is
        // dirty. Unlike `tier3_canary_state`'s reusable indices
        // (phase 3), this allows synthetic sections (empty
        // contributors, e.g. the Mach-O header / LINKEDIT region)
        // because phase 2b copies the entire output file from prev
        // — the synthetic regions inherit prev's bytes wholesale,
        // which is byte-correct.
        let mut tier3_fully_reusable = false;
        if layout_snapshot_active {
            let snapshot = layout.to_layout_snapshot();
            // Tier-2 canary: if the previous link's snapshot is on
            // disk, byte-compare against the fresh one. Divergence
            // means either layout went non-deterministic or the
            // snapshot format drifted — both block tier 3 from
            // safely consuming the snapshot, so we panic loud rather
            // than silently corrupt.
            if std::env::var_os("WILD_INCREMENTAL_LAYOUT_CANARY").is_some()
                && let Some(prev) = layout_snapshot::read_snapshot(args.output())
                && prev != snapshot
            {
                let prev_n = prev.len();
                let cur_n = snapshot.len();
                let mut first_diff: Option<(usize, String)> = None;
                for (i, (a, b)) in prev.sections.iter().zip(snapshot.sections.iter()).enumerate() {
                    if a != b {
                        first_diff = Some((
                            i,
                            format!(
                                "name={:?}/{:?} file={:#x}/{:#x} size={:#x}/{:#x} mem={:#x}/{:#x}",
                                String::from_utf8_lossy(&a.name),
                                String::from_utf8_lossy(&b.name),
                                a.file_offset,
                                b.file_offset,
                                a.file_size,
                                b.file_size,
                                a.mem_offset,
                                b.mem_offset,
                            ),
                        ));
                        break;
                    }
                }
                panic!(
                    "wild layout-canary mismatch: prev={prev_n} sections, cur={cur_n} sections; \
                     first divergence: {first_diff:?}"
                );
            }
            // Tier-3 dry-run: under WILD_INCREMENTAL_TIER3_PROBE=1,
            // intersect the previous snapshot's contributors map with
            // the current link's clean-input set and report how many
            // sections (and bytes) tier 3's writer integration WOULD
            // be able to mmap-preserve from the previous output.
            // Read-only — doesn't change link behaviour. Lets us
            // measure tier 3's potential ROI on a real bench before
            // we integrate with the writer.
            if std::env::var_os("WILD_INCREMENTAL_TIER3_PROBE").is_some()
                && let Some(prev_snap) = layout_snapshot::read_snapshot(args.output())
            {
                // Build the clean-input key set. Archive members
                // share their parent rlib's cleanness verdict (the
                // whole rlib is either clean or dirty), but each
                // member has its own bundle key. Walk the loaded
                // group_layouts to get path + entry_id for every
                // contributing input, then filter to those whose
                // PARENT file is in `clean_input_paths`.
                let clean_paths = clean_input_paths.as_ref();
                let mut clean_keys: hashbrown::HashSet<layout_snapshot::ContributorKey> =
                    hashbrown::HashSet::new();
                for group in &layout.group_layouts {
                    for file in &group.files {
                        let layout::FileLayout::Object(obj) = file else {
                            continue;
                        };
                        let path = obj.input.file.filename.as_path();
                        if let Some(set) = clean_paths
                            && !set.contains(path)
                        {
                            continue; // dirty rlib → member is dirty
                        }
                        let entry_id = obj
                            .input
                            .entry
                            .as_ref()
                            .map(|e| e.identifier.as_slice());
                        clean_keys.insert(parsed_input_cache::bundle_key_for(path, entry_id));
                    }
                }

                let dirty = prev_snap.dirty_section_indices(&clean_keys);
                let total_sections = prev_snap.len();
                let dirty_count = dirty.len();
                let clean_count = total_sections - dirty_count;
                let reusable_bytes: u64 = prev_snap
                    .sections
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !dirty.contains(i))
                    .map(|(_, s)| s.file_size)
                    .sum();
                let total_bytes: u64 =
                    prev_snap.sections.iter().map(|s| s.file_size).sum();
                eprintln!(
                    "wild tier-3 probe: {clean_count}/{total_sections} sections \
                     reusable, {reusable_bytes} / {total_bytes} bytes ({pct:.1}%)",
                    pct = if total_bytes == 0 {
                        0.0
                    } else {
                        100.0 * reusable_bytes as f64 / total_bytes as f64
                    }
                );
            }

            // Tier-3 phase 2 canary: stash (reusable_indices, snapshot)
            // for the post-write byte-compare. We need the SAME
            // clean-key derivation as the dry-run probe above
            // (handling archive members), so factor that out.
            if (std::env::var_os("WILD_INCREMENTAL_TIER3_CANARY").is_some()
                || std::env::var_os("WILD_INCREMENTAL_TIER3_SKIP").is_some())
                && let Some(prev_snap) = layout_snapshot::read_snapshot(args.output())
            {
                let clean_paths = clean_input_paths.as_ref();
                let mut clean_keys: hashbrown::HashSet<layout_snapshot::ContributorKey> =
                    hashbrown::HashSet::new();
                for group in &layout.group_layouts {
                    for file in &group.files {
                        let layout::FileLayout::Object(obj) = file else {
                            continue;
                        };
                        let path = obj.input.file.filename.as_path();
                        if let Some(set) = clean_paths
                            && !set.contains(path)
                        {
                            continue;
                        }
                        let entry_id = obj
                            .input
                            .entry
                            .as_ref()
                            .map(|e| e.identifier.as_slice());
                        clean_keys.insert(parsed_input_cache::bundle_key_for(path, entry_id));
                    }
                }
                let reusable = layout_snapshot::LayoutSnapshot::reusable_section_indices(
                    &prev_snap,
                    &snapshot,
                    &clean_keys,
                );
                tier3_fully_reusable = layout_snapshot::LayoutSnapshot::is_fully_reusable(
                    &prev_snap,
                    &snapshot,
                    &clean_keys,
                );
                tier3_canary_state = Some((reusable, snapshot.clone()));
            }

            // Persist the fresh snapshot for the next link.
            if let Err(e) = layout_snapshot::write_snapshot(args.output(), snapshot) {
                if std::env::var_os("WILD_INCREMENTAL_DEBUG").is_some() {
                    eprintln!(
                        "wild layout-snapshot: write to {} failed: {}",
                        args.output().display(),
                        e
                    );
                }
            }
        }

        // Tier-3 phase 2b: speculative writer-skip. If
        //   * `WILD_INCREMENTAL_TIER3_SKIP=1` is set,
        //   * every section is reusable (`reusable.len() == cur_snap.len()`),
        //   * AND the previous output's bytes are mmap'd,
        // then bypass the platform writer entirely and copy
        // prev_bytes → output. The byte-equivalence canary
        // (WILD_INCREMENTAL_TIER3_CANARY=1) has empirical evidence
        // that every reusable section's prev bytes equal the writer's
        // cold output; under the all-reusable case that proves the
        // wholesale copy is byte-correct including codesign (the
        // codesign references the file's content hash, which is the
        // same for two byte-identical files).
        //
        // Cases this fires (where whole-link-skip wouldn't):
        //   * args_hash differs slightly (e.g. wild upgrade) but
        //     layout + content stayed stable.
        //   * output_size verification falsely failed (timing).
        //   * pre/post-load whole-link-skip explicitly disabled.
        // Saves the entire writer phase (~280 ms on bevy-dylib-class
        // outputs).
        let did_speculative_skip =
            if std::env::var_os("WILD_INCREMENTAL_TIER3_SKIP").is_some()
                && tier3_fully_reusable
                && let Some(prev_bytes) = prev_output_mmap
            {
                // mmap-COW path: tell `file_writer` to open the
                // output `UpdateInPlace`. The file already contains
                // prev's bytes, so no memcpy is needed — the writer
                // closure becomes a no-op. Saves the 50 MB memcpy
                // on bevy-class outputs.
                tier3_skip::set(Some(tier3_skip::State {
                    reusable_ids: hashbrown::HashSet::new(),
                    ranges: Vec::new(),
                    prev_bytes,
                    use_in_place: true,
                }));
                output.set_size(prev_bytes.len() as u64);
                output.write(&layout, |_sized_output, _| {
                    // Output file is already prev's bytes via
                    // UpdateInPlace; nothing to do.
                    Ok(())
                })?;
                tier3_skip::set(None);
                if std::env::var_os("WILD_INCREMENTAL_DEBUG").is_some()
                    || std::env::var_os("WILD_INCREMENTAL_TIER3_SKIP").is_some()
                {
                    eprintln!(
                        "wild tier-3 skip: in-place reuse of {} bytes from prev output",
                        prev_bytes.len()
                    );
                }
                true
            } else {
                false
            };

        if !did_speculative_skip {
            // Tier-3 phase 3: partial writer-skip. When some
            // sections are reusable but not ALL (so phase 2b's
            // wholesale bypass can't fire), install per-section
            // skip state so the platform writer:
            //   (a) pre-fills reusable section ranges from
            //       prev_bytes BEFORE its emit loop runs, and
            //   (b) skips per-input-section iterations whose
            //       target output section is reusable.
            // Saves writer work proportional to the reusable
            // fraction. Cleared after write returns so the global
            // state doesn't leak across links.
            let installed_tier3_state =
                if std::env::var_os("WILD_INCREMENTAL_TIER3_SKIP").is_some()
                    && let Some((reusable, snap)) = tier3_canary_state.as_ref()
                    && !reusable.is_empty()
                    && reusable.len() < snap.sections.len()
                    && let Some(prev_bytes) = prev_output_mmap
                {
                    // Build the OutputSectionId set + the file
                    // ranges to pre-fill, in one pass.
                    let mut reusable_ids: hashbrown::HashSet<
                        crate::output_section_id::OutputSectionId,
                    > = hashbrown::HashSet::with_capacity(reusable.len());
                    let mut ranges: Vec<(usize, usize)> =
                        Vec::with_capacity(reusable.len());
                    for &i in reusable {
                        let s = &snap.sections[i];
                        reusable_ids.insert(
                            crate::output_section_id::OutputSectionId::from_u32(
                                i as u32,
                            ),
                        );
                        ranges.push((s.file_offset as usize, s.file_size as usize));
                    }
                    let total = snap.sections.len();
                    let n = reusable.len();
                    let bytes: u64 = ranges.iter().map(|&(_, sz)| sz as u64).sum();
                    if std::env::var_os("WILD_INCREMENTAL_DEBUG").is_some()
                        || std::env::var_os("WILD_INCREMENTAL_TIER3_SKIP").is_some()
                    {
                        eprintln!(
                            "wild tier-3 partial-skip: pre-filling {n}/{total} \
                             sections ({bytes} bytes) from prev output; writer \
                             will emit the remaining {} sections only",
                            total - n
                        );
                    }
                    tier3_skip::set(Some(tier3_skip::State {
                        reusable_ids,
                        ranges,
                        prev_bytes,
                        // mmap-COW: open the output `UpdateInPlace`
                        // so prev's bytes ARE the pre-fill.
                        use_in_place: true,
                    }));
                    true
                } else {
                    false
                };

            let result = P::write_output_file::<A>(&mut output, &layout);

            if installed_tier3_state {
                tier3_skip::set(None);
            }

            result?;
        }

        // Tier-3 phase 2 canary: byte-compare the freshly-written
        // output's reusable sections against the previous output's
        // pages. If every reusable section is byte-identical, tier
        // 3's writer-skip integration is empirically safe — the
        // dirty-bitmap predicate is correctly conservative on this
        // workload.
        if let (Some((reusable, snap)), Some(prev_bytes)) =
            (tier3_canary_state.as_ref(), prev_output_mmap)
        {
            // Re-open the new output so we can mmap it post-write.
            // The file_writer dropped its mmap in `flush()`, but
            // the bytes are committed to disk now.
            let new_mmap = std::fs::File::open(args.output())
                .ok()
                .and_then(|f| unsafe { memmap2::Mmap::map(&f) }.ok());
            if let Some(new_mmap) = new_mmap {
                let new_bytes: &[u8] = &new_mmap;
                let total_reusable = reusable.len();
                let mut byte_matched = 0usize;
                let mut bytes_verified: u64 = 0;
                let mut first_diverged: Option<(usize, String)> = None;
                for &i in reusable {
                    let s = &snap.sections[i];
                    let off = s.file_offset as usize;
                    let size = s.file_size as usize;
                    if off.saturating_add(size) > prev_bytes.len()
                        || off.saturating_add(size) > new_bytes.len()
                    {
                        // A reusable section's range falls off the
                        // end of either output — file size mismatch
                        // we'd want to know about. Count as
                        // diverged and capture the first occurrence.
                        if first_diverged.is_none() {
                            first_diverged = Some((
                                i,
                                format!(
                                    "{:?} off={off:#x} size={size:#x} prev_len={} new_len={}",
                                    String::from_utf8_lossy(&s.name),
                                    prev_bytes.len(),
                                    new_bytes.len(),
                                ),
                            ));
                        }
                        continue;
                    }
                    if prev_bytes[off..off + size] == new_bytes[off..off + size] {
                        byte_matched += 1;
                        bytes_verified += size as u64;
                    } else if first_diverged.is_none() {
                        // Find the first byte that differs for the
                        // diagnostic — diff::maybe_diff is global
                        // but the canary wants section-local detail.
                        let first_byte_off = (0..size)
                            .find(|j| prev_bytes[off + j] != new_bytes[off + j])
                            .unwrap_or(0);
                        first_diverged = Some((
                            i,
                            format!(
                                "{:?} off={off:#x} size={size:#x} first_diff_at=+{first_byte_off:#x}",
                                String::from_utf8_lossy(&s.name)
                            ),
                        ));
                    }
                }
                eprintln!(
                    "wild tier-3 canary: {byte_matched}/{total_reusable} sections \
                     byte-identical, {bytes_verified} bytes verified safe to reuse"
                );
                if let Some((idx, detail)) = first_diverged
                    && byte_matched != total_reusable
                {
                    eprintln!(
                        "wild tier-3 canary: first divergence at section #{idx}: {detail}"
                    );
                }
            }
        }

        // --emit-patch=<path>: write a byte-level diff between the
        // previous output and the freshly-written one so a debugger
        // (BugStalker on Linux; mach_vm_write equivalent on macOS) can
        // splice the changed bytes into a still-running process for
        // AOT edit-and-continue. Wild already has prev_output_mmap; we
        // re-mmap the new output and walk the two in parallel,
        // coalescing adjacent differing bytes into runs.
        if let (Some(patch_path), Some(prev_bytes)) =
            (args.common().emit_patch.as_ref(), prev_output_mmap)
        {
            let new_mmap = std::fs::File::open(args.output())
                .ok()
                .and_then(|f| unsafe { memmap2::Mmap::map(&f) }.ok());
            if let Some(new_mmap) = new_mmap {
                if let Err(e) = emit_patch_file(prev_bytes, &new_mmap, patch_path) {
                    eprintln!("wild --emit-patch failed: {e}");
                }
            }
        }

        diff::maybe_diff()?;

        // We've finished linking. We consider everything from this point onwards as shutdown.
        let (g1, g2) = timing_guard!("Shutdown");
        self.shutdown_scope.store(vec![Box::new(g1), Box::new(g2)]);

        Ok(LinkerOutput {
            layout: Some(Box::new(layout)),
        })
    }
}

impl Default for Linker {
    fn default() -> Self {
        Self::new()
    }
}

/// Pre-load variant of [`try_whole_link_skip`] — runs before
/// `load_inputs` has opened a single file. Verifies the cache's
/// paths + fingerprints + output size directly against the
/// filesystem, bypassing wild's input-resolution pipeline.
///
/// Trade-offs vs the post-load version:
///   * Wins ~130 ms (skip mmap + archive-member extract + symbol parse) when the cache is clean.
///   * May false-miss if the cache is slightly stale — e.g. user changed a `-L` search path such
///     that argv still hashes the same but the resolved input set would differ. In practice
///     argv-hash equality is a strong signal because cargo's invocation is deterministic; if the
///     argv changed, args_hash catches it.
///
/// Returns `true` on a safe skip. Never returns `true` without
/// output-file size + existence + every cached input present.
fn try_pre_load_skip<P: Platform>(args: &P::Args) -> bool {
    let argv: Vec<String> = std::env::args().collect();
    let args_hash = incremental_cache::compute_args_hash(&argv);
    let hashes_path = incremental_cache::hashes_path_for_output(args.output());
    let Some(cached) = incremental_cache::read_link_cache(&hashes_path) else {
        return false;
    };
    if cached.wild_version != incremental_cache::WILD_VERSION {
        return false;
    }
    if cached.args_hash != args_hash {
        return false;
    }
    // Every cached input path must still be present with a matching
    // fingerprint. This catches content changes AND missing / moved
    // inputs without going through wild's own resolver.
    if incremental_cache::verify_cached_inputs_unchanged(&cached.inputs).is_none() {
        return false;
    }
    // Output still on disk at expected size — defence against
    // manual edits / deletes since last link.
    let output_path = args.output();
    match std::fs::metadata(output_path) {
        Ok(m) if m.len() == cached.output_size => {
            eprintln!(
                "wild incremental: PRE-LOAD SKIP — output at {} reused, \
                 load_inputs bypassed",
                output_path.display()
            );
            true
        }
        Ok(_) | Err(_) => false,
    }
}

/// Returns `true` when the current link's signature (inputs + args +
/// wild version) matches the cached one and the previous output file
/// is still on disk at the expected size — i.e. when the caller is
/// safe to return `Ok(LinkerOutput { layout: None })` without running
/// resolve / layout / write. Returns `false` on any mismatch, missing
/// cache, missing output, or size disagreement.
///
/// Emits a terse stderr line explaining the decision so users running
/// with `WILD_INCREMENTAL_DEBUG=1` can see why a skip did or didn't
/// fire.
fn try_whole_link_skip<P: Platform>(file_loader: &FileLoader<'_>, args: &P::Args) -> bool {
    let inputs: Vec<(&std::path::Path, &[u8])> = file_loader
        .loaded_files
        .iter()
        .map(|f| (f.filename.as_path(), f.data()))
        .collect();
    let current_inputs = incremental_cache::hash_loaded_inputs(inputs);
    let argv: Vec<String> = std::env::args().collect();
    let current_args_hash = incremental_cache::compute_args_hash(&argv);

    let hashes_path = incremental_cache::hashes_path_for_output(args.output());
    let Some(cached) = incremental_cache::read_link_cache(&hashes_path) else {
        eprintln!(
            "wild incremental: no cache at {} — cold link (baseline will be \
             captured afterwards)",
            hashes_path.display()
        );
        return false;
    };

    let verdict =
        incremental_cache::classify_signature(&current_args_hash, &current_inputs, &cached);
    match verdict {
        incremental_cache::SignatureVerdict::FullMatch => {
            // Defence-in-depth: the cache believes the output is
            // intact, but verify against the filesystem before
            // trusting it. User could have deleted / truncated the
            // binary; size mismatch forces a cold link.
            let output_path = args.output();
            let size_ok = match std::fs::metadata(output_path) {
                Ok(m) if m.len() == cached.output_size => true,
                Ok(m) => {
                    eprintln!(
                        "wild incremental: signature matched but output size \
                         differs ({} on disk vs {} cached) — cold link",
                        m.len(),
                        cached.output_size
                    );
                    false
                }
                Err(e) => {
                    eprintln!(
                        "wild incremental: signature matched but output missing \
                         ({}: {}) — cold link",
                        output_path.display(),
                        e
                    );
                    false
                }
            };
            if size_ok {
                eprintln!(
                    "wild incremental: FULL LINK SKIP — output at {} reused",
                    output_path.display()
                );
                return true;
            }
            false
        }
        incremental_cache::SignatureVerdict::Mismatch(why) => {
            eprintln!("wild incremental: link signature mismatch: {:?}", why);
            false
        }
    }
}

/// Build a set of input paths whose content bytes fingerprint to the
/// same value they had at the last link — i.e. *safe to replay from
/// cache* under the tier-1 parse-skip read path. Mirrors
/// `classify_dirty`'s logic but returns the complement (clean rather
/// than dirty) and keyed by PathBuf for O(1) lookup during
/// `run_object_parse_skip`.
///
/// Returns `None` when no parse-skip gate is active — skips the
/// hashing work entirely on default-off links. Returns `Some(empty)`
/// when gates are active but there's no prior cache on disk, which
/// treats every input as dirty (the safe default — a first-link run
/// populates caches; subsequent runs can replay).
///
/// Archive members inherit their parent `.rlib`'s verdict because
/// `.wild-hashes` tracks whole-file fingerprints: a clean `.rlib`
/// means every member is cacheable; a dirty one forces a re-parse of
/// all members.
fn compute_clean_input_paths<P: Platform>(
    file_loader: &FileLoader<'_>,
    args: &P::Args,
) -> Option<std::collections::HashSet<std::path::PathBuf>> {
    // Fast path: no gate active → don't hash inputs at all.
    let canary = std::env::var_os("WILD_INCREMENTAL_PARSE_SKIP_CANARY").is_some();
    let read = std::env::var_os("WILD_INCREMENTAL_PARSE_SKIP_READ").is_some();
    if !canary && !read {
        return None;
    }

    let inputs: Vec<(&std::path::Path, &[u8])> = file_loader
        .loaded_files
        .iter()
        .map(|f| (f.filename.as_path(), f.data()))
        .collect();
    let current_inputs = incremental_cache::hash_loaded_inputs(inputs);

    let hashes_path = incremental_cache::hashes_path_for_output(args.output());
    let Some(cached) = incremental_cache::read_link_cache(&hashes_path) else {
        // No prior link cache → every input is dirty. Returning an
        // empty set (rather than None) keeps callers on the
        // dirty-by-default branch without forcing an extra
        // is-gate-active check.
        return Some(std::collections::HashSet::new());
    };

    let mut clean = std::collections::HashSet::with_capacity(current_inputs.len());
    for (path, hash) in &current_inputs {
        if let Some(cached_hash) = cached.inputs.get(path)
            && cached_hash == hash
        {
            clean.insert(path.clone());
        }
    }
    Some(clean)
}

/// Persist this link's signature next to the output binary so the
/// next link can check for whole-link-skip eligibility. Called only
/// from the successful-link path; errors are non-fatal (a missing
/// cache just forces the next link to cold-baseline).
///
/// If `file_loader.loaded_files` is empty the current link took the
/// pre-load-skip path — there are no inputs to hash and the previous
/// cache is already correct. Return without rewriting, otherwise we'd
/// overwrite a valid cache with an empty input set and the next skip
/// decision would falsely succeed with zero inputs to check.
fn persist_link_cache<'data, P: Platform>(file_loader: &FileLoader<'data>, args: &P::Args) {
    if file_loader.loaded_files.is_empty() {
        return;
    }
    let inputs: Vec<(&std::path::Path, &[u8])> = file_loader
        .loaded_files
        .iter()
        .map(|f| (f.filename.as_path(), f.data()))
        .collect();
    let current_inputs = incremental_cache::hash_loaded_inputs(inputs);
    let argv: Vec<String> = std::env::args().collect();
    let args_hash = incremental_cache::compute_args_hash(&argv);
    let output_size = std::fs::metadata(args.output())
        .map(|m| m.len())
        .unwrap_or(0);
    let cache = incremental_cache::LinkCache {
        args_hash,
        output_size,
        wild_version: incremental_cache::WILD_VERSION.to_owned(),
        inputs: current_inputs,
    };
    let hashes_path = incremental_cache::hashes_path_for_output(args.output());
    if let Err(e) = incremental_cache::write_link_cache(&hashes_path, &cache) {
        eprintln!(
            "wild incremental: failed to persist cache to {}: {}",
            hashes_path.display(),
            e
        );
    }
}

impl Drop for Linker {
    fn drop(&mut self) {
        timing_phase!("Drop inputs");
        self.inputs_arena = Arena::new();
        self.herd = Default::default();
    }
}

impl Drop for LinkerOutput<'_> {
    fn drop(&mut self) {
        timing_phase!("Drop layout");
        self.layout.take();
    }
}

/// Writes a dependency file in Makefile format.
fn write_dependency_file(
    dep_file_path: &Path,
    output_path: &Path,
    loaded_files: &[&InputFile],
) -> std::io::Result<()> {
    timing_phase!("Write dependency file");

    let file = std::fs::File::create(dep_file_path)?;
    let mut writer = BufWriter::new(file);

    // Collect unique dependency paths
    let mut seen = std::collections::HashSet::new();
    let mut deps = Vec::new();
    for input_file in loaded_files {
        // Skip temporary files. e.g. those generated by linker plugins.
        if input_file.modifiers.temporary {
            continue;
        }

        let path_str = input_file.filename.display().to_string();
        if seen.insert(path_str.clone()) {
            deps.push(path_str);
        }
    }

    write!(writer, "{}:", output_path.display())?;

    for dep in &deps {
        write!(writer, " {dep}")?;
    }

    writeln!(writer)?;

    for dep in &deps {
        writeln!(writer, "\n{dep}:")?;
    }

    Ok(())
}

/// Possibly initialise timing if a timing-related environment variable is active and it was enabled
/// in the build, otherwise, do nothing. See `BENCHMARKING.md` for details.
pub fn init_timing() -> Result {
    timing::setup()
}

pub fn should_fork(args: &Args) -> bool {
    args.common().should_fork()
}

pub fn activate_thread_pool(args: &mut Args) -> Result<crate::args::ThreadPool> {
    args.common_mut().activate_thread_pool()
}

/// Write a wild-patch text file describing every byte run that differs
/// between `prev` (the previous link's output) and `new` (this link's
/// output). Adjacent differing bytes are coalesced into a single run.
/// Designed to be consumed by an external patcher (e.g. BugStalker) that
/// ptrace-writes each run into a still-running process.
///
/// File format (v3):
/// ```text
/// # wild-patch v3
/// # old-size: <N>
/// # new-size: <M>
/// # old-blake3: <64-hex>
/// # new-blake3: <64-hex>
/// # entries: <K>
/// # fn: <symbol-name>
/// <hex-offset> <length> <hex-old-bytes> <hex-new-bytes>
/// ...
/// ```
///
/// Each data line carries BOTH the bytes that were at that offset in the
/// previous link and the bytes that are there in this link. The patcher
/// verifies the old bytes against the running process before writing —
/// if the running process has drifted (someone else patched it, an
/// earlier patch failed, the binary on disk is different from what's
/// running), the patcher reports a clear diagnostic instead of silently
/// corrupting the program.
///
/// `<hex-offset>` is the file offset (== virtual offset within the
/// `__TEXT` segment for typical Mach-O / `.text` for typical ELF)
/// where the run starts. `<length>` is its byte count. Both byte
/// strings are exactly `length * 2` hex chars.
///
/// On a tail entry (when `new.len() > prev.len()`), the old bytes that
/// extend beyond `prev.len()` are emitted as zeros — a fresh tail page
/// in a live process will read as zeros too, so the verification still
/// works in the typical case.
fn emit_patch_file(
    prev: &[u8],
    new: &[u8],
    path: &std::path::Path,
) -> std::io::Result<()> {
    use std::fmt::Write as _;

    let mut runs: Vec<(usize, usize)> = Vec::new();
    let common = prev.len().min(new.len());
    let mut i = 0;
    while i < common {
        if prev[i] != new[i] {
            let start = i;
            while i < common && prev[i] != new[i] {
                i += 1;
            }
            runs.push((start, i - start));
        } else {
            i += 1;
        }
    }
    if new.len() > prev.len() {
        runs.push((prev.len(), new.len() - prev.len()));
    }

    let symbol_ranges = patch_symbol_ranges(new);

    let mut out = String::new();
    writeln!(out, "# wild-patch v3").unwrap();
    writeln!(out, "# old-size: {}", prev.len()).unwrap();
    writeln!(out, "# new-size: {}", new.len()).unwrap();
    writeln!(out, "# old-blake3: {}", blake3::hash(prev).to_hex()).unwrap();
    writeln!(out, "# new-blake3: {}", blake3::hash(new).to_hex()).unwrap();
    writeln!(out, "# entries: {}", runs.len()).unwrap();
    for &(offset, length) in &runs {
        if let Some(symbol) = symbol_for_offset(&symbol_ranges, offset as u64) {
            writeln!(out, "# fn: {}", sanitize_patch_comment(symbol)).unwrap();
        }
        write!(out, "{offset:x} {length} ").unwrap();
        // old bytes (zero-padded for any tail beyond prev.len())
        for j in 0..length {
            let b = prev.get(offset + j).copied().unwrap_or(0);
            write!(out, "{b:02x}").unwrap();
        }
        write!(out, " ").unwrap();
        // new bytes
        for b in &new[offset..offset + length] {
            write!(out, "{b:02x}").unwrap();
        }
        writeln!(out).unwrap();
    }

    std::fs::write(path, out)
}

#[derive(Debug)]
struct PatchSymbolRange {
    file_start: u64,
    file_end: u64,
    name: String,
}

fn patch_symbol_ranges(bytes: &[u8]) -> Vec<PatchSymbolRange> {
    use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};

    let Ok(file) = object::File::parse(bytes) else {
        return Vec::new();
    };

    #[derive(Debug)]
    struct Candidate {
        file_start: u64,
        explicit_size: u64,
        section_end: u64,
        name: String,
    }

    let mut candidates = Vec::new();
    for symbol in file.symbols() {
        if !symbol.is_definition() || symbol.kind() != SymbolKind::Text {
            continue;
        }
        let Ok(name) = symbol.name() else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let Some(section_index) = symbol.section_index() else {
            continue;
        };
        let Ok(section) = file.section_by_index(section_index) else {
            continue;
        };
        if section.kind() != object::SectionKind::Text {
            continue;
        }
        let Some((section_file_start, section_file_size)) = section.file_range() else {
            continue;
        };
        let section_addr = section.address();
        let section_size = section.size();
        let symbol_addr = symbol.address();
        if symbol_addr < section_addr || symbol_addr >= section_addr.saturating_add(section_size) {
            continue;
        }
        let section_offset = symbol_addr - section_addr;
        if section_offset >= section_file_size {
            continue;
        }
        candidates.push(Candidate {
            file_start: section_file_start + section_offset,
            explicit_size: symbol.size(),
            section_end: section_file_start + section_file_size,
            name: name.to_owned(),
        });
    }

    candidates.sort_by(|a, b| {
        a.file_start
            .cmp(&b.file_start)
            .then_with(|| b.explicit_size.cmp(&a.explicit_size))
            .then_with(|| a.name.cmp(&b.name))
    });
    candidates.dedup_by(|a, b| a.file_start == b.file_start);

    let mut ranges = Vec::with_capacity(candidates.len());
    for (idx, candidate) in candidates.iter().enumerate() {
        let inferred_end = candidates
            .iter()
            .skip(idx + 1)
            .find(|next| next.file_start > candidate.file_start)
            .map(|next| next.file_start)
            .unwrap_or(candidate.section_end);
        let explicit_end = candidate
            .explicit_size
            .checked_add(candidate.file_start)
            .filter(|end| *end > candidate.file_start);
        let file_end = explicit_end.unwrap_or(inferred_end).min(candidate.section_end);
        if file_end > candidate.file_start {
            ranges.push(PatchSymbolRange {
                file_start: candidate.file_start,
                file_end,
                name: candidate.name.clone(),
            });
        }
    }
    ranges
}

fn symbol_for_offset(ranges: &[PatchSymbolRange], offset: u64) -> Option<&str> {
    let idx = ranges
        .partition_point(|range| range.file_start <= offset)
        .checked_sub(1)?;
    let range = &ranges[idx];
    (offset < range.file_end).then_some(range.name.as_str())
}

fn sanitize_patch_comment(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\r' => ' ',
            _ => c,
        })
        .collect()
}
