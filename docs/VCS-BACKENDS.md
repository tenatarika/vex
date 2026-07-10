# VCS Backends — Design (git · Arc · svn)

Status: **Phases 1-4 SHIPPED + Phase 5A SHIPPED** (2026-07-10) — `Vcs` trait +
`GitVcs` extraction (Phase 1), detection/override/`none` floor (Phase 2),
field-verified `ArcVcs` (Phase 3) and `SvnVcs` (Phase 4), and the blob-SHA parse
cache routed through the trait via `tracked_content_ids` (Phase 5A). **The rest
of Phase 5 is DESIGN/growth** (§6): `file_history` and `dirty_count`/`head_id`.
The plan abstracts vex's hard git dependency behind a `Vcs` trait so it also
runs against **Yandex Arc** and **Subversion**, with git as the default and
byte-identical current behavior preserved. Each phase is a separate reviewed
change; **history and staleness remain git-only** until the rest of Phase 5.

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
  `Unsupported` error with a backend-aware message. (Design sketch; the
  as-shipped wording lives in `SvnVcs::SINCE_BRANCHED_MSG` and redirects to
  `--since <rev>`, e.g. `--since 42`.)
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

**Phase 3 — `ArcVcs` (diff-scope). FIELD-VERIFIED (2026-07-10) against a real
`arc` install** (an `arcadia` working copy; capture log in §7a). Shipped
provisional 2026-07-10 (research-grounded), then verified the same day when a
real `arc` capture arrived — the `PROVISIONAL` banner and the runtime
`tracing::warn!` are dropped. `src/vcs/arc.rs` (`ArcVcs`), reachable via explicit
`--vcs arc` / `VEX_VCS=arc` / `.vex.toml vcs="arc"` / `.arc` marker. The `arc root`
FUSE **auto-probe stays deferred** (adds VFS latency to every `arc`-on-PATH run);
explicit selection is the entry point. Testable without `arc`: the `arc status
--json` parser + `reject_flaglike_rev` (unit) + graceful-failure-when-arc-absent
(integration). Verified command shapes — see §7a.

**Phase 4 — `SvnVcs` (diff-scope). FIELD-VERIFIED (2026-07-10) against a real
`svn` 1.14 working copy.** Unlike Arc, `svn` is open-source and installable
(`brew install subversion`), so it was verified from the start (no provisional
stage): a throwaway `svnadmin` repo captured every command shape, and a
skip-if-absent end-to-end test (`svn::tests::end_to_end_against_real_svn`)
exercises the live trait impl. `src/vcs/svn.rs` (`SvnVcs`). Verified shapes:
`svn info` (detect — `E155007` outside a working copy); `svn diff --summarize
--xml -r <rev>:HEAD` (`Since`); `svn status --xml` (`ChangedOnly` — local mods +
unversioned in one offline call). `merge_base=false` → `SinceBranched` returns
`Unsupported` with an actionable `--since` redirect (§5). **XML, not porcelain**
— svn's stable machine contract (like Arc's `--json`), parsed via `quick-xml`
(already in the tree transitively); porcelain's fixed-column layout breaks on
paths with spaces (field-verified: `src/with space.rs`). Blob-cache / history /
staleness stay git-only (svn has no blob store; non-SHA staleness → mtime deep
check, H1). Documented in `LIMITATIONS.md`.

**Phase 5A — `tracked_content_ids` (blob cache). SHIPPED (2026-07-10).** The
Phase-14.7 blob-SHA parse cache is routed through the trait: `GitVcs` implements
`tracked_content_ids` (the `ls-files -s` + `diff-files` dirty-exclusion two-step,
M1, moved byte-identically from `index::parse_cache::git_blobs`), and
`index::parse_cache::git_blobs::discover_tracked_blobs` is now a thin wrapper
that maps a declined/failed result to an **empty map** (INVERTED H2 — a cache
optimization, not a load-bearing "nothing changed" answer). `content_addressed`
capability bit: git=true; **arc=false for now** (arc blob SHAs are git-compatible
but `arc ls-files` is not yet field-verified — flip when it is); svn/none=false
(no content-addressed store). Consequence: forcing `--vcs none/arc/svn` during
indexing disables the git blob cache and falls back to xxh3/mtime (correct, just
slower) — the intended trait semantic.

**Phase 5B+ (growth, remaining) — widen the trait** to the other subsystems:
`file_history` (svn returns `partial: true` + `_meta rename_follow:false`, M4;
NB svn has no server-side grep across history, so the query-time walker does not
port cleanly), `dirty_count`/`head_id` with the `deep`-flag shortcut preserved
(staleness must not call `dirty_count` when `deep==false`; non-SHA backends gate
off the equality shortcut, H1). Each is its own reviewed change.

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
- **R2 (RESOLVED 2026-07-10):** the `arc` CLI surface was unverified when
  Phase 3 shipped; a real-`arc` field capture (§7a) confirmed `arc root`, `arc
  diff <a> <b> --name-only --no-color`, `arc diff -B`, and `arc status --json`,
  and refuted the `--` terminator assumption (dropped). `ArcVcs` uses its own
  arc invocations, not git reuse.
