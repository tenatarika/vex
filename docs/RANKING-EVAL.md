# Ranking Evaluation Harness

Phase 13.12 ships a small CI regression guard on vex's ranking quality.
This is **not** a research benchmark — the bar is "catch silent ranking
degradations before they reach users".

## Quick start

```bash
# Run the harness against the current index in the cwd.
vex eval

# Fail with non-zero exit if mean nDCG@10 drops below a threshold.
vex eval --min-ndcg 0.85

# Emit JSON for tooling.
vex eval --json

# Use a custom golden set (e.g. one tuned to a downstream repo).
vex eval --bench path/to/queries.toml
```

The harness consumes whatever index already exists at the project root.
It never builds one — running `vex eval` against a missing index is an
error, not a silent rebuild. Pair with `vex index` (or `vex update`) in
CI before invoking eval.

## Metrics

The harness computes three classical IR metrics per query and aggregates
them across the set:

* **nDCG@10** — Normalized Discounted Cumulative Gain at the top-10.
  Primary signal; tolerant of multiple acceptable answers; rank-aware
  via log-decay; normalized into `[0.0, 1.0]`. Reference: Järvelin &
  Kekäläinen 2002.
* **recall@10** — fraction of relevant items recovered in the top-10.
  Coarser than nDCG but useful for "did we even find the right file?"
  checks.
* **MRR** — Mean Reciprocal Rank. The "top-1 must be right" signal:
  `1 / rank_of_first_relevant_result`, or `0.0` if none.

Binary relevance: a result is either in the query's `acceptable_paths`
set or it isn't. We dedupe relevant-path tags so multiple top-N matches
to the same canonical file count once — the textbook semantics.

## Golden set schema

The bundled golden set lives at `benches/ranking_golden/queries.toml`.
Each entry:

```toml
[[queries]]
query = "SearchResult"                # raw input to `vex search`
query_type = "exact_symbol"           # exact_symbol | semantic | bm25_rare | fuzzy
expected_top_path = "src/search/mod.rs"  # OPTIONAL strict top-1 constraint
acceptable_paths = ["src/search/mod.rs"] # required: at least one
```

* `acceptable_paths` is matched via substring against the result's
  `path` field. Bias toward shorter prefixes so the assertion stays
  stable under file moves within a directory.
* `expected_top_path` is the strict top-1 constraint. When set, the
  harness records `top1_hit` per query — useful for spotting
  regressions where the ranking *almost* works but slips to rank 2.
* Empty `acceptable_paths` is **rejected at parse time** — every
  result would count as relevant, masking regressions.

## Adding queries

Add a query when you want to **guard** specific ranking behaviour, not
just to inflate the score. Good additions:

* A query type that's under-represented (more BM25-rare cases, edge
  fuzzy cases).
* A query that surfaced a real ranking bug in production — keep it as
  a regression fixture even after the fix.
* A query exercising a recently-shipped reranker signal so a future
  refactor can't silently neutralize it.

Avoid:

* Trivial queries (e.g. matching a unique 30-char identifier) — they
  always score 1.0 and waste evaluation budget.
* Queries with subjective "best" answers — pick something with a
  defensible top result.

After adding queries, rerun `vex eval` and update `BASELINE_NDCG` in
`tests/ranking_regression_test.rs` if the new floor moves.

## Baseline philosophy

The regression guard test (`tests/ranking_regression_test.rs`) pins a
floor on mean nDCG@10. The floor is captured against the current vex
source tree with a fresh index:

```text
v1.8.2 (Phase 13.12 commit) — mean nDCG@10 ≈ 0.89
BASELINE_NDCG = 0.85    # ~5% headroom for run-to-run variance
```

The headroom absorbs tie-breaking jitter in fusion's Reciprocal Rank
Fusion and the deterministic-but-implementation-defined sort in
structural search.

**When intentionally improving ranking**: rerun `vex eval`, observe the
new score, raise `BASELINE_NDCG`. Document in `CHANGELOG.md`.

**When the test fails unexpectedly**:

1. Reproduce locally — `vex eval` against a fresh index.
2. Bisect the offending commit if needed.
3. Either fix the regression, or — if the ranking change is intentional
   and the new score is worse but acceptable — lower `BASELINE_NDCG`
   with a CHANGELOG note explaining the trade-off.

NEVER lower `BASELINE_NDCG` just to silence the test.

## Semantic queries without embeddings

The bundled golden set includes "semantic" phrase queries
(`"manifest staleness"`, `"ref reader"`, …). Without `--semantic`
enabled at index time, these rely on BM25 + body-token matching, which
is weaker than full embedding-based semantic search. The recorded
baseline reflects this regime — running with `--semantic` should push
those scores higher.

Phase 13.12 intentionally records the no-semantic baseline because:

* CI cost: downloading the ONNX model on every CI run is wasteful.
* Cross-machine reproducibility: HNSW + embeddings introduce floating
  point variance the regression guard doesn't need.
* Phase 13.5 (`--max-tokens` budgeted output) will operate over the
  fused channels; the eval harness measures the same fused output.

A future phase can add a `vex eval --semantic` flag if/when the value
exceeds the CI cost.

## See also

* `src/eval/mod.rs` — metric implementations + unit tests.
* `src/eval/harness.rs` — golden-set loader + query driver.
* `tests/ranking_regression_test.rs` — the CI assertion.
* `.claude/Task/ROADMAP-improvements.md` — Phase 13.12 entry +
  rationale.
