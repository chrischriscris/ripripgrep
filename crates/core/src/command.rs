//! User-facing search execution shared by cold and indexed CLI searches.
//!
//! Matching and printing use the same grep crates as ripgrep. Indexes only
//! narrow candidates: traversal, metadata checks, and live verification preserve
//! filtering and output semantics, including newly created and changed files.
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use grep_printer::{ColorSpecs, JSONBuilder, StandardBuilder, SummaryBuilder, SummaryKind};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Encoding, SearcherBuilder};
use ignore::{WalkBuilder, overrides::OverrideBuilder, types::TypesBuilder};
use rayon::prelude::*;
use termcolor::Buffer;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Lines,
    Count,
    CountMatches,
    FilesWithMatches,
    FilesWithoutMatch,
    Quiet,
    Files,
    FilesQuiet,
    Json,
}

#[derive(Default)]
pub struct MatchOptions {
    pub patterns: Vec<String>,
    pub fixed: bool,
    pub insensitive: bool,
    pub smart_case: bool,
    pub word: bool,
    pub whole_line: bool,
    pub invert: bool,
    pub multiline: bool,
    pub dotall: bool,
    pub crlf: bool,
    pub null_data: bool,
    pub no_unicode: bool,
    pub text: bool,
    pub binary: bool,
    pub encoding: Option<String>,
    pub max_count: Option<u64>,
}

#[derive(Default)]
pub struct FilterOptions {
    pub hidden: bool,
    pub no_ignore: bool,
    pub no_ignore_vcs: bool,
    pub no_ignore_global: bool,
    pub no_ignore_parent: bool,
    pub follow: bool,
    pub globs: Vec<(String, bool)>,
    pub types: Vec<String>,
    pub types_not: Vec<String>,
    pub type_add: Vec<String>,
    pub ignore_files: Vec<PathBuf>,
    pub max_depth: Option<usize>,
    pub max_filesize: Option<u64>,
    pub one_file_system: bool,
    pub sort: bool,
    pub sort_reverse: bool,
}

#[derive(Default)]
pub struct PrintOptions {
    pub mode: Mode,
    pub stats: bool,
    pub passthru: bool,
    pub per_match: bool,
    pub filename: bool,
    pub line_number: bool,
    pub heading: bool,
    pub color: bool,
    pub color_specs: Vec<String>,
    pub column: bool,
    pub byte_offset: bool,
    pub only_matching: bool,
    pub replacement: Option<String>,
    pub before: usize,
    pub after: usize,
    pub context_separator: Option<Vec<u8>>,
    pub null: bool,
    pub trim: bool,
    pub max_columns: Option<u64>,
    pub max_columns_preview: bool,
    pub include_zero: bool,
    pub no_messages: bool,
    pub label: Option<PathBuf>,
}

pub struct CommandOptions {
    pub paths: Vec<PathBuf>,
    pub strip_dot: bool,
    pub matching: MatchOptions,
    pub filter: FilterOptions,
    pub print: PrintOptions,
    pub threads: usize,
    pub index_root: Option<PathBuf>,
}

/// Like rg: success with selected matches, no matches, or search error.
#[derive(Default)]
pub struct Outcome {
    pub matched: bool,
    pub errors: bool,
    pub broken_pipe: bool,
    printed: bool,
}

impl Outcome {
    pub fn exit_code(&self, quiet: bool) -> u8 {
        if self.broken_pipe || (quiet && self.matched) {
            0
        } else if self.errors {
            2
        } else if self.matched {
            0
        } else {
            1
        }
    }
}

/// Return rg's built-in file types, including any user definitions.
pub fn file_types(add: &[String]) -> Result<Vec<(String, Vec<String>)>> {
    let mut builder = TypesBuilder::new();
    builder.add_defaults();
    for definition in add {
        builder.add_def(definition)?;
    }
    Ok(builder
        .definitions()
        .into_iter()
        .map(|d| (d.name().to_owned(), d.globs().to_vec()))
        .collect())
}

