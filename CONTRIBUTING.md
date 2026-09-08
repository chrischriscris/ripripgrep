# Contributing

Bug reports, documentation improvements, and focused pull requests are welcome.
For substantial design changes, open an issue first to discuss the behavior and
tradeoffs. Be respectful and constructive in issues and reviews.

## Local workflow

Use Linux or macOS with current stable Rust. Clone the repository, create a
branch, and run these commands from the workspace root:

```sh
cargo test --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked
cargo build --release --locked
```

Keep engine logic in `crates/core` and CLI parsing in `crates/cli`. Use Rust's
standard formatting and naming conventions. Keep changes focused and avoid
unrelated reformatting.

Matching and indexing changes need regression tests. Index candidates must
include every true match; compare optimized paths against scans and run
`cargo test -p rrg-core --test differential_bigram --locked`. Performance changes
need reproducible benchmark evidence, including corpus revision, commands,
hardware, OS, and tool versions. Do not commit generated indexes, build output,
or benchmark JSON files.

## Pull requests

Explain the problem, resulting behavior, relevant issue, and validation results.
Use descriptive, component-prefixed commit subjects, for example
`Query: preserve matches for short literals`. Disclose known limitations and
avoid unsupported performance claims.

By submitting a contribution, you agree to license it under the project's
MIT OR Unlicense terms. Only submit work you have the right to contribute.
