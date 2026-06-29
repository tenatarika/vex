# Multi-repo Phase 2 — de-globalize the cache override (CacheResolver)

Status: **SHIPPED** (2026-06-29). Implements `docs/MULTIREPO.md` §6.1 / §8
phase 2. A workspace member keeps its own `cache_dir` / `local_cache`
(`.vex.toml`) instead of being rejected at load. No on-disk format change.
All §9 review resolutions folded in; 3036/3036 nextest, clippy + stable-fmt
clean. (TOCTOU on the double `find_and_load` accepted for MVP — see §9 /
LIMITATIONS §7.)

## 1. Problem

`src/util/config.rs` holds a process-global `CACHE_OVERRIDE:
OnceLock<CacheLayout>`, installed once at CLI startup (`src/cli/mod.rs:61`)
from the cwd/root config. 13 cache-path functions read it:

- **Direct:** `index_dir(root)`, `embed_cache_dir()`, `blob_cache_dir()`.
- **Indirect (call `index_dir`):** `index_path`, `hnsw_path`,
  `hash_index_path`, `body_tokens_path`, `bloom_path`, `git_history_path`,
  `rename_chains_path`, `manifest_path`, `state_path`, `embed_cache_path`.

Because the layout is single-valued per process, a workspace fanout cannot
give member A a `local_cache` layout and member B the platform layout — so
`Workspace::load` rejects any member whose own `.vex.toml` sets
`cache_dir`/`local_cache` (`workspace/mod.rs::member_sets_cache_override`).

## 2. Chosen realization — global-backed `CacheResolver`, set once

The user chose the full §6.1 `CacheResolver` (not the per-member
mutable-swap). We realize it as a **global-backed, immutable-after-install**
resolver rather than threading `&CacheResolver` through all 66 call sites.
Rationale: the 13 helpers keep their current signatures (`index_dir(root)`
etc.) — only their *bodies* consult the resolver — so the change is confined
to `config.rs` + the single install point + the workspace reject removal,
not a 66-site / 26-file param-threading diff (itself a large, risky change).
The resolver is **set exactly once** per process and **read-only** after, so
there is NO mutable-global ordering hazard (the rejected option A).

```rust
pub struct CacheResolver {
    /// Anchor for repo-AGNOSTIC caches (model weights, blob SHA cache).
    /// Single-repo: the project's resolved cache root. Workspace: the
    /// workspace root's resolved cache root. Keying these per-member would
    /// duplicate ~86 MB weights / the blob cache N× (§6.1).
    shared_root: PathBuf,
    /// Per-CANONICAL-root cache layout. Workspace: one entry per member.
    /// Single-repo: empty (everything uses `default`).
    members: HashMap<PathBuf, CacheLayout>,
    /// Layout for `index_dir(root)` when `root` ∉ `members`. Single-repo:
    /// the project's resolved layout (reproduces today's OnceLock exactly).
    /// Workspace: the platform default (hashed) — platform-cache members
    /// fall through here, so the map only needs custom-cache members.
    default: CacheLayout,
}

impl CacheResolver {
    fn layout_for(&self, root: &Path) -> &CacheLayout {
        self.members.get(root).unwrap_or(&self.default)
    }
}

static CACHE_RESOLVER: OnceLock<CacheResolver> = OnceLock::new();
```

Helper bodies become:

```rust
pub fn index_dir(root: &Path) -> PathBuf {
    let layout = resolver().layout_for(root);   // resolver() → installed or process default
    if layout.skip_hash_subdir { layout.root.clone() }
    else { layout.root.join(format!("{:016x}", xxh3_64(root.to_string_lossy().as_bytes()))) }
}
pub fn embed_cache_dir() -> PathBuf { resolver().shared_root.join("embeddings") }
pub fn blob_cache_dir()  -> PathBuf { resolver().shared_root.join("blobs") }
```

`resolver()` returns the installed `CacheResolver` or, if none installed
(library/test calls before install), a process-default resolver
(`shared_root = default_cache_root()`, `default = {platform, hashed}`,
empty members) — byte-identical to today's no-override path.

## 3. Install strategy — one decision point in `cli/mod.rs`