- **R3 (Phase-4 part SHIPPED; rename-follow deferred):** svn `SinceBranched` is
  hard-`Unsupported` (§5) — shipped and verified in Phase 4. The *rename-follow*
  gap stays deferred to Phase 5+ (history, not diff-scope) and resolves to
  degraded-with-loud-signal (`partial:true` + `_meta`), never silent (M4).
- **R4 (resolved — not a format bump):** `manifest.json` has no version marker;
  `vcs_kind` is a plain additive `Option` field per the existing convention,
  `git_head` key kept (§5). No migration to invent.
- **R5 (low):** nested markers — the **common** git-inside-arc case is handled
  by the `arc root`-before-git rule (§4, M3); genuinely-unrelated innermost
  markers (svn-in-git) fall to innermost. Both need a detection test.

---

## 7a. Arc CLI field-verify log (Phase 3 — VERIFIED 2026-07-10)

Verified against a real `arc` install (an `arcadia` working copy) via
`arc <cmd> --help` + live invocations. Result per op:

| Op | Shape used | Verdict |
|---|---|---|
| detect / `ensure_repo` | `arc root` (exit-code + path) | ✅ `arc root` → `/Users/…/arcadia` (prints working-copy root) |
| changed since rev | `arc diff <from> <to> --name-only --no-color` | ✅ two-arg rev form + `--name-only` + `--no-color` all in `arc diff --help`; `arc diff trunk HEAD --name-only` → newline-separated **repo-root-relative** paths |
| since-branched | `arc diff -B --name-only --no-color` | ✅ **pivoted to `-B`** — help: "`-B` show changes between merge-base(FROM_ID, TO_ID) and TO_ID. Default FROM_ID=trunk, TO_ID=HEAD". One command, replaces the merge-base ladder |
| working tree | `arc status --json` → `status.{changed,staged,untracked}[].path` | ✅ exact shape confirmed; untracked included in `status` (no `ls-files --others`); each entry `{status,type,path}` |
| merge-base (capability) | `arc merge-base --leftmost trunk HEAD` | ✅ returns a SHA — capability truthful, but `SinceBranched` uses `-B` instead |
| revision id | *(not used in diff-scope)* | — deferred to Phase 5 |

**Corrections applied from the capture:**
- **`--` terminator dropped.** `arc diff --help` documents no end-of-options
  `--`; free args are "Commit, branch or path". The git backend's `--`
  flag-injection guard is replaced by `reject_flaglike_rev` (refuse a rev
  starting with `-`).
- **`-z` absent** (not in `arc diff --help`) → newline-split confirmed correct.
- **`SinceBranched` → `arc diff -B`** — drops the unverified `arcadia/trunk`
  ladder candidate and the two-step `merge-base` + `diff`.

**Still unverified (low-priority residual):** whether `arc diff --name-only`
quotes paths containing spaces/newlines (git octal-escapes without `-z`; Arc has
no `-z`) — see the residual-gaps note below. The `arc root` auto-probe in
detection (§4) stays deferred by design (VFS latency), independent of this
verification.

### Graceful degradation (so a wrong assumption fails safe, not silent)

Retained after verification: `ArcVcs` is built to **fail loud, never silently
mislead** — the H2 principle extended to the "successful call, wrong output
shape" case that plain error-handling misses (guards a future `arc` whose JSON
shape drifts):

- **Unrecognized `arc status --json` → hard error, not empty.** A real `arc`
  emitting a different JSON shape would otherwise parse to an empty change set
  and silently report "nothing changed" (dropping every `--changed-only`
  result — the delete-safety footgun). `parse_arc_status_json` requires a
  recognizable envelope (`status` object or a known group) and `bail!`s
  otherwise. An empty set is returned only from a *recognized* shape.
- **Spawn failure / non-zero exit / bad flag → `VcsError::Failed`**, propagated
  as a clear command error — never mapped to an empty set (H2), never a panic.
- **No silent backend substitution.** An arc failure does NOT fall back to git
  or to "ignore `--since`": the user asked for arc, and a different backend
  would give a *different* changed-set. Failing loudly is correct for a
  scoping/safety feature.
- **Leading-`-` revision rejected.** `reject_flaglike_rev` refuses a `--since`
  value beginning with `-` (arc has no `--` terminator), so it can never be
  smuggled in as an arc flag.

**Residual gaps (post-verification):**
- **Path quoting untested.** `arc diff --name-only` emitted bare, root-relative
  paths in the capture, but no path contained a space or newline. Git
  octal-escapes such paths when `-z` is absent; whether Arc does the same (it
  has no `-z`) is unconfirmed. A path with special chars could parse wrong.
- **Subprocess timeout — RESOLVED.** Arc's FUSE/VFS mount can be slow or hang;
  every `arc` (and `svn`) invocation now runs under a bounded wall-clock timeout
  (shared `vcs::proc::wait_capturing` / `vcs_timeout`, default 60s, override
  `VEX_VCS_TIMEOUT_SECS`). Pure std (poll `try_wait` + thread-drained pipes +
  kill/reap); no `wait-timeout` crate (it's dev-only via `assert_cmd`).

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
