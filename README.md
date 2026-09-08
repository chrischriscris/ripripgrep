# ripripgrep

`rrg` is an experimental Rust search tool with a ripgrep-style CLI and optional
indexed queries. It uses ripgrep's matching, printing, and ignore libraries,
with rrg's bigram/trigram index for candidate filtering. It does not launch `rg`
or require it to be installed at runtime.

## Install

Linux or macOS and a current stable Rust toolchain are required. From a checkout:

```sh
cargo install --path crates/cli --locked
rrg --help
```

To build without installing, run `cargo build --release --locked` and use
`./target/release/rrg`. Windows and prebuilt binary releases are not available yet.

## Use it like rg

```sh
rrg 'fn main'                    # search the current directory
rrg -n 'fn main' crates/         # include line numbers
rrg -F 'SearchOptions' crates/   # literal search
rrg -i 'todo' src/ tests/        # multiple paths, ignore case
rrg -S 'todo' .                 # smart case
rrg -e 'TODO' -e 'FIXME' .       # multiple patterns
rrg -f patterns.txt .           # patterns from a file
rrg -g '*.rs' -g '!target/**' 'fn' .
rrg -t rust -C 2 'unsafe' .     # file types and context
rrg -c 'needle' .               # matching-line count per file
rrg -l 'needle' .               # only paths with matches
rrg --files -t rust             # list searchable Rust files
rrg --json 'needle' .           # JSON events for integrations
printf 'hello\nworld\n' | rrg 'world'
```

When stdout is a terminal, matches have colors, headings, and line numbers. In
pipes and redirected output, matching lines use plain text without automatic
line numbers. A single file or stdin omits the filename unless `-H` is supplied.
Use `--color`, `--heading`/`--no-heading`, `-n`/`-N`, and `-H`/`-I` to override.

Exit codes follow rg: **0** means a match, **1** means no match, and **2** means an
error. `-q` stops after a match without output. Search output contains no timing
or status chatter; `--stats` explicitly requests statistics.

Ignore files and hidden-file filtering work by default. `--hidden` includes
hidden files, `-u` disables ignore rules, `-uu` also includes hidden files, and
`-uuu` additionally searches binary files. Explicitly named files bypass normal
traversal filtering. `-L` follows symlinks.

`RIPGREP_CONFIG_PATH` is supported: its file contains one argument per line,
with blank lines and lines beginning with `#` ignored. Command-line options
follow configuration options. Use `--no-config` to disable configuration.

See [CLI compatibility](docs/cli-compatibility.md) for supported behavior and
remaining differences. This is not yet a complete implementation of every rg flag.

## Indexed queries

```sh
rrg --build-index .
rrg --use-index -F 'SearchOptions' .
rrg --use-index -n -C 2 -F 'SearchOptions' .
```

The index narrows candidate files; the same traversal and printer used for scans
handle the rest. The CLI discovers new files, checks build-time metadata before
excluding indexed files, and reads live contents for verification. Changed and
unindexed files are searched normally. Counts, context, filters, and exit codes
work the same way in both modes.

Candidate filtering currently applies to case-sensitive ASCII literal queries.
Regexes, case folding, transcoding, inverse searches, and other queries without a
safe candidate proof fall back to live scanning. Old indexes with relative paths
also fall back; rebuild to enable filtering. `--use-index` accepts one directory.

CLI queries run in-process. `--no-daemon` and `--check-stale` remain accepted for
existing scripts; metadata checks are always performed. The experimental daemon
and packed-query API remain available in the core library, but are not used by
the compatible CLI. This trades some warm-query speed for consistent behavior.

Indexes in `.rrg-index/` include packed source contents. Keep them local and load
only indexes you built yourself. Metadata validation is not snapshot isolation:
concurrent edits, preserved timestamps, and concurrent index rebuilds are not
covered by a consistency guarantee. Index formats and library APIs may change.

## Development

```sh
cargo test --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked
cargo bench -p rrg-core --bench baseline --locked
python3 scripts/check-terminal.py target/debug/rrg
```

Install `rg` 15.2.0 to run differential CLI tests. CI pins this version on both
Linux and macOS; set `RRG_REQUIRE_RG=1` locally to fail instead of skipping those tests
when rg is missing. The PTY test checks actual terminal rendering. See
[CONTRIBUTING.md](CONTRIBUTING.md).

- `crates/core`: scanning, index construction/storage, CLI search execution,
  query planning, and the experimental daemon.
- `crates/cli`: argument parsing and end-to-end compatibility tests.
- `crates/core/tests`: index differential and property tests.
- `scripts`: corpus setup, benchmarks, and terminal compatibility checks.

## Benchmarking

Install `hyperfine` and `rg`, then run:

```sh
cargo build --release --locked
scripts/setup-corpora.sh
scripts/bench.sh ~/.cache/rrg-corpora/ripgrep --quick
```

Repeated runs warm filesystem caches; this is not a cold-cache benchmark and
does not measure indexed queries. Record hardware, OS, Rust version, corpus
revision, tool versions, and commands with results. No general performance
advantage is claimed. Generated `bench-*.json` files are ignored.

## License and acknowledgments

Licensed under either [MIT](LICENSE-MIT) or [The Unlicense](UNLICENSE), at your
option. Contributions are accepted under the same terms.

Built on [ripgrep](https://github.com/BurntSushi/ripgrep)'s `grep-regex`,
`grep-searcher`, `grep-printer`, `grep-matcher`, and `ignore` libraries.
Dependencies retain their respective licenses.