`set_cache_override` is called once at `cli/mod.rs:49-79`, BEFORE dispatch,
and `cli.command` (with each subcommand's `--workspace` flag) is already
parsed there. So the resolver is built up-front, in one place:

```
let workspace_mode = extract_workspace_flag(&cli.command);   // new, mirrors extract_path_hint
if workspace_mode {
    if let Ok(ws) = Workspace::find_and_load(&config_root) {
        // shared_root = cache root resolved from config at ws.base()
        // members = { m.root (canonical) → resolve_cache_root(cli, member_cfg).into_layout() }
        // default = { platform default, hashed }
        install_workspace_resolver(ws, cli.cache_dir);
    } else {
        set_cache_override(resolved_cache.root, resolved_cache.skip_hash_subdir); // command bails later
    }
} else {
    set_cache_override(resolved_cache.root, resolved_cache.skip_hash_subdir);     // single-repo (today)
}
```

- `set_cache_override(root, skip)` is KEPT (back-compat for the 5
  test/bench/example callers + single-repo): it installs a single-layout
  resolver (`default = {root, skip}`, `shared_root = root`, empty members).
- `install_workspace_resolver` is new and installs the multi-member resolver.
- One `OnceLock` set per process → no re-set, no mutable hazard.

## 4. Canonicalization symmetry (the load-bearing invariant)

`members` is keyed by **canonical** root. `layout_for(root)` does a plain
`members.get(root)`. A non-canonical lookup misses → falls back to `default`.
This is SAFE because:

- Workspace `Member.root` is canonical by construction
  (`workspace/mod.rs:140` `joined.canonicalize()`), and every workspace
  fanout passes `&m.root` (Explore-confirmed: cmd_index/usages/update/search/
  callgraph all pass `m.root`).
- Single-repo `cmd_*` handlers canonicalize before `ensure_index_ready`
  (Explore-confirmed table), and single-repo `members` is empty anyway.
- A miss → `default` reproduces TODAY's behavior (platform root, hashed by
  the passed string) — correct for any platform-cache root. A miss is only
  *wrong* for a custom-cache member looked up by a non-canonical root, which
  cannot happen given canonical `Member.root`.

We do NOT add a runtime `canonicalize()` in `layout_for` (masks bugs, costs
a syscall). Instead: a unit test pins that a workspace member with
`local_cache` resolves to its in-tree dir via its canonical root, and an
integration test indexes+queries such a member. Reference incident:
[[feedback_cache_path_writer_reader_symmetry]] (Phase 14.8 Step 7).

## 5. `local_cache_active` flag

`cli/mod.rs` derives `ctx.local_cache_active = resolved_cache.skip_hash_subdir`
from the top-level config and the workspace commands `bail!` on it
("members would collide into one index dir"). With per-member layouts:

- The TOP-LEVEL flag now reflects the workspace ROOT's config (usually
  platform, `skip = false`). The blanket `bail!` in the workspace commands
  is REMOVED — per-member `local_cache` is now the supported case.
- Collision safety is preserved by the resolver: each member's layout is
  resolved from ITS OWN `source_dir`-anchored config, so two `local_cache`
  members land in their own in-tree `.vex_cache/` dirs (distinct roots), and
  platform members hash by canonical root. No member can alias another.

## 6. Remove the reject

Delete `workspace/mod.rs::member_sets_cache_override` and its call in
`resolve_member` (+ the rejection test). A member's `.vex.toml`
`cache_dir`/`local_cache` is now honoured.

## 7. Tests
1. `config.rs` unit: a `CacheResolver` with a `local_cache` member returns
   the in-tree dir for that member's canonical root and the platform-hashed
   dir for a platform member; `embed_cache_dir`/`blob_cache_dir` return the
   `shared_root` regardless of member.
2. `config.rs` unit: `set_cache_override` shim still yields today's
   single-layout behavior (`index_dir(anyRoot)` uses the one layout).
3. workspace unit: `Workspace::load` no longer rejects a member with
   `cache_dir`/`local_cache`.
4. e2e: a 2-member workspace where member B sets `local_cache = true`;
   `vex index --workspace` writes B's index in-tree
   (`B/.vex_cache/index.vex`) and A's under the platform hash; a query
   resolves both.

## 8. Risks
- **Install timing in `cli/mod.rs`** — building the resolver up-front means
  `cli/mod.rs` loads the workspace manifest (a second `find_and_load`; the
  command does its own). Cheap, but layering: `cli/mod.rs` becomes
  workspace-aware. Alternative (per-command install) is a footgun (forget
  one command → silent fallback). Up-front wins.
- **Canonicalization** — §4. The single biggest hazard; covered by the
  canonical-`Member.root` invariant + tests, NOT a runtime canonicalize.
- **`shared_root` for blob/embed under a `local_cache` workspace member** —
  intentionally NOT per-member: weights/blobs anchor to `shared_root`
  (workspace root). A member that wants portable weights is an explicit
  non-goal here (it gets the workspace-shared model, which is the right
  default for N members on one machine).

## 9. Review resolutions (architect + rust-reviewer, locked before scaffold)

