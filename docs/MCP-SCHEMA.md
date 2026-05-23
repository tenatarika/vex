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
| `symbol` | string | **Exact symbol name** (function/class/struct/etc.) — canonical resolution key (v1.7+). | `find_symbol`, `usages`, `implementations`, `callers`, `callees`, `similar` |
| `symbols` | string[] | Array of exact symbol names — batch lookup / existence probe. | `show`, `check` |
| `path` | string | Filesystem path to a single source file (absolute or relative to `project_root`). | `outline` |
| `pattern` | string | Regex pattern (`grep`) *or* structural AST pattern with `$METAVARS` (`pattern`). Tool docstring states which. | `grep`, `pattern` |
| `filter` | string | Substring path filter applied to result paths (single substring; use `include`/`exclude` for globs). | `grep`, `similar`, `duplicates` |
| `include` | string[] | Path-glob whitelist (gitignore syntax, repeatable). | every search-shaped tool |
| `exclude` | string[] | Path-glob blacklist, wins over `include` (repeatable). | every search-shaped tool |

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
  "mode": "text_scan",
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
  queried, `"text_scan"` for the legacy refs FST.
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
