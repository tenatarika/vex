# vex Cookbook — Agent Workflow Recipes

End-to-end recipes for chaining vex's MCP tools to solve common code-navigation tasks. Each recipe shows the **tool sequence** an agent should run, **why this ordering**, and **how to phrase the request** so an agent picks the right chain without prompting nudges.

All snippets use the MCP tool surface (`usages(...)`, `bundle(mode=..., ...)`) and the canonical v1.7+ vocabulary (`symbol` / `query` / `path` / `pattern` / `filter` / `include` / `exclude`); the CLI equivalents (`vex usages ... --strict`, `vex bundle ...`) are 1:1 with the same args. See [`docs/MCP-SCHEMA.md`](MCP-SCHEMA.md) for the canonical schema.

## Tool-selection cheat sheet

| You want to…                                 | Reach for                                          | Not                                |
| -------------------------------------------- | -------------------------------------------------- | ---------------------------------- |
| Locate a specific named symbol (exact name)  | `find_symbol(symbol="X")` or `check(symbols=["X"])` | `search` (ranked — surfaces neighbors when no local def), `grep`, `Read` |
| Fuzzy / partial / signature snippet          | `search(query="X")`                                | `find_symbol` (exact only)         |
| Search by meaning / description              | `find_similar(query="…description…")`              | `search` (lexical only)            |
| Get the body of a known symbol               | `show(symbols=["X"])`                              | `Read` on the whole file           |
| Find every reference to a symbol             | `usages(symbol="X", strict=true)`                  | `grep` (false hits in strings)     |
| Find regex hits in source text               | `grep(pattern="…regex…")`                          | `usages` (symbol-only)             |
| Find AST patterns                            | `pattern(pattern="…", lang="…")`                   | `grep` (no scope, no metavars)     |
| Find subtypes of a base class                | `implementations(symbol="Base")`                   | `grep "extends Base"`              |
| Who calls / who do I call                    | `callers` / `callees`                              | Reading file to find call sites    |
| Multi-hop "can A reach B"                    | `paths(from="A", to="B")` or `reachable(symbol="B")` | Manual graph walk                |
| Symbol-level diff vs git base                | `diff(base="origin/main")`                         | `git diff` (line-level only)       |
| Near-duplicate code                          | `duplicates(explain=true)`                         | Manual review                      |
| One-shot LLM context for a symbol            | `bundle(mode="symbol", symbol="X")`                | 4 separate tool calls              |
| One-shot PR-impact context                   | `bundle(mode="pr-impact", base="origin/main")`     | Reading every changed file         |
| Follow a call across a **language boundary** | `grep(pattern="<shared route/topic/key>")` — see Recipe 6 | `usages` / `callers` (no cross-language edges exist) |

**Guideline**: prefer one tool with the right args over many tool calls. `bundle` exists specifically to collapse 4-round-trip "show + callers + callees + similar" chains into one call.

## FAQ — `vex search Foo` returned the wrong things

**Symptom**: `vex search compile_query` ranks callers (functions that USE `compile_query`) above the symbol itself. The user expected "find the definition of `compile_query`"; got "files most relevant to the string `compile_query`".

**Why**: `vex search` is a 3-way RRF fusion over (structural FST, BM25, semantic). When the queried name has **no local definition** in the index (typical for symbols imported from external crates — `use chili_pg_utils::compile_query`), all three channels converge on "files that mention the name":
- Structural FST: 0 hits (no symbol with that name is defined locally).
- BM25: ranks up files where the name appears as a token — the callers + import statements.
- Semantic: drifts to caller-shaped contexts whose embeddings are close to the query.

This is by design — `vex search` is the **ranked-relevance** surface, not the **exact-lookup** surface. The output is correct given the inputs; the gap is in tool choice.

**Fix (in order of precision)**:
1. **`vex check <name>`** — fastest existence probe (bloom prefilter, ~10 µs); answers "is this defined here?" yes/no.
2. **`vex show <name>`** — extract definition body; returns nothing if undefined.
3. **`vex usages <name> --strict`** — every reference site, scope-bound. Returns callers + imports when the name is external.
4. **`vex outline <file>`** — list every symbol defined in one file when you suspect the symbol's location.
5. **`vex search <query>`** — only when you want ranked relevance, not exact lookup.

**Decision rule for agents**:
- "Find the definition of X" → `check` → `show`
- "Find all references to X" → `usages --strict`
- "Find symbols similar to a known X" → `similar` (post-`show` on a known seed)
- "Find code relevant to topic Y" → `search`

