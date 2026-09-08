//! M0 baseline search: parallel walk + ripgrep's matcher/searcher.
//!
//! This is deliberately "just" ripgrep's architecture reassembled as a
//! library. Its job is to be the parity floor we measure every future
//! change against. Replace pieces; never regress against this.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, WalkState};
use rayon::prelude::*;

/// Minimal counting sink: one line of state, zero allocation per match.
struct MatchCounter(usize);

impl Sink for MatchCounter {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        self.0 += 1;
        let _ = mat;
        Ok(true)
    }
}

/// Aggregate stats for a search run.
#[derive(Debug, Default, Clone)]
pub struct SearchStats {
    pub files_visited: usize,
    pub files_matched: usize,
    pub matches: usize,
    pub errors: usize,
    /// Lines-mode output (empty unless mode == Lines).
    pub output: String,
    /// Files-mode output (empty unless mode == Files).
    pub files: Vec<String>,
}

impl std::ops::Add for SearchStats {
    type Output = SearchStats;
    fn add(self, o: SearchStats) -> SearchStats {
        SearchStats {
            files_visited: self.files_visited + o.files_visited,
            files_matched: self.files_matched + o.files_matched,
            matches: self.matches + o.matches,
            errors: self.errors + o.errors,
            output: {
                let mut out = self.output;
                if out.is_empty() {
                    o.output
                } else {
                    out.push_str(&o.output);
                    out
                }
            },
            files: {
                let mut f = self.files;
                f.extend(o.files);
                f
            },
        }
    }
}

/// Options for [`search_path`].
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Case-insensitive matching.
    pub case_insensitive: bool,
    /// Interpret the pattern as a fixed string, not a regex.
    pub fixed_strings: bool,
    /// Apply .gitignore and friends (true matches ripgrep's default).
    pub respect_gitignore: bool,
    /// Follow symbolic links.
    pub follow_symlinks: bool,
    /// What to produce per match.
    pub mode: OutputMode,
}

/// What the search should emit (rg-shaped output modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    /// Aggregate counts only (benchmark + daemon-protocol default).
    #[default]
    Count,
    /// Print `path:line:text` for every matching line.
    Lines,
    /// Print only the paths of files with matches.
    Files,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            case_insensitive: false,
            fixed_strings: false,
            respect_gitignore: true,
            follow_symlinks: false,
            mode: OutputMode::Count,
        }
    }
}

/// Sink that formats matching lines rg-style (`path:line:text`). One
/// output event per matching line; repeated events for the same line
/// (possible with multi-match lines) are coalesced, matching rg's visual
/// output. Line numbers come from the searcher (it tracks them natively).
struct LineSink<'a> {
    out: &'a mut Vec<u8>,
    path: &'a [u8],
    last_line: u64,
    lines: usize,
}

impl Sink for LineSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _s: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let ln = mat.line_number().unwrap_or(0);
        if ln != 0 && ln == self.last_line {
            return Ok(true); // same line, another match — coalesce
        }
        self.last_line = ln;
        self.lines += 1;
        self.out.extend_from_slice(self.path);
        self.out.push(b':');
        self.out.extend_from_slice(ln.to_string().as_bytes());
        self.out.push(b':');
        if let Some(line) = mat.lines().next() {
            self.out.extend_from_slice(line);
        }
        Ok(true)
    }
}

