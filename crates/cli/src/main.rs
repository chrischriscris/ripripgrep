use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{ArgAction, CommandFactory, FromArgMatches, Parser, ValueEnum};
use rrg_core::command::{CommandOptions, FilterOptions, MatchOptions, Mode, PrintOptions};

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum Color {
    #[default]
    Auto,
    Always,
    Ansi,
    Never,
}
#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum Sort {
    Path,
    None,
}

/// Search files for patterns, with ripgrep-style arguments and output.
#[derive(Parser, Debug)]
#[command(
    name = "rrg",
    version,
    about,
    args_override_self = true,
    override_usage = "rrg [OPTIONS] PATTERN [PATH ...]\n       rrg [OPTIONS] -e PATTERN ... [PATH ...]\n       rrg [OPTIONS] -f PATTERNFILE ... [PATH ...]\n       rrg --files [PATH ...]\n       rrg --build-index [PATH]"
)]
struct Args {
    /// Pattern followed by files or directories. Use - to read stdin.
    #[arg(value_name = "PATTERN|PATH")]
    positional: Vec<OsString>,
    /// Search for each pattern (may be repeated).
    #[arg(short = 'e', long = "regexp", value_name = "PATTERN", action = ArgAction::Append, allow_hyphen_values = true)]
    patterns: Vec<String>,
    /// Read patterns from a file, one per line; - reads stdin.
    #[arg(short = 'f', long = "file", value_name = "PATTERNFILE")]
    pattern_files: Vec<PathBuf>,
    /// Treat patterns as literal strings.
    #[arg(short = 'F', long = "fixed-strings")]
    fixed: bool,
    /// Search case-insensitively.
    #[arg(short = 'i', long = "ignore-case", overrides_with_all = ["case_sensitive", "smart_case"])]
    insensitive: bool,
    /// Search case-sensitively (the default).
    #[arg(short = 's', long = "case-sensitive", overrides_with_all = ["insensitive", "smart_case"])]
    case_sensitive: bool,
    /// Ignore case unless the pattern contains uppercase letters.
    #[arg(short = 'S', long = "smart-case", overrides_with_all = ["case_sensitive", "insensitive"])]
    smart_case: bool,
    /// Only match whole words.
    #[arg(short = 'w', long = "word-regexp", overrides_with = "whole_line")]
    word: bool,
    /// Only match whole lines.
    #[arg(short = 'x', long = "line-regexp", overrides_with = "word")]
    whole_line: bool,
    /// Select non-matching lines.
    #[arg(short = 'v', long = "invert-match")]
    invert: bool,
    /// Allow matches to span multiple lines.
    #[arg(short = 'U', long = "multiline")]
    multiline: bool,
    /// Let a dot match newlines in multiline mode.
    #[arg(long = "multiline-dotall")]
    dotall: bool,
    /// Use CRLF-aware line anchors.
    #[arg(long)]
    crlf: bool,
    /// Use NUL as the line terminator.
    #[arg(long = "null-data")]
    null_data: bool,
    /// Disable Unicode-aware character classes.
    #[arg(long = "no-unicode")]
    no_unicode: bool,
    /// Search binary files as text.
    #[arg(short = 'a', long)]
    text: bool,
    /// Search binary files, reporting binary matches.
    #[arg(long)]
    binary: bool,
    /// Search using the specified encoding (auto, none, or an encoding label).
    #[arg(short = 'E', long)]
    encoding: Option<String>,
    /// Stop after this many matching lines per file.
    #[arg(short = 'm', long = "max-count")]
    max_count: Option<u64>,
    /// Print matching-line counts for each file.
    #[arg(short = 'c', long = "count", overrides_with = "count_matches")]
    count: bool,
    /// Print the number of individual matches per file.
    #[arg(long = "count-matches", overrides_with = "count")]
    count_matches: bool,
    /// Print only paths containing matches.
    #[arg(
        short = 'l',
        long = "files-with-matches",
        overrides_with = "files_without_match"
    )]
    files_with_matches: bool,
    /// Print only paths without matches.
    #[arg(long = "files-without-match", overrides_with = "files_with_matches")]
    files_without_match: bool,
    /// Print no output; exit 0 if a match exists, 1 otherwise.
    #[arg(short = 'q', long)]
    quiet: bool,
    /// List files that would be searched, without a pattern.
    #[arg(long, conflicts_with_all = ["patterns", "pattern_files", "json"])]
    files: bool,
    /// Emit newline-delimited JSON events and a summary.
    #[arg(long, conflicts_with_all = ["count", "count_matches", "files_with_matches", "files_without_match"])]
    json: bool,
    /// Show aggregate search statistics after the results.
    #[arg(long)]
    stats: bool,
    /// Print matching and non-matching lines.
    #[arg(long, alias = "passthrough", overrides_with_all = ["after", "before", "context"])]
    passthru: bool,
    /// Print each match as path:line:column:text for editor integration.
    #[arg(long)]
    vimgrep: bool,
    /// Force colors, headings, and line numbers.
    #[arg(short = 'p', long)]
    pretty: bool,
    /// Show line numbers (default when stdout is a terminal).
    #[arg(short = 'n', long = "line-number", overrides_with = "no_line_number")]
    line_number: bool,
    /// Suppress line numbers.
    #[arg(short = 'N', long = "no-line-number", overrides_with = "line_number")]
    no_line_number: bool,
    /// Print the path with each matching line.
    #[arg(short = 'H', long = "with-filename", overrides_with = "no_filename")]
    filename: bool,
    /// Suppress paths on matching lines.
    #[arg(short = 'I', long = "no-filename", overrides_with = "filename")]
    no_filename: bool,
    /// Group matches under a path heading.
    #[arg(long, overrides_with = "no_heading")]
    heading: bool,
    /// Print the path on each line instead of a heading.
    #[arg(long = "no-heading", overrides_with = "heading")]
    no_heading: bool,
    /// When to use terminal colors.
    #[arg(long, value_enum, default_value = "auto")]
    color: Color,
    /// Configure colors, e.g. match:fg:red (repeatable).
    #[arg(long = "colors")]
    colors: Vec<String>,
    /// Show 1-based column numbers.
    #[arg(long)]
    column: bool,
    /// Show 0-based byte offsets.
    #[arg(short = 'b', long = "byte-offset")]
    byte_offset: bool,
    /// Print only matched text.
    #[arg(short = 'o', long = "only-matching")]
    only_matching: bool,
    /// Replace matches in output; source files are never modified.
    #[arg(short = 'r', long = "replace", allow_hyphen_values = true)]
    replacement: Option<String>,
    /// Show this many lines after each match.
    #[arg(short = 'A', long = "after-context", overrides_with = "passthru")]
    after: Option<usize>,
    /// Show this many lines before each match.
    #[arg(short = 'B', long = "before-context", overrides_with = "passthru")]
    before: Option<usize>,
    /// Show this many lines before and after each match.
    #[arg(short = 'C', long, overrides_with = "passthru")]
    context: Option<usize>,
    /// Separate non-adjacent context groups with this text.
    #[arg(
        long = "context-separator",
        default_value = "--",
        overrides_with = "no_context_separator"
    )]
    context_separator: String,
    /// Suppress separators between context groups.
    #[arg(long = "no-context-separator", overrides_with = "context_separator")]
    no_context_separator: bool,
    /// Terminate file paths with NUL.
    #[arg(short = '0', long)]
    null: bool,
    /// Trim leading ASCII whitespace in output.
    #[arg(long)]
    trim: bool,
    /// Omit lines longer than this many bytes (0 disables the limit).
    #[arg(short = 'M', long = "max-columns")]
    max_columns: Option<u64>,
    /// Show a preview of lines exceeding the column limit.
    #[arg(long = "max-columns-preview")]
    max_columns_preview: bool,
    /// Include files with zero matches when printing counts.
    #[arg(long = "include-zero")]
    include_zero: bool,
    /// Suppress file traversal and read error messages.
    #[arg(long = "no-messages")]
    no_messages: bool,
    /// Set the displayed name for standard input.
    #[arg(long)]
    label: Option<PathBuf>,
    /// Search hidden files and directories.
    #[arg(long)]
    hidden: bool,
    /// -u ignores ignore files; -uu also searches hidden files; -uuu also searches binary files.
    #[arg(short = 'u', long = "unrestricted", action = ArgAction::Count)]
    unrestricted: u8,
    /// Do not respect ignore files.
    #[arg(long = "no-ignore")]
    no_ignore: bool,
    /// Do not respect version-control ignore rules.
    #[arg(long = "no-ignore-vcs")]
    no_ignore_vcs: bool,
    /// Do not respect global ignore rules.
    #[arg(long = "no-ignore-global")]
    no_ignore_global: bool,
    /// Do not read ignore files from parent directories.
    #[arg(long = "no-ignore-parent")]
    no_ignore_parent: bool,
    /// Follow symbolic links.
    #[arg(short = 'L', long)]
    follow: bool,
    /// Include or exclude paths matching this glob; prefix ! to exclude.
    #[arg(short = 'g', long = "glob", allow_hyphen_values = true)]
    globs: Vec<String>,
    /// Apply a case-insensitive glob.
    #[arg(long = "iglob", allow_hyphen_values = true)]
    iglobs: Vec<String>,
    /// Make all glob matching case-insensitive.
    #[arg(long = "glob-case-insensitive")]
    glob_case_insensitive: bool,
    /// Only search files of this type (repeatable).
    #[arg(short = 't', long = "type")]
    types: Vec<String>,
    /// Exclude files of this type (repeatable).
    #[arg(short = 'T', long = "type-not")]
    types_not: Vec<String>,
    /// Add a file type definition, e.g. web:*.html.
    #[arg(long = "type-add")]
    type_add: Vec<String>,
    /// List supported file types and their globs.
    #[arg(long = "type-list")]
    type_list: bool,
    /// Read additional ignore rules from this path.
    #[arg(long = "ignore-file")]
    ignore_files: Vec<PathBuf>,
    /// Descend at most this many directory levels.
    #[arg(long = "max-depth")]
    max_depth: Option<usize>,
    /// Skip files larger than this size; K, M and G suffixes are supported.
    #[arg(long = "max-filesize", value_parser = parse_size)]
    max_filesize: Option<u64>,
    /// Do not cross filesystem boundaries.
    #[arg(long = "one-file-system")]
    one_file_system: bool,
    /// Sort results by path, or use none for parallel traversal.
    #[arg(long, value_enum, overrides_with = "sortr")]
    sort: Option<Sort>,
    /// Sort results in reverse path order.
    #[arg(long, value_enum, overrides_with = "sort")]
    sortr: Option<Sort>,
    /// Number of search threads; 0 chooses automatically.
    #[arg(short = 'j', long, default_value = "0")]
    threads: usize,
    /// Ignore RIPGREP_CONFIG_PATH.
    #[arg(long = "no-config")]
    no_config: bool,
    /// Use the index to filter candidates, then verify live contents.
    #[arg(long = "use-index", conflicts_with_all = ["build_index", "files", "serve"])]
    use_index: bool,
    /// Build an index for a directory and exit.
    #[arg(long = "build-index", conflicts_with = "serve")]
    build_index: bool,
    /// Compatibility flag: indexed CLI searches always check file metadata.
    #[arg(long = "check-stale", requires = "use_index")]
    check_stale: bool,
    /// Compatibility flag: CLI verification runs in-process.
    #[arg(long = "no-daemon", requires = "use_index")]
    no_daemon: bool,
    #[arg(long, hide = true)]
    serve: bool,
}

