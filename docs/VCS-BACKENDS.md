# VCS Backends — Design (git · Arc · svn)

Status: **Phase 1 SHIPPED** (2026-07-10, commit `bcf75df`); **DESIGN for
Phases 2-5** (§6). The plan abstracts vex's hard git dependency behind a `Vcs`
trait so it also runs against **Yandex Arc** and **Subversion**, with git as the
default and byte-identical current behavior preserved. Implementation is phased
(§6) and each phase is a separate reviewed change. Phase 1 (extract `Vcs` trait
+ `GitVcs` for diff-scoping) has landed; blob-cache/history/staleness remain
git-only until later phases.

Grounding: the git-coupling survey (§1) enumerates every git shell-out; the
trait (§3) is the minimal surface those sites actually need.

---

## 1. Where vex depends on git today (the surface to abstract)

~25 `Command::new("git")` sites across 8 files, in four functional groups:

| Group | Files | git operation | Consumers |
|---|---|---|---|
| **Diff scoping** | `src/util/git_diff.rs` | `rev-parse --is-inside-work-tree`, `diff --name-only <range>`, merge-base, `status`/untracked | `--since` / `--since-branched` / `--changed-only` on search/usages/impact/bundle |
| **Content cache** | `src/index/parse_cache/git_blobs.rs` | `ls-files -s` → blob SHA per tracked file | Phase 14.7 blob-SHA parse cache |
| **History** | `src/history/mod.rs`, `presence.rs`, `src/index/history_builder/`, `src/store/git_history.rs` | `log --follow`, `rev-list`, blob presence | `vex history`, Phase 14.8 history index, rename chains |
| **Staleness** | `src/index/staleness.rs`, `src/index/manifest.rs` | `rev-parse HEAD`, dirty-tree count | index freshness / auto-update; manifest stores HEAD |

**Load-bearing observation:** vex *already* degrades when git is absent — the
blob cache disables and falls through to the xxh3/mtime path
(`git_blobs.rs` returns an empty map → existing hash path), and diff-scope
emits a clear "not a git repository" error. The abstraction **generalizes an
existing fallback contract**, it does not invent one.

---

## 2. Backend reality check (what each VCS can/can't do)

| Capability | git | **Arc** (Yandex) | **svn** |
|---|---|---|---|
| Changed paths since a revision | ✅ | ✅ (`arc` mimics git CLI) | ✅ (`svn diff --summarize -r`) |
| Working-tree dirty + untracked | ✅ | ✅ | ✅ (`svn status`) |
| Merge-base / `since-branched` | ✅ | ✅ (git-shaped) | ⚠️ branches are dir-copies — **no clean merge-base** |
| Current revision id | ✅ 40-hex SHA | ✅ git-compatible SHA | ⚠️ monotonic **integer** `rN`, not a SHA |
| Content id for cache keying | ✅ blob SHA | ✅ **git-blob-compatible SHA** | ❌ no content-addressed blob store |
| History walk w/ rename-follow | ✅ `log --follow` | ✅ (git-shaped) | ⚠️ `svn log` — rename = copy+delete, weaker follow |

Three facts drive the design:
1. **Arc is git-object-compatible.** Its blob SHAs match git's, and its CLI
   closely mirrors git (`arc log`, `arc diff`, `arc status`, `arc rev-parse`).
   So `ArcVcs` reuses git-shaped command logic with the `arc` binary, and the
   Phase 14.7 blob cache **works unchanged** (same SHA space).
2. **svn has no blob store** → `content_id` is `None` on svn → the parse cache
   uses the existing xxh3/mtime fallback (already the dirty-tree path). Not a
   regression; svn simply doesn't get the blob-cache speedup.
3. **svn revisions are integers, and its branches don't merge-base** → the
   manifest's stored revision must become a backend-opaque string (not
   assumed-SHA, §5), and `SinceBranched` is a **capability the svn backend
   declines** (clear error, not a silent wrong answer).

---

## 3. The `Vcs` trait (minimal surface)

