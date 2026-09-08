//! Candidate index using dense bigram bitsets and selective trigram postings.
//! Candidates are verified against file contents for exact matching.
//! A literal match must remain in the candidate set after filtering.
//!
//! Case handling: bigrams are ASCII-lowercased at build AND query time, so
//! candidates are a superset for case-sensitive queries too. The verification
//! scan enforces exact case semantics.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;

/// Sentinel: bigram key has no allocated column.
const NO_COLUMN: u16 = u16::MAX;
const KEY_SPACE: usize = 65_536; // u16 bigram keys (b1 << 8 | b2)
const MAGIC: &[u8; 8] = b"RPGBKIDX";
const FORMAT_VERSION: u32 = 8;

#[derive(Debug, Clone)]
pub struct BigramIndexConfig {
    /// Max distinct bigram columns. First `max_columns` distinct bigrams to
    /// appear claim a column; later ones stay unindexed, and any query that
    /// needs them safely degrades to MatchAll (never wrong results).
    pub max_columns: usize,
    /// Files larger than this are not indexed; they go to `fallback_paths`
    /// and are always scanned. Matches tgrep's 64 MiB default.
    pub max_file_size: u64,
    /// Trigram postings are stored for every trigram whose document
    /// frequency is at most this many files (the selective band). 0 disables
    /// the layer. Trigram postings give full 24-bit gram-space coverage
    /// where bigram columns can't reach — this is what cuts candidates for
    /// common literals like `spin_lock`.
    pub trigram_df_cap: usize,
    /// Soft byte budget for the varint postings arena.
    pub trigram_budget_bytes: usize,
    /// Soft byte budget for dense trigram bitmap columns.
    pub trigram_bitmap_budget_bytes: usize,
}

impl Default for BigramIndexConfig {
    fn default() -> Self {
        Self {
            max_columns: 5000,
            max_file_size: 64 * 1024 * 1024,
            trigram_df_cap: 0, // set by build() to file_count (cover everything)
            trigram_budget_bytes: 256 * 1024 * 1024,
            // Histogram-driven (linux kernel): grams with df >= ~4K number
            // only ~6.7K -> 80MB of flat 12KB columns covers them ALL.
            // Full gram coverage = tgrep-level selectivity at ~1/3 memory.
            trigram_bitmap_budget_bytes: 160 * 1024 * 1024,
        }
    }
}

/// Phase timings from a build.
#[derive(Debug, Clone, Copy)]
pub struct BuildTimings {
    pub walk: std::time::Duration,
    pub fill: std::time::Duration,
    /// Trigram postings pass (re-reads accepted files).
    pub trigrams: std::time::Duration,
    pub files: usize,
}

/// An immutable bigram candidate index over a fixed file set.
pub struct BigramIndex {
    /// Indexed file paths, file_id = index into this vec.
    pub paths: Vec<PathBuf>,
    /// Build-time metadata per path (same order): (mtime_nanos, len).
    /// Used to detect stale indexes — a file changed since build means the
    /// candidate guarantee no longer holds for it.
    pub file_meta: Vec<(u64, u64)>,
    /// Files excluded from indexing (too large / binary / unreadable).
    /// Always scanned during verification so results stay complete.
    pub fallback_paths: Vec<PathBuf>,
    /// Bigram key -> column id (or NO_COLUMN). Backed by the leaked mmap
    /// after load (zero-copy); leaked heap in the builder process.
    lookup: &'static [u16],
    /// Number of allocated columns (== column_counts.len()).
    n_columns: usize,
    /// ceil(paths.len() / 64)
    words: usize,
    /// Dense slab: n_columns * words u64 words.
    columns: &'static [u64],
    /// Files containing each column's bigram (popcount of its bitmap).
    column_counts: &'static [u32],
    /// Trigram layers: varint postings (low-df, with loc masks) + dense
    /// bitmap columns (high-df). See `TrigramLayers`.
    tg: TrigramLayers,
    /// Per-file byte spans inside contents.pack (start, len), indexed by
    /// file_id — NOT id-ordered: the fill pass writes each file at an
    /// atomically allocated offset. Query verification slices these spans.
    pub pack_spans: Vec<(u64, u64)>,
    /// Total contents.pack length in bytes (0 = no pack).
    pub pack_len: u64,
    /// Prefix-sum GLOBAL block counts per file (derivable from
    /// pack_spans; stored because both build and load have it in hand).
    pub block_prefix: Vec<u64>,
}

/// Block size for intra-file gram bitmaps. A needle shorter than this
/// spans at most two adjacent blocks, keeping {k, k+1} windows sound.
pub const BLOCK_SIZE: u64 = 32 * 1024;

/// What a query's bigrams say about the *indexed* file set.
#[derive(Debug, PartialEq, Eq)]
pub enum Candidates {
    /// Every indexed bigram of the literal exists, ANDed bitmaps yield these
    /// candidate file_ids.
    Superset(Vec<u32>),
    /// A bigram of the literal has an empty bitmap: no indexed file can
    /// contain the literal. (fallback_paths still get scanned.)
    /// NOTE: currently unreachable — a column only exists once some file
    /// claimed it, so absent bigrams hit MatchAll instead. Kept as a
    /// defensive branch for future streaming/overlay designs.
    None,
    /// Too few selective bigrams (< 2 distinct, or a needed bigram was never
    /// allocated a column): scan everything.
    MatchAll,
}

/// A query plan over bigram constraints, derived from a regex's HIR so that
/// filtering is always SOUND for the full pattern language (unions, optional
/// parts, classes). Modeled on microsoft/tgrep's And/Or decomposition.
///
/// Soundness rule: a Bigrams(set) node requires every file matching the
/// sub-pattern to contain ALL of `set`'s bigrams. Anything we can't prove
/// (classes, min=0 repetitions, lookarounds) contributes no constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// No constraint derivable: scan everything (safe fallback).
    All,
    /// Every bigram key here must be present in a matching file.
    Bigrams(BTreeSet<usize>),
    /// Any branch may match: candidate sets union.
    Union(Vec<Plan>),
    /// Every branch's constraint holds: candidate sets intersect.
    Intersect(Vec<Plan>),
}

fn bytes_bigrams(bytes: &[u8]) -> BTreeSet<usize> {
    bytes
        .windows(2)
        .map(|p| (p[0].to_ascii_lowercase() as usize) << 8 | p[1].to_ascii_lowercase() as usize)
        .collect()
}

pub fn literal_bigrams(s: &str) -> BTreeSet<usize> {
    bytes_bigrams(s.as_bytes())
}

/// Trigram-level plan (same shape as `Plan`, gram keys are 24-bit).
#[derive(Debug, Clone)]
pub enum TriPlan {
    All,
    /// Trigrams of ONE literal run: (needle-relative offset, gram). Offsets
    /// enable position-consistent mask rotation within the run.
    Grams(Vec<(usize, u32)>),
    Union(Vec<TriPlan>),
    Intersect(Vec<TriPlan>),
}

fn bytes_trigrams(bytes: &[u8]) -> Vec<(usize, u32)> {
    bytes
        .windows(3)
        .enumerate()
        .map(|(off, t)| {
            (
                off,
                ((t[0].to_ascii_lowercase() as u32) << 16)
                    | ((t[1].to_ascii_lowercase() as u32) << 8)
                    | t[2].to_ascii_lowercase() as u32,
            )
        })
        .collect()
}

/// Plan for an exact literal (trigram level).
pub fn tri_plan_for_literal(literal: &str) -> TriPlan {
    TriPlan::Grams(bytes_trigrams(literal.as_bytes()))
}

/// Decompose a regex into a trigram plan via its HIR. Literal segments
/// contribute Grams; classes/lookarounds nothing; min>=1 repetitions recurse.
pub fn tri_plan_for_regex(pattern: &str) -> Result<TriPlan> {
    let hir = regex_syntax::parse(pattern).context("regex parse")?;
    Ok(tri_decompose(&hir))
}

fn tri_decompose(hir: &regex_syntax::hir::Hir) -> TriPlan {
    use regex_syntax::hir::HirKind;
    match hir.kind() {
        HirKind::Literal(lit) => TriPlan::Grams(bytes_trigrams(&lit.0)),
        HirKind::Concat(parts) => TriPlan::Intersect(parts.iter().map(tri_decompose).collect()),
        HirKind::Alternation(branches) => {
            TriPlan::Union(branches.iter().map(tri_decompose).collect())
        }
        HirKind::Repetition(rep) if rep.min >= 1 => tri_decompose(&rep.sub),
        _ => TriPlan::All,
    }
}

/// Decompose a regex into a sound bigram plan via its HIR.
pub fn plan_for_regex(pattern: &str) -> Result<Plan> {
    let hir = regex_syntax::parse(pattern).context("regex parse")?;
    Ok(decompose(&hir))
}

/// Plan for an exact literal.
pub fn plan_for_literal(literal: &str) -> Plan {
    Plan::Bigrams(literal_bigrams(literal))
}

fn decompose(hir: &regex_syntax::hir::Hir) -> Plan {
    use regex_syntax::hir::HirKind;
    match hir.kind() {
        HirKind::Literal(lit) => Plan::Bigrams(bytes_bigrams(&lit.0)),
        // Concatenation: a match must contain every literal segment.
        HirKind::Concat(parts) => Plan::Intersect(parts.iter().map(decompose).collect()),
        // Alternation: a match contains ONE branch.
        HirKind::Alternation(branches) => Plan::Union(branches.iter().map(decompose).collect()),
        // Repetition with min >= 1 keeps the inner constraint; min = 0 means
        // the inner part may be absent entirely -> no constraint.
        HirKind::Repetition(rep) if rep.min >= 1 => decompose(&rep.sub),
        // Classes, anchors, lookarounds -> no constraint (sound fallback).
        _ => Plan::All,
    }
}