`vex search` also emits a stderr hint (v1.15.0+) when the query is identifier-shaped AND structural FST returned zero — it explicitly suggests the precise-lookup tools above. The hint is non-fatal and doesn't appear in the JSON envelope.

## Recipe 1 — Code archaeology

**Goal**: understand an unfamiliar feature well enough to safely change it. With Phase 14.8 (`vex index --history`), this extends from "what does the code look like now" to "how did it get this way".

**Ask the agent like**:
> "What does `process_payment` do, who calls it, and how has it evolved?"

**Tool sequence (present-state)**:

1. `find_symbol(symbol="process_payment")` — confirm it exists and locate definition. Returns one or more matches if overloaded.
2. `bundle(mode="symbol", symbol="process_payment", callers_max=10, callees_max=10, similar_max=5)` — one call returns the body, the top callers, the top callees, and semantically-similar symbols. Defaults give an LLM-sized context (~3-5k tokens).

**Tool sequence (historical, v1.15.0+)**:

3. `history(symbol="process_payment", limit=10)` — every commit that touched a blob containing this symbol, oldest first. With an indexed section (Phase 14.8 `vex index --history`), this is a ~10ms FST lookup; without, it shells out to `git log` (~seconds).
4. For each interesting commit in the history list, the SHA is shown — pair with `git show <sha> -- <file>` to inspect that revision's body if you need the exact diff.

**Why one `bundle` call**: each of `show`, `callers`, `callees`, `similar` separately costs an MCP round trip (network + agent token budget for the response envelope). The Phase 13 bundle exists to coalesce them; `mode="symbol"` is exactly the archaeology shape. The `history` call is a separate MCP tool because the result set scales with `commit_count`, not with current-symbol-count — wrapping it into bundle would bloat the typical `bundle(mode="symbol")` response for users who don't need history.

**Why enable `--history`**: without it, `vex history` shells out to git per query (seconds-scale latency on long-lived repos). With it, queries are ~ms — composable in agent loops without blocking. Cost: one-time `vex index --history` adds 10s-2min depending on repo size; storage adds 50-350% of `index.vex` size (scales with history depth, not current symbols). See `docs/HISTORY-INDEX.md` for the cost-benefit table.

**When to deviate**:
- If you only need the body (no relations or history), `show(symbols=["process_payment"], head=40)` is cheapest.
- If the symbol is overloaded across files, follow up with `find_symbol` and disambiguate by `path`.
- For a transitive reachability check (who eventually calls this), add `reachable(symbol="process_payment")` — but expect a wider set than `callers`.
- If `history` returns empty but you know the symbol existed: try `--no-index` (forces the walker, which may have a different match policy) or check `vex status` for a `History: no` line meaning the section isn't built.
- For symbols whose name has been **deleted** from HEAD: the indexed path finds them (NEW capability vs walker); the walker can't because its `git grep` probe runs at HEAD and finds nothing.

## Recipe 2 — Refactor across cross-file boundaries

**Goal**: rename / move / restructure a symbol with confidence that no caller is missed.

**Ask the agent like**:
> "Rename `OldName` to `NewName` everywhere — find all real usages first, including cross-file imports."

**Tool sequence**:

1. `find_symbol(symbol="OldName")` — confirm the symbol exists and you have the right one.
2. `usages(symbol="OldName", strict=true)` — **`strict=true` is the load-bearing flag**. Text-scan `usages` (default) returns string-literal / comment / wrong-scope false hits; `strict=true` reads the persistent scope-binder reference edges so cross-file imports are resolved for Rust / TypeScript / Python / C# / C++ / Go / Java / Kotlin (other languages fall back to text-scan and the response signals that).
3. For each unique caller file in the result set, optionally `show(symbols=["<caller_symbol>"], head=20)` — head-only views are usually enough to plan the rename.
4. Apply the rename (`Edit` / your editor of choice).
5. `usages(symbol="OldName", strict=true)` again — must return empty. If anything remains, either the binder couldn't resolve it (look at the response `signals`) or you missed a manual edit.
6. `usages(symbol="NewName", strict=true)` — should return the migrated set. Quick sanity check.