/// Search one in-memory slice (pack-backed verification) in `mode`.
/// Returns (matched_any, matching_lines, formatted_output for Lines).
pub fn scan_slice(
    matcher: &grep_regex::RegexMatcher,
    path: &Path,
    bytes: &[u8],
    opts: &SearchOptions,
    out: &mut Vec<u8>,
) -> (bool, usize) {
    if bytes.is_empty() {
        return (false, 0);
    }
    match opts.mode {
        OutputMode::Count => {
            // literals never get here through the verifier fast path;
            // this is the regex counting fallback
            let mut sink = MatchCounter(0);
            let mut searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .build();
            match searcher.search_slice(matcher, bytes, &mut sink) {
                Ok(()) => (sink.0 > 0, sink.0),
                Err(_) => (false, 0),
            }
        }
        OutputMode::Lines | OutputMode::Files => {
            let path_bytes = path.as_os_str().as_encoded_bytes().to_vec();
            let mut sink = LineSink {
                out,
                path: &path_bytes,
                last_line: 0,
                lines: 0,
            };
            let mut searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .line_number(true)
                .build();
            match searcher.search_slice(matcher, bytes, &mut sink) {
                Ok(()) => (sink.lines > 0, sink.lines),
                Err(_) => (false, 0),
            }
        }
    }
}