/// Trigram-plan evaluation result.
enum TriEval {
    /// No checkable constraint in this subtree.
    NoConstraint,
    /// A needed gram can't be checked: union semantics poisoned.
    MatchAll,
    Bitmap(Vec<u64>),
}

/// Internal evaluation result against the index.
enum Eval {
    /// Cannot filter safely: scan all indexed files.
    MatchAll,
    /// No constraint present: scan all indexed files.
    NoFilter,
    /// Exact candidate bitmap (superset of true matches).
    Bitmap(Vec<u64>),
}

impl BigramIndex {
    /// Build the index over `root` with a parallel walk + parallel fill.
    /// `index_dir`: where to write the contents.pack companion (pack is
    /// skipped when None — tests).
    pub fn build(
        root: &Path,
        config: &BigramIndexConfig,
        index_dir: Option<&Path>,
    ) -> Result<(Self, BuildTimings)> {
        let walk_start = std::time::Instant::now();
        let mut paths = Vec::new();
        collect_files(root, &mut paths)?;
        let walk = walk_start.elapsed();

        let fill_start = std::time::Instant::now();
        let file_count = paths.len();
        let words = file_count.div_ceil(64);

        let lookup_atomic: Vec<AtomicU16> =
            (0..KEY_SPACE).map(|_| AtomicU16::new(NO_COLUMN)).collect();
        let counts_atomic: Vec<AtomicU32> =
            (0..config.max_columns).map(|_| AtomicU32::new(0)).collect();
        // Atomic OR is required: distinct threads can set different bits of
        // the same u64 word (files sharing a 64-file block). Relaxed is fine —
        // there is no ordering dependency between column writes.
        let columns: Vec<AtomicU64> = (0..config.max_columns.saturating_mul(words))
            .map(|_| AtomicU64::new(0))
            .collect();
        let next_column = AtomicUsize::new(0);
        let next_id = AtomicUsize::new(0);
        let accepted: std::sync::Mutex<Vec<(usize, PathBuf, u64, u64)>> =
            std::sync::Mutex::new(Vec::with_capacity(file_count));
        let fallback = std::sync::Mutex::new(Vec::new());

        // contents.pack: written DURING the fill pass — each file lands at
        // an atomically allocated offset (spans, not id order). The trigram
        // pass then reads back from the pack mmap (one sequential-ish pass
        // over page cache) instead of re-opening 95K files. The pack MUST
        // be complete regardless of trigram selection: verification reads
        // these bytes, and a zero-filled hole is a silent false negative.
        let pack_path = index_dir.map(|dir| {
            fs::create_dir_all(dir).ok();
            dir.join("contents.pack")
        });
        let pack_file = pack_path
            .as_ref()
            .map(|p| fs::File::create(p))
            .transpose()?;
        let pack_alloc = std::sync::atomic::AtomicU64::new(0);
        let pack_span_start: Vec<std::sync::atomic::AtomicU64> = (0..file_count)
            .map(|_| std::sync::atomic::AtomicU64::new(0))
            .collect();
        let pack_span_len: Vec<std::sync::atomic::AtomicU64> = (0..file_count)
            .map(|_| std::sync::atomic::AtomicU64::new(0))
            .collect();
        let pack_errors: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

        // Trigram document-frequency collection (pass 1): sharded maps keep
        // lock contention near zero; shard = gram % N_SHARDS.
        const TG_SHARDS: usize = 64;
        const TG_DF_SAMPLE: usize = 4;
        let tg_df: Vec<std::sync::Mutex<std::collections::HashMap<u32, u32>>> = (0..TG_SHARDS)
            .map(|_| std::sync::Mutex::new(std::collections::HashMap::new()))
            .collect();

        // Parallel fill: each file marks its distinct bigrams in the shared
        // slab. Column allocation races resolve via CAS; per-file distinctness
        // is tracked in a per-file 8 KiB seen-set (stack allocated).
        //
        // file_ids are handed out only to ACCEPTED files (contiguous ids),
        // while the slab keeps `words` sized for the full walk (upper bound):
        // rejected files leave a few unused words behind, but ids stay dense
        // and `paths` ends up containing exactly the indexed set.
        paths.par_iter().for_each(|path| {
            // Reject oversized files before reading them into memory
            // (metadata pre-check; the post-read length check below covers
            // the TOCTOU window).
            if fs::metadata(path).map(|m| m.len()).unwrap_or(0) > config.max_file_size {
                fallback.lock().unwrap().push(path.clone());
                return;
            }
            let Ok(bytes) = fs::read(path) else {
                fallback.lock().unwrap().push(path.clone());
                return;
            };
            if bytes.len() as u64 > config.max_file_size
                || bytes[..bytes.len().min(8192)].contains(&b'\0')
            {
                fallback.lock().unwrap().push(path.clone());
                return;
            }
            let file_id = next_id.fetch_add(1, Ordering::Relaxed);
            // Pack write: allocate the span, write the exact bytes read
            // above. A failed write aborts the build (a zero-filled hole
            // would make verification silently report 0 matches).
            {
                let len = bytes.len() as u64;
                let start = pack_alloc.fetch_add(len, Ordering::Relaxed);
                pack_span_start[file_id].store(start, Ordering::Relaxed);
                pack_span_len[file_id].store(len, Ordering::Relaxed);
                if let Some(f) = &pack_file {
                    use std::os::unix::fs::FileExt;
                    if let Err(e) = f.write_all_at(&bytes, start) {
                        pack_errors
                            .lock()
                            .unwrap()
                            .push(format!("{}: pack write: {e}", path.display()));
                    }
                }
            }
            let mtime = fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as u64);
            accepted
                .lock()
                .unwrap()
                .push((file_id, path.clone(), mtime, bytes.len() as u64));

            let mut seen = [0u64; KEY_SPACE / 64];
            for pair in bytes.windows(2) {
                let key = (pair[0].to_ascii_lowercase() as usize) << 8
                    | pair[1].to_ascii_lowercase() as usize;
                seen[key / 64] |= 1 << (key % 64);
            }
            // df is sampled (1 in TG_DF_SAMPLE files) and scaled back at
            // selection time — band edges are fuzzy, sampling is fine.
            if file_id % TG_DF_SAMPLE == 0 {
                let mut tg_buf: Vec<u32> = Vec::with_capacity(4096);
                for tri in bytes.windows(3) {
                    let key = ((tri[0].to_ascii_lowercase() as u32) << 16)
                        | ((tri[1].to_ascii_lowercase() as u32) << 8)
                        | tri[2].to_ascii_lowercase() as u32;
                    tg_buf.push(key);
                }
                tg_buf.sort_unstable();
                tg_buf.dedup();
                for &g in &tg_buf {
                    let mut shard = tg_df[(g % TG_SHARDS as u32) as usize].lock().unwrap();
                    *shard.entry(g).or_insert(0) += 1;
                }
            }
            let word_lo = file_id / 64;
            let bit = 1u64 << (file_id % 64);
            for (word_idx, word) in seen.iter_mut().enumerate() {
                if *word == 0 {
                    continue;
                }
                let base_key = word_idx * 64;
                for bit_idx in word.trailing_ones() as usize..64 {
                    if *word & (1u64 << bit_idx) == 0 {
                        continue;
                    }
                    if let Some(col) = get_or_alloc_column(
                        &lookup_atomic,
                        &next_column,
                        base_key + bit_idx,
                        config.max_columns,
                    ) {
                        columns[col * words + word_lo].fetch_or(bit, Ordering::Relaxed);
                        counts_atomic[col].fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
        let fill = fill_start.elapsed();

        drop(pack_file); // all writes done; pass 2 reopens read-only
        {
            let errs = pack_errors.into_inner().unwrap();
            if !errs.is_empty() {
                anyhow::bail!(
                    "pack write failed for {} file(s); refusing to publish an unsound index: {}",
                    errs.len(),
                    errs.iter().take(5).cloned().collect::<Vec<_>>().join("; ")
                );
            }
        }
        let pack_len = pack_alloc.into_inner();
        if let Some(p) = &pack_path {
            if pack_len > 0 {
                // Trim the (sparse) overestimate. Write access required for
                // set_len; no truncate flag — contents must survive.
                let f = fs::OpenOptions::new().write(true).open(p)?;
                f.set_len(pack_len)?;
            }
        }

        let mut indexed: Vec<(usize, PathBuf, u64, u64)> = accepted.into_inner().unwrap();
        indexed.sort_unstable_by_key(|&(id, _, _, _)| id);
        let final_paths: Vec<PathBuf> = indexed.iter().map(|(_, p, _, _)| p.clone()).collect();
        let file_meta: Vec<(u64, u64)> = indexed.iter().map(|&(_, _, m, l)| (m, l)).collect();
        let indexed_count = final_paths.len();

        // Per-file pack spans (id-indexed) + prefix-sum block counts.
        let pack_spans: Vec<(u64, u64)> = (0..final_paths.len())
            .map(|id| {
                (
                    pack_span_start[id].load(Ordering::Relaxed),
                    pack_span_len[id].load(Ordering::Relaxed),
                )
            })
            .collect();
        let block_prefix: Vec<u64> = {
            let mut o = Vec::with_capacity(pack_spans.len() + 1);
            let mut acc = 0u64;
            o.push(0);
            for &(_, len) in &pack_spans {
                acc += len.div_ceil(BLOCK_SIZE);
                o.push(acc);
            }
            o
        };
        let total_blocks = *block_prefix.last().unwrap_or(&0) as usize;

        // Pack view for pass 2: read-only mmap of what fill just wrote
        // (page cache makes this a warm, mostly-sequential read).
        let pack_bytes: Option<&'static [u8]> = if pack_len > 0 {
            pack_path.as_ref().and_then(|p| {
                let f = fs::File::open(p).ok()?;
                let mmap = unsafe { memmap2::Mmap::map(&f).ok()? };
                let m: &'static mut memmap2::Mmap = Box::leak(Box::new(mmap));
                Some(&m[..])
            })
        } else {
            None
        };

        // ---- trigram postings layer (pass 2: read back from the pack) ----
        let tg_start = std::time::Instant::now();
        let df_cap = if config.trigram_df_cap == 0 {
            file_count
        } else {
            config.trigram_df_cap
        };
        let tg = build_trigram_postings(
            &tg_df,
            &final_paths,
            df_cap,
            3,
            4,
            words,
            config,
            pack_bytes,
            &pack_spans,
            &block_prefix,
            total_blocks,
        )?;
        let tg_build = tg_start.elapsed();
        drop(tg_df);

        let lookup: Vec<u16> = lookup_atomic.into_iter().map(|a| a.into_inner()).collect();
        let column_counts: Vec<u32> = counts_atomic.into_iter().map(|a| a.into_inner()).collect();
        let n_columns = column_counts.len();
        let columns: Vec<u64> = columns
            .into_iter()
            .map(|a: AtomicU64| a.into_inner())
            .collect();

        Ok((
            Self {
                paths: final_paths,
                file_meta,
                fallback_paths: fallback.into_inner().unwrap(),
                lookup: Box::leak(lookup.into_boxed_slice()),
                n_columns,
                words,
                columns: Box::leak(columns.into_boxed_slice()),
                column_counts: Box::leak(column_counts.into_boxed_slice()),
                tg,
                pack_spans,
                pack_len,
                block_prefix,
            },
            BuildTimings {
                walk,
                fill,
                trigrams: tg_build,
                files: indexed_count,
            },
        ))
    }

    /// File-level candidates for a literal.
    /// File-level candidates for a fixed string (convenience wrapper).
    pub fn candidates(&self, literal: &str) -> Candidates {
        self.candidates_plan(&Plan::Bigrams(literal_bigrams(literal)))
    }

    /// Evaluate a plan against the index. Never returns a set that can miss
    /// a matching file: where a constraint can't be evaluated (NO_COLUMN),
    /// the whole query degrades to MatchAll.
    pub fn candidates_plan(&self, plan: &Plan) -> Candidates {
        match self.eval(plan) {
            Eval::MatchAll | Eval::NoFilter => Candidates::MatchAll,
            Eval::Bitmap(bits) => {
                let n = self.paths.len();
                let out: Vec<u32> = bits
                    .iter()
                    .enumerate()
                    .flat_map(|(w, &b)| {
                        (0..64)
                            .filter(move |i| b & (1u64 << i) != 0)
                            .map(move |i| (w * 64 + i) as u32)
                    })
                    .take_while(|&id| (id as usize) < n)
                    .collect();
                Candidates::Superset(out)
            }
        }
    }

    fn eval(&self, plan: &Plan) -> Eval {
        match plan {
            Plan::All => Eval::NoFilter,
            Plan::Bigrams(keys) => {
                if keys.is_empty() {
                    return Eval::NoFilter;
                }
                let mut acc = vec![u64::MAX; self.words];
                for &key in keys {
                    let col = self.lookup[key];
                    if col == NO_COLUMN {
                        // Cannot prove presence -> cannot filter safely.
                        return Eval::MatchAll;
                    }
                    let base = col as usize * self.words;
                    for w in 0..self.words {
                        acc[w] &= self.columns[base + w];
                    }
                }
                Eval::Bitmap(acc)
            }
            Plan::Intersect(parts) => {
                let mut acc: Option<Vec<u64>> = None;
                for p in parts {
                    match self.eval(p) {
                        Eval::MatchAll => return Eval::MatchAll,
                        Eval::NoFilter => {}
                        Eval::Bitmap(b) => match &mut acc {
                            None => acc = Some(b),
                            Some(a) => {
                                for w in 0..self.words {
                                    a[w] &= b[w];
                                }
                            }
                        },
                    }
                }
                match acc {
                    Some(a) => Eval::Bitmap(a),
                    None => Eval::NoFilter,
                }
            }
            Plan::Union(parts) => {
                let mut acc = vec![0u64; self.words];
                for p in parts {
                    match self.eval(p) {
                        Eval::MatchAll | Eval::NoFilter => return Eval::MatchAll,
                        Eval::Bitmap(b) => {
                            for w in 0..self.words {
                                acc[w] |= b[w];
                            }
                        }
                    }
                }
                Eval::Bitmap(acc)
            }
        }
    }

    /// Serialize to `dir`. v6: section table + 8-byte-aligned sections so
    /// load() can mmap and view every slab zero-copy (no parse copies).
    pub fn save(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        let tmp = dir.join("index.bin.tmp");
        {
            const SEC: usize = 13; // ... + map_block_cols
            let mut w = io::BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);

            let path_bytes: Vec<u8> = {
                let mut v = Vec::new();
                for p in &self.paths {
                    write_path(&mut v, p)?;
                }
                v
            };
            let fb_bytes: Vec<u8> = {
                let mut v = Vec::new();
                for p in &self.fallback_paths {
                    write_path(&mut v, p)?;
                }
                v
            };
            let meta_bytes: Vec<u8> = {
                let mut v = Vec::with_capacity(self.file_meta.len() * 16);
                for &(mtime, len) in &self.file_meta {
                    v.extend_from_slice(&mtime.to_le_bytes());
                    v.extend_from_slice(&len.to_le_bytes());
                }
                v
            };
            let lookup_bytes: Vec<u8> = self.lookup.iter().flat_map(|v| v.to_le_bytes()).collect();
            let counts_bytes: Vec<u8> = self
                .column_counts
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            let columns_bytes: Vec<u8> =
                self.columns.iter().flat_map(|v| v.to_le_bytes()).collect();
            let tg_grams_bytes: Vec<u8> =
                self.tg.grams.iter().flat_map(|v| v.to_le_bytes()).collect();
            let tg_offsets_bytes: Vec<u8> = self
                .tg
                .offsets
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            let map_grams_bytes: Vec<u8> = self
                .tg
                .map_grams
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            let map_cols_bytes: Vec<u8> = self
                .tg
                .map_cols
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            // v8 pack section: pack_len u64 + (start,len) u64 pairs per file
            let pack_offsets_bytes: Vec<u8> = {
                let mut b = Vec::with_capacity(8 + self.pack_spans.len() * 16);
                b.extend_from_slice(&self.pack_len.to_le_bytes());
                for &(s, l) in &self.pack_spans {
                    b.extend_from_slice(&s.to_le_bytes());
                    b.extend_from_slice(&l.to_le_bytes());
                }
                b
            };
            let map_block_bytes: Vec<u8> = self
                .tg
                .map_block_cols
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();

            let mut sections = [(0u64, 0u64); SEC];
            let lens = [
                path_bytes.len(),
                fb_bytes.len(),
                meta_bytes.len(),
                lookup_bytes.len(),
                counts_bytes.len(),
                columns_bytes.len(),
                tg_grams_bytes.len(),
                tg_offsets_bytes.len(),
                self.tg.arena.len(),
                map_grams_bytes.len(),
                map_cols_bytes.len(),
                pack_offsets_bytes.len(),
                map_block_bytes.len(),
            ];
            let header_len = (8 + 4 + 4 + 8 * 8 + SEC * 16) as u64;
            let mut off = header_len;
            for (i, len) in lens.iter().enumerate() {
                off = (off + 7) & !7;
                sections[i] = (off, *len as u64);
                off += *len as u64;
            }

            w.write_all(MAGIC)?;
            w.write_all(&FORMAT_VERSION.to_le_bytes())?;
            w.write_all(&0u32.to_le_bytes())?; // pad to 8
            w.write_all(&(self.paths.len() as u64).to_le_bytes())?;
            w.write_all(&(self.fallback_paths.len() as u64).to_le_bytes())?;
            w.write_all(&(self.words as u64).to_le_bytes())?;
            w.write_all(&(self.n_columns as u64).to_le_bytes())?;
            w.write_all(&(self.tg.grams.len() as u64).to_le_bytes())?;
            w.write_all(&(self.tg.arena.len() as u64).to_le_bytes())?;
            w.write_all(&(self.tg.map_grams.len() as u64).to_le_bytes())?;
            w.write_all(&(self.tg.map_words as u64).to_le_bytes())?;
            for &(o, l) in &sections {
                w.write_all(&o.to_le_bytes())?;
                w.write_all(&l.to_le_bytes())?;
            }

            w.write_all(&path_bytes)?;
            pad_to(&mut w, sections[1].0)?;
            w.write_all(&fb_bytes)?;
            pad_to(&mut w, sections[2].0)?;
            w.write_all(&meta_bytes)?;
            pad_to(&mut w, sections[3].0)?;
            w.write_all(&lookup_bytes)?;
            pad_to(&mut w, sections[4].0)?;
            w.write_all(&counts_bytes)?;
            pad_to(&mut w, sections[5].0)?;
            w.write_all(&columns_bytes)?;
            pad_to(&mut w, sections[6].0)?;
            w.write_all(&tg_grams_bytes)?;
            pad_to(&mut w, sections[7].0)?;
            w.write_all(&tg_offsets_bytes)?;
            pad_to(&mut w, sections[8].0)?;
            w.write_all(self.tg.arena)?;
            pad_to(&mut w, sections[9].0)?;
            w.write_all(&map_grams_bytes)?;
            pad_to(&mut w, sections[10].0)?;
            w.write_all(&map_cols_bytes)?;
            pad_to(&mut w, sections[11].0)?;
            w.write_all(&pack_offsets_bytes)?;
            pad_to(&mut w, sections[12].0)?;
            w.write_all(&map_block_bytes)?;
            w.flush()?;
        }
        fs::rename(&tmp, dir.join("index.bin"))?;
        Ok(())
    }

    /// Load from `dir`. Mmaps `index.bin` and views every slab zero-copy:
    /// load cost is page-fault-in only (~4ms warm for 235MB vs ~70ms for
    /// read+parse). The mmap is leaked for 'static (one-shot CLI / daemon).
    pub fn load(dir: &Path) -> Result<Self> {
        let file = fs::File::open(dir.join("index.bin")).context("reading index.bin")?;
        let mmap = unsafe { memmap2::Mmap::map(&file).context("mmap index.bin")? };
        let bytes: &'static [u8] = &*Box::leak(Box::new(mmap));
        let mut r: &[u8] = bytes;

        let mut magic = [0u8; 8];
        r.read_exact_into(&mut magic)?;
        if &magic != MAGIC {
            bail!("not an rrg index (bad magic)");
        }
        let mut u32b = [0u8; 4];
        r.read_exact_into(&mut u32b)?;
        let version = u32::from_le_bytes(u32b);
        if version != FORMAT_VERSION {
            bail!("index format v{version} != supported v{FORMAT_VERSION}; rebuild");
        }
        r = &r[4..]; // pad

        let n_paths = take_u64(&mut r)? as usize;
        let n_fallback = take_u64(&mut r)? as usize;
        let words = take_u64(&mut r)? as usize;
        let n_columns = take_u64(&mut r)? as usize;
        let tg_n = take_u64(&mut r)? as usize;
        let tg_arena_len = take_u64(&mut r)? as usize;
        let tg_map_n = take_u64(&mut r)? as usize;
        let tg_map_words = take_u64(&mut r)? as usize;

        const SEC: usize = 13;
        let mut sections = [(0u64, 0u64); SEC];
        for s_ in sections.iter_mut() {
            let mut o = [0u8; 8];
            r.read_exact_into(&mut o)?;
            let mut l = [0u8; 8];
            r.read_exact_into(&mut l)?;
            *s_ = (u64::from_le_bytes(o), u64::from_le_bytes(l));
        }
        let (p_off, p_len) = sections[0];
        let (f_off, f_len) = sections[1];
        let (m_off, m_len) = sections[2];
        let (l_off, l_len) = sections[3];
        let (c_off, c_len) = sections[4];
        let (co_off, co_len) = sections[5];
        let (tg_g_off, tg_g_len) = sections[6];
        let (tg_o_off, tg_o_len) = sections[7];
        let (tg_a_off, tg_a_len) = sections[8];
        let (mg_off, mg_len) = sections[9];
        let (mc_off, mc_len) = sections[10];
        let (po_off, po_len) = sections[11];
        let (mb_off, mb_len) = sections[12];

        for &(o, l) in &sections {
            if o.checked_add(l).map_or(true, |e| e as usize > bytes.len()) {
                bail!("corrupt index: section overruns file");
            }
        }
        if l_len != (KEY_SPACE * 2) as u64
            || c_len != (n_columns * 4) as u64
            || co_len != (n_columns * words * 8) as u64
            || tg_g_len != (tg_n * 4) as u64
            || tg_o_len != ((tg_n + 1) * 4) as u64
            || mg_len != (tg_map_n * 4) as u64
            || mc_len != (tg_map_n * tg_map_words * 8) as u64
            || po_len != (8 + n_paths * 16) as u64
        {
            bail!("corrupt index: section sizes inconsistent");
        }
        let sec = |off: u64, len: u64| &bytes[off as usize..(off + len) as usize];

        let mut pr: &[u8] = sec(p_off, p_len);
        let mut paths = Vec::with_capacity(n_paths);
        for _ in 0..n_paths {
            paths.push(read_path(&mut pr)?);
        }
        let mut fr: &[u8] = sec(f_off, f_len);
        let mut fallback_paths = Vec::with_capacity(n_fallback);
        for _ in 0..n_fallback {
            fallback_paths.push(read_path(&mut fr)?);
        }
        if !pr.is_empty() || !fr.is_empty() {
            bail!("corrupt index: trailing path bytes");
        }

        let file_meta: Vec<(u64, u64)> = sec(m_off, m_len)
            .chunks_exact(16)
            .map(|c| {
                (
                    u64::from_le_bytes(c[..8].try_into().unwrap()),
                    u64::from_le_bytes(c[8..].try_into().unwrap()),
                )
            })
            .collect();
        if file_meta.len() != n_paths {
            bail!("corrupt index: meta count mismatch");
        }

        // Zero-copy slab views. Sections are 8-byte aligned by the writer
        // and the mmap base is page-aligned, so these casts are aligned.
        let lookup: &'static [u16] = unsafe {
            std::slice::from_raw_parts(sec(l_off, l_len).as_ptr() as *const u16, KEY_SPACE)
        };
        let column_counts: &'static [u32] = unsafe {
            std::slice::from_raw_parts(sec(c_off, c_len).as_ptr() as *const u32, n_columns)
        };
        let columns: &'static [u64] = unsafe {
            std::slice::from_raw_parts(
                sec(co_off, co_len).as_ptr() as *const u64,
                n_columns * words,
            )
        };
        let tg_grams: &'static [u32] = unsafe {
            std::slice::from_raw_parts(sec(tg_g_off, tg_g_len).as_ptr() as *const u32, tg_n)
        };
        let tg_offsets: &'static [u32] = unsafe {
            std::slice::from_raw_parts(sec(tg_o_off, tg_o_len).as_ptr() as *const u32, tg_n + 1)
        };
        if !tg_offsets.is_empty() && tg_offsets[tg_offsets.len() - 1] != tg_arena_len as u32 {
            bail!("corrupt index: trigram arena size mismatch");
        }
        let map_grams: &'static [u32] = unsafe {
            std::slice::from_raw_parts(sec(mg_off, mg_len).as_ptr() as *const u32, tg_map_n)
        };
        let map_cols: &'static [u64] = unsafe {
            std::slice::from_raw_parts(
                sec(mc_off, mc_len).as_ptr() as *const u64,
                tg_map_n * tg_map_words,
            )
        };
        let (pack_spans, pack_len): (Vec<(u64, u64)>, u64) = {
            let raw = sec(po_off, po_len);
            if raw.len() < 8 {
                bail!("corrupt index: pack section too small");
            }
            let pack_len = u64::from_le_bytes(raw[..8].try_into().unwrap());
            let mut spans = Vec::with_capacity((raw.len() - 8) / 16);
            for c in raw[8..].chunks_exact(16) {
                let s = u64::from_le_bytes(c[..8].try_into().unwrap());
                let l = u64::from_le_bytes(c[8..].try_into().unwrap());
                spans.push((s, l));
            }
            if spans.len() != paths.len() {
                bail!(
                    "corrupt index: pack spans {} != files {}",
                    spans.len(),
                    paths.len()
                );
            }
            (spans, pack_len)
        };
        let block_prefix: Vec<u64> = {
            let mut o = Vec::with_capacity(pack_spans.len() + 1);
            let mut acc = 0u64;
            o.push(0);
            for &(_, len) in &pack_spans {
                acc += len.div_ceil(BLOCK_SIZE);
                o.push(acc);
            }
            o
        };

        Ok(Self {
            paths,
            file_meta,
            fallback_paths,
            lookup,
            n_columns,
            words,
            columns,
            column_counts,
            tg: TrigramLayers {
                grams: tg_grams,
                offsets: tg_offsets,
                arena: sec(tg_a_off, tg_a_len),
                map_grams,
                map_cols,
                map_words: tg_map_words,
                map_block_cols: unsafe {
                    std::slice::from_raw_parts(
                        sec(mb_off, mb_len).as_ptr() as *const u64,
                        mb_len as usize / 8,
                    )
                },
                total_blocks: mb_len as usize / 8 * 64,
            },
            pack_spans,
            pack_len,
            block_prefix,
        })
    }

    pub fn staleness(&self) -> usize {
        self.paths
            .iter()
            .zip(&self.file_meta)
            .filter(|(p, meta)| {
                let (mtime, len) = **meta;
                fs::metadata(p).map_or(true, |md| {
                    let cur_len = md.len();
                    let cur_mtime = md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_nanos() as u64);
                    cur_mtime != mtime || cur_len != len
                })
            })
            .count()
    }

    /// In-memory index footprint (excludes paths).
    pub fn memory_bytes(&self) -> usize {
        self.lookup.len() * 2
            + self.column_counts.len() * 4
            + self.columns.len() * 8
            + self.tg.memory_bytes()
    }

    pub fn words(&self) -> usize {
        self.words
    }

    /// Debug: layer sizes + per-needle-gram coverage.
    pub fn tg_debug(&self, needle: &str) -> String {
        let (vg, mg, arena) = (
            self.tg.grams.len(),
            self.tg.map_grams.len(),
            self.tg.arena.len(),
        );
        let mut hits = String::new();
        for t in needle.as_bytes().windows(3) {
            let g = ((t[0].to_ascii_lowercase() as u32) << 16)
                | ((t[1].to_ascii_lowercase() as u32) << 8)
                | t[2].to_ascii_lowercase() as u32;
            hits.push_str(&format!(
                "{:02x}{:02x}{:02x}:var={}map={} ",
                t[0],
                t[1],
                t[2],
                self.trigram_posting(g).map(|p| p.len()).unwrap_or(0),
                self.trigram_map_col(g).is_some()
            ));
        }
        format!("varint_grams={vg} bitmap_grams={mg} arena={arena}B | {hits}")
    }

    pub fn n_columns(&self) -> usize {
        self.n_columns
    }
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