**Why this chain**:
- Step 2 with `strict=true` is the difference between "I think I got them all" and "the binder says I got them all." On a 50k-LOC Rust crate, `--strict` typically drops 30-60% of text-scan false hits.
- Step 5 is the verification gate. Skipping it is how stale references survive a rename.
- The binder coverage matrix lives in [`README.md` → Type-aware refs](../README.md#type-aware-refs). For wildcard-form imports (Python `from x import *`, Rust `use foo::*`, C++ `using namespace`) `strict=true` falls back to text-scan; expect the response `signals` to flag this.

**Variants**:
- For function signature changes (not just renames), follow up `callers("OldName")` and `show` each caller to inspect call sites for arity / type compatibility before applying the change.
- For moves across modules, `find_symbol` after the move to confirm the new location is what you expect.

## Recipe 3 — PR-impact analysis

**Goal**: reviewing a feature branch, understand the blast radius before approving.

**Ask the agent like**:
> "What did this branch change vs `main`, and what code depends on those changes?"

**Tool sequence**:

1. `diff(base="origin/main")` — symbol-level diff (added / removed / moved / body-changed) for files touched on the branch. Line-level `git diff` doesn't tell you "which functions changed"; this does.
2. `bundle(mode="pr-impact", base="origin/main", depth=2, tests_max=20)` — one call returns the changed symbols, their transitive callers up to `depth=2`, and a sample of tests that exercise the changed surface. The Phase 9 PR-impact bundle is calibrated for review workflows: response includes a `_meta.vex.dev/diff_filter` envelope showing how many candidates were dropped vs retained (visibility into what the bundle decided to surface).
3. For each high-risk change (large body diff, public API, called by many places), `reachable(symbol="<changed_symbol>")` — surfaces transitive consumers your reviewer eye might miss.
4. Optional: `similar(symbol="<changed_symbol>", explain=true)` — finds symbols semantically close to the changed one. If a developer changed `parse_json_v2` but `parse_json_legacy` looks similar, the similar-symbols response is a hint that the legacy path might need analogous treatment.

**Why this order**:
- Step 1 gives you the *what* (symbol-level). Step 2 gives you the *who depends* (calltree). Step 3 gives you the *who eventually depends* (transitive). Step 4 gives you the *who looks suspiciously similar* (semantic).
- `depth=2` is the bundle default — a 2-hop caller walk is usually the right blast radius for a single PR. Bump to `depth=3` for large refactor branches; the response cost roughly doubles per level.

**Variants**:
- For a "what tests should I run" question, the bundle's `tests_max` field caps how many test symbols come back. The response `mode_hints` includes a `tests_for` map keyed by changed symbol.
- For a "did this change reach production code paths" gate, filter by `--include 'src/**'` `--exclude 'tests/**'` to bias the reachable walk toward non-test consumers.

## Recipe 4 — Find dead code & duplicates

**Goal**: pre-release cleanup — identify symbols nobody uses and near-duplicate functions that could be consolidated.

**Ask the agent like**:
> "Find functions in this crate that look unused or duplicated, with the receipts."

**Tool sequence**:

1. `duplicates(explain=true, threshold=0.85)` — semantic near-duplicate symbol pairs across the repo. `explain=true` surfaces identifier-overlap (Jaccard) + a unified diff per pair, so you can tell at a glance whether the pair is a real dupe vs two methods that just share helpers. Default `threshold=0.8`; bump to `0.85+` to cut noise on first pass.
2. For each suspect from step 1, `usages(symbol="<name>", strict=true)` on both members of the pair. If one has zero callers, you've found a consolidation candidate (delete the unused one, retarget callers to the survivor).
3. For "unused" candidates not surfaced by `duplicates`, the search shape is `find_symbol` + `usages` per suspect; the cheaper sweep is `search(query="…likely-stale-substring…")` to surface candidates first, then verify with `usages --strict`.
4. `callers(symbol="<suspect>")` — verification that the zero-usages result wasn't a false negative on the binder coverage matrix (dynamic dispatch / reflection / generated code can be invisible — see [`docs/LIMITATIONS.md`](LIMITATIONS.md)).

**Why explain matters**: bare `duplicates` returns pairs; `explain=true` returns *why*. A reviewer looking at a 50-pair response can scan Jaccard scores + diffs in seconds; bare pairs require opening each file. The cost of `explain=true` is one extra Jaccard pass over the matched pairs, which dominates the wall-clock only on very large `top_n`.

**Variants**:
- For "find all unused public APIs", `search(query="…", visibility="public")` + `usages --strict` filter.
- For "consolidate near-duplicate tests", filter `duplicates` to test files: `duplicates(include=["tests/**"], threshold=0.9)`.
- Note the LIMITATIONS doc's honest caveats: function-scoped callers and dynamic dispatch are invisible. Always cross-check before deleting.

## Recipe 5 — Multi-codebase orchestration

**Goal**: agent needs to navigate two repos at once (e.g., the API crate and the client SDK).

**Approach**: run **two `vex-mcp` server entries** in the same agent config, one per repo. The MCP-side tool names collide unless you alias them, so the standard is one entry with `name: "vex-api"` and one with `name: "vex-client"`, each scoped via `VEX_ROOT`:

```jsonc
{
  "mcpServers": {
    "vex-api":    { "command": "/path/to/vex-mcp", "env": { "VEX_ROOT": "/repos/api"    } },
    "vex-client": { "command": "/path/to/vex-mcp", "env": { "VEX_ROOT": "/repos/client" } }
  }
}
```

The agent then sees `vex-api.search(...)` and `vex-client.search(...)` as distinct tools (exact namespacing convention is agent-specific; Claude Code shows them as `mcp__vex-api__search` etc.).

**When to use**: when one repo references another by symbol name (e.g., the SDK has a `Client.send_request` and the API has `handle_request`; you want the agent to navigate both during a "trace this end-to-end" prompt).

**When *not* to use**: if the two codebases are in the same monorepo, one `VEX_ROOT` pointing at the workspace root with `--include` filters at query time is cheaper than two server processes.

## Recipe 6 — Cross-language: follow a request across a service boundary

**Goal**: a TypeScript client calls an endpoint; the handler is in Go (or Python, or Java). The agent needs the handler, and `usages` cannot help — the call crosses a language boundary, so there is no symbol edge to follow.

**Approach**: search for the *shared string*, not the symbol. The route path, queue topic, env-var key or feature-flag name is the only thing both sides actually share, and `vex grep` is backed by a per-file trigram skip-index, so scanning the whole repo for one is fast even on large corpora.

```bash
# The literal both sides carry
vex grep 'v1/invoices' --format compact

# Route params differ per framework — match the stable prefix, not the whole path
#   TS:     fetch(`/api/v1/invoices/${id}`)
#   Go:     r.Get("/api/v1/invoices/{id}", h.Get)
#   Python: @app.get("/api/v1/invoices/{invoice_id}")
vex grep 'api/v1/invoices' --format compact

# Then pivot to structure once you know the handler's name
vex show InvoiceHandler
vex callers InvoiceHandler
```

Same shape for the other cross-boundary keys: `vex grep 'invoice.created'` for a queue topic, `vex grep 'INVOICE_API_URL'` for an env var, `vex grep 'CreateInvoiceRequest'` for a protobuf message whose generated stubs live in several languages.

**Why grep and not a smarter command**: vex deliberately does not synthesise cross-language edges. Published static extractors for exactly this problem top out around 0.68 recall on REST endpoints, and the tools that do it well (JetBrains, Glean) require an OpenAPI/proto spec as the join key rather than matching strings across languages. An edge that is wrong a third of the time is worse than no edge for an agent, because it cannot tell which third. The string match is honest: it returns evidence with `path:line`, and the agent judges it.

**Cut the noise**: generated stubs (`*.pb.go`, `*_pb2.py`) usually match the same strings as hand-written code and can bury it.

```bash
vex search CreateInvoice --exclude-generated
```

`--exclude-generated` recognises generator banners (`// Code generated … DO NOT EDIT.`, protoc, sqlc, bindgen, Diesel, OpenAPI Generator). It is a header heuristic, so a generator that writes no banner is invisible to it — it under-reports rather than hiding hand-written code.

**When *not* to use**: if you want "which services call this in production", use distributed tracing instead. Source search sees cold paths and un-exercised routes that tracing misses, but tracing sees dynamic dispatch, gateway rewrites and config-driven routing that no static tool can.

## See also

- [`README.md` → Integration](../README.md#integration) — install + per-agent MCP setup.
- [`integrations/`](../integrations/) — ready-to-paste MCP config snippets per agent.
- [`docs/MCP-SCHEMA.md`](MCP-SCHEMA.md) — canonical MCP parameter vocabulary.
- [`docs/LIMITATIONS.md`](LIMITATIONS.md) — what vex's call graph / `usages --strict` *can't* see (dynamic dispatch, reflection, generated code, wildcard imports).
- [`docs/SEMANTIC.md`](SEMANTIC.md) — semantic pipeline internals (relevant when `find_similar` / `similar` / `duplicates` returns unexpected results).
