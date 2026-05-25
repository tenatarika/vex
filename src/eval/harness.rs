//! Golden-set loading + query execution for `vex eval` (Phase 13.12).
//!
//! Splits cleanly from `src/eval/mod.rs`:
//!   * `mod.rs` holds the textbook IR metrics — no I/O, no vex
//!     internals, trivially unit-testable.
//!   * `harness.rs` (this file) wires those metrics to the real
//!     `IndexReader` + structural/BM25 channels and prints summaries.
//!
//! The harness reads the index that's already at `index_path` — it
//! never builds one. Callers (`vex eval` CLI handler, regression test)
//! own bootstrap and update concerns.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{mrr, ndcg_at_k, recall_at_k};
use crate::search::{fusion, structural, SearchResult};
use crate::store::reader::IndexReader;

/// Top-k window the harness evaluates against. Hard-coded so the
/// baseline numbers stored in the regression test are comparable across
/// runs — no `--k` flag.
pub const EVAL_K: usize = 10;

/// One row of the golden TOML file.
#[derive(Debug, Clone, Deserialize)]
pub struct GoldenQuery {
    pub query: String,
    pub query_type: String,
    /// Strict top-1 constraint (optional). When present, the harness
    /// records MRR against this single path.
    #[serde(default)]
    pub expected_top_path: Option<String>,
    /// Any one of these paths counts as a hit in the relevance set.
    /// At least one entry is required.
    pub acceptable_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct GoldenSet {
    pub queries: Vec<GoldenQuery>,
}

impl GoldenSet {
    /// Parse a golden set from a TOML file on disk.
    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read golden set {}", path.display()))?;
        Self::parse_str(&raw).with_context(|| format!("parse golden set {}", path.display()))
    }

    /// Parse a golden set from a TOML string. Validates that every
    /// query carries at least one `acceptable_paths` entry — an empty
    /// list would make every result "relevant" and silently mask
    /// regressions.
    ///
    /// Named `parse_str` (not `from_str`) so it doesn't shadow the
    /// `std::str::FromStr` convention — clippy's `should_implement_trait`
    /// would otherwise require implementing the full trait, which is
    /// overkill for an internal helper.
    pub fn parse_str(raw: &str) -> Result<Self> {
        let set: GoldenSet = toml::from_str(raw).context("parse TOML")?;
        for (idx, q) in set.queries.iter().enumerate() {
            if q.acceptable_paths.is_empty() {
                anyhow::bail!(
                    "query[{idx}] {:?} has empty acceptable_paths — would treat \
                     any result as relevant and mask regressions",
                    q.query
                );
            }
        }
        Ok(set)
    }
}

/// Per-query metrics row recorded after running the query.
#[derive(Debug, Clone, Serialize)]
pub struct QueryReport {
    pub query: String,
    pub query_type: String,
    pub ndcg_at_k: f64,
    pub recall_at_k: f64,
    pub mrr: f64,
    /// Whether the strict top-1 expectation (if any) was met. `None`
    /// for queries that didn't declare `expected_top_path`.
    pub top1_hit: Option<bool>,
    /// Top result's path, if the query returned anything. Useful when
    /// inspecting regressions in `--json` output.
    pub top_path: Option<String>,
    pub result_count: usize,
}

/// Aggregate scores plus the per-query rows.
#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub k: usize,
    pub total_queries: usize,
    pub mean_ndcg: f64,
    pub mean_recall: f64,
    pub mean_mrr: f64,
    /// Breakdown grouped by `query_type` (exact_symbol / semantic /
    /// bm25_rare / fuzzy). Empty buckets are omitted.
    pub by_type: Vec<TypeBucket>,
    pub per_query: Vec<QueryReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TypeBucket {
    pub query_type: String,
    pub count: usize,
    pub mean_ndcg: f64,
    pub mean_recall: f64,
    pub mean_mrr: f64,
}