fn walker(opts: &CommandOptions) -> Result<WalkBuilder> {
    let f = &opts.filter;
    let mut types = TypesBuilder::new();
    types.add_defaults();
    for def in &f.type_add {
        types.add_def(def)?;
    }
    for name in &f.types {
        types.select(name);
    }
    for name in &f.types_not {
        types.negate(name);
    }
    let mut overrides = OverrideBuilder::new(std::env::current_dir()?);
    for (glob, insensitive) in &f.globs {
        overrides.case_insensitive(*insensitive)?;
        overrides.add(glob)?;
    }
    let mut walk = WalkBuilder::new(&opts.paths[0]);
    for path in &opts.paths[1..] {
        walk.add(path);
    }
    walk.hidden(!f.hidden)
        .ignore(!f.no_ignore)
        .git_ignore(!f.no_ignore && !f.no_ignore_vcs)
        .git_exclude(!f.no_ignore && !f.no_ignore_vcs)
        .git_global(!f.no_ignore && !f.no_ignore_vcs && !f.no_ignore_global)
        .parents(!f.no_ignore && !f.no_ignore_parent)
        .follow_links(f.follow)
        .types(types.build()?)
        .overrides(overrides.build()?)
        .max_depth(f.max_depth)
        .max_filesize(f.max_filesize)
        .same_file_system(f.one_file_system)
        .skip_stdout(true)
        .add_custom_ignore_filename(".rgignore");
    for path in &f.ignore_files {
        if let Some(err) = walk.add_ignore(path) {
            return Err(err.into());
        }
    }
    if f.sort || f.sort_reverse {
        walk.sort_by_file_path(|a, b| a.cmp(b));
    }
    Ok(walk)
}

/// Metadata of indexed files that provably cannot match the query. Anything
/// unknown, changed, or unsupported by the planner must be searched live.
fn exclusions(opts: &CommandOptions) -> Result<HashMap<PathBuf, (u64, u64)>> {
    let Some(root) = &opts.index_root else {
        return Ok(HashMap::new());
    };
    let idx = crate::BigramIndex::load(&root.join(".rrg-index"))
        .context("no usable index found; run `rrg --build-index <path>` first")?;
    let m = &opts.matching;
    // Case folding, inline regex flags, transcoding and inverse queries need a
    // more capable candidate proof. Fall back rather than risk false negatives.
    if !m.fixed
        || m.insensitive
        || m.smart_case
        || m.invert
        || m.multiline
        || m.encoding.is_some()
        || m.patterns.iter().any(|p| !p.is_ascii())
        || opts.print.passthru
        || opts.print.mode == Mode::FilesWithoutMatch
        || opts.print.include_zero
    {
        return Ok(HashMap::new());
    }
    let mut selected = vec![false; idx.paths.len()];
    for pattern in &m.patterns {
        match idx.candidates_plan(&crate::plan_for_literal(pattern)) {
            crate::Candidates::MatchAll => return Ok(HashMap::new()),
            crate::Candidates::None => {}
            crate::Candidates::Superset(mut ids) => {
                idx.refine_plan_trigrams(&crate::tri_plan_for_literal(pattern), &mut ids);
                for id in ids {
                    selected[id as usize] = true;
                }
            }
        }
    }
    // Index paths can be relative to the index builder's working directory.
    // Only use entries resolving beneath this index root; unmatched paths scan.
    let root = root.canonicalize()?;
    let mut excluded = HashMap::new();
    for (id, path) in idx.paths.iter().enumerate() {
        if !selected[id]
            && path.is_absolute()
            && let Ok(path) = path.canonicalize()
            && path.starts_with(&root)
        {
            excluded.insert(path, idx.file_meta[id]);
        }
    }
    Ok(excluded)
}

fn unchanged_nonmatch(path: &Path, excluded: &HashMap<PathBuf, (u64, u64)>) -> bool {
    if excluded.is_empty() {
        return false;
    }
    let Ok(canonical) = path.canonicalize() else {
        return false;
    };
    let Some(&(mtime, len)) = excluded.get(&canonical) else {
        return false;
    };
    let Ok(meta) = path.metadata() else {
        return false;
    };
    let now = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    now.is_some_and(|t| t.as_nanos() as u64 == mtime && meta.len() == len)
}