> **v1 scope decision (both reviews).** The full six-op trait is
> over-engineered for v1, and the four git subsystems (§1) have very different
> value/risk. **v1 abstracts only diff-scoping** — the one subsystem with clear
> Arc/svn user demand where svn genuinely works (`svn diff --summarize`) and
> which is self-contained. Blob-cache, history, and the staleness *shortcut*
> stay **git-only** in v1; non-git backends hit their *existing* fallbacks
> (empty blob map → xxh3/mtime cache; mtime staleness; history errors on
> non-git, as today). This delivers the requested value (Arc/svn scoped search)
> at a fraction of the surface and sidesteps the H1 staleness trap, the M1
> cache-poisoning risk, and svn's weak rename-follow for v1. The trait grows
> additively to the other ops in a later phase once the `arc` CLI is
> field-verified (§6).

New module `src/vcs/`: `mod.rs` (trait + `Capabilities` + `detect`), `git.rs`,
`arc.rs`, `svn.rs`. **v1 trait surface** (the diff-scope subset):

```rust
/// A version-control backend. One instance per invocation, resolved once by
/// `detect()` / the `--vcs` override and installed in a `OnceLock` (mirroring
/// the existing `CacheResolver` pattern in `util::config`).
pub trait Vcs: Send + Sync {
    fn kind(&self) -> VcsKind;                 // Git | Arc | Svn | None
    fn capabilities(&self) -> Capabilities;    // feature bits; callers check before use

    /// H3 — repo-validity pre-flight. This is a load-bearing correctness guard,
    /// NOT incidental: `git diff` outside a repo exits 0 with help text → a
    /// silent empty changed-set. Every backend has its own detached/invalid
    /// mode (git no-index; svn `info` fails; arc FUSE detached). Callers MUST
    /// pre-flight this before trusting `changed_paths`. Returns `Failed` (with
    /// a backend+caller-composable reason), never a bare bool, so the two
    /// existing distinct error strings (`git_diff` vs `history`) survive (§6 L1).
    fn ensure_repo(&self, root: &Path) -> VcsResult<()>;

    /// Files changed for a `DiffScope`. Contract (H2): **never map a backend
    /// error to `Ok(vec![])`.** A non-zero backend exit is `Failed`; an
    /// unmappable scope (svn + SinceBranched) is `Unsupported`. `Ok(vec![])`
    /// means *and only means* "resolved, nothing changed". Conflating these
    /// silently turns a broken filter into "your query matched nothing".
    fn changed_paths(&self, root: &Path, scope: DiffScope) -> VcsResult<Vec<String>>;
}

pub struct Capabilities {
    pub merge_base: bool,          // SinceBranched supported (git/arc yes, svn no)
    // grows additively as later phases add ops:
    // content_addressed, rename_follow, sha_revisions
}
```

- `DiffScope` (existing, `git_diff.rs:29`) stays backend-agnostic
  (`Since(rev) | SinceBranched | ChangedOnly`); each backend translates it to
  its own CLI. `rev` strings pass through verbatim — the user speaks their
  backend's rev language (`HEAD~1` for git/arc, `-r 42` for svn). `SinceBranched`
  returns `Unsupported` when `!capabilities().merge_base` (svn), with a
  backend-aware message, not a wrong answer.