/// Run the harness against `reader`, returning the full report.
///
/// `EVAL_K * 5` is fetched per query so the search engine has room to
/// produce candidates before the rerank/filter pipeline narrows them.
/// We then truncate to `EVAL_K` for the metric computation — the same
/// window every metric uses, so cross-query comparisons stay honest.
pub fn run(reader: &IndexReader, set: &GoldenSet) -> Result<EvalReport> {
    let fetch_limit = EVAL_K * 5;
    let per_query: Vec<QueryReport> = set
        .queries
        .iter()
        .map(|q| run_query(reader, q, fetch_limit))
        .collect();
    Ok(aggregate(per_query))
}

fn run_query(reader: &IndexReader, q: &GoldenQuery, fetch_limit: usize) -> QueryReport {
    let results = search_for_eval(reader, &q.query, fetch_limit);
    // Normalize result paths for matching. Index paths are stored as
    // forward-slash relative paths; golden-set paths use the same
    // convention. We compare via `path_matches`, which accepts exact,
    // trailing-`/file`, or directory-prefix forms — but rejects
    // substring matches like `src/search` against `src/search_old.rs`
    // (H2 regression — substring matching silently inflated nDCG).
    let ranked_paths: Vec<String> = results.iter().map(|r| r.path.clone()).collect();
    let relevant: HashSet<String> = q.acceptable_paths.iter().cloned().collect();

    // Convert ranked paths into "tags" that match the relevance set.
    // Each ranked path is replaced by the first acceptable_path it
    // contains (or a unique sentinel if none).
    //
    // Critically, each acceptable_path tag may only contribute ONCE
    // — re-seeing the same canonical file at a later rank does not
    // re-trigger nDCG / recall, because binary relevance treats each
    // relevant item as a set element, not a stream. Without this
    // dedup nDCG can exceed 1.0 (a single relevant file matching 5
    // top-10 positions inflates the DCG numerator while IDCG only
    // sums one item) — exactly the bug observed during baseline
    // capture.
    let mut seen_tags: HashSet<String> = HashSet::new();
    let tagged: Vec<String> = ranked_paths
        .iter()
        .enumerate()
        .map(|(idx, p)| match match_tag(p, &q.acceptable_paths) {
            Some(tag) if seen_tags.insert(tag.clone()) => tag,
            // Either no acceptable_path matched OR we've already counted
            // this tag — emit a unique sentinel either way so neither
            // case contributes to the relevance set.
            _ => format!("__miss_{idx}"),
        })
        .collect();

    let ndcg = ndcg_at_k(&tagged, &relevant, EVAL_K);
    let recall = recall_at_k(&tagged, &relevant, EVAL_K);
    let mrr_v = mrr(&tagged, &relevant);

    let top_path = ranked_paths.first().cloned();
    let top1_hit = q.expected_top_path.as_ref().map(|expected| {
        top_path
            .as_deref()
            .map(|p| path_matches(p, expected.as_str()))
            .unwrap_or(false)
    });

    QueryReport {
        query: q.query.clone(),
        query_type: q.query_type.clone(),
        ndcg_at_k: ndcg,
        recall_at_k: recall,
        mrr: mrr_v,
        top1_hit,
        top_path,
        result_count: ranked_paths.len(),
    }
}

fn match_tag(path: &str, acceptable: &[String]) -> Option<String> {
    acceptable
        .iter()
        .find(|a| path_matches(path, a.as_str()))
        .cloned()
}

/// Path-prefix matching for golden-set relevance. The earlier
/// `path.contains(acceptable)` form was H2: `src/search` would match
/// `src/search_old.rs` and silently inflate nDCG. We accept three
/// canonical shapes:
///
///   1. Exact match — `actual == acceptable`.
///   2. Trailing match — `actual` ends with `/{acceptable}` (so the
///      golden can say `mod.rs` and we still match `src/foo/mod.rs`).
///   3. Directory-prefix match — `actual` starts with `{acceptable}/`
///      (so a golden of `src/search` matches `src/search/mod.rs` but
///      NOT `src/search_old.rs`).
fn path_matches(actual: &str, acceptable: &str) -> bool {
    actual == acceptable
        || actual.ends_with(&format!("/{acceptable}"))
        || actual.starts_with(&format!("{acceptable}/"))
}