fn matcher(m: &MatchOptions) -> Result<RegexMatcher> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .fixed_strings(m.fixed)
        .case_insensitive(m.insensitive)
        .case_smart(m.smart_case)
        .unicode(!m.no_unicode)
        .word(m.word)
        .whole_line(m.whole_line)
        .multi_line(true)
        .dot_matches_new_line(m.dotall)
        .crlf(m.crlf);
    if m.multiline {
        builder.line_terminator(None);
    } else if !m.crlf || m.null_data {
        builder.line_terminator(Some(if m.null_data { b'\0' } else { b'\n' }));
    }
    Ok(builder.build_many(&m.patterns)?)
}

struct FileResult {
    bytes: Vec<u8>,
    matched: bool,
    stats: grep_printer::Stats,
}

fn search_one(
    path: &Path,
    matcher: &RegexMatcher,
    opts: &CommandOptions,
    colors: &ColorSpecs,
) -> Result<FileResult> {
    let m = &opts.matching;
    let p = &opts.print;
    let stdin = path == Path::new("-");
    let label = if stdin {
        p.label.as_deref().unwrap_or(Path::new("<stdin>"))
    } else {
        path
    };
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(p.line_number || p.mode == Mode::Json)
        .passthru(p.passthru)
        .invert_match(m.invert)
        .multi_line(m.multiline)
        .before_context(p.before)
        .after_context(p.after)
        .max_matches(m.max_count)
        .binary_detection(if m.text || m.null_data {
            BinaryDetection::none()
        } else if m.binary || stdin || opts.paths.iter().any(|root| root == path) {
            BinaryDetection::convert(0)
        } else {
            BinaryDetection::quit(0)
        });
    if m.null_data {
        builder.line_terminator(grep_matcher::LineTerminator::byte(0));
    } else if m.crlf {
        builder.line_terminator(grep_matcher::LineTerminator::crlf());
    }
    if let Some(encoding) = &m.encoding {
        if encoding == "none" {
            builder.encoding(None).bom_sniffing(false);
        } else if encoding != "auto" {
            builder.encoding(Some(Encoding::new(encoding)?));
        }
    }
    let mut searcher = builder.build();
    let mut buffer = if p.color {
        Buffer::ansi()
    } else {
        Buffer::no_color()
    };
    macro_rules! search {
        ($sink:expr) => {
            if stdin {
                searcher.search_reader(matcher, io::stdin().lock(), $sink)
            } else {
                searcher.search_path(matcher, path, $sink)
            }
        };
    }
    let (matched, stats) = match p.mode {
        Mode::Lines => {
            let mut printer = StandardBuilder::new();
            printer
                .stats(p.stats)
                .per_match(p.per_match)
                .per_match_one_line(p.per_match)
                .path(p.filename)
                .heading(p.heading)
                .color_specs(colors.clone())
                .column(p.column)
                .byte_offset(p.byte_offset)
                .only_matching(p.only_matching)
                .replacement(p.replacement.as_ref().map(|s| s.as_bytes().to_vec()))
                .trim_ascii(p.trim)
                .max_columns(p.max_columns)
                .max_columns_preview(p.max_columns_preview)
                .separator_context(p.context_separator.clone())
                .path_terminator(if p.null { Some(0) } else { None });
            let mut printer = printer.build(&mut buffer);
            let mut sink = printer.sink_with_path(matcher, label);
            search!(&mut sink)?;
            (sink.has_match(), sink.stats().cloned().unwrap_or_default())
        }
        Mode::Json => {
            let mut printer = JSONBuilder::new().build(&mut buffer);
            let mut sink = printer.sink_with_path(matcher, label);
            search!(&mut sink)?;
            (sink.has_match(), sink.stats().clone())
        }
        mode => {
            let kind = match mode {
                Mode::Count => SummaryKind::Count,
                Mode::CountMatches => SummaryKind::CountMatches,
                Mode::FilesWithMatches => SummaryKind::PathWithMatch,
                Mode::FilesWithoutMatch => SummaryKind::PathWithoutMatch,
                _ => SummaryKind::QuietWithMatch,
            };
            let mut printer = SummaryBuilder::new()
                .kind(kind)
                .stats(p.stats)
                .path(p.filename)
                .color_specs(colors.clone())
                .exclude_zero(!p.include_zero)
                .path_terminator(if p.null { Some(0) } else { None })
                .build(&mut buffer);
            let mut sink = printer.sink_with_path(matcher, label);
            search!(&mut sink)?;
            (sink.has_match(), sink.stats().cloned().unwrap_or_default())
        }
    };
    Ok(FileResult {
        bytes: buffer.as_slice().to_vec(),
        matched,
        stats,
    })
}