fn get_or_alloc_column(
    lookup: &[AtomicU16],
    next_column: &AtomicUsize,
    key: usize,
    max_columns: usize,
) -> Option<usize> {
    let fast = lookup[key].load(Ordering::Relaxed);
    if fast != NO_COLUMN {
        return Some(fast as usize);
    }
    let new_col = next_column.fetch_add(1, Ordering::Relaxed);
    if new_col >= max_columns {
        // Budget exhausted — but a racing thread may have claimed a column
        // for this key after our fast-path load. Re-check before giving up,
        // otherwise the bit for (this file, this key) would be silently
        // dropped: a FALSE NEGATIVE in the candidate set.
        let cur = lookup[key].load(Ordering::Relaxed);
        return if cur == NO_COLUMN {
            None
        } else {
            Some(cur as usize)
        };
    }
    match lookup[key].compare_exchange(
        NO_COLUMN,
        new_col as u16,
        Ordering::Relaxed,
        Ordering::Relaxed,
    ) {
        // Ok(_) means we won the CAS: our `new_col` is now installed.
        Ok(_) => Some(new_col),
        // Another thread won the race; their column id is valid for this key.
        Err(existing) => Some(existing as usize),
    }
}

fn collect_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    use ignore::WalkState;

    let (tx, rx) = mpsc::channel::<PathBuf>();
    let tx = std::sync::Arc::new(tx);
    let walker = ignore::WalkBuilder::new(root).build_parallel();
    let tx_factory = tx.clone();
    walker.run(move || {
        let tx = tx_factory.clone();
        Box::new(
            move |entry: Result<ignore::DirEntry, ignore::Error>| -> WalkState {
                if let Ok(e) = entry {
                    if e.file_type().is_some_and(|ft| ft.is_file()) {
                        let _ = tx.send(e.into_path());
                    }
                }
                WalkState::Continue
            },
        )
    });
    drop(tx);
    out.extend(rx);
    Ok(())
}

