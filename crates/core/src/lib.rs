//! rpg-core — the search engine core for ripripgrep.
//!
//! Architecture roadmap (see README.md):
//! - M0 (this module): cold-scan parity baseline built on ripgrep's own
//!   engine crates (`grep-regex`, `grep-searcher`, `ignore`). We embed their
//!   best-in-class matcher/walker, then replace layers from the bottom up.
//! - M1: async/overlapped I/O pipeline (io_uring on Linux, overlapped on Win)
//!   pipelining walk -> read -> SIMD prefilter -> regex.
//! - M2: adaptive n-gram index + lock-free daemon for instant warm queries.

pub mod command;
pub mod index;
pub mod query;
pub mod search;
pub mod serve;

pub use index::{
    BigramIndex, BigramIndexConfig, Candidates, Plan, TriPlan, plan_for_literal, plan_for_regex,
    tri_plan_for_literal, tri_plan_for_regex,
};
pub use query::{QueryAnswer, QueryOptions, run_query};
pub use search::{SearchOptions, SearchStats, count_occurrences_mmap, search_files, search_path};
