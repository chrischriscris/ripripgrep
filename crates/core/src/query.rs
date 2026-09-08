//! Shared indexed-query execution: one implementation used by the CLI
//! (in-process fallback) and the daemon (hot index).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::index::{BigramIndex, Candidates, Plan, TriPlan};
use crate::search::{OutputMode, SearchOptions, search_files};

#[derive(Debug, Clone)]
pub struct QueryOptions {
    pub fixed_strings: bool,
    pub case_insensitive: bool,
    /// Full (mtime, len) validation before filtering. Costs one stat per
    /// indexed file.
    pub check_stale: bool,
    /// Output shape (Count keeps the benchmarked fast paths).
    pub mode: OutputMode,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct QueryAnswer {
    pub scanned: usize,
    pub candidates: usize,
    pub mode: String,
    pub fallback: usize,
    pub matched: usize,
    pub matches: usize,
    pub query_ms: f64,
    pub stale: Option<usize>,
    /// Lines-mode output (`path:line:text`), empty in Count/Files mode.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
    /// Files-mode paths, empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

/// Build the sound candidate plan for a pattern.
fn plan_for(opts: &QueryOptions, pattern: &str) -> Plan {
    if opts.fixed_strings {
        crate::index::plan_for_literal(pattern)
    } else {
        crate::index::plan_for_regex(pattern).unwrap_or(Plan::All)
    }
}

/// Trigram refinement plan for a pattern (None = nothing checkable).
fn tri_plan_for(opts: &QueryOptions, pattern: &str) -> Option<TriPlan> {
    if opts.fixed_strings {
        Some(crate::index::tri_plan_for_literal(pattern))
    } else {
        crate::index::tri_plan_for_regex(pattern).ok()
    }
}

/// Open + validate the packed contents store for an index.
/// Returns (mmap, per-file prefix offsets) when usable.
fn open_pack(idx: &BigramIndex, index_dir: &Path) -> Option<(memmap2::Mmap, Vec<(u64, u64)>)> {
    if idx.pack_len == 0 {
        return None;
    }
    let pack_path = index_dir.join("contents.pack");
    let meta = std::fs::metadata(&pack_path).ok()?;
    if meta.len() != idx.pack_len {
        return None;
    }
    let file = std::fs::File::open(&pack_path).ok()?;
    let mmap = unsafe { memmap2::Mmap::map(&file).ok()? };
    Some((mmap, idx.pack_spans.clone()))
}

/// Execute an indexed query end-to-end: candidates -> trigram refinement ->
/// verification of candidate files (pack slices when available, live files
/// otherwise) + fallback files (always live from disk).
pub fn run_query(
    idx: &BigramIndex,
    index_dir: &Path,
    pattern: &str,
    opts: &QueryOptions,
    forced: &[PathBuf],
) -> Result<QueryAnswer> {
    let tq = Instant::now();

    // Dirty files (watcher-reported): their pack slices are stale, so they
    // are verified LIVE and removed from the pack-backed candidate set.
    // Over-inclusion is free (verification reads real contents); the only
    // unsoundness would be missing a changed file, hence the forced set.
    let forced_ids: HashSet<u32> = if forced.is_empty() {
        HashSet::new()
    } else {
        let map: std::collections::HashMap<&Path, u32> = idx
            .paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.as_path(), i as u32))
            .collect();
        forced
            .iter()
            .filter_map(|p| map.get(p.as_path()).copied())
            .collect()
    };

    let mut stale = None;
    let mut force_live = false;
    let plan = if opts.check_stale {
        let s = idx.staleness();
        if s > 0 {
            eprintln!("warning: {s} indexed files changed since build; falling back to full scan");
            stale = Some(s);
            // SOUNDNESS: candidates below would otherwise be verified from
            // STALE pack bytes (changed content can stop matching = false
            // negatives). Every indexed file is verified live instead.
            force_live = true;
            Plan::All
        } else {
            plan_for(opts, pattern)
        }
    } else {
        plan_for(opts, pattern)
    };

    let (mut candidate_ids, mode) = match idx.candidates_plan(&plan) {
        Candidates::Superset(mut ids) => {
            let mut refined = "bigram filter";
            if let Some(tri) = tri_plan_for(opts, pattern)
                && idx.refine_plan_trigrams(&tri, &mut ids)
            {
                refined = "bigram+trigram filter";
            }
            (ids, refined)
        }
        Candidates::None => (Vec::new(), "no candidate files"),
        Candidates::MatchAll => (
            (0..idx.paths.len() as u32).collect(),
            "full scan (unselective bigrams)",
        ),
    };