fn pad_to<T: Write + io::Seek>(w: &mut T, target: u64) -> io::Result<()> {
    let pos = w.stream_position()?;
    if target > pos {
        w.write_all(&vec![0u8; (target - pos) as usize])?;
    }
    Ok(())
}

fn write_path(w: &mut impl Write, p: &Path) -> io::Result<()> {
    let bytes = p.as_os_str().as_encoded_bytes();
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)
}

fn read_path(r: &mut &[u8]) -> Result<PathBuf> {
    let mut lenb = [0u8; 4];
    r.read_exact_into(&mut lenb)?;
    let len = u32::from_le_bytes(lenb) as usize;
    let (a, b) = r.split_at(len);
    *r = b;
    // Paths came from the filesystem, so the encoding is valid for this OS.
    Ok(PathBuf::from(unsafe {
        std::ffi::OsStr::from_encoded_bytes_unchecked(a)
    }))
}

trait ReadExactInto {
    fn read_exact_into(&mut self, buf: &mut [u8]) -> io::Result<()>;
}

impl ReadExactInto for &[u8] {
    fn read_exact_into(&mut self, buf: &mut [u8]) -> io::Result<()> {
        if self.len() < buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "index truncated",
            ));
        }
        let (a, b) = self.split_at(buf.len());
        buf.copy_from_slice(a);
        *self = b;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// trigram postings layer
