# ripripgrep

`rrg` is an experimental search tool written in Rust, with parallel filesystem
scanning and optional indexed queries. It uses ripgrep's matching and ignore
crates, with bigram/trigram candidate filtering and a local watcher daemon for
repeated searches.

The project is in early development. It is not a drop-in replacement for ripgrep,
and no general performance advantage is claimed. See [benchmarking](#benchmarking)
for reproducible comparisons.

## Build and install

Linux or macOS and a current stable Rust toolchain with Rust 2024 support are
required. Windows is not supported: the daemon uses Unix domain sockets.

From a checkout:

```sh
cargo build --release --locked
./target/release/rrg --help
cargo install --path crates/cli --locked
```

There are no published binary releases or crates.io installation instructions yet.

## Search

```sh
rrg 'fn main' crates/            # regular expression, matching lines
rrg -F 'SearchOptions' crates/   # literal string
rrg -i 'todo' .                 # case-insensitive search
rrg -l 'fn main' crates/         # paths with matches
rrg -c 'fn main' crates/         # aggregate counts and timing
```

The path defaults to the current directory. Scanning respects ignore rules and
skips hidden files by default.

## Indexed queries

```sh
rrg --build-index .
rrg --use-index -F 'SearchOptions' .
rrg --use-index --no-daemon --check-stale 'fn main' .
```

Indexes live in `.rrg-index/` inside the search root. Indexed queries normally
start a background daemon that holds the index in memory and watches for changes.
Use `--no-daemon` or `RRG_NO_DAEMON=1` to query in-process. The daemon currently
has no dedicated stop command; stop its process using your operating system's
process tools before removing or replacing its index.

Index files include packed source contents. Keep them local, exclude them from
version control, and only load indexes you built yourself.

## Current limitations

- Indexed queries can return stale results. `--check-stale` checks indexed files'
  modification times and lengths and falls back to scanning on detected changes;
  it does not discover every newly added file. Rebuild after adding files, or use
  a cold scan when completeness matters.
- The watcher and daemon lifecycle are experimental; concurrent rebuilds and
  queries are not a supported consistency guarantee.
- `-c` prints an aggregate summary, not ripgrep's per-file counts. Indexed queries
  with no matches may print a summary. Exit status does not distinguish a
  successful search with zero matches from one with matches.
- `-u` is currently parsed but does not enable hidden-file searching.
- Index format and library APIs may change without compatibility guarantees.

## Development

```sh
cargo test --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked
cargo bench -p rrg-core --bench baseline --locked
```

CI runs tests and release builds on Linux and macOS. See
[CONTRIBUTING.md](CONTRIBUTING.md) for contribution and validation guidance.

- `crates/core`: scanning, index construction/storage, query planning, daemon.
- `crates/cli`: the `rrg` executable.
- `crates/core/tests`: differential and property tests.
- `scripts`: corpus setup and end-to-end benchmarks.

## Benchmarking

Install `hyperfine` and `rg`, then:

```sh
cargo build --release --locked
scripts/setup-corpora.sh
scripts/bench.sh ~/.cache/rrg-corpora/ripgrep --quick
```

The setup script downloads shallow Linux and ripgrep checkouts. The benchmark
compares cold-scan matching-line output for the same literals and regex, with
output discarded by hyperfine. Repeated runs warm filesystem caches; this is
not a cold-cache benchmark and does not measure indexed queries. Record hardware,
OS, Rust version, corpus commit, tool versions, and the exact command with any
results. Generated `bench-*.json` files are ignored.

## License and acknowledgments

Licensed under either [MIT](LICENSE-MIT) or [The Unlicense](UNLICENSE), at your
option. Contributions are accepted under the same terms.

Built on [ripgrep](https://github.com/BurntSushi/ripgrep)'s `grep-regex`,
`grep-searcher`, `grep-printer`, and `ignore` crates. Dependencies retain their
respective licenses.
