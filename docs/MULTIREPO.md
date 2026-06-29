# Multi-Repository Index — Design

Status: **PHASES 1, 3, 4a SHIPPED** (2026-06-29). `.vex-workspace.toml`
manifest + resolver, `vex index --workspace`, and `vex search / check /
grep --workspace` are implemented (see §8 for per-phase status). Design
pass 2026-06-28, revised after architect + rust-reviewer review (the
`CacheResolver` workspace-root/member-root split, hidden-static catalogue,
phase-4 split, cross-repo Pass-2 non-conflict, ranking/HNSW-recall risks).

**Shipped MVP limitations** (also in `docs/LIMITATIONS.md`): members are
indexed/queried sequentially; results are grouped per-repo with no unified
cross-repo ranking; cross-repo `--strict`/call-graph refs are invisible
(each member resolves within itself); `vex search --why` and per-result
JSON `signals` are single-repo only (`--why` is a clap conflict with
`--workspace`); `--limit` is per-member (totals up to N×limit);
hash-less cache layouts (`local_cache`) are rejected in workspace mode.

This document proposes a "workspace" mode that lets one `vex` invocation
index and search across several independent repositories (e.g. a folder
of microservice repos) instead of a single project root. It is grounded
in a prior-art survey (zoekt, OpenGrok, Hound, livegrep, GNU GLOBAL) and
a map of vex's current single-root assumptions.

---

## 1. Goals / non-goals

**Goals**
- Index N independent repo roots and query them as one corpus from a
  single CLI invocation (`search`, `check`, `usages`, `impact`, etc.).
- Per-repo incremental update: changing one repo re-indexes only that
  repo.
- Each result is attributed to its source repo.
- Zero on-disk format change to the per-repo index itself.

**Non-goals (MVP)**
- Cross-repo `--strict` reference resolution / go-to-def. Strict refs
  stay per-repo (see §7).
- A long-running server / daemon. This stays a CLI.
- Compound-shard packing for thousands of tiny repos (premature; revisit
  only if per-index overhead actually bites).

---

## 2. Current single-root assumptions (what we must work around)

| Area | Anchor | Assumption |
| --- | --- | --- |
| Cache keying | `src/util/config.rs::index_dir` + `CACHE_OVERRIDE: OnceLock` (set once in `src/cli/mod.rs`) | One project root → one `<cache>/<xxh3(canonical root)>/` dir. The override is **single-valued per process**. |
| Root resolution | `src/cli/common.rs::resolve_root` / `extract_path_hint` | A single root from `--path` or cwd, threaded everywhere. |
| Path storage | `crate::util::paths::to_rel_posix` (single boundary) | Every stored path is **relative to the one root**. `repo-a/src/main.rs` and `repo-b/src/main.rs` both store as `src/main.rs` → collision. |
| Manifest / incremental | `src/index/manifest.rs` (+ `index.state` sidecar) | One manifest per root; `files` keyed by rel-path; `imported_by` per-root. |
| Cross-file resolution | `src/store/writer.rs` Pass-2 `name_to_global` | Single-candidate fallback over one corpus. Merging repos **increases** ambiguous-name collisions → worse precision. |
| Git / staleness / watch | `src/index/staleness.rs::read_git_head`, `src/watch` | One `.git`, one HEAD per index. |

**The load-bearing insight:** the cache is *already* keyed per canonical
path. So N repos already map to N independent, correctly-shaped index
dirs. The collision/precision problems above only appear if we try to
**merge** repos into one index. We don't — see §4.

---

## 3. Prior art (summary)

| Tool | Model | Tradeoff |
| --- | --- | --- |
| zoekt (Sourcegraph) | One **shard per repo**; query fans out per shard, merged by priority. Cross-repo nav is a separate SCIP layer. | Free per-repo incremental + zero-downtime swap; no cross-repo semantics in the index. |
| OpenGrok | **Index per project**; multi-select = union of independent indexes. | Trivially parallel/incremental; no cross-project resolution. |
| Hound | **Index per repo**, results grouped by repo (attribution by *which searcher produced it*). | Simplest isolation; pure regex, no symbols. |
| livegrep | **One merged index** with a repo-tagged file map. | Unified query + cheap attribution; **full corpus rebuild on any change**. |
| GNU GLOBAL (gtags) | **DB per tree** + ordered fallback chain (`GTAGSLIBPATH`, first-hit-wins). | True per-tree incremental + cross-tree *definition* lookup via ordered fallback. |

