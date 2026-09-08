//! Differential property test: the bigram index's candidate set must always
//! be a superset of the true match set for any literal. This is the invariant
//! that makes indexed search correct — verification does the rest.

use std::fs;

use proptest::prelude::*;
use rrg_core::{BigramIndex, BigramIndexConfig, SearchOptions, search_files};
use tempfile::TempDir;

/// Deterministic random corpus generator: `n_files` files of random ASCII
/// words over a small alphabet (forces realistic bigram reuse + columns).
fn make_corpus(dir: &std::path::Path, files: &[(usize, Vec<String>)]) {
    for (id, words) in files {
        let p = dir.join(format!("file_{id:03}.txt"));
        fs::write(p, words.join(" ")).unwrap();
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn candidates_superset_of_true_matches(
        n_files in 1usize..12,
        seed_words in prop::collection::vec(
            prop::collection::vec("[a-z]{2,8}", 1..40),
            1..12,
        ),
        needle in "[a-z]{3,8}",
    ) {
        let dir = TempDir::new().unwrap();
        make_corpus(dir.path(), &(0..n_files).zip(seed_words).collect::<Vec<_>>());

        let (idx, _t) =
            BigramIndex::build(dir.path(), &BigramIndexConfig::default(), None).unwrap();

        // Ground truth: brute-force scan of every file.
        let all: Vec<std::path::PathBuf> = idx.paths.clone();
        let truth = search_files(&needle, &all, &SearchOptions {
            fixed_strings: true,
            ..Default::default()
        })
        .unwrap();

        // Indexed path: candidates + verification over the same files.
        let (mut cand_ids, mode) = match idx.candidates(&needle) {
            rrg_core::Candidates::Superset(ids) => (ids, 0u8),
            rrg_core::Candidates::None => (Vec::new(), 1),
            rrg_core::Candidates::MatchAll => ((0..all.len() as u32).collect(), 2),
        };
        idx.refine_trigrams(&needle, &mut cand_ids);
        let cand_files: Vec<std::path::PathBuf> =
            cand_ids.iter().map(|&i| all[i as usize].clone()).collect();
        let got = search_files(&needle, &cand_files, &SearchOptions {
            fixed_strings: true,
            ..Default::default()
        })
        .unwrap();

        let msg = format!(
            "mode={mode}: indexed search missed matches (needle={needle:?})"
        );
        prop_assert_eq!(truth.matches, got.matches, "{}", msg);
        prop_assert_eq!(truth.files_matched, got.files_matched);
    }

    #[test]
    fn mmap_counter_matches_oracle(
        n_files in 1usize..12,
        seed_words in prop::collection::vec(
            prop::collection::vec("[A-Za-z]{2,8}", 1..40),
            1..12,
        ),
        needle in "[A-Za-z]{3,8}",
        max_columns in 2usize..16,
    ) {
        let dir = TempDir::new().unwrap();
        make_corpus(dir.path(), &(0..n_files).zip(seed_words).collect::<Vec<_>>());
        // Mixed case corpus exercises the ASCII-fold superset path; the
        // oracle ignores case to match the index's fold-then-verify design.
        let (idx, _t) = BigramIndex::build(
            dir.path(),
            &BigramIndexConfig { max_columns, ..Default::default() },
            None,
        )
        .unwrap();
        let all: Vec<std::path::PathBuf> = idx.paths.clone();
        let truth = search_files(&needle, &all, &SearchOptions {
            fixed_strings: true,
            case_insensitive: true,
            ..Default::default()
        })
        .unwrap();
        let got = rrg_core::count_occurrences_mmap(&needle, &all, true);
        let msg = format!("mmap counter diverged (needle={needle:?})");
        prop_assert_eq!(truth.matches, got.matches, "{}", msg);
        prop_assert_eq!(truth.files_matched, got.files_matched);
    }

    #[test]
    fn tiny_column_budget_never_loses_matches(
        seed_words in prop::collection::vec(
            prop::collection::vec("[a-z]{2,10}", 1..60),
            8..24,
        ),
        needle in "[a-z]{3,10}",
        max_columns in 1usize..6,
    ) {
        let dir = TempDir::new().unwrap();
        make_corpus(dir.path(), &(0..seed_words.len()).zip(seed_words).collect::<Vec<_>>());
        let (idx, _t) = BigramIndex::build(
            dir.path(),
            &BigramIndexConfig { max_columns, ..Default::default() },
            None,
        )
        .unwrap();
        let all: Vec<std::path::PathBuf> = idx.paths.clone();
        let truth = search_files(&needle, &all, &SearchOptions {
            fixed_strings: true,
            ..Default::default()
        })
        .unwrap();
        let mut ids = match idx.candidates(&needle) {
            rrg_core::Candidates::Superset(ids) => ids,
            _ => (0..all.len() as u32).collect(),
        };
        // Exercise the full CLI flow: bigram candidates + trigram refinement
        // (both tiers). Must remain a superset of true matches.
        idx.refine_trigrams(&needle, &mut ids);
        let files: Vec<_> = ids.iter().map(|&i| all[i as usize].clone()).collect();
        let got = search_files(&needle, &files, &SearchOptions {
            fixed_strings: true,
            ..Default::default()
        })
        .unwrap();
        prop_assert_eq!(truth.matches, got.matches, "false negatives under column pressure");
        prop_assert_eq!(truth.files_matched, got.files_matched);
    }
}