// ---------------------------------------------------------------------------

/// Select grams by document frequency (df ≤ cap, rarest first) under a byte
/// budget, then re-read accepted files once to emit per-gram posting lists
/// (sorted file_ids, delta-varint encoded).
#[allow(clippy::type_complexity)]
/// A decoded posting: file id and the OR of (position mod 8) masks for the
/// gram within that file. Masks enable position-consistency filtering: for
/// needle trigrams at offsets o1..ok, a candidate must have masks compatible
/// with a single anchor position.
pub type Posting = (u32, u8);

/// Rotate an 8-bit mask RIGHT by k: bit b of the result is set iff bit
/// (b + k) mod 8 was set in m. Used to derive which anchor positions are
/// compatible with a gram occurring `k` bytes later.
fn rotr8(m: u8, k: u32) -> u8 {
    debug_assert!(k < 8);
    if k == 0 { m } else { (m >> k) | (m << (8 - k)) }
}

/// Selected trigram layers produced by the build.
pub struct TrigramLayers {
    /// Tier 1: delta-varint postings with loc masks (low-df grams).
    pub grams: &'static [u32],
    pub offsets: &'static [u32],
    pub arena: &'static [u8],
    /// Tier 2: dense bitmap columns (high-df grams, flat 12KB per gram
    /// regardless of df — ~4x cheaper per byte than varint at high df).
    /// Sorted grams; column i covers words starting at i*words.
    pub map_grams: &'static [u32],
    pub map_cols: &'static [u64],
    pub map_words: usize,
    /// Block-level presence bitmaps for map_grams (same order): gram g's
    /// bit b set = g occurs in 32KB block b (global block numbering =
    /// prefix sum of per-file block counts). Enables position-consistent
    /// pruning INSIDE candidate files.
    pub map_block_cols: &'static [u64],
    pub total_blocks: usize,
}

impl TrigramLayers {
    pub fn empty() -> Self {
        Self {
            grams: &[],
            offsets: &[0],
            arena: &[],
            map_grams: &[],
            map_cols: &[],
            map_words: 0,
            map_block_cols: &[],
            total_blocks: 0,
        }
    }

    pub fn memory_bytes(&self) -> usize {
        self.grams.len() * 4
            + self.offsets.len() * 4
            + self.arena.len()
            + self.map_grams.len() * 4
            + self.map_cols.len() * 8
            + self.map_block_cols.len() * 8
    }
}