**Consensus:** per-repo-index + query-fanout is the dominant, lowest-risk
model. The only working prior art for cross-repo *symbol* resolution is
either a separate global-symbol-ID layer (SCIP) or gtags' ordered
fallback — never merged into the trigram/text index.

---

## 4. Chosen architecture: per-repo index + query fanout

```
workspace root/
├─ .vex-workspace.toml        # declares the member repos
├─ service-a/                 # git repo A  → <cache>/<xxh3(canon A)>/index.*
├─ service-b/                 # git repo B  → <cache>/<xxh3(canon B)>/index.*
└─ libs/shared/               # git repo C  → <cache>/<xxh3(canon C)>/index.*
```

Each member repo keeps its **own independent index dir** (today's format,
unchanged). A workspace command:

1. Resolves the member list from `.vex-workspace.toml`.
2. For each member, computes its existing per-repo cache dir and opens
   its index (indexing it first if stale).
3. Runs the query against each index (rayon-parallel), tags every result
   with its owning repo, and merges/ranks.

This sidesteps every collision/precision problem in §2 because **no merge
happens at the index layer** — paths stay rel-to-their-own-root, each
Pass-2 corpus stays per-repo, each manifest/git-HEAD stays per-repo.

---

## 5. Workspace manifest

`.vex-workspace.toml` at the workspace root:

```toml
# Explicit list beats "every subdir is a repo" convention (Hound/livegrep
# both chose explicit over OpenGrok's convention): it decouples logical
# identity + display name from on-disk layout and supports nesting.
[[repo]]
path = "service-a"          # relative to the workspace file (or absolute)
name = "service-a"          # optional display name; defaults to dir name

[[repo]]
path = "libs/shared"
name = "shared"

# optional: glob discovery as sugar, expanded to explicit entries at load
# discover = ["services/*", "libs/*"]
```

Resolution rules:
- `path` is canonicalized; the per-repo cache dir is `xxh3(canonical
  path)` — **identical** to what single-repo mode already computes, so a
  repo indexed standalone and as a workspace member shares one index dir.
- Duplicate / overlapping (nested) member paths are rejected at load with
  a clear error.

---

## 6. Layer-by-layer changes

### 6.1 Cache / config — a `CacheResolver`, NOT just "remove the override"
**Correction from design review:** `CACHE_OVERRIDE: OnceLock` is only the
*cache-root* override (`--cache-dir` / `VEX_CACHE_DIR` / `.vex.toml`
`cache_dir` / `local_cache`). It is already keyed per-root *downstream* —
`index_dir(project_root)` takes the root and hashes it — so for the
**default platform-cache case, de-globalizing is a no-op**: `index_dir(A)`
and `index_dir(B)` already return distinct dirs with the override set once
to the platform root. It only bites when a member's own `.vex.toml` sets
`cache_dir` / `local_cache` (different cache root + `skip_hash_subdir`
layout): the single `OnceLock` holds one `CacheLayout` for the whole
process, so fanout would route every member through whichever member's
config won the `set()`.

Two functions are repo-**agnostic** by design and must NOT be keyed
per-member: `embed_cache_dir()` (model weights) and `blob_cache_dir()`
(content-addressed blob SHA cache). Keying them per-member would duplicate
weights/blobs N times. They must anchor to the **workspace root**.

Shape:

```rust
pub struct CacheResolver {
    workspace_root: PathBuf,                 // embed_cache_dir, blob_cache_dir
    members: HashMap<PathBuf, CacheLayout>,  // canonical member root → layout
}
```

Thread `&CacheResolver` into pipeline entry points instead of reading the
global. Keep `CACHE_OVERRIDE` for single-root mode (no churn on
non-workspace commands); the resolver wraps it.

**MVP scope:** platform-cache members only. Reject per-member `cache_dir`
/ `local_cache` at workspace load with a clear error until `index_dir` and
the ~15 `*_path(root)` helpers are parameterized off the global. This
keeps phase 2 small.