/// Search `root` for `pattern`, returning aggregate stats.
///
/// Parallel walk via `ignore`'s threaded walker; per-file counting via
/// ripgrep's searcher. File-count sink per file keeps overhead minimal —
/// line-level output comes later, once we own the pipeline.
pub fn search_path(pattern: &str, root: &Path, opts: &SearchOptions) -> Result<SearchStats> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .fixed_strings(opts.fixed_strings)
        .build(pattern)?;

    let files_visited = AtomicUsize::new(0);
    let files_matched = AtomicUsize::new(0);
    let matches = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);
    let out_buf = Mutex::new(Vec::new());
    let files_out = Mutex::new(Vec::new());

    let walker = WalkBuilder::new(root)
        .git_ignore(opts.respect_gitignore)
        .git_global(opts.respect_gitignore)
        .ignore(opts.respect_gitignore)
        .parents(opts.respect_gitignore)
        .follow_links(opts.follow_symlinks)
        .threads(std::thread::available_parallelism().map_or(4, |n| n.get()))
        .build_parallel();

    walker.run(|| {
        let matcher = matcher.clone();
        let files_visited = &files_visited;
        let files_matched = &files_matched;
        let matches = &matches;
        let errors = &errors;
        let out_buf = &out_buf;
        let files_out = &files_out;

        // Reuse the searcher across files on this worker thread instead of
        // rebuilding per file (rg does the same; saves allocs per file).
        thread_local! {
            static SEARCHER: std::cell::RefCell<Searcher> =
                std::cell::RefCell::new(
                    SearcherBuilder::new()
                        .binary_detection(BinaryDetection::quit(b'\x00'))
                        .build(),
                );
        }

        Box::new(
            move |entry: Result<ignore::DirEntry, ignore::Error>| -> WalkState {
                let Ok(entry) = entry else {
                    errors.fetch_add(1, Ordering::Relaxed);
                    return WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    return WalkState::Continue;
                }
                files_visited.fetch_add(1, Ordering::Relaxed);

                let n: Result<usize, ()> = SEARCHER.with(|s| {
                    let mut s = s.borrow_mut();
                    match opts.mode {
                        OutputMode::Count => {
                            let mut sink = MatchCounter(0);
                            let r = s.search_path(&matcher, entry.path(), &mut sink);
                            Ok(r.map(|_| sink.0).unwrap_or(0))
                        }
                        OutputMode::Lines => {
                            let mut buf = Vec::with_capacity(4096);
                            let path_bytes = entry.path().as_os_str().as_encoded_bytes().to_vec();
                            let mut sink = LineSink {
                                out: &mut buf,
                                path: &path_bytes,
                                last_line: 0,
                                lines: 0,
                            };
                            let r = s.search_path(&matcher, entry.path(), &mut sink);
                            let n = r.map(|_| sink.lines).unwrap_or(0);
                            if n > 0 {
                                out_buf.lock().unwrap().extend_from_slice(&buf);
                            }
                            Ok(n)
                        }
                        OutputMode::Files => {
                            let mut sink = MatchCounter(0);
                            let r = s.search_path(&matcher, entry.path(), &mut sink);
                            let n = r.map(|_| sink.0).unwrap_or(0);
                            if n > 0 {
                                files_out.lock().unwrap().push(entry.path().to_path_buf());
                            }
                            Ok(n)
                        }
                    }
                });
                match n {
                    Ok(x) if x > 0 => {
                        files_matched.fetch_add(1, Ordering::Relaxed);
                        matches.fetch_add(x, Ordering::Relaxed);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                WalkState::Continue
            },
        )
    });

    Ok(SearchStats {
        files_visited: files_visited.load(Ordering::Relaxed),
        files_matched: files_matched.load(Ordering::Relaxed),
        matches: matches.load(Ordering::Relaxed),
        errors: errors.load(Ordering::Relaxed),
        output: String::from_utf8_lossy(&out_buf.into_inner().unwrap()).into_owned(),
        files: files_out
            .into_inner()
            .unwrap()
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
    })
}

/// Search an explicit list of files (index candidates + fallbacks),
/// returning aggregate stats. Order-independent; parallel via rayon.
pub fn search_files(pattern: &str, files: &[PathBuf], opts: &SearchOptions) -> Result<SearchStats> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .fixed_strings(opts.fixed_strings)
        .build(pattern)?;

    let files_matched = AtomicUsize::new(0);
    let matches_total = AtomicUsize::new(0);
    let out_buf: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    let files_out: Mutex<Vec<String>> = Mutex::new(Vec::new());

    files.par_iter().for_each(|path| {
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .line_number(true)
            .build();
        match opts.mode {
            OutputMode::Count => {
                let mut sink = MatchCounter(0);
                if searcher.search_path(&matcher, path, &mut sink).is_ok() && sink.0 > 0 {
                    files_matched.fetch_add(1, Ordering::Relaxed);
                    matches_total.fetch_add(sink.0, Ordering::Relaxed);
                }
            }
            OutputMode::Lines => {
                let mut buf = Vec::with_capacity(4096);
                let path_bytes = path.as_os_str().as_encoded_bytes().to_vec();
                let mut sink = LineSink {
                    out: &mut buf,
                    path: &path_bytes,
                    last_line: 0,
                    lines: 0,
                };
                if searcher.search_path(&matcher, path, &mut sink).is_ok() && sink.lines > 0 {
                    files_matched.fetch_add(1, Ordering::Relaxed);
                    matches_total.fetch_add(sink.lines, Ordering::Relaxed);
                    out_buf.lock().unwrap().extend_from_slice(&buf);
                }
            }
            OutputMode::Files => {
                let mut sink = MatchCounter(0);
                if searcher.search_path(&matcher, path, &mut sink).is_ok() && sink.0 > 0 {
                    files_matched.fetch_add(1, Ordering::Relaxed);
                    matches_total.fetch_add(sink.0, Ordering::Relaxed);
                    files_out.lock().unwrap().push(path.display().to_string());
                }
            }
        }
    });

    Ok(SearchStats {
        files_visited: files.len(),
        files_matched: files_matched.load(Ordering::Relaxed),
        matches: matches_total.load(Ordering::Relaxed),
        errors: 0,
        output: String::from_utf8_lossy(&out_buf.into_inner().unwrap()).into_owned(),
        files: files_out.into_inner().unwrap(),
    })
}

