use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use rrg_core::{BigramIndex, BigramIndexConfig, SearchOptions, search_path};

/// rrg — ripripgrep. Filesystem search with optional indexed queries.
#[derive(Parser)]
#[command(name = "rrg", version, about)]
struct Args {
    /// Pattern to search for. (Search mode when no subcommand.)
    pattern: Option<String>,

    /// Root path to search or index. Defaults to the current directory.
    path: Option<PathBuf>,

    /// Case-insensitive matching.
    #[arg(short = 'i')]
    ignore_case: bool,

    /// Treat the pattern as a literal string, not a regex.
    #[arg(short = 'F')]
    fixed_strings: bool,

    /// Search hidden files and directories.
    #[arg(short = 'u', long = "unrestricted")]
    hidden: bool,

    /// Use the bigram index at <path>/.rrg-index (build it with `rrg --build-index <path>`).
    #[arg(long = "use-index")]
    use_index: bool,

    /// Build a bigram index at <path>/.rrg-index and exit.
    #[arg(long = "build-index", conflicts_with = "use_index")]
    build_index: bool,

    /// Validate every indexed file's mtime/len before querying (costs one
    /// stat per file; falls back to a full scan when any file changed).
    #[arg(long = "check-stale", requires = "use_index")]
    check_stale: bool,

    /// Run the hot-index daemon on <path> (long-running; used internally by
    /// auto-spawn). Skips in-process searches.
    #[arg(long = "serve", hide = true)]
    serve: bool,

    /// Skip the daemon fast path and search in-process.
    #[arg(long = "no-daemon", requires = "use_index")]
    no_daemon: bool,

    /// Print aggregate counts and timing instead of matching lines.
    #[arg(short = 'c', long = "count")]
    count: bool,

    /// Print only the paths of files with matches (rg -l).
    #[arg(short = 'l', long = "files-with-matches", conflicts_with = "count")]
    files_with_matches: bool,
}

fn main() -> Result<()> {
    let mut args = Args::parse();

    if args.build_index {
        // `rrg --build-index <path>`: with the flag set, the first positional
        // is the target path, not a pattern.
        let root = args
            .path
            .take()
            .or_else(|| args.pattern.take().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        return cmd_build_index(&root);
    }

    let root = args.path.take().unwrap_or_else(|| PathBuf::from("."));

    if args.serve {
        let root = args
            .path
            .take()
            .or_else(|| args.pattern.take().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        return rrg_core::serve::serve(&root, true);
    }

    let Some(pattern) = args.pattern else {
        anyhow::bail!("no pattern given (see --help); or use --build-index to index");
    };

    let mode = if args.count {
        rrg_core::search::OutputMode::Count
    } else if args.files_with_matches {
        rrg_core::search::OutputMode::Files
    } else {
        rrg_core::search::OutputMode::Lines
    };
    let opts = SearchOptions {
        case_insensitive: args.ignore_case,
        fixed_strings: args.fixed_strings,
        mode,
        ..Default::default()
    };

    if args.use_index {
        return cmd_indexed_search(&pattern, &root, &opts, args.check_stale, args.no_daemon);
    }

    let t0 = Instant::now();
    let stats = search_path(&pattern, &root, &opts)?;
    if opts.mode == rrg_core::search::OutputMode::Lines {
        print!("{}", stats.output);
    } else if opts.mode == rrg_core::search::OutputMode::Files {
        for f in &stats.files {
            println!("{f}");
        }
    } else {
        println!(
            "{} files visited, {} matched, {} matches, {} errors [{:.1?}]",
            stats.files_visited,
            stats.files_matched,
            stats.matches,
            stats.errors,
            t0.elapsed()
        );
    }
    Ok(())
}

fn cmd_build_index(root: &PathBuf) -> Result<()> {
    let t0 = Instant::now();
    let (idx, timings) = BigramIndex::build(
        root,
        &BigramIndexConfig::default(),
        Some(&root.join(".rrg-index")),
    )?;
    let dir = root.join(".rrg-index");
    idx.save(&dir)?;
    println!(
        "indexed {} files ({} fallback) in {:.1?} [walk {:.1?}, fill {:.1?}, trigrams {:.1?}]; \
         index memory: {:.1} MiB; saved to {}",
        timings.files,
        idx.fallback_paths.len(),
        t0.elapsed(),
        timings.walk,
        timings.fill,
        timings.trigrams,
        idx.memory_bytes() as f64 / (1024.0 * 1024.0),
        dir.display()
    );
    Ok(())
}

fn cmd_indexed_search(
    pattern: &str,
    root: &PathBuf,
    opts: &SearchOptions,
    check_stale: bool,
    no_daemon: bool,
) -> Result<()> {
    let t0 = Instant::now();
    let index_dir = root.join(".rrg-index");

    // Daemon fast path: hot index + watcher; per-query cost is one UDS
    // round trip. Falls back silently to the in-process path on any
    // failure (never blocks the user on daemon machinery).
    if !no_daemon && std::env::var("RRG_NO_DAEMON").is_err() {
        let req = rrg_core::serve::QueryRequest {
            pattern: pattern.to_string(),
            fixed_strings: opts.fixed_strings,
            case_insensitive: opts.case_insensitive,
            check_stale,
            mode: opts.mode,
        };
        if let Ok(a) =
            rrg_core::serve::query_via_daemon(&rrg_core::serve::socket_path(&index_dir), &req)
        {
            print_indexed_answer(&a, t0.elapsed());
            return Ok(());
        }
        let exe = std::env::current_exe()?;
        let _ = rrg_core::serve::try_spawn_daemon(&exe, root, &index_dir);
        if let Ok(a) =
            rrg_core::serve::query_via_daemon(&rrg_core::serve::socket_path(&index_dir), &req)
        {
            print_indexed_answer(&a, t0.elapsed());
            return Ok(());
        }
        // fall through to in-process
    }

    let idx = BigramIndex::load(&index_dir)
        .context("no index found; run `rrg --build-index <path>` first")?;
    let load_elapsed = t0.elapsed();

    if std::env::var("RRG_DEBUG").is_ok() {
        eprintln!("[debug] {}", idx.tg_debug(pattern));
    }

    let qopts = rrg_core::QueryOptions {
        fixed_strings: opts.fixed_strings,
        case_insensitive: opts.case_insensitive,
        check_stale,
        mode: opts.mode,
    };
    let answer = rrg_core::run_query(&idx, &index_dir, pattern, &qopts, &[])?;
    print_indexed_answer(&answer, load_elapsed);
    Ok(())
}

fn print_indexed_answer(answer: &rrg_core::QueryAnswer, load_elapsed: std::time::Duration) {
    if !answer.output.is_empty() {
        print!("{}", answer.output);
        return;
    }
    if !answer.files.is_empty() {
        for f in &answer.files {
            println!("{f}");
        }
        return;
    }
    println!(
        "{} files scanned ({} candidates via {}, {} fallback), {} matched, {} matches [{:.1?} query, {:.1?} load]",
        answer.scanned,
        answer.candidates,
        answer.mode,
        answer.fallback,
        answer.matched,
        answer.matches,
        std::time::Duration::from_secs_f64(answer.query_ms / 1000.0),
        load_elapsed
    );
}
