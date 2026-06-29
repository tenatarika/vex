# Multi-repo Phase 8 — MCP `--workspace` surface

Status: **SHIPPED** (2026-06-29). Gives agents the same `--workspace` fan-out
the CLI has (phases 1-7). No on-disk format change; no CLI change (the binary
already does everything). All §9 review resolutions folded in; 169/169
vex-mcp tests (CI `cargo test --workspace`), clippy + stable-fmt clean.

## 1. Architecture (why it's cheap)

The MCP server **shells out** to the `vex` binary
(`crates/vex-mcp/src/main.rs:197`): each tool is a `build_*(args,
project_root) -> (subcommand, Vec<flag>)`; `handle_tool_call` runs `vex
<subcommand> <flags> --format json` in `current_dir(project_root)` and wraps
the stdout envelope. So "support `--workspace`" = push `--workspace` into the
flag vec + declare the param in the tool descriptor. The CLI's
`build_workspace_resolver` + fan-out do the rest.

## 2. Tools to cover (full CLI↔MCP parity)

The CLI commands that accept `--workspace` (`extract_workspace_flag`):
`search, grep, check, usages, impact, callers, callees, reachable, index,
update` (+ `watch`, not an MCP tool). Map to MCP tools:

| MCP tool | subcommand | covered |
| --- | --- | --- |
| `search` | search | ✅ (gate `--why`, see §4) |
| `find_symbol` | search | ✅ (if it emits `search`) |
| `grep` | grep | ✅ |
| `check` | check | ✅ |
| `usages` | usages | ✅ |
| `impact` | impact | ✅ |
| `callers` / `callees` / `reachable` | (graph) | ✅ |
| `index` / `update` | index / update | ✅ (admin) |
| everything else (similar/duplicates/pattern/show/outline/history/tests_for/paths/diff/status/eval/bundle/implementations) | — | ❌ no CLI `--workspace` |

## 3. Mechanism

- New shared arg helper `args::push_workspace(&mut extra, args)` (mirrors
  `push_auto_update` etc.): `if opt_bool(args, "workspace", false)? {
  extra.push("--workspace".into()) }`. Each covered `build_*` calls it.
- Each covered tool's descriptor (`descriptors.rs`) gains:
  `"workspace": { "type": "boolean", "default": false, "description": "Fan
  the query across every repo in the nearest .vex-workspace.toml (set
  project_root at/above it); results are grouped by repo. See
  docs/MULTIREPO.md." }`. Snapshot test (`snapshots/`) regenerated.

## 4. `--why` clap conflict (search)

CLI `--workspace` is a hard clap-conflict with `--why`. `build_search`
currently pushes `--why` when `why: true`. Resolution: when BOTH `workspace`
and `why` are requested, DROP `--why` (workspace wins) and record a deprecated/
notice string so the agent learns why the trace is absent — OR simply skip
`--why` and document that `why` is ignored in workspace mode (single-repo only,
matching the CLI). Decision: **skip `--why` in workspace mode**, mirroring the
CLI's own "`--why` is single-repo only" contract; note it in the `why`
descriptor text. (No other covered tool pushes a `--workspace`-conflicting flag.)

## 5. Response shape (NO response.rs change)

`vex <cmd> --workspace --format json` emits the standard `ResponseEnvelope`
(`print_envelope` nests the payload under `results`), with the payload being
`{ "workspace": "<path>", "repos": [ { "repo", ... } ] }` instead of a flat
array. `response.rs` lifts `content.results` verbatim into
`structuredContent.results` (`response.rs:81,102`), so the agent receives the
grouped-by-repo object as the typed payload with NO code change. The only
agent-visible contract: **in workspace mode `structuredContent.results` is an
object `{workspace, repos:[...]}`, not the flat per-tool array** — call this
out in each covered tool's `workspace` param description so agents branch on
shape. (Confirmed: search/check/usages/impact/callers/callees/reachable
workspace branches all emit `{workspace, repos}` via `print_envelope`.)

## 6. `project_root` semantics

The MCP `project_root` arg (or `$VEX_ROOT`) is the `current_dir` for the spawn.
In workspace mode the agent sets it to the workspace root (or any dir at/below
it that has `.vex-workspace.toml` at/above) — `--workspace` walks up to find
the manifest, same as the CLI. Document in the `workspace` param description.