/// Build both trigram tiers.
///
/// Tier selection by document frequency (sampled, scaled by `df_scale`):
/// - df in [crossover, df_cap] -> bitmap column (df-independent cost; take
///   highest-df first, they are the most likely needle grams).
/// - df in [df_min, crossover) -> varint postings (cost grows with df).
///
/// `crossover` is where varint cost (df*3B) meets bitmap cost (words*8B).
#[allow(clippy::type_complexity)]
fn build_trigram_postings(
    tg_df: &[std::sync::Mutex<std::collections::HashMap<u32, u32>>],
    final_paths: &[PathBuf],
    df_cap: usize,
    df_min: usize,
    df_scale: usize,
    words: usize,
    config: &BigramIndexConfig,
    pack_bytes: Option<&'static [u8]>,
    pack_spans: &[(u64, u64)],
    block_prefix: &[u64],
    total_blocks: usize,
) -> Result<TrigramLayers> {
    use std::collections::HashMap;

    // NOTE: the pack is written during the FILL pass now (span-allocated
    // write_at); empty trigram selection here is safe — no pack coupling.
    if config.trigram_budget_bytes == 0 || df_cap == 0 || df_cap <= df_min {
        return Ok(TrigramLayers::empty());
    }
    let _ = words; // bitmap tier sizing (unused at default budget)

    // Band partition from the sampled df map. The bitmap tier only engages
    // when trigram_bitmap_budget_bytes > 0.
    if std::env::var("RRG_DF_DEBUG").is_ok() {
        let mut all: Vec<u32> = Vec::new();
        for m in tg_df {
            for (&g, &df) in m.lock().unwrap().iter() {
                all.push((df as usize).saturating_mul(df_scale) as u32);
            }
        }
        all.sort_unstable();
        let n = all.len();
        let bands = [
            1u32,
            4,
            16,
            64,
            256,
            1024,
            4096,
            8192,
            16384,
            32768,
            65536,
            u32::MAX,
        ];
        eprintln!(
            "[df-hist] distinct sampled grams={n} total_pairs_sampled={}",
            all.iter().map(|&d| d as u64).sum::<u64>()
        );
        let mut lo = 0u64;
        for &b in &bands {
            let cnt = all.partition_point(|&d| d <= b) as u64;
            let pairs: u64 = all[..cnt as usize].iter().map(|&d| d as u64).sum::<u64>() - lo;
            eprintln!("[df-hist] df<={b}: grams={cnt} pairs={pairs}");
            lo += pairs;
        }
    }
    let use_bitmap = config.trigram_bitmap_budget_bytes > 0;
    let crossover = ((words * 8) / 3).max(64); // varint cost == bitmap cost

    let mut var_band: Vec<(u32, u32)> = Vec::new();
    let mut map_band: Vec<(u32, u32)> = Vec::new();
    for m in tg_df {
        for (&g, &df) in m.lock().unwrap().iter() {
            let df = (df as usize).saturating_mul(df_scale);
            if df >= df_min && df <= df_cap {
                if use_bitmap && df >= crossover {
                    map_band.push((g, df as u32));
                } else {
                    var_band.push((g, df as u32));
                }
            }
        }
    }

    // Tier 2 (bitmap): flat cost per gram -> take ALL of them, capped by
    // the column budget. Highest df first (most likely needle grams).
    map_band.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    let col_bytes = words * 8;
    let max_cols = config
        .trigram_bitmap_budget_bytes
        .saturating_div(col_bytes.max(1));
    let mut map_grams: Vec<u32> = map_band.iter().take(max_cols).map(|&(g, _)| g).collect();
    // MUST be ascending: query-side lookup is a binary_search over this.
    map_grams.sort_unstable();

    // Tier 1 (varint): cheap per gram -> take ALL of them, cheapest first;
    // if the budget runs out, drop the most expensive (highest df) ones —
    // those are exactly the ones the bitmap tier would have covered.
    var_band.sort_unstable_by(|a, b| a.1.cmp(&b.1));
    let mut var_selected: Vec<u32> = Vec::new();
    let mut budget = config.trigram_budget_bytes;
    for &(g, df) in &var_band {
        let est = df as usize * 3;
        if est > budget {
            continue;
        }
        budget -= est;
        var_selected.push(g);
    }

    if std::env::var("RRG_DF_DEBUG").is_ok() {
        eprintln!(
            "[sel] crossover={crossover} var_band={} map_band={} var_selected={} map_grams={} budget_left={budget}",
            var_band.len(),
            map_band.len(),
            var_selected.len(),
            map_grams.len(),
        );
        // probe needle grams given via RRG_DF_PROBE="spi pin"
        if let Ok(probe) = std::env::var("RRG_DF_PROBE") {
            let mut map: HashMap<u32, u32> = HashMap::new();
            for m in tg_df {
                for (&g, &df) in m.lock().unwrap().iter() {
                    map.insert(g, (df as usize).saturating_mul(df_scale) as u32);
                }
            }
            for word in probe.split_whitespace() {
                for t in word.as_bytes().windows(3) {
                    let g = ((t[0].to_ascii_lowercase() as u32) << 16)
                        | ((t[1].to_ascii_lowercase() as u32) << 8)
                        | t[2].to_ascii_lowercase() as u32;
                    eprintln!(
                        "[sel] probe gram {g:#x} ({}) df={:?} var_sel={} map_sel={}",
                        std::str::from_utf8(t).unwrap_or("?"),
                        map.get(&g),
                        var_selected.contains(&g),
                        map_grams.contains(&g),
                    );
                }
            }
        }
    }
    if var_selected.is_empty() && map_grams.is_empty() {
        // No grams selected: skip extraction. The pack was already written
        // during the fill pass (soundness no longer depends on this pass).
        return Ok(TrigramLayers::empty());
    }
    let mut var_sorted = var_selected.clone();
    var_sorted.sort_unstable();

    // Membership bitmaps over the 24-bit gram space (2 MiB each).
    const TG_SPACE: usize = 1 << 24;
    let mut var_bits = vec![0u8; TG_SPACE / 8];
    for &g in &var_sorted {
        var_bits[(g >> 3) as usize] |= 1 << (g & 7);
    }
    let mut map_bits = vec![0u8; TG_SPACE / 8];
    for &g in &map_grams {
        map_bits[(g >> 3) as usize] |= 1 << (g & 7);
    }
    // gram -> bitmap column (dense assignment in sorted order)
    let map_col_of: HashMap<u32, u16> = map_grams
        .iter()
        .enumerate()
        .map(|(i, &g)| (g, i as u16))
        .collect();

    // Pass 2: read back from the pack (one mmap; spans sorted by start so
    // parallel workers traverse mostly-sequential page-cache ranges).
    const OUT_SHARDS: usize = 64;
    let lists: Vec<std::sync::Mutex<Vec<(u32, u32, u8)>>> = (0..OUT_SHARDS)
        .map(|_| std::sync::Mutex::new(Vec::new()))
        .collect();
    let map_cols_atomic: Vec<AtomicU64> = (0..map_grams.len() * words)
        .map(|_| AtomicU64::new(0))
        .collect();
    let map_block_atomic: Vec<AtomicU64> = (0..map_grams.len() * total_blocks.div_ceil(64))
        .map(|_| AtomicU64::new(0))
        .collect();

    let mut visit_order: Vec<u32> = (0..final_paths.len() as u32).collect();
    if pack_bytes.is_some() {
        visit_order.sort_unstable_by_key(|&id| pack_spans[id as usize].0);
    }

    visit_order.par_iter().for_each(|&file_id| {
        let file_id = file_id as usize;
        let (start, len) = pack_spans[file_id];
        // Soundness: a missing/unreadable pack region must not silently
        // become an empty slice (0 matches = false negative); abort loudly.
        let bytes: &[u8] = match pack_bytes {
            Some(pk) => {
                let end = start.checked_add(len).and_then(|e| e.checked_sub(1));
                match end.and_then(|e| pk.get(start as usize..=e as usize)) {
                    Some(s) => s,
                    None => {
                        // fall back to a live read for this file
                        let path = &final_paths[file_id as usize];
                        match fs::read(path) {
                            Ok(b) => &*Box::leak(b.into_boxed_slice()),
                            Err(e) => panic!(
                                "pack span out of range for {} ({e}); corrupt pack",
                                path.display()
                            ),
                        }
                    }
                }
            }
            None => {
                let path = &final_paths[file_id as usize];
                match fs::read(path) {
                    Ok(b) => &*Box::leak(b.into_boxed_slice()),
                    Err(_) => return,
                }
            }
        };
        if bytes.len() < 3 {
            return;
        }
        let mut local: HashMap<u32, u8> = HashMap::with_capacity(256);
        let word_lo = file_id / 64;
        let bit = 1u64 << (file_id % 64);
        // Bitmap-tier bits are idempotent — common grams repeat
        // 10^4-10^6x per file, mostly inside the same block. Cache the last
        // (gram, block); emit atomics only on block transitions.
        let words_per_col = total_blocks.div_ceil(64);
        let mut last_g: u32 = u32::MAX;
        let mut last_col: usize = 0;
        let mut last_gblock: u64 = u64::MAX;
        for (pos, t) in bytes.windows(3).enumerate() {
            let g = ((t[0].to_ascii_lowercase() as u32) << 16)
                | ((t[1].to_ascii_lowercase() as u32) << 8)
                | t[2].to_ascii_lowercase() as u32;
            let hi3 = (g >> 3) as usize;
            let bit8 = 1 << (g & 7);
            if var_bits[hi3] & bit8 != 0 {
                *local.entry(g).or_insert(0) |= 1 << (pos % 8);
            } else if map_bits[hi3] & bit8 != 0 {
                if g != last_g {
                    last_g = g;
                    last_col = map_col_of[&g] as usize;
                    last_gblock = u64::MAX;
                }
                let gblock = block_prefix[file_id] + (pos as u64 / BLOCK_SIZE);
                if gblock != last_gblock {
                    last_gblock = gblock;
                    map_cols_atomic[last_col * words + word_lo].fetch_or(bit, Ordering::Relaxed);
                    map_block_atomic[last_col * words_per_col + (gblock / 64) as usize]
                        .fetch_or(1 << (gblock % 64), Ordering::Relaxed);
                }
            }
        }
        // group by shard, one lock per shard per file; records are flat
        // (gram, file_id, mask) — no nested maps, no per-pair hashing here
        // (the local map already deduped per file). Sort per
        // shard once at encode time instead of HashMap + per-gram sorts.
        let mut drain: Vec<(u32, u8)> = Vec::with_capacity(local.len());
        for kv in local.drain() {
            drain.push(kv);
        }
        drain.sort_unstable_by_key(|&(g, _)| g % OUT_SHARDS as u32);
        let mut i = 0;
        while i < drain.len() {
            let shard = (drain[i].0 % OUT_SHARDS as u32) as usize;
            let mut j = i;
            while j < drain.len() && (drain[j].0 % OUT_SHARDS as u32) as usize == shard {
                j += 1;
            }
            let mut m = lists[shard].lock().unwrap();
            for &(g, mask) in &drain[i..j] {
                m.push((g, file_id as u32, mask));
            }
            i = j;
        }
    });

    // Encode tier 1: sort each shard's records by (gram, id), collect the
    // distinct grams globally, then delta-varint each gram's run directly
    // out of its shard.
    let shard_lists: Vec<Vec<(u32, u32, u8)>> = lists
        .into_iter()
        .map(|m| {
            let mut v = m.into_inner().unwrap();
            v.sort_unstable_by_key(|&(g, id, _)| (g, id));
            v
        })
        .collect();
    let mut grams: Vec<u32> = Vec::new();
    for v in &shard_lists {
        let mut last = u32::MAX;
        for &(g, _, _) in v {
            if g != last {
                grams.push(g);
                last = g;
            }
        }
    }
    grams.sort_unstable();
    let mut arena: Vec<u8> = Vec::with_capacity(config.trigram_budget_bytes / 2);
    let mut offsets: Vec<u32> = Vec::with_capacity(grams.len() + 1);
    offsets.push(0);
    for &g in &grams {
        let shard = (g % OUT_SHARDS as u32) as usize;
        let v = &shard_lists[shard];
        let lo = v.partition_point(|&(gg, _, _)| gg < g);
        let hi = v.partition_point(|&(gg, _, _)| gg <= g);
        let mut prev = 0u32;
        for &(_, id, mask) in &v[lo..hi] {
            let mut x = id - prev;
            loop {
                let b = (x & 0x7F) as u8;
                x >>= 7;
                if x == 0 {
                    arena.push(b | 0x80); // sentinel high bit marks LAST byte
                    break;
                }
                arena.push(b);
            }
            arena.push(mask);
            prev = id;
        }
        offsets.push(arena.len() as u32);
    }

    let map_cols: Vec<u64> = map_cols_atomic
        .into_iter()
        .map(|a| a.into_inner())
        .collect();
    let map_block_cols: Vec<u64> = map_block_atomic
        .into_iter()
        .map(|a| a.into_inner())
        .collect();

    // Leak: build is a one-shot process; the daemon keeps one index alive.
    // 'static slices let load() mmap without any parse copies.
    Ok(TrigramLayers {
        grams: Box::leak(grams.into_boxed_slice()),
        offsets: Box::leak(offsets.into_boxed_slice()),
        arena: Box::leak(arena.into_boxed_slice()),
        map_grams: Box::leak(map_grams.into_boxed_slice()),
        map_cols: Box::leak(map_cols.into_boxed_slice()),
        map_words: words,
        map_block_cols: Box::leak(map_block_cols.into_boxed_slice()),
        total_blocks,
    })
}

impl BigramIndex {
    /// Decode a trigram's tier-1 posting list. None if gram not in band.
    fn trigram_posting(&self, gram: u32) -> Option<Vec<Posting>> {
        let idx = self.tg.grams.binary_search(&gram).ok()?;
        let start = self.tg.offsets[idx] as usize;
        let end = self.tg.offsets[idx + 1] as usize;
        let mut out = Vec::with_capacity((end - start) * 2);
        let mut prev = 0u32;
        let mut i = start;
        while i < end {
            let mut delta = 0u32;
            let mut shift = 0;
            loop {
                let b = self.tg.arena[i];
                i += 1;
                if b & 0x80 != 0 {
                    delta |= ((b & 0x7F) as u32) << shift;
                    break;
                }
                delta |= (b as u32) << shift;
                shift += 7;
            }
            let mask = self.tg.arena[i];
            i += 1;
            prev += delta;
            out.push((prev, mask));
        }
        Some(out)
    }

    /// Bitmap column for a tier-2 gram, if present.
    fn trigram_map_col(&self, gram: u32) -> Option<&[u64]> {
        let idx = self.tg.map_grams.binary_search(&gram).ok()?;
        let base = idx * self.tg.map_words;
        Some(&self.tg.map_cols[base..base + self.tg.map_words])
    }

