//! `tools/list` JSON-RPC payload — the canonical descriptor for every
//! tool the MCP server exposes. The crate-level `#![recursion_limit =
//! "512"]` in `main.rs` is what lets the single `serde_json::json!([...])`
//! macro tree below expand to completion; do NOT split this into smaller
//! `json!(...)` literals without first verifying the macro recursion
//! depth.
//!
//! Extracted from `main.rs` in the v1.21 split — see
//! `.claude/Task/v1.21-vex-mcp-split.md`.

use serde_json::Value;

/// Tools that accept `--workspace` (multi-repo fan-out). Mirrors the CLI's
/// `extract_workspace_flag` set, minus `find_symbol` (a thin exact-name probe
/// — cross-repo existence is `check`, ranked is `search`) and `watch` (not an
/// MCP tool). The `workspace` param is injected into these tools' schemas by
/// [`inject_workspace_param`] so the definition lives in one place.
pub(crate) const WORKSPACE_TOOLS: &[&str] = &[
    "search",
    "grep",
    "check",
    "usages",
    "impact",
    "callers",
    "callees",
    "reachable",
    "index",
    "update",
];

/// Add a `workspace` boolean property to every [`WORKSPACE_TOOLS`] descriptor.
/// Done in post-processing (not inline in the json!) so the param's
/// description is defined once and the covered set stays a single list.
fn inject_workspace_param(tools: &mut Value) {
    let prop = serde_json::json!({
        "type": "boolean",
        "default": false,
        "description": "Multi-repo: fan out across every repo declared in the nearest `.vex-workspace.toml` (set `project_root` at or above it — the manifest is found by walking up). Results become an object `{workspace, repos:[...]}` grouped by repo, NOT the flat per-tool array — branch on shape. `why` is ignored in workspace mode (single-repo only)."
    });
    let Some(arr) = tools.as_array_mut() else {
        return;
    };
    for tool in arr {
        let is_covered = tool
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| WORKSPACE_TOOLS.contains(&n));
        if !is_covered {
            continue;
        }
        if let Some(props) = tool
            .pointer_mut("/inputSchema/properties")
            .and_then(Value::as_object_mut)
        {
            props.insert("workspace".to_string(), prop.clone());
        }
    }
}

