//! Criterion bench over a synthetic corpus: the parity floor for cold scan.
//!
//! Real-repo end-to-end numbers come from scripts/bench.sh (hyperfine, over
//! corpora from scripts/setup-corpora.sh). This micro-bench exists so every
//! commit gets a regression signal even without a 96K-file checkout.

use std::fs;
use std::path::Path;

use criterion::{Criterion, criterion_group, criterion_main};
use rrg_core::{SearchOptions, search_path};
use tempfile::TempDir;

/// Build a deterministic synthetic repo: `n_dirs` dirs x `files_per_dir`
/// files, each `lines_per_file` lines mixing code-like filler with a
/// sprinkling of the benchmark needles.
fn build_corpus(root: &Path, n_dirs: usize, files_per_dir: usize, lines_per_file: usize) {
    let needles = [
        "fn main",
        "fn helper",
        "return Ok(())",
        "needle_token",
        "TODO",
    ];
    for d in 0..n_dirs {
        let dir = root.join(format!("dir_{d:03}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..files_per_dir {
            let mut body = String::with_capacity(lines_per_file * 40);
            for line in 0..lines_per_file {
                if line % 97 == 0 {
                    let needle = needles[(d + f + line) % needles.len()];
                    body.push_str(&format!("pub fn {needle}_shim() {{ /* line {line} */ }}\n"));
                } else {
                    body.push_str("let x = compute(value_a, value_b) + offset;\n");
                }
            }
            fs::write(dir.join(format!("file_{f:03}.rs")), body).unwrap();
        }
    }
}

fn bench_cold_scan(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    build_corpus(dir.path(), 40, 25, 400); // 1000 files, ~4MB

    let mut group = c.benchmark_group("cold_scan");
    group.throughput(criterion::Throughput::Bytes(
        fs::read_dir(dir.path()).unwrap().count().max(1) as u64 * 4000,
    ));

    group.bench_function("fixed_string", |b| {
        b.iter(|| {
            search_path(
                "fn main",
                dir.path(),
                &SearchOptions {
                    fixed_strings: true,
                    ..Default::default()
                },
            )
            .unwrap()
        })
    });

    group.bench_function("regex", |b| {
        b.iter(|| {
            search_path(
                r"fn (main|helper)\w*",
                dir.path(),
                &SearchOptions::default(),
            )
            .unwrap()
        })
    });

    group.finish();
}

criterion_group!(benches, bench_cold_scan);
criterion_main!(benches);