- `VcsResult` distinguishes **Unsupported** (backend can't) from **Failed**
  (backend could but errored) — so "svn can't merge-base" and "git crashed" are
  never conflated, and neither is ever silently an empty set (H2).
- **`_meta.diff_filter` gains a `resolved: bool`** so a consumer can tell "0
  changed files" from "the filter never ran" (H2 observability).

---

## 4. Detection & override

Resolution order (first match wins), once per invocation:
1. **Explicit override** — `--vcs <git|arc|svn|none>` flag → `VEX_VCS` env →
   `.vex.toml` `vcs = "..."`. `none` disables all VCS features (mtime-only
   staleness, no diff-scope, no history) — the honest floor for unsupported
   environments.
2. **Marker walk** — walk up from the target dir for `.git` / `.svn` / `.arc`.
   Arc caveat: Arc worktrees are frequently **FUSE-mounted** and may not expose
   a plain `.arc` dir; fall back to probing `arc root` (bounded, cached) when
   no marker is found but the `arc` binary exists. **Needs field verification**
   against a real Arc checkout (the `arc` CLI is not on the dev machine).
3. **Default** — `git` if the `git` binary resolves; else `none`.

**Nested git-inside-arc is the COMMON case, not exotic (M3).** Arc monorepos
routinely contain vendored `.git` dirs, and the FUSE mount may not expose `.arc`
at the level where a nested `.git` sits below — so a naive innermost-marker walk
picks the vendored `.git` and indexes a sub-tree as git when the user is in an
Arc repo. Therefore (Phase 3 target): when the `arc` binary is on PATH, **probe
`arc root` before concluding `git`**, and if the `arc` root is an ancestor of
the found `.git`, prefer the outer Arc root. Only genuinely-unrelated innermost
markers (e.g. an svn checkout inside a git repo) fall back to the innermost rule.

> **Phase 2 as-shipped:** the `arc root` probe and the Arc-preference above are
> **deferred to Phase 3** (they only make sense once `ArcVcs` can actually
> diff-scope). Phase 2 detection is **markers-only and git wins a co-located
> tie**, precisely so a git-in-arc monorepo does *not* lose its nested-`.git`
> diff-scoping before `ArcVcs` exists. See §6 Phase 2.

**Detection flip is observable, never a silent reindex (M3).** Detection can
flip across invocations from an innocuous PATH change (the `arc` binary appears
on run 2). The manifest records the detected `vcs_kind`; on mismatch, `staleness`
logs *why* at `warn!` and surfaces it via `_meta.vex.dev/vcs` — it does not
silently trigger a mystery full rebuild.

---

## 5. Cross-cutting changes beyond the trait

- **Manifest `vcs_kind` — an additive `Option` field, NOT a format bump
  (corrected).** `manifest.json` has **no version marker at all** (unlike the
  binary `index.vex` `MAGIC`/`VERSION`); its back-compat mechanism *is* the
  "every field `#[serde(default, skip_serializing_if)]`, no
  `deny_unknown_fields`" invariant. So adding `vcs_kind: Option<String>` is a
  plain additive field, exactly like `embedder_id`/`call_graph`/`bm25` before
  it — no migration mechanism to invent. **Keep the existing `git_head` wire
  key** (reinterpreted as a backend-opaque revision; add
  `#[serde(alias = "git_head")]` if ever renamed) — a straight rename silently
  empties it on every existing index under `serde(default)`. Absent `vcs_kind`
  ⇒ treat as `git` (the only pre-change backend), so existing indexes don't
  spuriously reindex on upgrade.
- **H1 — non-SHA staleness is a silent-wrong-answer trap; gate the shortcut.**
  `staleness::check_git` decides freshness by *string-equality* of saved vs
  current `head_id`. That is correct only when the revision is a content
  fingerprint (git/arc SHA). For svn, `rN` is a global repo counter, and a
  **mixed-revision working copy** can have an unchanged `rN` over a changed tree
  → *Fresh reported for a stale index*. So the equality shortcut is gated on a
  `sha_revisions` capability; non-SHA backends **always** fall through to the
  existing mtime/content deep-check (`check_mtime`, already correct and
  backend-agnostic). Never trust revision equality as "fresh" unless the
  revision fingerprints the tree.
- **Blob cache physical path stays un-namespaced by `vcs_kind` (corrected —
  was the worst idea in the first draft).** `blob_cache_dir` is a *global*,
  SHA-addressed store (`<shared_root>/blobs/<sha>.bin`) shared across projects
  for dedupe. Namespacing its physical layout by `vcs_kind` would silently
  cold-start every warm cache on the machine the first time detection flips
  (git↔arc), trading Phase-14.7's measured −49.8% warm-cache win for nothing —
  and doubling the writer/reader-symmetry incident surface
  (`feedback_cache_path_writer_reader_symmetry`). Since git/arc share the SHA
  space (collision-safe when SHAs agree) and svn never populates the cache
  (`content_addressed=false`, v1 doesn't touch it anyway), **no physical
  namespace is needed.** Only the manifest `vcs_kind` field (a staleness
  gate, not a file-layout axis) is added.
- **`--since-branched` on svn** — the flag stays accepted but returns the
  `Unsupported` error with a backend-aware message ("`--since-branched`
  requires merge-base; not available on svn — use `--since -r<N>`").
- **`_meta.vex.dev/vcs` — emit the declined capabilities too (L3).** Not just
  `git|arc|svn|none` but the capability bits, e.g.
  `{"kind":"svn","merge_base":false}`, so a declined capability (H2/M4) is
  machine-observable in one place. Additive, ungated diagnostic (like
  `semantic_channel`).

---

## 6. Phased rollout (each phase = one reviewed change)

Phases 1–3 are v1 (diff-scope only); 4–5 are the additive growth phases.

**Phase 1 — extract `Vcs` trait + `GitVcs`, DIFF-SCOPE ONLY (pure refactor,
zero behavior change).** Only `util::git_diff` becomes a thin `&dyn Vcs`
caller (`ensure_repo` + `changed_paths`); its single caller is
`cli/common.rs::resolve_diff_filter`. Blob-cache/history/staleness are **not
touched** in v1. **Byte-identical git behavior is the contract**, with two
named caveats the reviews surfaced: (a) preserve `git_diff`'s load-bearing arg
order (`--` after the range, `-z`, `--no-renames`) and the `resolve_merge_base`
candidate ladder + "which refs tried" diagnostic — keep them *inside* `GitVcs`,
don't flatten behind the trait; (b) the trait method returns a structured error
variant, the **caller** composes the final message text, so `git_diff`'s and
`history`'s two *distinct* "not a repo" strings both survive (L1). Narrow fan-in
(one call site), so this is a *local* refactor, not the wide cross-cutting risk
the first draft feared.

**Phase 2 — detection + override + `none`. SHIPPED (2026-07-10) with two
deliberate narrowings vs. the original bullet:**
- Shipped: `--vcs` flag / `VEX_VCS` env / `.vex.toml vcs` override chain
  (flag > env > config > detect > `none`), marker auto-detect, and the `NoVcs`
  floor (git-only in practice; arc/svn/none decline cleanly). `src/vcs/detect.rs`
  + `src/vcs/none.rs`; override precedence covered by `tests/cli_vcs_test.rs`.
- **Deferred to Phase 3/4 (no consumer yet):** `_meta.vex.dev/vcs` and the
  manifest `vcs_kind` field. Emitting either now is write-only scaffolding —
  "which backend answered" is trivially "git" until a non-git backend exists,
  and `vcs_kind` staleness-gating (§5 H1) needs staleness routed through the
  trait (Phase 4). Until `_meta.vex.dev/vcs` lands, a mistyped override warns at
  `tracing::warn!` only; the `--vcs` help documents it affects diff-scope only.
- **Detection is markers-only; git WINS a co-located `.git`/`.arc` tie** — the
  §4 `arc root` probe and outer-Arc-preference are **deferred to Phase 3**.
  Reason: with no Arc *backend* yet, preferring Arc over a nested `.git` would
  *regress* git-in-arc monorepo users (they'd lose the diff-scoping the nested
  `.git` gives them today). The probe lands together with `ArcVcs`. **No
  physical cache namespacing** (§5).

**Phase 3 — `ArcVcs` (diff-scope). SHIPPED PROVISIONAL (2026-07-10), research-
grounded, UNVERIFIED against a real `arc`.** `arc` was not available on the dev
machine, so instead of a field capture (R2) the command shapes were grounded in
public research (third-party arc clients EVGVir/yandex-arc, anton-rudeshko/zsh-arc;
Yandex Habr writeup). `src/vcs/arc.rs` (`ArcVcs`), reachable via explicit
`--vcs arc` / `VEX_VCS=arc` / `.vex.toml vcs="arc"` / `.arc` marker. The `arc root`
FUSE **auto-probe stays deferred** (unverifiable + adds VFS latency to every
`arc`-on-PATH run); explicit selection is the entry point. Testable without
`arc`: the `arc status --json` parser (unit) + graceful-failure-when-arc-absent
(integration). **Command shapes MUST be field-verified before trusting** — see
the checklist in §7a.

**Phase 4 — `SvnVcs` (diff-scope).** `merge_base=false` (SinceBranched
declined, §5); `svn status` / `svn diff --summarize -r`; integer-revision
`head_id`; non-SHA staleness falls to mtime (H1). Document in `LIMITATIONS.md`.

**Phase 5+ (growth, additive) — widen the trait** to the other subsystems once
Arc CLI is field-verified: `tracked_content_ids` (blob cache; git/arc only,
preserving the `ls-files` + `diff-files` dirty-exclusion two-step, M1),
`file_history` (svn returns `partial: true` + `_meta rename_follow:false`, M4),
`dirty_count`/`head_id` with the `deep`-flag shortcut preserved (staleness
must not call `dirty_count` when `deep==false`). Each is its own reviewed change.

---

## 7. Risks & open questions

- **R1 (med, downgraded):** Phase-1 extraction. Both reviews confirmed the
  fan-in is *narrow* (diff-scope has one caller, `resolve_diff_filter`; nothing
  runs inside a rayon closure), so this is a local mechanical move, not a wide
  cross-cutting rewrite. Residual risk is behavior drift in `git_diff`'s arg
  order / merge-base ladder / the two distinct error strings — mitigated by the
  byte-identical contract + existing tests (`non_git_repo_errors_actionably`,
  `since_finds_files_modified_in_head`, `path_with_space_is_handled`) and a
  literal, line-reviewed move.
- **R2 (high, on critical path for Phase 3):** the `arc` CLI surface is
  unverified on this machine. Phase 3 must open with a field-capture of real
  `arc status / diff --summarize / rev-parse` output. If `arc` diverges from
  git shapes, `ArcVcs` needs its own parsers, not git reuse.
- **R3 (med, growth-phase):** svn `SinceBranched` is hard-`Unsupported` (§5);
  the *rename-follow* gap is deferred to Phase 5+ and resolves to
  degraded-with-loud-signal (`partial:true` + `_meta`), never silent (M4).
- **R4 (resolved — not a format bump):** `manifest.json` has no version marker;
  `vcs_kind` is a plain additive `Option` field per the existing convention,
  `git_head` key kept (§5). No migration to invent.
- **R5 (low):** nested markers — the **common** git-inside-arc case is handled
  by the `arc root`-before-git rule (§4, M3); genuinely-unrelated innermost
  markers (svn-in-git) fall to innermost. Both need a detection test.

---

## 7a. Arc CLI field-verify checklist (Phase 3)

`ArcVcs` (`src/vcs/arc.rs`) ships against these research-grounded shapes. Run
each against a real `arc` install (`arc <cmd> --help` + a live invocation) and
correct `arc.rs` where reality diverges. Confidence from the public research:

| Op | Shape used | Confidence | To verify |
|---|---|---|---|
| detect / `ensure_repo` | `arc root` (exit-code + path) | high | that non-zero exit outside a working copy; no `.arc` on-disk marker on FUSE mounts |
| changed since rev | `arc diff <from> <to> --name-only --no-color` | high (cmd), med (`<from> <to>` two-arg) | `..` range vs two-arg; `--` terminator; **`-z` support** (we newline-split — no `-z` attested) |
| working tree | `arc status --json` → `status.{changed,staged,untracked}[].path` | high | exact JSON shape; that untracked is included (no separate `ls-files --others`) |
| since-branched | `arc merge-base --leftmost <ref> HEAD`, ladder `arcadia/trunk` → `trunk` | high (merge-base, trunk name) | arg order; whether `--leftmost` is required; ladder completeness |
| revision id | *(not used in diff-scope)* | — | `arc rev-parse` likely absent; use `arc info --json` if a rev id is ever needed (Phase 5) |

**Known-unverifiable (Arc is Yandex-internal):** `-z`/`--` on diff, `--others`
on `ls-files`, `arc rev-parse`, git-SHA-1 compatibility of Arc commit hashes.
Prefer `--json` (Arc's stable machine contract) over porcelain where a choice
exists. Once verified, drop the `PROVISIONAL` banner in `arc.rs` and flip on the
`arc root` auto-probe in detection (§4).

---

## 8. References

- `src/util/git_diff.rs` — `DiffScope`, `ChangedPaths::resolve` (§1 diff group)
- `src/index/parse_cache/git_blobs.rs` — `discover_tracked_blobs` (content cache)
- `src/history/mod.rs` — `find_symbol_history` (history walker)
- `src/index/staleness.rs` — `read_git_head`, dirty-count deep check
- `src/index/manifest.rs` — stored HEAD (§5 opaque-revision change)
- `reference_phase_14_7_blob_cache`, `reference_history_walker`,
  `reference_phase_14_8_history_index` — the git-coupled subsystems
- `docs/LIMITATIONS.md` — where svn capability gaps get documented