    if !forced_ids.is_empty() {
        candidate_ids.retain(|id| !forced_ids.contains(id));
    }

    let sopts = SearchOptions {
        case_insensitive: opts.case_insensitive,
        fixed_strings: opts.fixed_strings,
        mode: opts.mode,
        ..Default::default()
    };

    // Compile the verification engine once per query to avoid per-file overhead.
    let verifier =
        crate::search::SliceVerifier::new(pattern, opts.fixed_strings, opts.case_insensitive)?;

    let mut dirty_stats = crate::search::SearchStats::default();
    if !forced_ids.is_empty() {
        dirty_stats = search_files(
            pattern,
            &forced_ids
                .iter()
                .map(|&id| idx.paths[id as usize].clone())
                .collect::<Vec<_>>(),
            &sopts,
        )?;
    }

    let stats = if force_live {
        // Full live scan: candidates + fallback, all read from disk.
        let mut files: Vec<PathBuf> = candidate_ids
            .iter()
            .map(|&id| idx.paths[id as usize].clone())
            .collect();
        files.extend(idx.fallback_paths.iter().cloned());
        let live = search_files(pattern, &files, &sopts)?;
        dirty_stats + live
    } else if let Some((pack, offsets)) = open_pack(idx, index_dir) {
        // Pack-backed verification: contiguous slices of one mmap.
        use rayon::prelude::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fv = AtomicUsize::new(0);
        let fm = AtomicUsize::new(0);
        let mm = AtomicUsize::new(0);
        let out_buf: Mutex<Vec<u8>> = Mutex::new(Vec::new());
        let files_out: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let bytes: &[u8] = &pack[..];
        match opts.mode {
            OutputMode::Count => {
                candidate_ids.par_iter().for_each(|&id| {
                    fv.fetch_add(1, Ordering::Relaxed);
                    let (start, len) = offsets[id as usize];
                    let slice = &bytes[start as usize..start as usize + len as usize];
                    let n = verifier.count_matching_lines(slice);
                    if n > 0 {
                        fm.fetch_add(1, Ordering::Relaxed);
                        mm.fetch_add(n, Ordering::Relaxed);
                    }
                });
            }
            OutputMode::Lines | OutputMode::Files => {
                // Emit modes: one shared matcher; per-candidate slices go
                // through the searcher with a line-formatter sink.
                let matcher = grep_regex::RegexMatcherBuilder::new()
                    .case_insensitive(opts.case_insensitive)
                    .fixed_strings(opts.fixed_strings)
                    .build(pattern)?;
                candidate_ids.par_iter().for_each(|&id| {
                    fv.fetch_add(1, Ordering::Relaxed);
                    let (start, len) = offsets[id as usize];
                    let slice = &bytes[start as usize..start as usize + len as usize];
                    let path = &idx.paths[id as usize];
                    let mut buf = Vec::with_capacity(4096);
                    let (hit, n) =
                        crate::search::scan_slice(&matcher, path, slice, &sopts, &mut buf);
                    if hit {
                        fm.fetch_add(1, Ordering::Relaxed);
                        mm.fetch_add(n, Ordering::Relaxed);
                        match opts.mode {
                            OutputMode::Lines => out_buf.lock().unwrap().extend_from_slice(&buf),
                            OutputMode::Files => {
                                files_out.lock().unwrap().push(path.display().to_string())
                            }
                            OutputMode::Count => unreachable!(),
                        }
                    }
                });
            }
        }
        let disk = search_files(pattern, &idx.fallback_paths, &sopts)?;
        dirty_stats
            + disk
            + crate::search::SearchStats {
                files_visited: fv.load(Ordering::Relaxed),
                files_matched: fm.load(Ordering::Relaxed),
                matches: mm.load(Ordering::Relaxed),
                errors: 0,
                output: String::from_utf8_lossy(&out_buf.into_inner().unwrap()).into_owned(),
                files: files_out.into_inner().unwrap(),
            }
    } else {
        let mut files: Vec<PathBuf> = candidate_ids
            .iter()
            .map(|&id| idx.paths[id as usize].clone())
            .collect();
        files.extend(idx.fallback_paths.iter().cloned());
        let live = search_files(pattern, &files, &sopts)?;
        dirty_stats + live
    };

    Ok(QueryAnswer {
        scanned: stats.files_visited,
        candidates: candidate_ids.len(),
        mode: mode.to_string(),
        fallback: idx.fallback_paths.len(),
        matched: stats.files_matched,
        matches: stats.matches,
        query_ms: tq.elapsed().as_secs_f64() * 1000.0,
        stale,
        output: stats.output,
        files: stats.files,
    })
}
