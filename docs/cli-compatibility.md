# CLI compatibility

The compatibility target is rg's everyday search workflow, with native rrg
execution. Differential tests compare output bytes and exit status with rg;
JSON comparisons exclude elapsed time. PTY tests check terminal defaults.

## Implemented

| Area | Interface |
| --- | --- |
| Input | `PATTERN [PATH ...]`, `-e`, `-f`, stdin, explicit `-`, `--` |
| Matching | `-F`, `-i`, `-s`, `-S`, `-w`, `-x`, `-v`, `-U`, `--multiline-dotall`, `--no-unicode` |
| Input format | `-a`, `--binary`, `--crlf`, `--null-data`, `-E` |
| Selection | `-c`, `--count-matches`, `--include-zero`, `-l`, `--files-without-match`, `-q`, `--files`, `-m` |
| Formatting | `-n`/`-N`, `-H`/`-I`, headings, colors, `-p`, `--column`, `-b`, `-o`, `-r`, `--trim`, `-M`, `--max-columns-preview` |
| Context | `-A`, `-B`, `-C`, `--context-separator`, `--no-context-separator`, `--passthru` |
| Integrations | `--json`, `--vimgrep`, `-0`, `--stats` |
| Filtering | `--hidden`, `-u`/`-uu`/`-uuu`, `--no-ignore`, `--no-ignore-vcs`, `--no-ignore-global`, `--no-ignore-parent`, `--ignore-file` |
| Globs/types | `-g`, `--iglob`, `--glob-case-insensitive`, `-t`, `-T`, `--type-add`, `--type-list` |
| Traversal | `-L`, `--max-depth`, `--max-filesize`, `--one-file-system`, `--sort path`, `--sortr path`, `-j` |
| Configuration | `RIPGREP_CONFIG_PATH`, `--no-config`, `--help`, `--version` |

Results are unordered unless sorting is requested. File contents and Unix paths
are printed as bytes, preserving non-UTF-8 data. `-r` replaces output only.
`--files-without-match` succeeds when at least one file without a match is found.
With `-q`, finding a match takes precedence over file errors, as in rg.

## Remaining differences

This is an explicit compatibility surface, not a claim of universal flag parity.
Unknown flags fail with exit code 2 rather than being silently ignored.

- PCRE2 (`-P`/`--engine`), compressed search (`-z`), preprocessors (`--pre`),
  and automatic engine switching are not implemented.
- Shell-completion/manpage generation, hyperlink configuration, custom output
  separators, time-based sorting, regex memory limits, and some fine-grained
  ignore/configuration switches are not implemented.
- Output is buffered per file. Explicit line-buffering controls are unavailable.
- Diagnostic wording and help/version output identify rrg and differ from rg.
  Some conflicting flag combinations can produce different precedence or errors.
- The index is an rrg extension. It is only used when `--use-index` is supplied,
  and safe filtering requires a case-sensitive ASCII literal query. Other modes
  scan live files. Indexed statistics describe the files actually searched.
- CLI index verification runs in-process, not through the experimental daemon.
  Index validation uses file size and modification time and is not a filesystem
  snapshot. See the README for consistency limits.
- Only Linux and macOS are currently supported.

## Regression checks

```sh
RRG_REQUIRE_RG=1 cargo test -p rrg --test rg_compat --locked
cargo test -p rrg-core --locked
python3 scripts/check-terminal.py target/debug/rrg
```

For every added flag, compare both output and status against rg on a small
fixture. Include combinations with other output modes where semantics differ.
Index changes must retain every true match and verify changed and newly created
files, not just stable indexed corpora.

## Small CLI benchmark

`python3 scripts/bench-cli.py` builds a temporary corpus of 1,000 files, each
3,579 bytes, checks identical matching-line output, and measures 10 runs after
two warmups. It reports platform and tool versions. Pass
`--baseline /path/to/previous/rrg` to compare with the original CLI; the baseline
command accounts for its formerly mandatory line numbers. Build both binaries
in release mode. This measures a small warm-cache corpus, not general throughput
or indexed-query performance.