**Canonicalize-once invariant (tested):** member roots are canonicalized
**exactly once** at workspace load; fanout consumes `Member.root` and
**never re-derives** a root from a member's `.vex.toml` or a relative
manifest `path`. `resolve_root` does not canonicalize today — every
command does it at the call site — so a single load-time boundary is
*safer* than the status quo, but only if nothing downstream re-resolves
(the macOS `/tmp`-symlink fallback hazard; Phase 14.8 Step 7 precedent).

### 6.1b Hidden process-global state to fix
- `STALE_REASON: LazyLock<Mutex<Option<String>>>` (`src/cli/stale_signal.rs`)
  is **first-write-wins** — member A's stale failure would poison the
  signal for all members. Make it per-member (`Vec<(member, reason)>`).
  Fix alongside read-side fanout (phase 4), not after.
- `NO_RESULTS: AtomicBool` — fine ("no results across all members" is
  still one boolean).
- `PARSERS: thread_local!` — fine (keyed by `Language`, not root).
- Global rayon pool (`init_rayon_pool`) — already initialized once before
  fanout; correct. But `--jobs` / `VEX_JOBS` now means workspace-wide, not
  per-member — document it.

### 6.2 CLI surface
- `vex index --workspace [<workspace-file>]` — index every member (skip
  unchanged via each member's existing manifest/staleness check).
- Query commands gain `--workspace`: `vex search --workspace "X"`,
  `vex usages --workspace Foo`, `vex impact --workspace Bar`.
- Auto-detect: if cwd (or `--path`) contains `.vex-workspace.toml` and no
  explicit single-repo index, default to workspace mode. (Decision: opt
  in explicitly first, auto-detect later.)

### 6.3 Query fanout + result attribution
- A `Workspace { members: Vec<Member> }` type; `Member { root, cache_dir,
  display_name }`.
- Fan out per member with rayon; each result carries a `repo:
  Option<RepoId>` field (display name + which member produced it) — **attribution
  at the result layer, not via path-prefixing** (every fanout tool does
  this; path-prefixing would corrupt the rel-path contract at
  `to_rel_posix`).
- Merge: concatenate, then apply the existing per-command ranker across
  the union. Output groups by repo (or interleaves by score with a
  `[repo]` tag) — `--format json` gains a `repo` field per result.

**Result-field ripple (design-review note):** `SearchResult`
(`src/search/mod.rs`) has ~61 construction sites; adding `repo:
Option<RepoId>` with `#[serde(skip_serializing_if = "Option::is_none")]`
is additive (single-repo JSON unchanged) but every literal must supply it
(impl `Default` or `..Default::default()`). The sharper risk is the 7
hand-rolled `serde_json::json!` outputs in `src/cli/output.rs`
(print_similar/duplicates/diff/paths/tests_for/reachable) — they bypass
`SearchResult`, so each needs the `repo` key added manually. Add an
integration test asserting `"repo"` is present in workspace-mode JSON
before merging phase 4. For channel commands, annotate at the **post-channel
merge step** (the handler knows which member it queried) — no change to
`ChannelContext`.

### 6.4 Incremental update
- Per-repo manifests already exist. `vex update --workspace` walks members
  and runs each member's normal incremental update (git-HEAD staleness +
  blob cache, Phase 14.7) independently. A symbol moving between repos
  looks like a delete in one + add in the other — acceptable for MVP.

### 6.5 Watch (later)
- Watch mode would arm watchers per member root and route a changed file
  to its owning member's incremental update. Out of MVP scope.

---

## 7. Cross-repo symbol resolution — the key decision

**MVP: per-repo only, documented as a limitation.** `--strict`, the call
graph, and `imported_by` cascade stay within a member repo. Rationale:
prior art is unanimous that cross-repo symbol resolution does not belong
in the search index, and merging corpora would *reduce* binder precision
(more ambiguous-name collisions decline in Pass-2 `name_to_global`).
Non-strict `search` / `usages` / `check` still fan out across all members.

**Later (additive): gtags-style ordered fallback.** For a symbol left
`Unresolved` by a member's own Pass-2, consult sibling member indexes in
declared order (first-hit-wins, with a `--through`-style union override).
This keeps each repo's rayon-parallel build intact and never merges the
corpora. A true global-symbol-ID layer (SCIP-like) is net-new territory,
not a port — out of scope.