## 7. Tests
- Unit (per covered `build_*`): `workspace: true` pushes `--workspace`;
  default omits it.
- `build_search`: `workspace: true` + `why: true` → `--workspace` present,
  `--why` absent (the §4 gate).
- Descriptor snapshot regenerated; a test asserts each covered tool's schema
  has a `workspace` property.
- Integration (`tests.rs` harness, which feeds canned tool calls): one
  workspace tool call round-trips (may need the harness to stub the spawn —
  confirm whether tests.rs actually spawns `vex` or mocks; if it spawns, a
  workspace integration test needs a real `.vex-workspace.toml` fixture +
  indexed members, which is heavy — prefer the build_* unit tests + the
  existing CLI e2e for behaviour, and keep the MCP test at the arg-construction
  layer).

## 8. Risks / non-goals
- **`index`/`update` via MCP `--workspace`** index/refresh every member — a
  heavier operation than single-repo; fine (the agent opted in), but note the
  latency in the descriptor.
- **Per-result `signals` / `--why`** stay single-repo (the CLI omits them in
  workspace output); the MCP `why` param is a no-op in workspace mode.
- **No new MCP tool** — `--workspace` is a boolean param on existing tools,
  not a separate `*_workspace` tool, keeping the surface flat.

## 9. Review resolutions (rust-reviewer, locked before scaffold)

**CRITICAL — the `--why` gate applies to BOTH `search` AND `usages`.**
`Usages.workspace` is `conflicts_with = "why"` (`args.rs:463`), same as
`Search` (`args.rs:327`), and `build_usages` pushes `--why` when `why: true`
(`graph.rs:28-29`). So `build_search` AND `build_usages` must skip `--why`
when `workspace: true` (workspace wins; `--why` is single-repo only, matching
the CLI). No other covered `build_*` pushes `--why`.

**CRITICAL (verified, no change) — envelope wrapping.** All workspace
branches emit via `print_envelope` (search/check/usages/impact/callers/
callees/reachable/grep — grep confirmed `cmd_grep.rs:173`), which wraps in
`ResponseEnvelope { protocol_version, capabilities, _meta, results }`. So
`response.rs:83` `is_envelope` is true and `content.results`
(`{workspace, repos:[...]}`) lifts verbatim into `structuredContent.results`
(`response.rs:102`, accepts any `Value`, not just arrays). NO response.rs
change.

**HIGH — exclude `find_symbol` from workspace coverage.** It emits the
`search` subcommand with `--limit 10` as a thin exact-name probe; running it
`--workspace` would silently become a ranked grouped-by-repo search — a
semantic surprise. Cross-repo existence is better served by `check
--workspace`, ranked cross-repo by `search --workspace`. So `find_symbol`
does NOT get the param. Covered set: **search, grep, check, usages, impact,
callers, callees, reachable, index, update** (10 tools).

**HIGH — `push_workspace` is the plain helper for 8 tools; `search` + `usages`
gate `--why` inline.** `args::push_workspace(&mut extra, args)` (plain push)
for grep/check/impact/callers/callees/reachable/index/update. `build_search`
and `build_usages` compute `let workspace = opt_bool(...workspace...)`, push
`--workspace` when set, and push `--why` ONLY when `why && !workspace`.

**MEDIUM — descriptor notes.** Each covered tool's `workspace` param
description states: (a) results become an object `{workspace, repos:[...]}`,
not the flat array; (b) set `project_root` at/above the `.vex-workspace.toml`
(the manifest is found by walking UP from `project_root`/cwd — a `project_root`
pointing at a member dir BELOW the manifest works only if the manifest is an
ancestor). For `grep`: note it does not auto-update (pre-existing), so stale
members are possible — pass an indexed workspace. The `capabilities`
introspection tool stays workspace-unaware (it does not accept `--workspace`);
note this so agents don't expect it to advertise per-tool workspace support.

**Tests.** `build_*` unit per covered tool (workspace:true pushes
`--workspace`; default omits). `build_search` AND `build_usages`:
`workspace:true + why:true` → `--workspace` present, `--why` absent. Descriptor
snapshot regenerated; assert each covered tool's schema has a `workspace`
property and `find_symbol` does NOT. Tests live at the `build_command`
arg-construction layer (`tests.rs` does not spawn `vex`).