/// Run a CLI search, streaming each file's output under a single stdout lock.
/// Files are searched in parallel unless ordering was explicitly requested.
pub fn run(opts: &CommandOptions) -> Result<Outcome> {
    anyhow::ensure!(!opts.paths.is_empty(), "no input paths");
    let matcher = matcher(&opts.matching)?;
    let mut specs = grep_printer::default_color_specs();
    specs.extend(
        opts.print
            .color_specs
            .iter()
            .map(|s| s.parse())
            .collect::<Result<Vec<_>, _>>()?,
    );
    let colors = ColorSpecs::new(&specs);
    let excluded = if matches!(opts.print.mode, Mode::Files | Mode::FilesQuiet) {
        HashMap::new()
    } else {
        exclusions(opts)?
    };
    let started = std::time::Instant::now();
    let total_stats = Mutex::new(grep_printer::Stats::new());
    let outcome = Mutex::new(Outcome::default());
    let stop = AtomicBool::new(false);
    let stdout = Mutex::new(io::stdout());
    let mut paths = Vec::new();
    let report = |error: &dyn std::fmt::Display| {
        outcome.lock().unwrap().errors = true;
        if !opts.print.no_messages {
            eprintln!("rrg: {error}");
        }
    };
    for entry in walker(opts)?.build() {
        match entry {
            Err(err) => report(&err),
            Ok(entry) => {
                if let Some(err) = entry.error() {
                    report(err);
                }
                if entry.is_stdin() || entry.file_type().is_some_and(|t| t.is_file()) {
                    let path = if entry.is_stdin() {
                        PathBuf::from("-")
                    } else {
                        // rg elides ./ on directory traversal, preserving explicit file paths.
                        let path = entry.path();
                        if opts.strip_dot && entry.depth() > 0 {
                            path.strip_prefix(".").unwrap_or(path).to_path_buf()
                        } else {
                            path.to_path_buf()
                        }
                    };
                    paths.push(path);
                }
            }
        }
    }
    if opts.filter.sort_reverse {
        paths.reverse();
    }
    let process = |path: &PathBuf| {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let result = if matches!(opts.print.mode, Mode::Files | Mode::FilesQuiet) {
            let mut bytes = path.as_os_str().as_encoded_bytes().to_vec();
            bytes.push(if opts.print.null { 0 } else { b'\n' });
            if opts.print.mode == Mode::FilesQuiet {
                bytes.clear();
            }
            Ok(FileResult {
                bytes,
                matched: true,
                stats: Default::default(),
            })
        } else if unchanged_nonmatch(path, &excluded) {
            return;
        } else {
            search_one(path, &matcher, opts, &colors)
        };
        match result {
            Err(err) => report(&format!("{}: {err:#}", path.display())),
            Ok(file) => {
                *total_stats.lock().unwrap() += &file.stats;
                let mut state = outcome.lock().unwrap();
                if !file.bytes.is_empty() {
                    let mut writer = stdout.lock().unwrap();
                    let separator = if state.printed && opts.print.mode == Mode::Lines {
                        if opts.print.heading {
                            Some(Vec::new())
                        } else if !opts.print.passthru
                            && (opts.print.before > 0 || opts.print.after > 0)
                        {
                            opts.print.context_separator.clone()
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    state.printed = true;
                    let written = if let Some(separator) = separator {
                        writer
                            .write_all(&separator)
                            .and_then(|_| writer.write_all(b"\n"))
                    } else {
                        Ok(())
                    }
                    .and_then(|_| writer.write_all(&file.bytes));
                    if let Err(err) = written {
                        if err.kind() == io::ErrorKind::BrokenPipe {
                            state.broken_pipe = true;
                        } else {
                            state.errors = true;
                            if !opts.print.no_messages {
                                eprintln!("rrg: {err}");
                            }
                        }
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                state.matched |= file.matched;
                if matches!(opts.print.mode, Mode::Quiet | Mode::FilesQuiet) && file.matched {
                    stop.store(true, Ordering::Relaxed);
                }
            }
        }
    };
    if opts.filter.sort
        || opts.filter.sort_reverse
        || opts.threads == 1
        || paths.iter().any(|p| p == Path::new("-"))
    {
        for path in &paths {
            process(path);
        }
    } else {
        let mut pool = rayon::ThreadPoolBuilder::new();
        if opts.threads != 0 {
            pool = pool.num_threads(opts.threads);
        }
        pool.build()?.install(|| paths.par_iter().for_each(process));
    }
    let state = outcome.into_inner().unwrap();
    if !state.broken_pipe && (opts.print.mode == Mode::Json || opts.print.stats) {
        let stats = total_stats.into_inner().unwrap();
        let mut writer = stdout.into_inner().unwrap();
        if opts.print.mode == Mode::Json {
            let duration = started.elapsed();
            let summary = serde_json::json!({"type": "summary", "data": {
                "elapsed_total": {"secs": duration.as_secs(), "nanos": duration.subsec_nanos(), "human": format!("{:.6}s", duration.as_secs_f64())},
                "stats": stats
            }});
            serde_json::to_writer(&mut writer, &summary)?;
            writer.write_all(b"\n")?;
        } else {
            writeln!(
                writer,
                "\n{} matches\n{} matched lines\n{} files contained matches\n{} files searched\n{} bytes printed\n{} bytes searched\n{:.6} seconds spent searching\n{:.6} seconds total",
                stats.matches(),
                stats.matched_lines(),
                stats.searches_with_match(),
                stats.searches(),
                stats.bytes_printed(),
                stats.bytes_searched(),
                stats.elapsed().as_secs_f64(),
                started.elapsed().as_secs_f64()
            )?;
        }
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_excludes_only_unchanged_non_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("yes.txt"), "needle here\n").unwrap();
        std::fs::write(root.join("no.txt"), "totally different\n").unwrap();
        let index_dir = root.join(".rrg-index");
        let (idx, _) =
            crate::BigramIndex::build(&root, &Default::default(), Some(&index_dir)).unwrap();
        idx.save(&index_dir).unwrap();
        let mut opts = CommandOptions {
            paths: vec![root.clone()],
            strip_dot: false,
            threads: 1,
            index_root: Some(root.clone()),
            matching: MatchOptions {
                patterns: vec!["needle".into()],
                fixed: true,
                ..Default::default()
            },
            filter: Default::default(),
            print: Default::default(),
        };
        let excluded = exclusions(&opts).unwrap();
        assert!(unchanged_nonmatch(&root.join("no.txt"), &excluded));
        assert!(!unchanged_nonmatch(&root.join("yes.txt"), &excluded));
        std::fs::write(
            root.join("no.txt"),
            "needle now present with a different length\n",
        )
        .unwrap();
        std::fs::write(root.join("new.txt"), "needle new\n").unwrap();
        assert!(!unchanged_nonmatch(&root.join("no.txt"), &excluded));
        assert!(!unchanged_nonmatch(&root.join("new.txt"), &excluded));
        opts.matching.invert = true;
        assert!(exclusions(&opts).unwrap().is_empty());
        opts.matching.invert = false;
        opts.matching.insensitive = true;
        assert!(exclusions(&opts).unwrap().is_empty());
    }
}