fn parse_size(value: &str) -> std::result::Result<u64, String> {
    let (digits, scale) = match value.as_bytes().last() {
        Some(b'K') => (&value[..value.len() - 1], 1024),
        Some(b'M') => (&value[..value.len() - 1], 1024 * 1024),
        Some(b'G') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|v| v.checked_mul(scale))
        .ok_or_else(|| format!("invalid size: {value}"))
}

fn cli_args() -> Result<Vec<OsString>> {
    let raw: Vec<_> = std::env::args_os().collect();
    let mut args = vec![raw[0].clone()];
    let no_config = Args::try_parse_from(&raw).is_ok_and(|args| args.no_config);
    if !no_config
        && let Some(path) = std::env::var_os("RIPGREP_CONFIG_PATH").filter(|s| !s.is_empty())
    {
        let config = std::fs::read_to_string(&path).context("cannot read RIPGREP_CONFIG_PATH")?;
        for line in config.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                args.push(OsString::from(line));
            }
        }
    }
    args.extend(raw.into_iter().skip(1));
    Ok(args)
}

fn stdin_readable() -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata("/dev/stdin").is_ok_and(|m| {
        let t = m.file_type();
        t.is_file() || t.is_fifo() || t.is_socket()
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            if err
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
            {
                return ExitCode::SUCCESS;
            }
            eprintln!("rrg: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<u8> {
    let matches = Args::command().get_matches_from(cli_args()?);
    let mut args = Args::from_arg_matches(&matches)?;
    if args.build_index || args.serve {
        anyhow::ensure!(
            args.positional.len() <= 1 && args.patterns.is_empty() && args.pattern_files.is_empty(),
            "index commands accept one directory and no pattern"
        );
        let root = args
            .positional
            .first()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        if args.serve {
            rrg_core::serve::serve(&root, true)?;
        } else {
            let root = root.canonicalize()?;
            let (idx, timings) = rrg_core::BigramIndex::build(
                &root,
                &Default::default(),
                Some(&root.join(".rrg-index")),
            )?;
            idx.save(&root.join(".rrg-index"))?;
            eprintln!(
                "indexed {} files in {:.2?}; saved to {}",
                timings.files,
                timings.walk + timings.fill + timings.trigrams,
                root.join(".rrg-index").display()
            );
        }
        return Ok(0);
    }
    if args.type_list {
        let mut out = io::stdout().lock();
        for (name, globs) in rrg_core::command::file_types(&args.type_add)? {
            writeln!(out, "{name}: {}", globs.join(", "))?;
        }
        return Ok(0);
    }
    let explicit_patterns = !args.patterns.is_empty() || !args.pattern_files.is_empty();
    let stdin_patterns = args.pattern_files.iter().any(|p| p == Path::new("-"));
    if !explicit_patterns && !args.files {
        anyhow::ensure!(
            !args.positional.is_empty(),
            "a pattern is required; see --help"
        );
        let pattern = args
            .positional
            .remove(0)
            .into_string()
            .map_err(|_| anyhow::anyhow!("pattern is not valid UTF-8"))?;
        args.patterns.push(pattern);
    }
    for path in &args.pattern_files {
        let mut text = String::new();
        if path == Path::new("-") {
            io::stdin().read_to_string(&mut text)?;
        } else {
            text = std::fs::read_to_string(path).with_context(|| format!("{}", path.display()))?;
        }
        args.patterns.extend(text.lines().map(str::to_owned));
    }
    let patterns = args.patterns;
    let implicit_paths = args.positional.is_empty();
    let mut paths: Vec<PathBuf> = args.positional.into_iter().map(PathBuf::from).collect();
    if paths.is_empty() {
        paths.push(
            if !args.files && !args.use_index && !stdin_patterns && stdin_readable() {
                PathBuf::from("-")
            } else {
                PathBuf::from(".")
            },
        );
    }
    let index_root = if args.use_index {
        anyhow::ensure!(
            paths.len() == 1 && paths[0].is_dir(),
            "--use-index requires one directory"
        );
        Some(paths[0].clone())
    } else {
        None
    };
    let terminal = io::stdout().is_terminal();
    let mode = if args.quiet && args.files {
        Mode::FilesQuiet
    } else if args.quiet {
        Mode::Quiet
    } else if args.files {
        Mode::Files
    } else if args.files_with_matches {
        Mode::FilesWithMatches
    } else if args.files_without_match {
        Mode::FilesWithoutMatch
    } else if args.count_matches || args.count && args.only_matching {
        Mode::CountMatches
    } else if args.count {
        Mode::Count
    } else if args.json {
        Mode::Json
    } else {
        Mode::Lines
    };
    let filename = !args.no_filename
        && (args.filename || args.vimgrep || paths.len() > 1 || paths[0].is_dir());
    let line_number = !args.no_line_number
        && (args.line_number
            || args.column
            || args.vimgrep
            || args.pretty
            || terminal && mode == Mode::Lines && paths[0] != Path::new("-"));
    let color = if args.pretty && !matches!(args.color, Color::Never) {
        true
    } else {
        match args.color {
            Color::Always | Color::Ansi => true,
            Color::Never => false,
            Color::Auto => {
                terminal
                    && std::env::var_os("NO_COLOR").is_none()
                    && std::env::var("TERM").ok().as_deref() != Some("dumb")
            }
        }
    };
    let mut globs = Vec::new();
    for (key, values, insensitive) in [
        ("globs", &args.globs, args.glob_case_insensitive),
        ("iglobs", &args.iglobs, true),
    ] {
        if let Some(indices) = matches.indices_of(key) {
            globs.extend(
                indices
                    .zip(values)
                    .map(|(i, g)| (i, g.clone(), insensitive)),
            );
        }
    }
    globs.sort_by_key(|(i, _, _)| *i);
    let opts = CommandOptions {
        paths,
        strip_dot: implicit_paths,
        threads: args.threads,
        index_root,
        matching: MatchOptions {
            patterns,
            fixed: args.fixed,
            insensitive: args.insensitive,
            smart_case: args.smart_case,
            word: args.word,
            whole_line: args.whole_line,
            invert: args.invert,
            multiline: args.multiline,
            dotall: args.dotall,
            crlf: args.crlf,
            null_data: args.null_data,
            no_unicode: args.no_unicode,
            text: args.text,
            binary: args.binary || args.unrestricted >= 3,
            encoding: args.encoding,
            max_count: args.max_count,
        },
        filter: FilterOptions {
            hidden: args.hidden || args.unrestricted >= 2,
            no_ignore: args.no_ignore || args.unrestricted >= 1,
            no_ignore_vcs: args.no_ignore_vcs,
            no_ignore_global: args.no_ignore_global,
            no_ignore_parent: args.no_ignore_parent,
            follow: args.follow,
            globs: globs.into_iter().map(|(_, g, i)| (g, i)).collect(),
            types: args.types,
            types_not: args.types_not,
            type_add: args.type_add,
            ignore_files: args.ignore_files,
            max_depth: args.max_depth,
            max_filesize: args.max_filesize,
            one_file_system: args.one_file_system,
            sort: args.sort == Some(Sort::Path),
            sort_reverse: args.sortr == Some(Sort::Path),
        },
        print: PrintOptions {
            mode,
            stats: args.stats && !args.quiet,
            passthru: args.passthru,
            per_match: args.vimgrep,
            filename,
            line_number,
            heading: !args.vimgrep && !args.no_heading && (args.heading || args.pretty || terminal),
            color,
            color_specs: args.colors,
            column: args.column || args.vimgrep,
            byte_offset: args.byte_offset,
            only_matching: args.only_matching,
            replacement: args.replacement,
            before: args.before.or(args.context).unwrap_or(0),
            after: args.after.or(args.context).unwrap_or(0),
            context_separator: if args.no_context_separator {
                None
            } else {
                Some(args.context_separator.into_bytes())
            },
            null: args.null,
            trim: args.trim,
            max_columns: args.max_columns.filter(|&n| n != 0),
            max_columns_preview: args.max_columns_preview,
            include_zero: args.include_zero,
            no_messages: args.no_messages,
            label: args.label,
        },
    };
    Ok(rrg_core::command::run(&opts)?.exit_code(args.quiet))
}