/// Fast path: count matching LINES of a fixed string across `files` using
/// mmap + SIMD memmem (memchr's NEON/SSE kernels). No line splitting, no
/// Sink machinery, zero-copy haystacks. Semantics identical to `rg -c`
/// (matching lines, binary-quit at first NUL anywhere).
pub fn count_occurrences_mmap(
    needle: &str,
    files: &[PathBuf],
    case_insensitive: bool,
) -> SearchStats {
    let needle: Needle = if case_insensitive {
        Needle::Aho(
            aho_corasick::AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([needle])
                .expect("valid needle"),
        )
    } else {
        Needle::Mem(memchr::memmem::Finder::new(needle))
    };

    let files_visited = AtomicUsize::new(0);
    let files_matched = AtomicUsize::new(0);
    let matches = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);

    // Returns (matched_any, matching_lines) for one file.
    let count_one = |path: &PathBuf| -> (bool, usize) {
        // mmap (zero-copy); fall back to read on failure (empty/pipes/etc).
        let mmap = fs::File::open(path)
            .ok()
            .and_then(|f| unsafe { memmap2::Mmap::map(&f).ok() });
        let read_buf: Option<Vec<u8>> = if mmap.is_some() {
            None
        } else {
            Some(fs::read(path).unwrap_or_default())
        };
        let bytes: &[u8] = match &mmap {
            Some(m) => &m[..],
            None => read_buf.as_deref().unwrap_or(&[]),
        };
        if bytes.is_empty() {
            return (false, 0);
        }
        // Binary-quit semantics, identical to BinaryDetection::quit: matches
        // after the first NUL anywhere are dropped. Scan for NUL lazily and
        // only up to the last match position — the common case (no NUL before
        // any match, or no matches at all) pays a bounded or zero cost.
        let (n, last_at) = needle.matching_lines_and_last(bytes);
        if n == 0 {
            return (false, 0);
        }
        let n = match memchr::memchr(b'\0', &bytes[..=last_at]) {
            None => n,
            Some(end) => needle.matching_lines(&bytes[..end]),
        };
        (n > 0, n)
    };

    files.par_iter().for_each(|path| {
        files_visited.fetch_add(1, Ordering::Relaxed);
        if fs::File::open(path).is_err() {
            errors.fetch_add(1, Ordering::Relaxed);
        }
        let (hit, n) = count_one(path);
        if hit {
            files_matched.fetch_add(1, Ordering::Relaxed);
            matches.fetch_add(n, Ordering::Relaxed);
        }
    });

    SearchStats {
        files_visited: files_visited.load(Ordering::Relaxed),
        files_matched: files_matched.load(Ordering::Relaxed),
        matches: matches.load(Ordering::Relaxed),
        errors: errors.load(Ordering::Relaxed),
        output: String::new(),
        files: Vec::new(),
    }
}

/// Search a raw byte slice with the regex engine, returning aggregate stats
/// (pack-backed verification path for regex queries).
pub fn search_slice_lines(
    pattern: &str,
    bytes: &[u8],
    opts: &SearchOptions,
) -> Result<SearchStats> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .fixed_strings(opts.fixed_strings)
        .build(pattern)?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();
    let mut sink = MatchCounter(0);
    let visited = if bytes.is_empty() { 0 } else { 1 };
    let matched = match searcher.search_slice(&matcher, bytes, &mut sink) {
        Ok(()) => (sink.0 > 0) as usize,
        Err(_) => 0,
    };
    Ok(SearchStats {
        files_visited: visited,
        files_matched: matched,
        matches: sink.0,
        errors: 0,
        output: String::new(),
        files: Vec::new(),
    })
}

/// Per-query verification engine: compile ONCE, reuse across every
/// candidate slice (thread-local Searcher reuse for the regex path).
/// Compiling the matcher per candidate dominated regex query time.
pub enum SliceVerifier {
    /// Case-sensitive literal: memmem Finder per slice is O(1)-cheap.
    LiteralCS(String),
    /// Case-insensitive literal: one shared Aho-Corasick automaton.
    LiteralCI(std::sync::Arc<aho_corasick::AhoCorasick>),
    /// Regex: one shared compiled matcher + per-thread Searcher.
    Regex(std::sync::Arc<grep_regex::RegexMatcher>),
}