**No conflict with the Pass-2 architect constraint (explicit):** the
locked constraint is that *cross-file* resolution piggybacks on the
writer's `name_to_global` loop (not a per-language hook / new pipeline
stage). The cross-repo fallback is a **query-time read over N already-built
indexes** for symbols a member's own (unchanged) Pass-2 left unresolved —
it never touches the writer loop, never merges corpora, never serializes
the per-repo parallel build. Categorically outside the constraint.

---

## 8. Phased plan

1. ✅ **Workspace manifest + resolver** (`src/workspace/mod.rs`) — parse
   `.vex-workspace.toml`, canonicalize, map to per-repo cache dirs. Reject
   overlaps + per-member cache overrides. `Workspace::find_and_load` is the
   single entry point all `--workspace` commands share.
2. **De-globalize the cache override** — deferred. MVP rejects hash-less
   layouts (`local_cache`) in workspace mode instead, so the global
   `CACHE_OVERRIDE: OnceLock` is left intact.
3. ✅ **`vex index --workspace`** — indexes every member into its own dir
   (reuses the per-repo pipeline; each member uses its own `.vex.toml`).
4. **Read-side fanout** — split by risk class:
   - ✅ **4a — `search` / `check` / `grep`**: shipped. Attribution is at the
     OUTPUT layer (group-by-repo), NOT a `repo` field on result structs —
     this sidestepped a 61-site ripple. `STALE_REASON` per-member fix was
     NOT done (still global first-write-wins — documented in LIMITATIONS).
   - **4b — `usages` / `impact` / call-graph**: fanout is correct but a
     *semantic* change — a usage in repo B of a symbol defined in repo A
     is **missing**. Document the "results are per-repo; cross-repo refs
     invisible" limitation in `docs/LIMITATIONS.md` in the *same* change,
     so the MVP never looks like cross-repo go-to-def while silently not
     being it.
5. **`vex update --workspace`** — per-member incremental + **membership
   reconciliation**: index missing/stale members, warn on orphaned index
   dirs from removed/moved members (moved → new canonical path → new
   xxh3 → new dir, old one orphaned).
6. **(Opt.) cross-repo fallback resolution** (§7 option B).
7. **(Opt.) workspace watch mode.**

Phases 1–4 are the MVP and require **no binary-format change**.

---

## 9. Risks / open questions

- **Ranking across repos** — per-index BM25 idf is NOT comparable across
  members (same term → higher idf in the smaller corpus → global top-`N`
  over-represents small repos). **MVP: per-member `--limit` then
  group-by-repo display**; document that totals are up to N×members.
  Unified ranking (corpus-size-aware idf normalization or RRF-across-members)
  is later work.
- **`--limit` budget semantics** — per-repo or total? MVP = per-member
  (see ranking). Agent consumers may expect a total budget; revisit.
- **Semantic (HNSW) fanout** — two tradeoffs, not one: (a) usearch open +
  OpenMP-internal threads can contend with outer rayon workers — bench at
  N≈20 before shipping; (b) **recall**: per-graph top-k merged across N is
  approximate and degrades vs one graph over the union, worsening as
  k/limit shrinks relative to N. Mitigate by over-fetching `k' > limit`
  per member before the global merge (the existing path-filter over-fetch
  pattern in `cmd_search`).
- **`.vex-workspace.toml` vs `.vex.toml`** — they are **orthogonal**:
  member `.vex.toml` governs *that member's* index build (excludes,
  embedder, sections); the workspace file governs *membership + display*
  only. Not competing. (Auto-detect / discovery needs the same walk-up-to-
  root semantics as `load_config`; running a query from inside one member
  should stay single-repo, not silently widen.)
- **Path filters across members** — `--filter-path` / `--scope` globs match
  rel-paths; confirm they apply per-member (rel-to-member-root) for MVP, or
  whether a `repo:path` qualifier is needed to scope a glob to one member.
- **Concurrent read + write (watch phase only)** — N open mmap readers
  while a workspace update rewrites a member needs the existing
  atomic-rename + drop-old-`IndexReader` protocol. Not an MVP concern
  (watch is phase 7); note as a constraint.
- **Scale ceiling** — fanout is linear in N. Fine for tens of repos;
  thousands would want zoekt-style compound packing (explicit non-goal).
