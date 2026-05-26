# MCP schema vocabulary (vex 1.7+)

The vex MCP server exposes ~16 tools to LLMs and IDE-style clients via
the Model Context Protocol. v1.8 added `strict` to `usages` (binder-
resolved refs, see [README → Type-aware refs](../README.md#type-aware-refs)).
Before v1.7 the argument naming had drifted
— `name`, `symbol`, `query`, `file`, `pattern`, `filter` were all in
play for what should have been a small canonical vocabulary. v1.7
standardises the field names while accepting the pre-v1.7 aliases for
back-compat. Clients using a legacy alias get a deprecation notice via
the JSON-RPC `_meta.deprecated_args` field on every call.

## Parameter vocabulary

The MCP tool descriptions are deliberately tuned for LLM tool-selection
(Phase 13.1, snapshot-locked in
`crates/vex-mcp/src/snapshots/vex_mcp__tests__tool_descriptors.snap`).
Every parameter description uses one of the canonical role names below,
so an agent learning the schema once can reuse the same mental model
across every tool.

| Field | Type | Meaning | Used by |
| --- | --- | --- | --- |
| `query` | string | **Free-text** — symbol name, partial name, signature snippet, or natural-language description. Not regex; not for exact resolution. | `search`, `find_similar` |
| `symbol` | string | **Exact symbol name** (function/class/struct/etc.) — canonical resolution key (v1.7+). | `find_symbol`, `usages`, `implementations`, `callers`, `callees`, `similar`, `bundle` |
| `symbols` | string[] | Array of exact symbol names — batch lookup / existence probe. | `show`, `check` |
| `path` | string | Filesystem path to a single source file (absolute or relative to `project_root`). | `outline` |
| `pattern` | string | Regex pattern (`grep`) *or* structural AST pattern with `$METAVARS` (`pattern`). Tool docstring states which. | `grep`, `pattern` |
| `filter` | string | Substring path filter applied to result paths (single substring; use `include`/`exclude` for globs). | `grep`, `similar`, `duplicates` |
| `include` | string[] | Path-glob whitelist (gitignore syntax, repeatable). | every search-shaped tool |
| `exclude` | string[] | Path-glob blacklist, wins over `include` (repeatable). | every search-shaped tool |
| `mode` | enum | **Bundle assembly mode** — `symbol` / `pr-impact` / `project`. Discriminator for per-mode required fields (see [Bundle modes](#bundle-modes-v19)). | `bundle` |
| `base` | string | Git base revision to diff against (e.g. `origin/main`, `HEAD~3`, a SHA). | `bundle` (mode: `pr-impact`) |
| `depth` | integer | Transitive callers walk depth (default 2). | `bundle` (mode: `pr-impact`) |
| `path_glob` | string | Single path glob applied as a post-rank filter (separate from the universal `include`/`exclude` arrays). | `bundle` (mode: `project`) |
| `top_n` | integer | Max top-ranked symbols to return (default 30). | `bundle` (mode: `project`) |

**Convention** (enforced by the 13.1 snapshot test): tool `description`
fields lead with action verb + indexed-vs-scan tradeoff + latency hint
+ "prefer over X" framing where there's a non-vex alternative (grep,
Read, git diff, ast-grep). Per-parameter `description` fields point
back at the right tool when the agent picked the wrong input shape
(e.g. `query` reminds the agent "use grep for regex").

Other fields (`limit`, `threshold`, `semantic`, `auto_update`, `explain`,
`min_body_lines`, `project_root`, `strict`) are role-specific and were
already consistent across tools.

### `strict` (v1.8+)

The `usages` tool accepts `"strict": true` to opt into the v5 scope-
binder-resolved reference edges. Default `false` keeps the legacy
behaviour. When set on an index built before v1.8 (no
`reference_edges` section), the call fails with a "re-run `vex index`"
error rather than silently returning incomplete results.

## Bundle modes (v1.9+)

The `bundle` tool replaces the 4-round-trip agent loop (`show → callers
→ callees → similar`) with one call. The MCP schema is **flat**:
`mode` is the only required field; mode-specific args are validated
server-side and surface as JSON-RPC errors when missing. (Architect
decision A4 — zero `oneOf` discriminated unions, mirrors the existing
flat-schema convention used by every other vex tool.)

### Required + optional fields per mode

| Mode | Required | Optional (mode-specific) |
| --- | --- | --- |
| `symbol` | `symbol` | `callers_max` (10), `callees_max` (10), `similar_max` (5) |
| `pr-impact` | `base` | `depth` (2), `tests_max` (20) |
| `project` | — | `top_n` (30), `path_glob` |

Universal optional fields (every mode): `project_root`, `auto_update`,
`include`, `exclude`.

### Response shape

```jsonc
{
  "protocol_version": "v1",
  "capabilities": { "signals": true, "bundle_modes": [...], ... },
  "_meta": {
    "vex.dev/index_age_ms": 1200,
    // pr-impact only:
    "vex.dev/diff_filter": {
      "scope": "pr-impact:HEAD",
      "changed_paths": ["src/lib.rs"],
      "retained": 4,
      "dropped": 0
    }
  },
  "results": {
    "mode": "symbol" | "pr-impact" | "project",
    "items": [ /* see role enum below */ ],
    "mode_hints": { /* per-mode keys, see below */ }
  }
}
```

The MCP wrapper lifts `protocol_version` and `capabilities` to the
JSON-RPC `result` top level and exposes `results` under
`structuredContent.results`. Signals live in `structuredContent`
(visible to the LLM); `_meta` is invisible to the LLM per the MCP
spec — use it for observability (`index_age_ms`, `traceparent`,
`diff_filter`).

### `items[i].role` enum

Each item carries a `role: &'static str` discriminator naming which
sub-list of the bundle it came from:

| Role | Emitted by mode | Meaning |
| --- | --- | --- |
| `body` | `symbol` | The resolved seed symbol — carries `body: string` with the full source body. |
| `caller` | `symbol` | A direct caller of the seed. |
| `callee` | `symbol` | A direct callee of the seed. |
| `similar` | `symbol` | A semantic-similar match — carries `similarity: f32` (cosine). |
| `changed` | `pr-impact` | A symbol whose `(name, body)` differs between `base` and the working tree. |
| `transitive_caller` | `pr-impact` | A non-test symbol that reaches a `changed` symbol within `depth` hops over the call graph. |
| `test` | `pr-impact` | A test symbol that reaches a `changed` symbol. Heuristic: path contains `/tests/` / `/test/` / `_test.` / `.test.` / `/spec/` / `/__tests__/`, OR signature starts with `#[test]` / `#[tokio::test...]` / `#[cfg(test)]`. |
| `top` | `project` | A top-N symbol by reverse call-graph indegree. The indegree count is exposed under `signals.indegree` (Phase 13.2 additive field; absent on every other code path). |

`rank_percentile` is **global** monotonic-descending across the full
`items` array (preserves the v1.9 search-envelope invariant).
`role_rank` is per-role 0-indexed for callers that want within-bucket
ordering after sorting the bundle by `rank_percentile`.

### `mode_hints` per-mode shape

```jsonc
// mode: symbol
{
  "callers_count": 2, "callees_count": 2, "similar_count": 0,
  "callers_truncated": false, "callees_truncated": false, "similar_truncated": false,
  "has_call_graph": true, "has_vectors": false,
  "empty_reason": null | "symbol_not_found"
}

// mode: pr-impact
{
  "base": "HEAD", "depth": 2,
  "changed_count": 1, "transitive_caller_count": 2, "test_count": 1,
  "tests_truncated": false, "unreachable_changes": [],
  "empty_reason": null | "no_changes"
}

// mode: project
{
  "scoring": "reverse_indegree", "top_n": 30, "path_glob": null,
  "total_ranked_symbols": 12, "has_call_graph": true,
  "empty_reason": null | "no_call_graph" | "no_call_edges" | "path_glob_filtered_all"
}
```

### Mode-specific guarantees

- **`symbol`** soft-degrades when the index has no vectors — `similar`
  block is empty, `has_vectors: false`. NOT an error. Unknown symbol
  → exit 0 + `empty_reason: "symbol_not_found"`.
- **`pr-impact`** is the only mode that **hard-errors** when the index
  was built `--no-call-graph` — the BFS layer requires persistent
  caller edges. Empty diff → exit 0 + `empty_reason: "no_changes"`.
- **`project`** soft-degrades on `--no-call-graph` (empty items +
  `empty_reason: "no_call_graph"`). Indegree scoring is **experimental
  — structural lower bound**, NOT PageRank (architect decision A5;
  PageRank revival blocked on the 13.12 ranking-eval harness gaining a
  project-importance ground truth).

## Pre-v1.7 aliases (still accepted)

| Tool | Legacy field | Canonical field |
| --- | --- | --- |
| `find_symbol` | `name` | `symbol` |
| `usages` | `name` | `symbol` |
| `implementations` | `name` | `symbol` |
| `callers` | `name` | `symbol` |
| `callees` | `name` | `symbol` |
| `similar` | `name` | `symbol` |
| `outline` | `file` | `path` |
| `check` | `names` | `symbols` |
| `show` | `symbol` (singular) | `symbols: [name]` |

Sending a legacy field still works. The MCP response surfaces a
`_meta.deprecated_args` array listing every legacy name the client
used:

```json
{
  "content": [{ "type": "text", "text": "[...]" }],
  "_meta": {
    "deprecated_args": ["name"]
  }
}
```

`_meta` is the MCP-reserved metadata bucket — clients that don't read
it see the same `content` array as before. Aliases will be **removed
in a future major release**; migrate when convenient.

> **Spec note.** The MCP specification documents `_meta` at the
> *request* level for context passing. Its use in tool-call *responses*
> is a vex extension; strict-validating clients can safely ignore the
> field. If MCP later formalises `_meta` in responses we can migrate
> without a breaking change because the key is already self-describing.

## `--why` / `why: true` — diagnostic traces

When a query returns surprising results, set `why: true` (MCP) or pass
`--why` (CLI). The CLI prints a one-line JSON trace to **stderr** (so
`vex search Foo --why | jq` still works on stdout); the MCP wrapper
picks up the stderr line and surfaces it under `_meta.why` on the
JSON-RPC response.

Five tools currently support `--why`, each with a domain-specific
trace shape:

| Tool | Trace shape (high-level) |
| --- | --- |
| `search` | `normalized_query`, per-channel hits (FST/BM25/semantic), fallbacks (e.g. `["fuzzy"]`), filter snapshot |
| `pattern` | `mode` (indexed / live_scan), `root_kind_inferred`, `candidate_files` / `total_files`, `fallback_reason` |
| `usages` | `mode` (`strict` / `text_scan`), `hits_before_filter`, `hits_after_filter`, `prefix_suggestions` (`"Did you mean"` count when no exact hits), `filter_applied` |
| `similar` | `seed_resolved`, `threshold_applied`, `candidates_before_filter`, `candidates_after_filter`, `filter_applied` |
| `duplicates` | `threshold_applied`, `min_body_lines_applied`, `pairs_before_filter`, `pairs_after_filter`, `filter_applied` |

Each trace is built post-hoc from values the handler already has in
scope — the fast path pays nothing when `--why` is off.

### `search` trace

Shape:

```json
{
  "normalized_query": "paymentprocessor",
  "channels": [
    { "name": "fst",      "hits": 17 },
    { "name": "bm25",     "hits":  4 },
    { "name": "semantic", "hits":  0 }
  ],
  "fallbacks": ["fuzzy"],
  "filter_applied": {
    "filter": "src/billing/",
    "include": ["src/**"],
    "exclude": ["**/*.gen.rs"],
    "kind": ["fn"]
  }
}
```

Use it to answer:

- *"Why didn't BM25 fire?"* — the channel's `hits: 0` indicates the
  index has no BM25 data (rebuild with `vex index`) or the query was
  too short for the BM25 tokeniser.
- *"Did fuzzy fallback engage?"* — `fallbacks: ["fuzzy"]` means no
  exact-FST hit and the result list is best-effort.
- *"Was a filter narrowing my results?"* — the `filter_applied` block
  shows the active include/exclude/kind state, so leftover shell-history
  flags are easy to catch.

The trace is built post-hoc from the un-truncated channel result lists
the Search handler already has in scope. Turning the flag on adds a few
allocations for the channel clones; it is safe to leave on for
interactive use but off by default in automated pipelines.

### `usages` trace

```json
{
  "mode": "fst_lookup",
  "mode_legacy": "text_scan",
  "hits_before_filter": 17,
  "hits_after_filter": 4,
  "prefix_suggestions": null,
  "filter_applied": {
    "filter": "src/",
    "include": ["src/**"],
    "exclude": []
  }
}
```

- `mode` — `"strict"` when the v5 `reference_edges` section was
  queried, `"fst_lookup"` for the refs FST (Phase 14.4 rename;
  emitted as `"text_scan"` in v1.8 – v1.9).
- `mode_legacy` — back-compat alias mirroring `mode`, except it
  keeps emitting the pre-14.4 label (`"text_scan"`) when `mode ==
  "fst_lookup"`. Slated for removal in v1.12 — read `mode` if you
  can; both fields point at the same data path.
- `hits_before_filter` vs `hits_after_filter` — pin "no refs anywhere"
  vs "refs dropped by the path filter".
- `prefix_suggestions` — `n` when zero exact hits and the
  `Did you mean` prefix-fallback engaged with `n` candidates. `null`
  when there were exact hits OR `--strict` is in use (the strict path
  has no prefix-fallback today).

### `similar` trace

```json
{
  "seed_resolved": true,
  "threshold_applied": 0.5,
  "candidates_before_filter": 12,
  "candidates_after_filter": 4,
  "filter_applied": { "include": ["src/billing/**"], "exclude": [] }
}
```

- `seed_resolved=false` is the load-bearing signal that the seed
  symbol didn't match any indexed name — distinct from "threshold
  filtered everything".
- `candidates_before_filter` is the HNSW return list after the
  threshold; `candidates_after_filter` is what remains after path
  filters + `--limit`.

### `duplicates` trace

```json
{
  "threshold_applied": 0.9,
  "min_body_lines_applied": 5,
  "pairs_before_filter": 17,
  "pairs_after_filter": 8,
  "filter_applied": { "filter": "tests/", "include": [], "exclude": [] }
}
```

Lets a caller spot "the threshold ate the result set" vs "the path
filter narrowed too aggressively" without re-running.

## Stability guarantees

- The canonical vocabulary above is part of the v1.7 stable API and
  will not be renamed.
- Legacy aliases are best-effort: they keep working through the v1.x
  series, and we will not remove them in a patch release.
- New optional fields may be added at any minor release. New required
  fields are a major-version change.