/// Drive the same fused-channel ranking that `vex search` uses, minus
/// semantic (the regression test runs without embeddings to keep CI
/// fast). When BM25 isn't built, fall back to structural-only — the
/// harness still measures what's reachable.
fn search_for_eval(reader: &IndexReader, query: &str, limit: usize) -> Vec<SearchResult> {
    let structural_results = structural::search_with_fuzzy(reader, query, limit);
    let bm25_results = if reader.has_bm25() {
        crate::search::bm25::search(reader, query, limit)
    } else {
        Vec::new()
    };

    let fused = if bm25_results.is_empty() {
        structural_results
    } else {
        fusion::fuse_many(vec![structural_results, bm25_results], limit)
    };

    // Apply the same default rerank pass `vex search` uses — empty
    // KindSelector and no context_path mean it scores on shape signals
    // only. Drops Markdown headings, boosts test-path matches for test
    // queries, etc.
    let rerank_ctx = crate::search::rerank::RerankContext {
        kind_hints: Vec::new(),
        context_path: None,
    };
    crate::search::rerank::rerank(query, &rerank_ctx, fused)
}

fn aggregate(per_query: Vec<QueryReport>) -> EvalReport {
    let total = per_query.len();
    let (sum_ndcg, sum_recall, sum_mrr) = per_query
        .iter()
        .fold((0.0_f64, 0.0_f64, 0.0_f64), |(n, r, m), q| {
            (n + q.ndcg_at_k, r + q.recall_at_k, m + q.mrr)
        });
    let denom = total.max(1) as f64;
    let by_type = bucket_by_type(&per_query);
    EvalReport {
        k: EVAL_K,
        total_queries: total,
        mean_ndcg: sum_ndcg / denom,
        mean_recall: sum_recall / denom,
        mean_mrr: sum_mrr / denom,
        by_type,
        per_query,
    }
}

fn bucket_by_type(per_query: &[QueryReport]) -> Vec<TypeBucket> {
    // Stable order: discover types in insertion order, then aggregate.
    let mut order: Vec<String> = Vec::new();
    for q in per_query {
        if !order.iter().any(|t| t == &q.query_type) {
            order.push(q.query_type.clone());
        }
    }
    order
        .into_iter()
        .map(|t| {
            let rows: Vec<&QueryReport> = per_query.iter().filter(|q| q.query_type == t).collect();
            let count = rows.len();
            let denom = count.max(1) as f64;
            let (n, r, m) = rows.iter().fold((0.0, 0.0, 0.0), |(n, r, m), q| {
                (n + q.ndcg_at_k, r + q.recall_at_k, m + q.mrr)
            });
            TypeBucket {
                query_type: t,
                count,
                mean_ndcg: n / denom,
                mean_recall: r / denom,
                mean_mrr: m / denom,
            }
        })
        .collect()
}