pub(crate) fn tool_descriptors() -> Value {
    let mut tools = serde_json::json!([
        {
            "name": "search",
            "description": "Hybrid structural + semantic code search across the indexed codebase. Fuses FST exact + BM25 + semantic channels in a single ranked list (~4ms FST hit, ~7-15ms with semantic). Prefer over grep for symbol or identifier lookup — grep does a full-scan (seconds on large repos) and returns line matches; this returns ranked symbol records with kind, signature, and line ranges. Use this when you need to find a definition by name, signature shape, or meaning rather than guessing a regex. Supports `filter` (substring path filter), `kind` (kind-boost / restrict), `context_path` (proximity hint), `no_bm25` (disable BM25 channel), and `no_stale_check` (skip pre-call staleness probe).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Free-text query: symbol name, partial name, signature snippet, or natural-language description. Not for regex (use grep) or exact-only resolution (use find_symbol)." },
                    "limit": { "type": "integer", "description": "Max results", "default": 20 },
                    "semantic": { "type": "boolean", "description": "Enable the semantic vector channel (requires `vex index --semantic`); adds ~3-10ms but lets natural-language queries hit", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why` in the response: normalized query, per-channel hits (FST/BM25/semantic/fuzzy), filter_applied snapshot", "default": false },
                    "filter_path": { "type": "string", "description": "Substring path filter applied to result paths (single substring; use include/exclude for glob patterns). Legacy alias: `filter`." },
                    "kind": { "type": "array", "items": { "type": "string" }, "description": "Boost results matching one or more kinds (repeatable). Canonical names (function, struct, class, …) plus aliases: def, comment, test, ref." },
                    "context_path": { "type": "string", "description": "Boost results near this file path (e.g. the agent's current editor file)." },
                    "no_bm25": { "type": "boolean", "description": "Disable the BM25 channel for this query (auto-on when the index has BM25 data otherwise).", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true (which already refreshes).", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (e.g. 'tests/**'); repeat for multiple globs" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob (wins over include); repeat for multiple globs" },
                    "visibility": { "type": "string", "enum": ["public", "private", "protected", "internal"], "description": "Keep only symbols whose signature contains this explicit visibility keyword (no inferred defaults)" },
                    "async_only": { "type": "boolean", "description": "Keep only async/suspend functions", "default": false },
                    "no_async": { "type": "boolean", "description": "Exclude async/suspend functions", "default": false },
                    "static_only": { "type": "boolean", "description": "Keep only static class members", "default": false },
                    "sealed_only": { "type": "boolean", "description": "Keep only sealed (or Java-`final`) types", "default": false },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false },
                    "code_only": { "type": "boolean", "description": "(v1.20.0 D4) Drop results in prose-format files (`*.md`/`*.markdown`/`*.txt`/`*.rst`/`*.adoc`). Default off so 'README' still finds the README; pass for code-intent queries where CHANGELOG/README headings would pollute the top of the result list.", "default": false }
                },
                "required": ["query"]
            }
        },
        {
            "name": "find_symbol",
            "description": "Resolve a symbol by exact name (with prefix fallback) against the FST inverted index (~4ms). Prefer over search when the symbol name is known and you want exactly that record back, not a fused-rank list. Prefer over grep for `git grep 'class Foo'`-style definition lookup — grep scans every byte; this is a constant-time index probe.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact symbol name (function/class/struct/etc.) — canonical key (v1.7+). Use search for partial or fuzzy names." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "find_similar",
            "description": "Semantic-only search by natural-language description (e.g. 'payment processing' → ChargeUseCase, BillingService). Uses the HNSW vector index built by `vex index --semantic` (~7-15ms). Prefer over search when you do not know any concrete identifier and want concept-level matching; prefer search when you have a partial name (search fuses semantic + lexical channels for better recall on identifier-shaped queries).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language description of the concept (not an identifier; use find_symbol for those)." },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "outline",
            "description": "List every symbol (kind + line range) in a single source file via cached tree-sitter parse. Prefer over Read when you only need the file's structure (what's in here?) rather than the full byte stream — outline returns ~50 lines of structured records vs reading thousands of lines of source.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Filesystem path to the source file — canonical key (v1.7+). Absolute or relative to project_root." },
                    "file": { "type": "string", "description": "DEPRECATED — use `path`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "index",
            "description": "Build or rebuild the vex index from scratch. Run once per project; use `update` afterward for incremental refreshes. Set semantic=true to also generate embeddings (slower; required for find_similar / similar / duplicates).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": { "type": "string", "description": "Absolute path to the project root to index" },
                    "semantic": { "type": "boolean", "description": "Also generate per-symbol embeddings (enables semantic search / similar / duplicates; adds ~30-90s on a medium repo)", "default": false },
                    "gpu": { "type": "boolean", "description": "Use the GPU for embedding generation if this vex build supports it (DirectML on Windows / CoreML on macOS prebuilts; CUDA via source build), with silent CPU fallback. Only speeds up cold/large semantic builds. Omit to let .vex.toml gpu/device or $VEX_DEVICE decide; pass false to force CPU even when config enables GPU." },
                    "device": { "type": "string", "description": "Advanced: pin a specific embedding execution provider (cpu | auto | cuda | directml | coreml). Mutually exclusive with `gpu`." }
                },
                "required": ["project_root"]
            }
        },
        {
            "name": "update",
            "description": "Incremental index refresh: only re-parses files whose mtime changed since the last index. Prefer over `index` when an index already exists — typically <1s on small change sets vs full rebuild cost. Most other tools default to auto_update=true and call this implicitly.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": { "type": "string", "description": "Absolute path to the project root whose index should be refreshed" },
                    "semantic": { "type": "boolean", "description": "Also refresh embeddings for changed files", "default": false },
                    "gpu": { "type": "boolean", "description": "Use the GPU for embedding generation if this vex build supports it, with silent CPU fallback. Mostly a no-op for incremental updates (few/zero embeddings recomputed). Omit to let .vex.toml gpu/device or $VEX_DEVICE decide; pass false to force CPU." },
                    "device": { "type": "string", "description": "Advanced: pin a specific embedding execution provider (cpu | auto | cuda | directml | coreml). Mutually exclusive with `gpu`." }
                },
                "required": ["project_root"]
            }
        },
        {
            "name": "status",
            "description": "Report index statistics: symbol count, byte size, embedding presence, last-update timestamp. Use to confirm an index exists and is fresh before running search-shaped tools.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" }
                }
            }
        },
        {
            "name": "eval",
            "description": "Run the ranking-quality harness against a golden query set and return nDCG@10 / recall@10 / MRR per query and aggregated. Indexless in the sense that it never builds — consumes whatever index already lives at the project root (run `index` first if missing). Intended as a CI regression guard. MCP defaults to `json: true` so agents receive structured `EvalReport` JSON instead of the human-readable summary the CLI emits.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "bench": { "type": "string", "description": "Path to the golden-set TOML. Defaults to the bundled `benches/ranking_golden/queries.toml` on the CLI side; pass this when running against a fixture." },
                    "min_ndcg": { "type": "number", "description": "Fail with non-zero exit if mean nDCG@10 drops below this floor. Default 0.0 (always succeed). CI pins a recorded floor.", "default": 0.0 },
                    "json": { "type": "boolean", "description": "Emit the EvalReport as JSON to stdout. Default `true` in MCP context (agents want structured output) — note the CLI default is `false`. Set explicitly to `false` to fall back to the human-readable summary.", "default": true },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" }
                }
            }
        },
        {
            "name": "show",
            "description": "Extract the full source body of one or more symbols by name (function, class, struct, etc.) using cached symbol byte-offsets (~4ms per symbol). Prefer over Read when you need a specific definition — show returns just that body, while Read pulls the entire file (often 10-100x more tokens). Accepts an array, so a single call replaces several Read calls. Phase 13.3 truncation: `signature_only` (signature line only), `head` (first N body lines), `no_body` (signature + leading doc only), `collapsed` (collapse nested methods — v1.9 NO-OP). Also supports `filter` (substring path filter), `kind` (kind-restrict), `context_path` (proximity hint), and `no_stale_check`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Exact symbol names to extract — canonical key (v1.7+). Pass the array form even for a single symbol." },
                    "symbol": { "type": "string", "description": "DEPRECATED — use `symbols: [name]`. Pre-v1.7 singular alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max bodies returned per symbol name (handles overloads / duplicates)", "default": 1 },
                    "filter_path": { "type": "string", "description": "Substring path filter applied to result paths (single substring; use include/exclude for glob patterns). Legacy alias: `filter`." },
                    "kind": { "type": "array", "items": { "type": "string" }, "description": "Boost results matching one or more kinds (repeatable). Same vocabulary as `search.kind`." },
                    "context_path": { "type": "string", "description": "Boost results near this file path (e.g. the agent's current editor file)." },
                    "signature_only": { "type": "boolean", "description": "Phase 13.3: print only the signature line(s). Mutually exclusive with `head`, `no_body`, `collapsed`.", "default": false },
                    "head": { "type": "integer", "minimum": 1, "description": "Phase 13.3: print only the first N body lines and append `... (M more lines)`. Mutually exclusive with `signature_only`, `no_body`, `collapsed`." },
                    "no_body": { "type": "boolean", "description": "Phase 13.3: print signature + leading docstring only; drop the body. Mutually exclusive with `signature_only`, `head`, `collapsed`.", "default": false },
                    "collapsed": { "type": "boolean", "description": "Phase 13.3: collapse nested methods inside a class/impl/module. v1.9 NO-OP (flag-shape stable; emits a stderr warning). Mutually exclusive with `signature_only`, `head`, `no_body`.", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["symbols"]
            }
        },
        {
            "name": "usages",
            "description": "Find every reference to a symbol across the codebase. Prefer over grep for refactor-style `find all callers` queries — grep on a common identifier returns string-literal and comment noise; usages with strict=true uses the scope-binder to resolve real cross-file refs (Rust/TypeScript/Python/C#/C++). Without strict, runs the legacy refs FST (~4ms) — v1.20.0 also strips the row at the symbol's own definition line and prose mentions in `*.md`/`*.markdown`/`*.txt`/`*.rst`/`*.adoc` (override with `include_self` / `include_docs`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact symbol name to find references to — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "strict": { "type": "boolean", "description": "Use scope-resolved (type-aware) references from the binder — drops string-literal/comment/wrong-scope noise. Recommended for refactor work; falls back to legacy refs FST on languages without binder support.", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why`: mode (strict/fst_lookup), mode_legacy (back-compat alias for v1.9.x consumers, removed in v1.12), hits before/after path filter, prefix-suggestion count when no exact hits, def_site_dropped / docs_dropped counts (v1.20.0), filter snapshot.", "default": false },
                    "include_self": { "type": "boolean", "description": "(non-strict only) Keep the row at the symbol's own definition line. v1.20.0+ strips it by default — `find all callers` queries don't want the declaration showing up as a usage. No-op when `strict=true` (the scope-binder excludes the def-site by construction).", "default": false },
                    "include_docs": { "type": "boolean", "description": "(non-strict only) Keep matches in `*.md` / `*.markdown` / `*.txt` / `*.rst` / `*.adoc` files. v1.20.0+ strips them by default — README/CHANGELOG mentions of a symbol are prose, not callers. No-op when `strict=true`.", "default": false },
                    "filter_path": { "type": "string", "description": "Substring path filter applied to result paths (single substring; use include/exclude for glob patterns). Legacy alias: `filter`." },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "impact",
            "description": "Delete-safety blast-radius report. Composes four independent reference channels — strict refs (binder-resolved v5 edges), the legacy FST refs, `grep \\b<Name>\\b` against the project, and direct call-graph callers — into a single verdict (`safe` / `unsafe` / `uncertain`). Use this BEFORE proposing to delete or rename a symbol; one call collapses what CLAUDE.md previously documented as a manual dance across usages → grep → callers. Verdict rule: `unsafe` if strict_refs > 0 OR call_graph_callers > 0 (binder/graph confirmed real usage); `uncertain` if only text channels (FST / grep) hit (likely string-dispatch / decorator / comment mentions); `safe` only when every channel reports zero hits. `results` shape: { symbol, verdict, verdict_explanation, channels: { strict_refs, fst_refs, grep_word_boundary, call_graph_callers } } where each channel block has { available, count, sample[], truncated }.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact symbol name to assess — canonical key." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable). Applied to every channel — useful for scoping to e.g. `src/**` when assessing a library symbol." },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)." },
                    "exclude_docs": { "type": "boolean", "description": "(v1.20.1, D4 parity) Opt-in: drop text-channel hits in prose-format files (`*.md`/`*.markdown`/`*.txt`/`*.rst`/`*.adoc`). Default off so a symbol mentioned only in CHANGELOG still yields `uncertain`; pass when you want a code-only blast radius (binder channels are unaffected).", "default": false },
                    "depth": { "type": "integer", "description": "(v1.21.0) BFS hop budget for transitive callers. `1` (default) reports direct callers only via `call_graph_callers`; `>= 2` enables the `transitive_callers` channel, walking the call graph backward up to N hops. Silently clamped to `[1, 16]`. Use to see the full upstream blast radius (`outer -> middle -> leaf` chain surfaces `outer` at depth=2).", "minimum": 1, "maximum": 16 }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "tests_for",
            "description": "Find test functions that transitively cover a target symbol (Phase 13.10). Walks the call graph backwards from `<target>`, keeps rows under recognized test-path globs (Rust / Python / TS-JS / Go / Java / Kotlin / C# / C++), stamps each row with a `framework` label (`pytest`, `jest`, `go-test`, …) so an agent can pick the right runner without parsing paths. Prefer over grep `test.*Foo` — that misses transitively-covered helpers and produces lots of false positives. v1.20.0 (D5) surface — the CLI subcommand exists since v1.19.0 but was MCP-invisible.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Symbol whose test coverage to find — the function/method/class you want to know is tested." },
                    "symbol": { "type": "string", "description": "DEPRECATED alias for `target`; still accepted, emits a deprecated_args notice in _meta." },
                    "max_hops": { "type": "integer", "description": "Maximum reverse-call-graph hops from `target`. Default 6.", "default": 6 },
                    "limit": { "type": "integer", "description": "Max results to return.", "default": 200 },
                    "test_pattern": { "type": "array", "items": { "type": "string" }, "description": "Glob patterns for test paths (repeatable). When set, REPLACES the default pattern set (does NOT append) — pass the full set you want." },
                    "include_fixtures": { "type": "boolean", "description": "Admit non-test-named helpers (fixtures) under test paths via a one-hop forward callee walk. Default off — only `test_*` / `*Test` names surface.", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["target"]
            }
        },
        {
            "name": "history",
            "description": "Every historical version of a symbol reachable from a chosen tip. With `vex index --history` previously run, queries hit a persistent FST sidecar (~ms); without it, shells out to `git log` (~seconds). Indexed mode also finds symbols whose name has been DELETED from HEAD — the walker can't. Use this to inspect how a function's body / signature changed over time, find when a bug was introduced, or recover a deleted symbol's last definition. NOTE: omitting `limit` returns the full history (walker mode is unbounded by default — set `limit` to cap latency on long-lived repos). `exact_presence: true` adds seconds-scale latency per file — only pass when you specifically need the exact commit set, not the convex-hull span. v1.20.0 (D5) surface — the CLI subcommand has existed since v1.15.0 but was MCP-invisible.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Symbol name to walk through history. Matched whole-word via `git grep --word-regexp`, then filtered post-parse to exact `name == query`." },
                    "name": { "type": "string", "description": "DEPRECATED alias for `symbol`; still accepted, emits a deprecated_args notice in _meta." },
                    "depth": { "type": "integer", "description": "Max commits to walk per file (walker mode). Unbounded by default; bump down on long-lived repos to keep latency in check." },
                    "branch": { "type": "string", "description": "Restrict the walk to this revision (`refs/heads/foo`, `origin/main`, a SHA). Defaults to `HEAD`." },
                    "limit": { "type": "integer", "description": "Cap the total result set. Omit for unbounded (walker mode) — set explicitly on long-lived repos to keep latency in check. The walker stops as soon as the limit is reached." },
                    "no_index": { "type": "boolean", "description": "Force the v1.16 query-time walker even when a `git_history` section is present. Default (`HistoryMode::Auto`) picks the indexed path when available and falls back to the walker otherwise. Use for regression-checking the walker against the indexed path.", "default": false },
                    "since": { "type": "string", "description": "Keep only entries whose commit date is `>= YYYY-MM-DD` (inclusive)." },
                    "until": { "type": "string", "description": "Keep only entries whose commit date is `<= YYYY-MM-DD` (inclusive)." },
                    "author": { "type": "string", "description": "Keep only entries whose commit author contains this substring (case-insensitive). Walker-only — the indexed path rejects this with an error pointing at `no_index: true`." },
                    "kind": { "type": "string", "description": "Keep only entries whose symbol kind matches exactly (lowercase: `function` / `struct` / `impl` / …)." },
                    "diff": { "type": "boolean", "description": "Render unified diffs between consecutive historical versions of the same `(symbol, kind)` pair instead of repeating the full body for each entry. Cuts output noise on deep histories. Mutually exclusive with `exact_presence`.", "default": false },
                    "exact_presence": { "type": "boolean", "description": "For each entry, list the exact set of commits where its blob lived in the file. Defeats the convex-hull span representation (LIMITATIONS §4c #4). Adds latency.", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "grep",
            "description": "Regex content search across files (ripgrep-equivalent, no index needed). Use this for searching inside string literals, comments, config values, or any non-symbol text. Prefer search / find_symbol / usages for identifier lookups — those are index-backed (~4ms) while grep is a full-scan and returns raw line matches without symbol context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern (Rust regex syntax) to match against file contents." },
                    "filter_path": { "type": "string", "description": "Substring path filter applied to result paths (single substring; use include/exclude for glob patterns). Legacy alias: `filter`." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "text": { "type": "boolean", "description": "Force-read every file, bypassing the binary-file skip (extension denylist + NUL/high-control content sniff). Escape hatch for a legitimately-textual file that got misclassified as binary; a genuinely invalid-UTF-8 file is still skipped. CLI equivalent: `-a`/`--text` (ripgrep parity).", "default": false }
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "implementations",
            "description": "Find every concrete type that extends a base class / implements a trait / interface. Walks the indexed inheritance edges (covers generic-parameterised bases). Prefer over grep for `find all subclasses of Foo` — grep misses `: Foo<T>`, indirect inheritance, and trait impls; this resolves the real hierarchy. Supports diff scoping: `since` / `since_branched` / `changed_only` (mutually exclusive) to restrict to recently-touched code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact name of the base class / trait / interface — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "subtypes",
            "description": "Find every TRANSITIVE subtype of a base class / interface — the full descendant tree via extends/implements edges (not just direct implementations; use `implementations` for direct-only). Requires a v8+ index with hierarchy edges (no live-walk fallback) — if the index predates this feature or has no hierarchy section, this returns an empty result with a hint to re-run `vex index`. Supports diff scoping: `since` / `since_branched` / `changed_only` (mutually exclusive) to restrict to recently-touched code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact name of the base class / trait / interface — canonical key." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "depth": { "type": "integer", "description": "Max BFS hops (transitive descent) from the queried type. Bounds how many inheritance levels deep the search goes; independent of the mandatory cycle-detection guard. Must be in `[1, 4096]` (64 is a generous default real hierarchies never approach).", "default": 64, "minimum": 1, "maximum": 4096 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "callers",
            "description": "Direct callers of a function via the persistent call-graph FST (~4ms when indexed; falls back to live-scan). Prefer over grep for `who calls Foo?` — grep on the function name hits doc comments and string literals; the call-graph edges are resolved at parse time. Phase 14.2 + 14.2.2 + 14.2.1: Python/Java function/method decorators, Kotlin annotations / C# method+constructor attributes, and TypeScript method decorators / Rust outer attributes on fns/methods emit forward edges, so `callers GetMapping` lists every Spring handler, `callers get` lists every FastAPI route, `callers HttpGet` every ASP.NET action, `callers JvmStatic` every Kotlin function annotated `@JvmStatic`, `callers Get` every Nest.js `@Get(...)`, `callers test` every Rust `#[tokio::test]` (the rightmost identifier of the decorator/attribute path becomes the callee; arguments are ignored — `#[serde(rename = \"x\")]` → `serde`, not `rename`). Rust `#[derive(...)]` is filtered (compile-time codegen, not call edges). Note the rightmost-identifier convention means `callers get` mixes decorator handlers with any regular `.get()` call — narrow with `include`/`exclude` if needed. Pair with `paths` for multi-hop chains. Supports diff scoping: `since` / `since_branched` / `changed_only` (mutually exclusive) to restrict callers to recently-touched code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact function name — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running — enables the call-graph fast path (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "callees",
            "description": "Direct callees of a function via the persistent call-graph FST (~4ms when indexed; falls back to live-scan). Prefer over Read+manual scanning when you want to know what a function calls without reading the whole body — callees gives the resolved outgoing edges as records. Phase 14.2 + 14.2.2 + 14.2.1: Python/Java decorators, Kotlin annotations, C# method/constructor attributes, TypeScript method decorators, and Rust outer attributes on fns/methods are surfaced as callees of the decorated function (decorator factories like `@lru_cache(maxsize=128)`, `@Inject`, `@Get(\"/x\")`, or `#[tokio::test]` appear as the path-rightmost identifier `lru_cache` / `Inject` / `Get` / `test` alongside regular body calls). Rust `#[derive(...)]` is intentionally filtered. Supports diff scoping: `since` / `since_branched` / `changed_only` (mutually exclusive) to restrict callees to recently-touched code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact function name — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running — enables the call-graph fast path (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "pattern",
            "description": "Structural AST pattern matching: match code by shape, not text. Metavars: `$NAME` captures an identifier or balanced expression, `$_` is a wildcard, `$$$` is an anonymous ellipsis, `$$$NAME` / `$$NAME` is a named ellipsis that captures multi-line bodies or arg lists, repeated metavars enforce back-reference equality. Composition: space-flanked ` && ` and ` || ` join sub-patterns (AND requires both shapes in the file with shared captures agreeing; OR takes the union). Prefer over grep / ast-grep for cross-language structural queries — grep cannot match nested syntax, and ast-grep needs per-language scripts; vex pattern works on the cached tree-sitter parse with a skeleton prefilter (~10-50ms). Set `why: true` to inspect indexed vs live-scan mode. Supports diff scoping: `since` (rev), `since_branched` (since this branch diverged from main), `changed_only` (working-tree changes) — mutually exclusive.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Structural pattern with $METAVARS (e.g. `fn $NAME($$ARGS) -> Result<$T, $E> { $$$BODY }`, `interface $N || class $N`). NOT regex — see grep for regex." },
                    "lang": { "type": "string", "description": "Language: rust, python, typescript, go, java, csharp, ruby, kotlin, swift, cpp, php, sql, markdown" },
                    "limit": { "type": "integer", "description": "Max matches to return", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD` (accepts anything `git diff` understands: `main`, `HEAD~3`, `origin/main`, SHA). Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false },
                    "why": { "type": "boolean", "description": "Surface a ScanTrace under `_meta.why` in the response: mode (indexed/live_scan), root_kind_inferred, candidate_files / total_files, fallback_reason." }
                },
                "required": ["pattern", "lang"]
            }
        },
        {
            "name": "diff",
            "description": "Symbol-level diff between a git revision and the working tree: lists added / removed / moved / body-changed symbols on the touched files. Prefer over `git diff` + manual scanning for PR review — git diff returns line hunks while this returns structured symbol records, so an agent can iterate over changed-functions directly instead of parsing unified-diff text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "base": { "type": "string", "description": "Git revision to compare against (e.g. main, HEAD~3, origin/main). Working tree is the new side." },
                    "limit": { "type": "integer", "description": "Max changes to return", "default": 500 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist changes by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist changes by path glob; wins over include (repeatable)" }
                },
                "required": ["base"]
            }
        },
        {
            "name": "paths",
            "description": "Enumerate every caller chain from `from` to `to` in the persistent call graph (multi-hop, max 6 by default). Prefer over repeated `callers` calls when you need to know how a function gets reached from a known entry point — paths walks the edges itself in a single response. Requires a v4 index with call graph (built without `--no-call-graph`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Exact name of the starting function (caller / entry point)." },
                    "to": { "type": "string", "description": "Exact name of the destination function (callee being investigated)." },
                    "max_hops": { "type": "integer", "description": "Maximum hops between from and to", "default": 6 },
                    "max_paths": { "type": "integer", "description": "Maximum paths to enumerate (caps output, aborts traversal early)", "default": 50 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist intermediate steps by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist intermediate steps by path glob; wins over include (repeatable)" }
                },
                "required": ["from", "to"]
            }
        },
        {
            "name": "reachable",
            "description": "Every symbol that transitively calls `target` (the full upstream blast radius). Prefer over repeated `callers` walks when assessing the impact of changing a function — reachable does the closure in one call. Requires a v4 index with call graph.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Exact symbol name whose callers (direct + transitive) you want." },
                    "max_hops": { "type": "integer", "description": "Maximum hops to walk back from target", "default": 6 },
                    "limit": { "type": "integer", "description": "Max results", "default": 200 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["target"]
            }
        },
        {
            "name": "check",
            "description": "Batch existence probe: confirm whether one or more symbol names exist in the index without paying for body extraction or ranked search (~4ms total). Use before show / usages / callers when working from an unverified list — skip the symbols that don't exist instead of letting downstream tools error.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Exact symbol names to probe — canonical key (v1.7+)." },
                    "names": { "type": "array", "items": { "type": "string" }, "description": "DEPRECATED — use `symbols`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false }
                },
                "required": ["symbols"]
            }
        },
        {
            "name": "similar",
            "description": "Nearest neighbours of an EXISTING symbol by its stored embedding (HNSW lookup, ~7-15ms). Distinct from find_similar (which embeds a free-text query). Use this when you have a function in hand and want `what else in this repo looks like it?` — useful for dedup, refactor planning, and finding parallel implementations. Requires `vex index --semantic`. Supports diff scoping: `since` (rev), `since_branched`, `changed_only` (mutually exclusive) and `no_stale_check`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Exact name of an existing indexed symbol to use as the seed — canonical key (v1.7+)." },
                    "name": { "type": "string", "description": "DEPRECATED — use `symbol`. Pre-v1.7 alias, still accepted; emits a deprecated_args notice in _meta." },
                    "limit": { "type": "integer", "description": "Max results", "default": 10 },
                    "threshold": { "type": "number", "description": "Minimum cosine similarity (0.0..1.0); raise to tighten matches", "default": 0.5 },
                    "filter_path": { "type": "string", "description": "Substring path filter applied to result paths (single substring; use include/exclude for glob patterns). Legacy alias: `filter`." },
                    "explain": { "type": "boolean", "description": "Include reasoning per match: identifier-set Jaccard overlap + truncated unified diff between bodies", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why`: seed resolution, applied threshold, candidates before/after path filter, filter snapshot.", "default": false },
                    "since": { "type": "string", "description": "Restrict results to files changed between `<rev>..HEAD`. Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict results to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict results to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob, gitignore syntax (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "capabilities",
            "description": "Return vex protocol version + capability matrix for client capability negotiation.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "bundle",
            "description": "Multi-source bundle — replaces 4 round-trips (show → callers → callees → similar) with 1. Three modes: `symbol` (body + callers + callees + similar for a named symbol; ~10ms), `pr-impact` (changed symbols + transitive callers + tests for a git base ref; ~50ms), `project` (top-N symbols by reverse call-graph indegree; ~5ms). Prefer over chaining find_symbol/show/callers/callees when you need cross-section context on one symbol or a PR. Mode-specific args are validated server-side; only `mode` is universally required. Response shape is uniform — `{ protocol_version, capabilities, _meta, results: { mode, items[], mode_hints } }`. Each `items[i]` carries 13.11 signals plus a `role` discriminator (`body | caller | callee | similar | changed | transitive_caller | test | top`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["symbol", "pr-impact", "project"], "description": "Bundle assembly mode" },
                    "symbol": { "type": "string", "description": "(mode: symbol) Symbol name to resolve via the symbol FST" },
                    "base": { "type": "string", "description": "(mode: pr-impact) Git base revision to diff against (e.g. `origin/main`, `HEAD~3`, a SHA)" },
                    "depth": { "type": "integer", "description": "(mode: pr-impact) Transitive callers walk depth", "default": 2 },
                    "path_glob": { "type": "string", "description": "(mode: project) Single path glob filter applied to ranked symbols (e.g. `src/**`); separate from the universal `include`/`exclude` arrays" },
                    "top_n": { "type": "integer", "description": "(mode: project) Max number of top-ranked symbols", "default": 30 },
                    "callers_max": { "type": "integer", "description": "(mode: symbol) Max direct callers", "default": 10 },
                    "callees_max": { "type": "integer", "description": "(mode: symbol) Max direct callees", "default": 10 },
                    "similar_max": { "type": "integer", "description": "(mode: symbol) Max semantic-similar matches; gated on `vex index --semantic`", "default": 5 },
                    "tests_max": { "type": "integer", "description": "(mode: pr-impact) Max test-classified items", "default": 20 },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist results by path glob (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist results by path glob; wins over include (repeatable)" }
                },
                "required": ["mode"]
            }
        },
        {
            "name": "duplicates",
            "description": "Repo-wide near-duplicate scan: pairs of symbols whose embeddings exceed `threshold`. Use for refactor planning (`where else does this logic live?`) and dedup. Prefer over manual similar-walks — duplicates evaluates all pairs once with `min_body_lines` filtering out trivial bodies. Requires `vex index --semantic`. Supports diff scoping: `since` (rev), `since_branched`, `changed_only` (mutually exclusive) and `no_stale_check`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "threshold": { "type": "number", "description": "Minimum cosine similarity (0.0..1.0); 0.9 keeps only very close pairs", "default": 0.9 },
                    "limit": { "type": "integer", "description": "Max pairs to return", "default": 50 },
                    "min_body_lines": { "type": "integer", "description": "Skip symbols with body shorter than this many lines (filters trivial wrappers)", "default": 5 },
                    "filter_path": { "type": "string", "description": "Substring path filter — keep pairs where at least one symbol's path contains this substring. Legacy alias: `filter`." },
                    "explain": { "type": "boolean", "description": "Include reasoning per pair: identifier-set Jaccard overlap + truncated unified diff between the two bodies", "default": false },
                    "why": { "type": "boolean", "description": "Surface a JSON trace under `_meta.why`: applied threshold + min_body_lines, pairs before/after path filter, filter snapshot.", "default": false },
                    "since": { "type": "string", "description": "Restrict pairs to files changed between `<rev>..HEAD`. Mutually exclusive with `since_branched` and `changed_only`." },
                    "since_branched": { "type": "boolean", "description": "Restrict pairs to files changed since this branch diverged from `origin/main` (or `main`/`master`). Mutually exclusive with `since` and `changed_only`.", "default": false },
                    "changed_only": { "type": "boolean", "description": "Restrict pairs to working-tree changes (staged + unstaged + untracked). Mutually exclusive with `since` and `since_branched`.", "default": false },
                    "project_root": { "type": "string", "description": "Absolute path to the project root (defaults to the MCP working directory)" },
                    "auto_update": { "type": "boolean", "description": "Auto-update the index if stale, or bootstrap it if missing, before running (default: true)", "default": true },
                    "no_stale_check": { "type": "boolean", "description": "Skip the staleness check that runs before each call; assumes the index is fresh. Redundant when `auto_update` is true.", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Whitelist pairs by path glob — a pair is kept when at least one side matches (repeatable)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Blacklist pairs by path glob — a pair is dropped when either side matches (repeatable)" }
                }
            }
        }
    ]);
    inject_workspace_param(&mut tools);
    tools
}