**HIGH — per-member `local_cache_active`; the `.gitignore` leak (architect).**
The workspace fanout hard-codes `local_cache_active = false` into
`ensure_index_ready` (cmd_usages.rs:484, cmd_search.rs:433) and `run_for_root`
(cmd_index.rs:237). After Phase 2 a member CAN be `local_cache`, and
`ensure_index_exists`/`run_for_root` gate the `*` `.gitignore` write +
`create_dir_all` on that flag — so a `local_cache` member's in-tree
`.vex_cache/` would get NO `.gitignore` → committable cache leak. FIX: derive
per-member `local_cache_active = resolve_cache_root(None, &member_cfg)
.skip_hash_subdir` in the fanout and thread THAT (not the literal `false`)
into both `run_for_root` and `ensure_index_ready`. Add an e2e assert that
`B/.vex_cache/.gitignore` exists after `vex index --workspace`.

**HIGH — `shared_root` in the single-layout shim = the passed `root`, NOT
`default_cache_root()` (rust M4).** Otherwise a single-repo `local_cache`
user's `blob_cache_dir`/`embed_cache_dir` silently move from
`<project>/.vex_cache/{blobs,embeddings}` to the platform default — a
back-compat break. `set_cache_override(path, skip)` must build
`CacheResolver { shared_root: path.clone(), default: {path, skip}, members:
{} }`. Add a unit test pinning `blob_cache_dir()`/`embed_cache_dir()` under a
`local_cache` shim install.

**HIGH — `resolver()` `&'static` lifetime shape (rust H1).** `layout_for`
returns `&CacheLayout` borrowed from the resolver, so the resolver must be
`'static`. Shape: `static CACHE_RESOLVER: OnceLock<CacheResolver>` +
`static DEFAULT_RESOLVER: LazyLock<CacheResolver>` (platform default, hashed,
empty members). `fn resolver() -> &'static CacheResolver {
CACHE_RESOLVER.get().unwrap_or(&DEFAULT_RESOLVER) }`. NO `unwrap_or_default()`
(returns a temporary — cannot borrow `'static`).

**CRITICAL (test design) — OnceLock cannot reset between unit tests in one
binary (rust C1/H4/M5).** Any test that INSTALLS a resolver must be a separate
integration-test process. §7 items 3-4 (install + e2e) → `tests/` integration
files (workspace_index_test.rs is per-process per assert_cmd subprocess →
clean). §7 items 1-2 must NOT call the global installer: test `CacheResolver`
constructors + `layout_for`/`shared_root` as PURE methods on a locally-built
value (no global), so they stay `config.rs` unit tests without contaminating
the OnceLock. `set_cache_override` shim back-compat is exercised by the
existing `tests/parse_cache_pipeline_test.rs` / bench callers (separate
processes), not a new config.rs unit test.

**HIGH — keep ONE global, migrate cleanly (architect).** Replace
`CACHE_OVERRIDE: OnceLock<CacheLayout>` with `CACHE_RESOLVER:
OnceLock<CacheResolver>`. `set_cache_override` is KEPT as a shim writing the
NEW lock (single-layout resolver), so the 5 test/bench/example callers
(output.rs:2079, benches/perf_v113.rs:250, benches/bundle.rs:152,
examples/phase148_bench.rs:92, tests/parse_cache_pipeline_test.rs:41) need NO
change. Do NOT leave the old lock alongside (dead global).

**MEDIUM — canonicalize-idempotence is the real invariant (architect).** The
write path re-canonicalizes at `pipeline/mod.rs:243`, so the resolver key is
`m.root.canonicalize()`, which equals `m.root` only by idempotence (member
roots are already canonical, `workspace/mod.rs:140`). §4 holds, but add a unit
test `index_dir(m.root) == index_dir(m.root.canonicalize())` for a
`local_cache` member, and a `debug_assert!` in `CacheResolver::workspace` that
every `members` key equals its own `canonicalize()` (defense vs a future
non-canonical insert — the cited Phase 14.8 incident class).

**MEDIUM — TOCTOU on the double `find_and_load` (both).** cli/mod.rs builds
the resolver from load #1; the command iterates load #2. A member added
between loads → `layout_for` miss → `default`. Wrong dir only for a *newly
added local_cache* member in that race window. Accepted for MVP (documented);
the clean fix (thread the built `Workspace` from cli/mod.rs into the command,
killing both the double-load and the window) is deferred — note in LIMITATIONS.

**MEDIUM — private fields + constructors (rust M1).** `CacheResolver` fields
private; `CacheResolver::single(root, skip)` and `CacheResolver::workspace(
shared_root, members, default)` constructors. `layout_for` + accessors are the
only read API. Keep `CacheLayout` module-private.

**MEDIUM — `local_cache_active` guard catalogue (rust C2/M3).** The blanket
workspace `bail!(local_cache_active)` in cmd_index/update/search/check/
callgraph/usages/impact is REMOVED (per-member layouts make collision
impossible — `reject_overlaps` already forbids aliasing). The field stays in
`CmdCtx` for the single-repo `.gitignore` path (still valid). Document that in
workspace mode the top-level flag is workspace-root-derived (≈ always false)
and per-member skip-hash is derived in the fanout.
