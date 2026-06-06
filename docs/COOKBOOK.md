# vex Cookbook — Agent Workflow Recipes

End-to-end recipes for chaining vex's MCP tools to solve common code-navigation tasks. Each recipe shows the **tool sequence** an agent should run, **why this ordering**, and **how to phrase the request** so an agent picks the right chain without prompting nudges.

All snippets use the MCP tool surface (`usages(...)`, `bundle(mode=..., ...)`) and the canonical v1.7+ vocabulary (`symbol` / `query` / `path` / `pattern` / `filter` / `include` / `exclude`); the CLI equivalents (`vex usages ... --strict`, `vex bundle ...`) are 1:1 with the same args. See [`docs/MCP-SCHEMA.md`](MCP-SCHEMA.md) for the canonical schema.

## Tool-selection cheat sheet

| You want to…                                 | Reach for                                          | Not                                |
| -------------------------------------------- | -------------------------------------------------- | ---------------------------------- |
| Locate a specific named symbol               | `find_symbol(symbol="X")`                          | `grep`, `Read`                     |
| Search by name / signature / partial         | `search(query="X")`                                | `Grep`                             |
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

**Guideline**: prefer one tool with the right args over many tool calls. `bundle` exists specifically to collapse 4-round-trip "show + callers + callees + similar" chains into one call.

## Recipe 1 — Code archaeology

**Goal**: understand an unfamiliar feature well enough to safely change it.

**Ask the agent like**:
> "What does `process_payment` do, who calls it, and what does it depend on?"

**Tool sequence**:

1. `find_symbol(symbol="process_payment")` — confirm it exists and locate definition. Returns one or more matches if overloaded.
2. `bundle(mode="symbol", symbol="process_payment", callers_max=10, callees_max=10, similar_max=5)` — one call returns the body, the top callers, the top callees, and semantically-similar symbols. Defaults give an LLM-sized context (~3-5k tokens).

**Why one `bundle` call**: each of `show`, `callers`, `callees`, `similar` separately costs an MCP round trip (network + agent token budget for the response envelope). The Phase 13 bundle exists to coalesce them; `mode="symbol"` is exactly the archaeology shape.

**When to deviate**:
- If you only need the body (no relations), `show(symbols=["process_payment"], head=40)` is cheaper.
- If the symbol is overloaded across files, follow up with `find_symbol` and disambiguate by `path`.
- For a transitive reachability check (who eventually calls this), add `reachable(symbol="process_payment")` — but expect a wider set than `callers`.

## Recipe 2 — Refactor across cross-file boundaries

**Goal**: rename / move / restructure a symbol with confidence that no caller is missed.

**Ask the agent like**:
> "Rename `OldName` to `NewName` everywhere — find all real usages first, including cross-file imports."

**Tool sequence**:

1. `find_symbol(symbol="OldName")` — confirm the symbol exists and you have the right one.
2. `usages(symbol="OldName", strict=true)` — **`strict=true` is the load-bearing flag**. Text-scan `usages` (default) returns string-literal / comment / wrong-scope false hits; `strict=true` reads the persistent scope-binder reference edges so cross-file imports are resolved for Rust / TypeScript / Python / C# / C++ (other languages fall back to text-scan and the response signals that).
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

## See also

- [`README.md` → Integration](../README.md#integration) — install + per-agent MCP setup.
- [`integrations/`](../integrations/) — ready-to-paste MCP config snippets per agent.
- [`docs/MCP-SCHEMA.md`](MCP-SCHEMA.md) — canonical MCP parameter vocabulary.
- [`docs/LIMITATIONS.md`](LIMITATIONS.md) — what vex's call graph / `usages --strict` *can't* see (dynamic dispatch, reflection, generated code, wildcard imports).
- [`docs/SEMANTIC.md`](SEMANTIC.md) — semantic pipeline internals (relevant when `find_similar` / `similar` / `duplicates` returns unexpected results).