impl SliceVerifier {
    pub fn new(pattern: &str, fixed_strings: bool, case_insensitive: bool) -> Result<Self> {
        Ok(if fixed_strings {
            if case_insensitive {
                SliceVerifier::LiteralCI(std::sync::Arc::new(
                    aho_corasick::AhoCorasick::builder()
                        .ascii_case_insensitive(true)
                        .build([pattern])?,
                ))
            } else {
                SliceVerifier::LiteralCS(pattern.to_string())
            }
        } else {
            SliceVerifier::Regex(std::sync::Arc::new(
                RegexMatcherBuilder::new()
                    .case_insensitive(case_insensitive)
                    .build(pattern)?,
            ))
        })
    }

    /// Matching-line count for one candidate slice (rg -c semantics:
    /// matching lines, binary-quit at first NUL anywhere).
    pub fn count_matching_lines(&self, bytes: &[u8]) -> usize {
        match self {
            SliceVerifier::LiteralCS(needle) => count_matching_lines_bytes(needle, bytes, false),
            SliceVerifier::LiteralCI(aho) => {
                let n = Needle::AhoRef(aho);
                count_lines_with(&n, bytes)
            }
            SliceVerifier::Regex(matcher) => {
                if bytes.is_empty() {
                    return 0;
                }
                thread_local! {
                    static SEARCHER: std::cell::RefCell<Searcher> =
                        std::cell::RefCell::new(
                            SearcherBuilder::new()
                                .binary_detection(BinaryDetection::quit(b'\x00'))
                                .build(),
                        );
                }
                SEARCHER.with(|s| {
                    let mut s = s.borrow_mut();
                    let mut sink = MatchCounter(0);
                    match s.search_slice(&**matcher, bytes, &mut sink) {
                        Ok(()) => sink.0,
                        Err(_) => 0,
                    }
                })
            }
        }
    }
}

/// Count matching lines of a literal in a raw byte slice (pack-backed
/// verification path). Binary-quit semantics like the searcher.
pub fn count_matching_lines_bytes(needle: &str, bytes: &[u8], case_insensitive: bool) -> usize {
    let n: Needle = if case_insensitive {
        Needle::Aho(
            aho_corasick::AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([needle])
                .expect("valid needle"),
        )
    } else {
        Needle::Mem(memchr::memmem::Finder::new(needle))
    };
    count_lines_with(&n, bytes)
}

fn count_lines_with(n: &Needle<'_>, bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let (lines, last_at) = n.matching_lines_and_last(bytes);
    if lines == 0 {
        return 0;
    }
    match memchr::memchr(b'\0', &bytes[..=last_at]) {
        None => lines,
        Some(end) => n.matching_lines(&bytes[..end]),
    }
}

