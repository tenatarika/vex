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

## Canonical field vocabulary

| Field | Type | Meaning | Used by |
| --- | --- | --- | --- |
| `query` | string | Free-text search query — symbol name, partial name, or natural language description | `search`, `find_similar` |
| `symbol` | string | Exact symbol name (function/class/struct/etc.) for resolution-style commands | `find_symbol`, `usages`, `implementations`, `callers`, `callees`, `similar` |
| `symbols` | string[] | Multiple symbol names (existence-check / batch lookup) | `show`, `check` |
| `path` | string | Filesystem path to a single source file | `outline` |
| `pattern` | string | Regex pattern matched against file contents | `grep` |
| `filter` | string | Substring path filter applied to result paths | `grep`, `similar`, `duplicates` |
| `include` | string[] | Path-glob whitelist (gitignore syntax, repeatable) | every search-shaped tool |
| `exclude` | string[] | Path-glob blacklist, wins over `include` | every search-shaped tool |

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

## `--why` / `why: true` — search trace

When a search returns surprising results, set `why: true` (MCP) or pass
`--why` (CLI) to the `search` tool. The CLI prints a JSON trace to
**stderr** (so `vex search Foo --why | jq` still works on stdout); the
MCP wrapper inherits the same stderr emission via the spawned `vex`
process — and a future MCP iteration may surface the trace in `_meta`
too.

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

## Stability guarantees

- The canonical vocabulary above is part of the v1.7 stable API and
  will not be renamed.
- Legacy aliases are best-effort: they keep working through the v1.x
  series, and we will not remove them in a patch release.
- New optional fields may be added at any minor release. New required
  fields are a major-version change.
