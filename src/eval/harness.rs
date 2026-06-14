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
use crate::search::{fusion, structural, MatchType, SearchResult};
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
    /// `MatchType` of each top-EVAL_K result, in rank order. Position 0
    /// is the rank-1 result, etc. Empty when the query returned nothing.
    /// Phase 13.12.1: lets reviewers see which channels actually pulled
    /// the top results (e.g. all `Hybrid` vs all `Structural`).
    ///
    /// Stored as `MatchType` (not `String`) so adding a new variant
    /// surfaces as a compile error in `bucket_by_channel`'s aggregation
    /// path rather than as a silent new string in the JSON output.
    /// Serializes as the bare variant name via `MatchType`'s `serde`
    /// derive, so consumers see the same shape as before.
    pub top_match_types: Vec<MatchType>,
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
    /// Breakdown grouped by `MatchType` (which channel produced the
    /// result). Phase 13.12.1: answers "is the semantic channel actually
    /// contributing anything, or is fusion riding on structural/bm25?".
    pub by_channel: Vec<ChannelBucket>,
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

/// Aggregate contribution of one search channel (variant of `MatchType`).
///
/// `top1_count` is the strongest signal — "how often did this channel
/// produce the rank-1 result". `top_k_count` is the looser version
/// counting any appearance within the EVAL_K window. Both are simple
/// counts so a future delta against a baseline run reads naturally.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelBucket {
    pub channel: String,
    pub top1_count: usize,
    pub top_k_count: usize,
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

    // Capture channels in the same EVAL_K window we score against. The
    // metric truncation already lives inside ndcg_at_k / recall_at_k /
    // mrr — mirror it here so aggregation by_channel only counts what
    // the metrics actually saw.
    let top_match_types: Vec<MatchType> = results
        .iter()
        .take(EVAL_K)
        .map(|r| r.match_type.clone())
        .collect();

    QueryReport {
        query: q.query.clone(),
        query_type: q.query_type.clone(),
        ndcg_at_k: ndcg,
        recall_at_k: recall,
        mrr: mrr_v,
        top1_hit,
        top_path,
        result_count: ranked_paths.len(),
        top_match_types,
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
///
/// **Cross-platform:** `IndexReader` returns paths with the host
/// separator — `\` on Windows, `/` elsewhere. Golden-set entries always
/// use `/`. Normalize `actual` at the comparison boundary so the same
/// golden set scores identically on every platform; without this, every
/// query scored nDCG/recall/MRR = 0 on Windows because no shape matched.
fn path_matches(actual: &str, acceptable: &str) -> bool {
    let actual = actual.replace('\\', "/");
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
    let by_channel = bucket_by_channel(&per_query);
    EvalReport {
        k: EVAL_K,
        total_queries: total,
        mean_ndcg: sum_ndcg / denom,
        mean_recall: sum_recall / denom,
        mean_mrr: sum_mrr / denom,
        by_type,
        by_channel,
        per_query,
    }
}

fn bucket_by_type(per_query: &[QueryReport]) -> Vec<TypeBucket> {
    use std::collections::HashMap;

    // Stable order: discover types in first-seen order, dedupe via the
    // HashMap so the seen-check is O(1) per result instead of O(|order|).
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    for q in per_query {
        if seen.insert(q.query_type.clone(), ()).is_none() {
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

/// Aggregate per-channel attribution across all queries.
///
/// Walks every `top_match_types` row. The rank-0 entry feeds `top1_count`
/// (the channel that produced the top result); every entry within the
/// EVAL_K window feeds `top_k_count`. Channels are emitted in
/// first-seen order so JSON output stays stable across runs.
fn bucket_by_channel(per_query: &[QueryReport]) -> Vec<ChannelBucket> {
    use std::collections::HashMap;

    let mut order: Vec<MatchType> = Vec::new();
    let mut top1: HashMap<MatchType, usize> = HashMap::new();
    let mut topk: HashMap<MatchType, usize> = HashMap::new();
    for q in per_query {
        for (rank, ch) in q.top_match_types.iter().enumerate() {
            // Use topk's key set as the "have we seen this channel"
            // check — every channel that lands in `order` is unconditionally
            // inserted into topk in the same iteration, so the HashMap
            // doubles as the seen-set without a parallel structure.
            if !topk.contains_key(ch) {
                order.push(ch.clone());
            }
            *topk.entry(ch.clone()).or_insert(0) += 1;
            if rank == 0 {
                *top1.entry(ch.clone()).or_insert(0) += 1;
            }
        }
    }
    order
        .into_iter()
        .map(|ch| {
            // topk is guaranteed to contain `ch` (we pushed `ch` to
            // `order` precisely when we inserted it into topk above).
            // top1 may legitimately be missing — a channel can appear
            // at rank ≥1 across all queries and never at rank 0.
            let top_k = *topk
                .get(&ch)
                .expect("channel in order implies channel in topk");
            let top_1 = top1.get(&ch).copied().unwrap_or(0);
            ChannelBucket {
                channel: ch.to_string(),
                top1_count: top_1,
                top_k_count: top_k,
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

    if !report.by_channel.is_empty() {
        println!();
        println!("By channel:");
        for b in &report.by_channel {
            println!(
                "  {:<12} top1={:<3} top{}={}",
                b.channel, b.top1_count, report.k, b.top_k_count
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
    fn path_matches_normalizes_windows_separator() {
        // IndexReader returns host-separator paths on Windows
        // (`src\store\reader.rs`). Golden TOML always uses forward
        // slashes. Without normalization, every comparison failed and
        // every query reported nDCG/recall/MRR = 0 on Windows CI.
        assert!(
            path_matches("src\\store\\reader.rs", "src/store/reader.rs"),
            "exact-match shape must work across separators"
        );
        assert!(
            path_matches("src\\store\\reader.rs", "store/reader.rs"),
            "trailing-`/file` shape must work across separators"
        );
        assert!(
            path_matches("src\\store\\reader.rs", "src/store"),
            "directory-prefix shape must work across separators"
        );
        // Neighbour-directory rule still holds after normalization.
        assert!(
            !path_matches("src\\search_old.rs", "src/search"),
            "post-normalize H2 protection must still reject neighbours"
        );
    }

    fn make_report(
        name: &str,
        qtype: &str,
        ndcg: f64,
        top_match_types: Vec<MatchType>,
    ) -> QueryReport {
        QueryReport {
            query: name.into(),
            query_type: qtype.into(),
            ndcg_at_k: ndcg,
            recall_at_k: ndcg,
            mrr: ndcg,
            top1_hit: Some(ndcg > 0.5),
            top_path: top_match_types.first().map(|_| format!("{name}.rs")),
            result_count: top_match_types.len(),
            top_match_types,
        }
    }

    #[test]
    fn aggregate_computes_means() {
        let per_query = vec![
            make_report("a", "exact_symbol", 1.0, vec![MatchType::Structural]),
            make_report("b", "exact_symbol", 0.0, vec![]),
        ];
        let report = aggregate(per_query);
        assert!((report.mean_ndcg - 0.5).abs() < 1e-9);
        assert!((report.mean_recall - 0.5).abs() < 1e-9);
        assert!((report.mean_mrr - 0.5).abs() < 1e-9);
        assert_eq!(report.by_type.len(), 1);
        assert_eq!(report.by_type[0].count, 2);
    }

    #[test]
    fn bucket_by_channel_counts_top1_and_topk() {
        // Three queries: first two have Hybrid as rank-1, third has
        // Structural. `Bm25` only appears at rank 2 (never top-1).
        let per_query = vec![
            make_report(
                "q1",
                "exact_symbol",
                1.0,
                vec![MatchType::Hybrid, MatchType::Bm25, MatchType::Structural],
            ),
            make_report(
                "q2",
                "semantic",
                0.5,
                vec![MatchType::Hybrid, MatchType::Bm25],
            ),
            make_report("q3", "fuzzy", 1.0, vec![MatchType::Structural]),
        ];
        let buckets = bucket_by_channel(&per_query);

        // First-seen order: Hybrid (q1 rank 0), Bm25 (q1 rank 1), Structural (q1 rank 2).
        let names: Vec<&str> = buckets.iter().map(|b| b.channel.as_str()).collect();
        assert_eq!(names, vec!["Hybrid", "Bm25", "Structural"]);

        let by_name: std::collections::HashMap<&str, &ChannelBucket> =
            buckets.iter().map(|b| (b.channel.as_str(), b)).collect();

        assert_eq!(
            by_name["Hybrid"].top1_count, 2,
            "Hybrid wins top-1 in q1+q2"
        );
        assert_eq!(by_name["Hybrid"].top_k_count, 2);

        assert_eq!(by_name["Bm25"].top1_count, 0, "Bm25 never at rank 0");
        assert_eq!(by_name["Bm25"].top_k_count, 2, "Bm25 at rank 1 in q1+q2");

        assert_eq!(
            by_name["Structural"].top1_count, 1,
            "Structural wins top-1 in q3 only"
        );
        assert_eq!(by_name["Structural"].top_k_count, 2);
    }

    #[test]
    fn bucket_by_channel_empty_when_no_results() {
        let per_query = vec![make_report("q", "exact_symbol", 0.0, vec![])];
        let buckets = bucket_by_channel(&per_query);
        assert!(buckets.is_empty(), "no channels seen → empty bucket list");
    }

    #[test]
    fn bucket_by_channel_all_same_channel() {
        // Realistic case: fusion is healthy and every query's top-k
        // window is dominated by Hybrid. top1_count must equal the
        // query count, top_k_count must equal the sum of window sizes.
        let per_query = vec![
            make_report(
                "q1",
                "exact_symbol",
                1.0,
                vec![MatchType::Hybrid, MatchType::Hybrid, MatchType::Hybrid],
            ),
            make_report(
                "q2",
                "exact_symbol",
                1.0,
                vec![MatchType::Hybrid, MatchType::Hybrid],
            ),
        ];
        let buckets = bucket_by_channel(&per_query);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].channel, "Hybrid");
        assert_eq!(buckets[0].top1_count, 2);
        assert_eq!(buckets[0].top_k_count, 5);
    }
}