/// Pretty-print the report to stdout. Stays terse — three section
/// headers, one row per type, one row per query. JSON consumers go
/// through `serde_json::to_string_pretty(&report)`.
pub fn print_text_report(report: &EvalReport) {
    println!(
        "Ranking eval — {} queries, k={}",
        report.total_queries, report.k
    );
    println!("---");
    println!("Overall:");
    println!("  mean nDCG@{}    {:>6.3}", report.k, report.mean_ndcg);
    println!("  mean recall@{} {:>6.3}", report.k, report.mean_recall);
    println!("  mean MRR       {:>6.3}", report.mean_mrr);

    if !report.by_type.is_empty() {
        println!();
        println!("By query type:");
        for b in &report.by_type {
            println!(
                "  {:<14} n={:<3} nDCG={:.3}  recall={:.3}  MRR={:.3}",
                b.query_type, b.count, b.mean_ndcg, b.mean_recall, b.mean_mrr
            );
        }
    }

    println!();
    println!("Per query:");
    for q in &report.per_query {
        let top1 = match q.top1_hit {
            Some(true) => "top1=Y",
            Some(false) => "top1=N",
            None => "top1=-",
        };
        println!(
            "  [{:<12}] {:<28} nDCG={:.3} recall={:.3} MRR={:.3} {}",
            q.query_type, q.query, q.ndcg_at_k, q.recall_at_k, q.mrr, top1
        );
        if q.result_count == 0 {
            println!("       (no results)");
        } else if let Some(tp) = &q.top_path {
            println!("       top: {tp}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[queries]]
query = "Foo"
query_type = "exact_symbol"
expected_top_path = "src/foo.rs"
acceptable_paths = ["src/foo.rs"]

[[queries]]
query = "bar baz"
query_type = "semantic"
acceptable_paths = ["src/bar.rs", "src/baz.rs"]
"#;

    #[test]
    fn parses_well_formed_set() {
        let set = GoldenSet::parse_str(SAMPLE).unwrap();
        assert_eq!(set.queries.len(), 2);
        assert_eq!(set.queries[0].query, "Foo");
        assert_eq!(
            set.queries[0].expected_top_path.as_deref(),
            Some("src/foo.rs")
        );
        assert_eq!(set.queries[1].acceptable_paths.len(), 2);
    }

    #[test]
    fn rejects_empty_acceptable_paths() {
        let raw = r#"
[[queries]]
query = "Bad"
query_type = "exact_symbol"
acceptable_paths = []
"#;
        let err = GoldenSet::parse_str(raw).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("empty acceptable_paths"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn match_tag_returns_first_match() {
        let acceptable = vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()];
        assert_eq!(
            match_tag("src/foo.rs", &acceptable),
            Some("src/foo.rs".to_string())
        );
        assert_eq!(match_tag("src/other.rs", &acceptable), None);
    }

    #[test]
    fn match_tag_does_not_match_neighboring_directory() {
        // H2 regression: substring matching inflated nDCG by counting
        // `src/search_old.rs` as a hit when acceptable was `src/search`.
        // Path-prefix semantics must reject that.
        let acceptable = vec!["src/search".to_string()];
        assert_eq!(
            match_tag("src/search_old.rs", &acceptable),
            None,
            "neighboring directory must not match"
        );
        assert_eq!(
            match_tag("src/search/mod.rs", &acceptable),
            Some("src/search".to_string()),
            "directory prefix must match"
        );

        // Exact file path should match itself.
        let acceptable = vec!["src/search/mod.rs".to_string()];
        assert_eq!(
            match_tag("src/search/mod.rs", &acceptable),
            Some("src/search/mod.rs".to_string()),
            "exact match must hit"
        );

        // Trailing-suffix form: `mod.rs` accepted against `foo/mod.rs`.
        let acceptable = vec!["mod.rs".to_string()];
        assert_eq!(
            match_tag("src/foo/mod.rs", &acceptable),
            Some("mod.rs".to_string()),
            "trailing /file suffix must match"
        );

        // Substring-but-not-prefix-or-suffix must NOT match.
        let acceptable = vec!["foo".to_string()];
        assert_eq!(
            match_tag("src/foobar.rs", &acceptable),
            None,
            "substring without separator must not match"
        );
    }

    #[test]
    fn aggregate_computes_means() {
        let per_query = vec![
            QueryReport {
                query: "a".into(),
                query_type: "exact_symbol".into(),
                ndcg_at_k: 1.0,
                recall_at_k: 1.0,
                mrr: 1.0,
                top1_hit: Some(true),
                top_path: Some("x".into()),
                result_count: 1,
            },
            QueryReport {
                query: "b".into(),
                query_type: "exact_symbol".into(),
                ndcg_at_k: 0.0,
                recall_at_k: 0.0,
                mrr: 0.0,
                top1_hit: Some(false),
                top_path: None,
                result_count: 0,
            },
        ];
        let report = aggregate(per_query);
        assert!((report.mean_ndcg - 0.5).abs() < 1e-9);
        assert!((report.mean_recall - 0.5).abs() < 1e-9);
        assert!((report.mean_mrr - 0.5).abs() < 1e-9);
        assert_eq!(report.by_type.len(), 1);
        assert_eq!(report.by_type[0].count, 2);
    }
}