/// Searcher over a single literal, dispatching to the fastest backend.
enum Needle<'n> {
    /// SIMD memmem (memchr) — case-sensitive only.
    Mem(memchr::memmem::Finder<'n>),
    /// Aho-Corasick automaton — supports ASCII case-insensitivity.
    Aho(aho_corasick::AhoCorasick),
    /// Borrowed shared automaton (per-query verifier path).
    AhoRef(&'n aho_corasick::AhoCorasick),
}

impl Needle<'_> {
    /// Count distinct matching LINES. Matches arrive in ascending order, so
    /// tracking the previously-counted line start suffices; this counts a
    /// line once no matter how many matches it holds or where they sit.
    /// Returns (lines, byte offset of the last match).
    fn matching_lines_and_last(&self, bytes: &[u8]) -> (usize, usize) {
        let mut lines = 0usize;
        let mut last_line_start = usize::MAX;
        let mut last_at = 0usize;
        let mut count_at = |at: usize| {
            let line_start = memchr::memrchr(b'\n', &bytes[..at]).map_or(0, |i| i + 1);
            if line_start != last_line_start {
                lines += 1;
                last_line_start = line_start;
            }
            last_at = at;
        };
        match self {
            Needle::Mem(f) => {
                for at in f.find_iter(bytes) {
                    count_at(at);
                }
            }
            Needle::Aho(a) => {
                for m in a.find_iter(bytes) {
                    count_at(m.start());
                }
            }
            Needle::AhoRef(a) => {
                for m in a.find_iter(bytes) {
                    count_at(m.start());
                }
            }
        }
        (lines, last_at)
    }

    /// Recount over a (possibly truncated) haystack.
    fn matching_lines(&self, bytes: &[u8]) -> usize {
        self.matching_lines_and_last(bytes).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn line_sink_matches_rg_semantics() {
        let dir = TempDir::new().unwrap();
        // 3 matches on line 1 must emit ONE line; binary-quit at NUL.
        fs::write(dir.path().join("a.txt"), "TODO TODO TODO\nplain\nTODO\n").unwrap();
        fs::write(dir.path().join("b.txt"), "TODO\0TODO\n").unwrap();
        let files = vec![dir.path().join("a.txt"), dir.path().join("b.txt")];
        let opts = SearchOptions {
            mode: OutputMode::Lines,
            ..Default::default()
        };
        let stats = search_files("TODO", &files, &opts).unwrap();
        assert_eq!(stats.files_matched, 1); // b.txt binary-quit before any line
        assert_eq!(stats.matches, 2);
        let lines: Vec<&str> = stats.output.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "one output line per MATCHING line: {lines:?}"
        );
        assert!(lines[0].contains("a.txt:1:TODO TODO TODO"), "{lines:?}");
        assert!(lines[1].contains("a.txt:3:TODO"), "{lines:?}");
    }

    #[test]
    fn mmap_counter_matches_searcher_semantics() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "needle x needle\nplain\nneedle\n").unwrap();
        fs::write(dir.path().join("c.txt"), "abc needle here\n").unwrap(); // mid-line only
        fs::write(dir.path().join("d.txt"), "needle\0needle\n").unwrap(); // binary-quit
        let files = vec![
            dir.path().join("a.txt"),
            dir.path().join("c.txt"),
            dir.path().join("d.txt"),
        ];

        // 4 matching lines: a.txt line1 (2 matches, counts once), a.txt
        // line3, c.txt mid-line, d.txt pre-NUL line (quit semantics).
        let stats = count_occurrences_mmap("needle", &files, false);
        assert_eq!(stats.files_matched, 3);
        assert_eq!(stats.matches, 4);

        let ci = count_occurrences_mmap("NEEDLE", &files, true);
        assert_eq!(ci.matches, 4);
        let cs = count_occurrences_mmap("NEEDLE", &files, false);
        assert_eq!(cs.matches, 0);

        // No-match files must not inflate files_matched (P0-2 regression).
        let only_b = vec![dir.path().join("c.txt")];
        let none = count_occurrences_mmap("zzzz", &only_b, false);
        assert_eq!(none.files_matched, 0);
        assert_eq!(none.matches, 0);
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn finds_matches_across_files() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("a.rs"), "fn main() {}\nfn helper() {}\n");
        write(&dir.path().join("sub/b.py"), "def main(): pass\n");

        let stats = search_path(
            "fn main",
            dir.path(),
            &SearchOptions {
                fixed_strings: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(stats.files_matched, 1); // only a.rs contains "fn main"
        assert_eq!(stats.matches, 1);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn respects_gitignore() {
        let dir = TempDir::new().unwrap();
        // The ignore crate requires a git repo for .gitignore to apply
        // (matches ripgrep's default); TempDir isn't one, so fake it.
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        write(&dir.path().join(".gitignore"), "ignored/\n");
        write(&dir.path().join("kept.txt"), "needle\n");
        write(&dir.path().join("ignored/skip.txt"), "needle\n");

        let stats = search_path("needle", dir.path(), &SearchOptions::default()).unwrap();

        assert_eq!(stats.files_visited, 1); // kept.txt only (.gitignore is hidden, skipped by default like rg)
        assert_eq!(stats.files_matched, 1);
    }
}