    /// Intersect a candidate set with the trigram layers for a literal.
    /// Tier 1 lists intersect with position-consistency masks; tier 2 bitmap
    /// columns AND into the result. Purely an intersection: the superset
    /// guarantee is untouched. Returns false if nothing changed.
    pub fn refine_trigrams(&self, literal: &str, ids: &mut Vec<u32>) -> bool {
        let bytes = literal.as_bytes();
        if bytes.len() < 3 {
            return false;
        }
        let mut grams: Vec<(usize, u32)> = bytes
            .windows(3)
            .enumerate()
            .map(|(off, t)| {
                (
                    off,
                    ((t[0].to_ascii_lowercase() as u32) << 16)
                        | ((t[1].to_ascii_lowercase() as u32) << 8)
                        | t[2].to_ascii_lowercase() as u32,
                )
            })
            .collect();
        grams.sort_unstable_by_key(|&(_, g)| g);
        grams.dedup_by_key(|x| x.1);
        self.refine_grams(&grams, ids)
    }

    /// Core refinement over (needle offset, gram) pairs (deduped).
    fn refine_grams(&self, grams: &[(usize, u32)], ids: &mut Vec<u32>) -> bool {
        if self.tg.grams.is_empty() && self.tg.map_grams.is_empty() {
            return false;
        }
        let lists: Vec<(usize, Vec<Posting>)> = {
            let mut l: Vec<(usize, Vec<Posting>)> = grams
                .iter()
                .filter_map(|&(off, g)| self.trigram_posting(g).map(|p| (off, p)))
                .collect();
            l.sort_unstable_by_key(|(_, p)| p.len());
            l
        };
        let cols: Vec<&[u64]> = grams
            .iter()
            .filter_map(|&(_, g)| self.trigram_map_col(g))
            .collect();
        if lists.is_empty() && cols.is_empty() {
            return false;
        }

        let mut bits = vec![0u64; self.words];
        let mut off0 = usize::MAX;
        if !lists.is_empty() {
            // Position-consistent intersection over tier-1 lists, rarest
            // first. Anchor = rarest gram; every other gram's mask is
            // rotated by the anchor-relative needle offset.
            off0 = lists[0].0;
            let mut acc: Vec<Posting> = lists[0].1.clone();
            for (off_i, l) in &lists {
                if *off_i == off0 {
                    continue;
                }
                let d = ((*off_i as i64 - off0 as i64).rem_euclid(8)) as u32;
                let mut per_file: std::collections::HashMap<u32, u8> =
                    std::collections::HashMap::with_capacity(l.len());
                for (id, m) in l {
                    per_file.insert(*id, rotr8(*m, d));
                }
                acc.retain(|(id, m)| match per_file.get(id) {
                    Some(req) => m & req != 0,
                    None => false,
                });
                if acc.is_empty() {
                    break;
                }
            }
            for (id, _) in &acc {
                bits[(*id as usize) / 64] |= 1 << (*id as usize % 64);
            }
        } else {
            for b in bits.iter_mut() {
                *b = u64::MAX;
            }
        }
        // Tier 2: plain AND (no position info in bitmaps).
        for col in cols {
            for (w, c) in bits.iter_mut().zip(col.iter()) {
                *w &= c;
            }
        }

        let n = self.paths.len();
        let before = ids.len();
        ids.retain(|&id| {
            let (w, b) = (id as usize / 64, id as usize % 64);
            (id as usize) < n
                && bits[w] & (1 << b) != 0
                && self.block_survives(grams, off0, id as usize)
        });
        ids.len() != before
    }

    /// Block-level pruning for bitmap-tier grams: file survives iff some
    /// local block k satisfies every bitmap gram's {k+d, k+d+1} presence
    /// window (d = anchor-relative needle offset / 32KB). Sound: a needle
    /// occurrence anchors in some block k, and any other needle gram occurs
    /// at a byte offset < 2 blocks away.
    fn block_survives(&self, grams: &[(usize, u32)], off0: usize, file_id: usize) -> bool {
        if std::env::var("RRG_CALL_DEBUG").is_ok() {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let c = N.fetch_add(1, Ordering::Relaxed);
            if c < 3 {
                eprintln!(
                    "[call] block_survives file_id={file_id} path={:?}",
                    self.paths.get(file_id)
                );
            }
        }
        if std::env::var("RRG_NO_BLOCK").is_ok()
            || self.tg.map_block_cols.is_empty()
            || file_id + 1 >= self.block_prefix.len()
        {
            return true;
        }
        let total_blocks = *self.block_prefix.last().unwrap_or(&0) as usize;
        let wpg = total_blocks.div_ceil(64).max(1);
        let base = self.block_prefix[file_id];
        let n_blocks = (self.block_prefix[file_id + 1] - base).max(1);
        if std::env::var("RRG_PROBE_FILE")
            .ok()
            .and_then(|p| self.paths.get(file_id).map(|q| q.ends_with(&p)))
            .unwrap_or(false)
        {
            eprintln!("[probe] file_id={file_id} blocks={n_blocks} base={base}");
            for &(off, g) in grams {
                if let Ok(col) = self.tg.map_grams.binary_search(&g) {
                    let slab = &self.tg.map_block_cols[col * wpg..][..wpg];
                    let mut set = 0usize;
                    for k in 0..n_blocks {
                        let b = base + k;
                        if b < total_blocks as u64 {
                            let w = slab[(b / 64) as usize];
                            if w & (1 << (b % 64)) != 0 {
                                set += 1;
                            }
                        }
                    }
                    eprintln!("[probe] gram {g:#x} off={off} set in {set}/{n_blocks} blocks");
                } else {
                    eprintln!("[probe] gram {g:#x} off={off} NOT in bitmap tier");
                }
            }
        }
        let mut survivors = vec![true; n_blocks as usize];
        for &(off, g) in grams {
            let col = match self.tg.map_grams.binary_search(&g) {
                Ok(i) => i,
                Err(_) => continue, // gram not in bitmap tier: skip (sound)
            };
            // Anchor-relative byte distance may be NEGATIVE (anchor can be
            // a later needle gram): the gram's block is then k-1. Use
            // signed euclidean division for the block window.
            let rel = off as i64 - off0 as i64;
            let q = rel.div_euclid(BLOCK_SIZE as i64);
            let slab = &self.tg.map_block_cols[col * wpg..][..wpg];
            let present = |b: i64| -> bool {
                if b < 0 || b >= total_blocks as i64 {
                    return false;
                }
                let w = slab[(b / 64) as usize];
                w & (1 << (b % 64)) != 0
            };
            for k in 0..n_blocks as i64 {
                if !survivors[k as usize] {
                    continue;
                }
                let b1 = base as i64 + k + q;
                let b2 = b1 + 1;
                if !present(b1) && !present(b2) {
                    survivors[k as usize] = false;
                }
            }
            if !survivors.iter().any(|&x| x) {
                if let Ok(path) = std::env::var("RRG_PRUNE_LOG") {
                    use std::sync::OnceLock;
                    static LOG: OnceLock<std::sync::Mutex<std::fs::File>> = OnceLock::new();
                    let f = LOG.get_or_init(|| {
                        std::sync::Mutex::new(
                            std::fs::OpenOptions::new()
                                .create(true)
                                .write(true)
                                .truncate(true)
                                .open(&path)
                                .unwrap(),
                        )
                    });
                    use std::io::Write;
                    let mut f = f.lock().unwrap();
                    let _ = writeln!(
                        f,
                        "{}",
                        self.paths
                            .get(file_id)
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    );
                }
                return false;
            }
        }
        let r = survivors.iter().any(|&x| x);
        if std::env::var("RRG_PROBE_FILE")
            .ok()
            .and_then(|p| self.paths.get(file_id).map(|q| q.ends_with(&p)))
            .unwrap_or(false)
        {
            eprintln!(
                "[probe] block_survives RETURN={r} survivors_set={}",
                survivors.iter().filter(|&&x| x).count()
            );
        }
        r
    }

    /// Trigram-level refinement driven by a regex-derived plan: per-branch
    /// literal trigrams, unioned/intersected exactly like the bigram plan.
    /// Grams not present in either layer are skipped (sound: weaker filter).
    pub fn refine_plan_trigrams(&self, plan: &TriPlan, ids: &mut Vec<u32>) -> bool {
        let r = match self.tri_eval(plan) {
            TriEval::NoConstraint | TriEval::MatchAll => false,
            TriEval::Bitmap(bits) => {
                let n = self.paths.len();
                let before = ids.len();
                ids.retain(|&id| {
                    let (w, b) = (id as usize / 64, id as usize % 64);
                    (id as usize) < n && bits[w] & (1 << b) != 0
                });
                ids.len() != before
            }
        };
        if let Ok(path) = std::env::var("RRG_CAND_LOG") {
            use std::sync::OnceLock;
            static LOG: OnceLock<std::sync::Mutex<std::fs::File>> = OnceLock::new();
            let f = LOG.get_or_init(|| {
                std::sync::Mutex::new(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(&path)
                        .unwrap(),
                )
            });
            use std::io::Write;
            let mut f = f.lock().unwrap();
            for &id in ids.iter() {
                let _ = writeln!(f, "{}", self.paths[id as usize].display());
            }
        }
        r
    }

    fn tri_eval(&self, plan: &TriPlan) -> TriEval {
        match plan {
            TriPlan::All => TriEval::NoConstraint,
            TriPlan::Grams(pairs) => {
                if pairs.is_empty() {
                    return TriEval::NoConstraint;
                }
                // Grams come from ONE literal run, so offsets are true
                // needle offsets: full position-consistent rotation applies.
                let lists: Vec<(usize, Vec<Posting>)> = {
                    let mut l: Vec<(usize, Vec<Posting>)> = pairs
                        .iter()
                        .filter_map(|&(off, g)| self.trigram_posting(g).map(|p| (off, p)))
                        .collect();
                    l.sort_unstable_by_key(|(_, p)| p.len());
                    l
                };
                let cols: Vec<&[u64]> = pairs
                    .iter()
                    .filter_map(|&(_, g)| self.trigram_map_col(g))
                    .collect();
                if lists.is_empty() && cols.is_empty() {
                    return TriEval::NoConstraint;
                }
                let mut bits = vec![0u64; self.words];
                if !lists.is_empty() {
                    let off0 = lists[0].0;
                    let mut acc: Vec<Posting> = lists[0].1.clone();
                    for (off_i, l) in &lists {
                        if *off_i == off0 {
                            continue;
                        }
                        let d = ((*off_i as i64 - off0 as i64).rem_euclid(8)) as u32;
                        let mut per_file: std::collections::HashMap<u32, u8> =
                            std::collections::HashMap::with_capacity(l.len());
                        for (id, m) in l {
                            per_file.insert(*id, rotr8(*m, d));
                        }
                        acc.retain(|(id, m)| match per_file.get(id) {
                            Some(req) => m & req != 0,
                            None => false,
                        });
                        if acc.is_empty() {
                            break;
                        }
                    }
                    for (id, _) in &acc {
                        bits[(*id as usize) / 64] |= 1 << (*id as usize % 64);
                    }
                } else {
                    for b in bits.iter_mut() {
                        *b = u64::MAX;
                    }
                }
                for col in cols {
                    for (w, c) in bits.iter_mut().zip(col.iter()) {
                        *w &= c;
                    }
                }
                // Block-level prune per candidate file (offsets are true
                // needle offsets within this literal run).
                let probe_hit = |idx: &Self, id: usize| -> bool {
                    std::env::var("RRG_PROBE_FILE")
                        .ok()
                        .and_then(|p| idx.paths.get(id).map(|q| q.ends_with(&p)))
                        .unwrap_or(false)
                };
                if std::env::var("RRG_PHASES").is_ok() {
                    let pre: usize = bits.iter().map(|w| w.count_ones() as usize).sum();
                    let probe_set = (0..self.paths.len())
                        .find(|&i| probe_hit(self, i))
                        .map(|i| bits[i / 64] & (1 << (i % 64)) != 0);
                    eprintln!("[block-prune] pre={pre} probe_bit_set={probe_set:?}");
                }
                if !pairs.is_empty() {
                    let off0 = pairs[0].0;
                    let n = self.paths.len();
                    for (w, word) in bits.iter_mut().enumerate() {
                        let mut keep = *word;
                        while keep != 0 {
                            let bit = keep.trailing_zeros() as usize;
                            keep &= keep - 1;
                            let id = w * 64 + bit;
                            if id < n && !self.block_survives(pairs, off0, id) {
                                *word &= !(1u64 << bit);
                            }
                        }
                    }
                }
                if std::env::var("RRG_PHASES").is_ok() {
                    let post: usize = bits.iter().map(|w| w.count_ones() as usize).sum();
                    let probe_set = (0..self.paths.len())
                        .find(|&i| probe_hit(self, i))
                        .map(|i| bits[i / 64] & (1 << (i % 64)) != 0);
                    eprintln!("[block-prune] post={post} probe_bit_set={probe_set:?}");
                }
                TriEval::Bitmap(bits)
            }
            TriPlan::Intersect(parts) => {
                let mut acc: Option<Vec<u64>> = None;
                for p in parts {
                    match self.tri_eval(p) {
                        TriEval::MatchAll => return TriEval::MatchAll,
                        TriEval::NoConstraint => {}
                        TriEval::Bitmap(b) => match &mut acc {
                            None => acc = Some(b),
                            Some(a) => {
                                for (x, y) in a.iter_mut().zip(b.iter()) {
                                    *x &= y;
                                }
                            }
                        },
                    }
                }
                acc.map_or(TriEval::NoConstraint, TriEval::Bitmap)
            }
            TriPlan::Union(parts) => {
                let mut bits = vec![0u64; self.words];
                for p in parts {
                    match self.tri_eval(p) {
                        TriEval::MatchAll | TriEval::NoConstraint => return TriEval::MatchAll,
                        TriEval::Bitmap(b) => {
                            for (x, y) in bits.iter_mut().zip(b.iter()) {
                                *x |= y;
                            }
                        }
                    }
                }
                TriEval::Bitmap(bits)
            }
        }
    }
}

fn take_u64(r: &mut &[u8]) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact_into(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(p: impl AsRef<Path>, s: &str) {
        let p = p.as_ref();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn candidates_are_exact_superset() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path().join("a.txt"),
            "the quick brown fox\nspin_lock() call\n",
        );
        write(dir.path().join("b.txt"), "nothing relevant here\n");
        write(dir.path().join("sub/c.txt"), "spin_lock and spin_locks\n");
        write(dir.path().join("binary.bin"), "ok\0fine"); // binary -> fallback

        let (idx, timings) =
            BigramIndex::build(dir.path(), &BigramIndexConfig::default(), None).unwrap();
        assert_eq!(timings.files, 3); // indexed (binary.bin excluded)
        assert_eq!(idx.paths.len(), 3);
        assert_eq!(idx.fallback_paths.len(), 1);

        match idx.candidates("spin_lock") {
            Candidates::Superset(ids) => {
                let hit_paths: BTreeSet<_> =
                    ids.iter().map(|&i| idx.paths[i as usize].clone()).collect();
                assert!(hit_paths.contains(&dir.path().join("a.txt")));
                assert!(hit_paths.contains(&dir.path().join("sub/c.txt")));
                assert!(!hit_paths.contains(&dir.path().join("b.txt")));
            }
            other => panic!("expected Superset, got {other:?}"),
        }

        // Bigram absent from the entire corpus -> its column was never
        // allocated -> MatchAll (safe: full scan finds nothing either).
        assert_eq!(idx.candidates("zzzzz"), Candidates::MatchAll);

        // Single-char literal -> MatchAll.
        assert_eq!(idx.candidates("x"), Candidates::MatchAll);
    }

    #[test]
    fn case_insensitive_candidates_superset_case_sensitive() {
        let dir = TempDir::new().unwrap();
        write(dir.path().join("a.txt"), "SPIN_LOCK\n");
        let (idx, _) = BigramIndex::build(dir.path(), &BigramIndexConfig::default(), None).unwrap();
        // Lowercase query finds the uppercase file (superset); exact case is
        // enforced later by verification.
        assert!(matches!(
            idx.candidates("spin_lock"),
            Candidates::Superset(_)
        ));
    }

    #[test]
    fn regex_plans_are_sound() {
        let dir = TempDir::new().unwrap();
        write(dir.path().join("a.txt"), "alpha spin_lock here\n");
        write(dir.path().join("b.txt"), "mutex_unlock only\n");
        write(dir.path().join("c.txt"), "wait_queue alone\n");
        write(dir.path().join("d.txt"), "nothing at all\n");
        let (idx, _) = BigramIndex::build(dir.path(), &BigramIndexConfig::default(), None).unwrap();

        // Alternation: union of the three literals' candidate sets must
        // cover a/b/c. (Regression: naive pattern-string bigrams miss.)
        let plan = plan_for_regex("spin_lock|mutex_unlock|wait_queue").unwrap();
        match idx.candidates_plan(&plan) {
            Candidates::Superset(ids) => {
                let hits: BTreeSet<_> =
                    ids.iter().map(|&i| idx.paths[i as usize].clone()).collect();
                assert!(hits.contains(&dir.path().join("a.txt")));
                assert!(hits.contains(&dir.path().join("b.txt")));
                assert!(hits.contains(&dir.path().join("c.txt")));
            }
            other => panic!("expected Superset, got {other:?}"),
        }

        // Optional part: (foo)? contributes nothing; 'wait' must still drive
        // the filter soundly.
        let plan = plan_for_regex("(xyzzy)?wait").unwrap();
        match idx.candidates_plan(&plan) {
            Candidates::Superset(ids) => {
                let hits: BTreeSet<_> =
                    ids.iter().map(|&i| idx.paths[i as usize].clone()).collect();
                assert!(hits.contains(&dir.path().join("c.txt")));
            }
            other => panic!("expected Superset, got {other:?}"),
        }

        // Pure class: no literal -> MatchAll.
        let plan = plan_for_regex("[a-z]+\\d+").unwrap();
        assert_eq!(idx.candidates_plan(&plan), Candidates::MatchAll);
    }

    #[test]
    fn column_budget_degrades_to_match_all() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path().join("a.txt"),
            "needle in a haystack with xy and qz\n",
        );
        let cfg = BigramIndexConfig {
            max_columns: 3,
            ..Default::default()
        };
        let (idx, _) = BigramIndex::build(dir.path(), &cfg, None).unwrap();
        // "haystack" has >3 distinct bigrams; some key lost the column race
        // or was never allocated -> must be MatchAll, never a wrong answer.
        assert_eq!(idx.candidates("haystack"), Candidates::MatchAll);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        write(dir.path().join("a.txt"), "alpha beta gamma spin_lock\n");
        write(dir.path().join("b.txt"), "delta\n");
        write(dir.path().join("big.bin"), "skipped\0binary");

        let (idx, _) = BigramIndex::build(dir.path(), &BigramIndexConfig::default(), None).unwrap();
        let idx_dir = dir.path().join(".rrg-index");
        idx.save(&idx_dir).unwrap();
        let loaded = BigramIndex::load(&idx_dir).unwrap();

        assert_eq!(loaded.paths, idx.paths);
        assert_eq!(loaded.file_meta, idx.file_meta);
        assert_eq!(loaded.fallback_paths, idx.fallback_paths);
        assert_eq!(loaded.words, idx.words);
        assert_eq!(loaded.n_columns(), idx.n_columns());
        assert_eq!(loaded.memory_bytes(), idx.memory_bytes());
        match (idx.candidates("spin_lock"), loaded.candidates("spin_lock")) {
            (Candidates::Superset(a), Candidates::Superset(b)) => assert_eq!(a, b),
            other => panic!("mismatch: {other:?}"),
        }
    }
}
