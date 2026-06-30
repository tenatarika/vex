use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;
use std::path::PathBuf;

/// Per-query path scope filters — `--include <glob>` and `--exclude <glob>`,
/// both repeatable. Flatten into every search-shaped subcommand for a
/// consistent UX.
#[derive(Args, Clone, Debug, Default)]
pub struct ScopeArgs {
    /// Whitelist results by path glob (repeatable, case-sensitive). Example:
    /// `--include 'tests/**' --include 'crates/**'`.
    #[arg(long, value_name = "GLOB")]
    pub include: Vec<String>,

    /// Blacklist results by path glob (repeatable, case-sensitive).
    /// Wins over `--include`. Example: `--exclude '**/*.gen.*'`.
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,
}

/// Diff-context filters (Phase 13.7-D3). Restrict search-shaped commands
/// to the set of files affected by a git ref-range or working-tree state.
/// The three variants are mutually exclusive; default (no flag set) means
/// "no diff filter — return everything".
///
/// Empirical anchor: rtk-ai reports -75% PR-review token spend when the
/// agent filters its `git diff` to changed files only — this is the same
/// idea applied to vex's symbol/usages/pattern/graph queries.
#[derive(Args, Clone, Debug, Default)]
pub struct DiffFilterArgs {
    /// Restrict results to symbols in files changed between `<rev>..HEAD`.
    /// Accepts any revision spec `git diff` understands (`main`, `HEAD~3`,
    /// `origin/main`, a SHA, …).
    #[arg(long, value_name = "REV", conflicts_with_all = ["since_branched", "changed_only"])]
    pub since: Option<String>,

    /// Restrict results to symbols in files changed since this branch
    /// diverged from main/master. Tries `origin/main`, `origin/master`,
    /// `main`, then `master` in that order.
    #[arg(long, conflicts_with_all = ["since", "changed_only"])]
    pub since_branched: bool,

    /// Restrict results to symbols in working-tree changes (staged +
    /// unstaged + untracked, gitignore-respecting).
    #[arg(long, conflicts_with_all = ["since", "since_branched"])]
    pub changed_only: bool,
}

impl DiffFilterArgs {
    /// True when any of the three diff scopes is requested. Currently
    /// consumed by tests + downstream MCP wrappers — keep `pub` so the
    /// API stays stable even if the local dispatch path inlines the
    /// equivalent `scope().is_some()` check.
    #[allow(dead_code)]
    pub fn has_scope(&self) -> bool {
        self.since.is_some() || self.since_branched || self.changed_only
    }

    /// Resolve the active scope into a borrowed enum. Returns `None` when
    /// no diff filter is requested — callers short-circuit the `git`
    /// invocation in that case.
    pub fn scope(&self) -> Option<crate::util::git_diff::DiffScope<'_>> {
        if let Some(rev) = self.since.as_deref() {
            return Some(crate::util::git_diff::DiffScope::Since(rev));
        }
        if self.since_branched {
            return Some(crate::util::git_diff::DiffScope::SinceBranched);
        }
        if self.changed_only {
            return Some(crate::util::git_diff::DiffScope::ChangedOnly);
        }
        None
    }
}

/// Symbol metadata filters (11.6). Post-filter that narrows results
/// by lexical inspection of each symbol's captured signature line —
/// no format bump, no re-parsing.
#[derive(Args, Clone, Debug, Default)]
pub struct MetadataArgs {
    /// Keep only symbols whose signature contains an explicit
    /// visibility keyword. Aliases: `pub` / `priv`. Default-visibility
    /// (Rust private, TS class-member public) is not inferred; only
    /// explicit keywords match.
    #[arg(long, value_name = "VIS")]
    pub visibility: Option<String>,

    /// Keep only async / suspend functions.
    #[arg(long)]
    pub async_only: bool,

    /// Exclude async / suspend functions (mutually exclusive with `--async-only`).
    #[arg(long, conflicts_with = "async_only")]
    pub no_async: bool,

    /// Keep only static class members.
    #[arg(long)]
    pub static_only: bool,

    /// Keep only sealed (or Java-`final`) types.
    #[arg(long)]
    pub sealed_only: bool,
}

/// v1.12.0 S8.4 — `env!("VEX_VERSION")` would refuse to compile when the
/// build.rs that defines the var hasn't run (vex consumed as a git
/// dependency from a stripped workspace, IDE rust-analyzer in some
/// configurations, doc-tests from another crate). `option_env!` makes
/// the var optional and falls back to Cargo's always-defined
/// `CARGO_PKG_VERSION`. Production builds keep printing the git-describe
/// string set by `build.rs`; the fallback only fires in degraded build
/// environments where `--version` formerly broke the whole binary.
const VEX_VERSION: &str = match option_env!("VEX_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(
    name = "vex",
    version = VEX_VERSION,
    about = "Fast hybrid structural + semantic code search"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Output format (overrides .vex.toml)
    #[arg(long, global = true)]
    pub format: Option<OutputFormat>,

    /// Override the cache root for the index (overrides .vex.toml and $VEX_CACHE_DIR).
    /// Accepts absolute paths, `~/...`, or paths relative to the current directory.
    #[arg(long, global = true, value_name = "PATH")]
    pub cache_dir: Option<PathBuf>,
}

#[derive(Copy, Clone, clap::ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    /// Compact single-line output, optimized for LLM token efficiency
    Compact,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Build index for a project directory
    Index {
        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Generate semantic embeddings (slower but enables semantic search)
        #[arg(long)]
        semantic: bool,

        /// Disable semantic embeddings for this rebuild (overrides .vex.toml).
        /// Existing HNSW + embed cache are preserved on disk so a future
        /// `--semantic` build can reuse them; semantic search falls back to
        /// brute-force on size mismatch in the meantime. Pass `--drop-semantic`
        /// to also delete them.
        #[arg(long, conflicts_with = "semantic")]
        no_semantic: bool,

        /// v1.15.1: explicitly delete the on-disk semantic channel
        /// (HNSW + hash-index sidecar + embedder cache). Without this
        /// `--no-semantic` only skips the rebuild; with it, all semantic
        /// artifacts are wiped and the next `--semantic` build re-embeds
        /// from scratch. Requires `--no-semantic`.
        #[arg(long, requires = "no_semantic")]
        drop_semantic: bool,

        /// Embedder ID for semantic indexing (default: minilm-l6-v2)
        #[arg(long)]
        embedder: Option<String>,

        /// Worker threads for parallel indexing. Default = 80% of cores, rounded up.
        /// Pass `0` to use all cores; pass N for exactly N workers.
        #[arg(short = 'j', long)]
        jobs: Option<usize>,

        /// Skip the persistent call-graph section. `vex callers`/`vex callees`
        /// will fall back to live-scan. Persisted in the manifest so `vex
        /// update` honours the opt-out across incremental rebuilds.
        #[arg(long)]
        no_call_graph: bool,

        /// Skip the BM25 channel. Hybrid search drops the third RRF channel
        /// and uses structural (+ semantic if enabled). Persisted in the
        /// manifest like `--no-call-graph`.
        #[arg(long)]
        no_bm25: bool,

        /// Skip the v6 pattern-skeleton side-section (11.4). `vex pattern`
        /// keeps using live-scan instead of the indexed prefilter. Same
        /// sticky-opt-out semantics as `--no-call-graph` / `--no-bm25`.
        #[arg(long)]
        no_pattern_index: bool,

        /// Exit immediately with a "busy" status if another vex instance
        /// is currently building the same index, instead of waiting for
        /// it to finish. Useful for editor integrations and CI cron jobs
        /// that would rather no-op than wedge for a peer's parse + embed.
        /// Text mode prints a "Skipped" line and exits 0; JSON mode emits
        /// `{"status":"busy",...}` so scripts can branch on outcome.
        #[arg(long)]
        no_wait: bool,

        /// Build the Phase 14.8 `git_history` section. Walks the repo's
        /// commit history, dedups blobs by SHA, parses each unique
        /// (path, blob) pair through the 14.7 cache, and persists a
        /// per-symbol historical index so `vex history <Symbol>` runs in
        /// FST-lookup time (~ms) instead of shelling out to `git log` per
        /// query (~seconds). Opt-in — adds 5-30s to cold index and ~10%
        /// to index.vex size on long-lived repos.
        ///
        /// Phase 14.8 Step 3 scaffold — flag is parsed but the builder
        /// lands in Step 4b. Today `--history` is a no-op.
        #[arg(long)]
        history: bool,

        /// Cap history walk at N newest commits (global, NOT per-file).
        /// Mirrors `git log -nN` semantics. Implies `--history`. Bump
        /// down on multi-thousand-commit repos to keep build time in
        /// check; unbounded by default (full history reachable from
        /// `HEAD`).
        #[arg(long, value_name = "N", requires = "history")]
        history_depth: Option<usize>,

        /// Use the GPU for embedding generation, if this vex build was compiled
        /// with a gpu-* feature (DirectML on Windows / CoreML on macOS prebuilts;
        /// CUDA via source build). Falls back to CPU if no GPU is usable.
        #[arg(long, conflicts_with = "no_gpu")]
        gpu: bool,

        /// Force CPU embedding even if `gpu = true` is set in .vex.toml.
        #[arg(long)]
        no_gpu: bool,

        /// Advanced: pick a specific embedding execution provider
        /// (cpu | auto | cuda | directml | coreml). Mutually exclusive with --gpu/--no-gpu.
        #[arg(long, value_name = "DEVICE", conflicts_with_all = ["gpu", "no_gpu"])]
        device: Option<String>,

        /// Index every repo declared in a `.vex-workspace.toml` (found at or
        /// above `--path`/cwd). Each member is indexed into its own per-repo
        /// index dir. Per-member `cache_dir`/`local_cache` is unsupported in
        /// workspace mode (see docs/MULTIREPO.md).
        #[arg(long)]
        workspace: bool,
    },

    /// Search symbols by name or semantics
    Search {
        /// Search query
        query: String,

        /// Max results to return
        #[arg(short, long, default_value = "20")]
        limit: usize,

        /// Enable semantic (vector) search
        #[arg(long)]
        semantic: bool,

        /// Disable semantic search (overrides .vex.toml)
        #[arg(long, conflicts_with = "semantic")]
        no_semantic: bool,

        /// Filter results by path substring (e.g. "src/api/" or "tests/")
        #[arg(short = 'f', long = "filter")]
        filter_path: Option<String>,

        /// Boost results matching one or more kinds. Repeatable and
        /// comma-separated: `--kind fn,method` or `--kind fn --kind struct`.
        /// Accepts canonical kind names (function, struct, class, …) plus
        /// aliases: def (all definitions), comment (headings), test
        /// (test-path), ref (reserved for vex usages, no-op here).
        #[arg(short = 'k', long, value_name = "KIND")]
        kind: Vec<String>,

        /// Boost results near this file path (e.g. your current editor file)
        #[arg(long = "context-path")]
        context_path: Option<String>,

        /// Auto-update index if stale before searching
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        /// Disable BM25 channel (auto-on when the index has BM25 data)
        #[arg(long)]
        no_bm25: bool,

        /// v1.20.0 (D4): drop results whose path is a doc / prose file
        /// (`*.md`, `*.markdown`, `*.txt`, `*.rst`, `*.adoc`). Default
        /// off so e.g. searching `README` still finds the README. Pass
        /// for code-intent queries when CHANGELOG/README headings are
        /// polluting the top of the result list.
        #[arg(long)]
        code_only: bool,

        #[command(flatten)]
        meta: MetadataArgs,

        /// Append a JSON trace to stderr after the result list:
        /// normalized query, per-channel hit counts (FST / BM25 /
        /// semantic / fuzzy fallback), and the filter snapshot. Useful
        /// when results look wrong and you want to know what was
        /// actually searched.
        #[arg(long)]
        why: bool,

        #[command(flatten)]
        scope: ScopeArgs,

        #[command(flatten)]
        diff: DiffFilterArgs,

        /// Search every repo in a `.vex-workspace.toml` (found at or above
        /// cwd), grouping results by repo. Per-result JSON `signals` are
        /// single-repo only (omitted in workspace mode). See docs/MULTIREPO.md.
        #[arg(long, conflicts_with = "why")]
        workspace: bool,
    },

    /// Delete-safety blast-radius report. Composes four independent
    /// reference channels — strict refs (binder-resolved v5 edges),
    /// the legacy FST refs, `grep \b<Name>\b` against the project,
    /// and direct call-graph callers — into a single verdict
    /// (`safe` / `unsafe` / `uncertain`) so an agent doesn't have to
    /// hand-assemble safety from four sub-commands. v1.20.0 (F1).
    Impact {
        /// Symbol name to assess.
        name: String,

        /// Project root path (defaults to cwd).
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Auto-update index if stale (or bootstrap if missing) before searching.
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely.
        #[arg(long)]
        no_stale_check: bool,

        /// Opt-in: drop text-channel hits in prose-format files
        /// (`*.md`/`*.markdown`/`*.txt`/`*.rst`/`*.adoc`). Off by default
        /// so a symbol mentioned only in CHANGELOG still yields
        /// `Uncertain`; pass when you want a code-only blast radius
        /// (v1.20.1 — D4 parity with `vex search --code-only`).
        #[arg(long)]
        exclude_docs: bool,

        /// BFS hop budget for transitive callers. `1` (default) reports
        /// direct callers only via `call_graph_callers`; `≥ 2` enables
        /// the `transitive_callers` channel, walking the call graph
        /// backward up to N hops. Silently clamped to `[1, 16]`.
        /// v1.21.0.
        #[arg(long, default_value = "1")]
        depth: u32,

        #[command(flatten)]
        scope: ScopeArgs,

        /// Assess the symbol in every repo of a `.vex-workspace.toml`
        /// (found at or above `--path`/cwd), one verdict per repo.
        /// Cross-repo references are NOT seen (each repo resolves within
        /// itself). See docs/MULTIREPO.md.
        #[arg(long)]
        workspace: bool,
    },

    /// Find all usages/references of a symbol.
    ///
    /// Default mode reads from the legacy refs FST (identifier mentions on
    /// 7 T1 languages via AST walk; line-scan fallback elsewhere). Use
    /// `--strict` for binder-resolved refs on Rust / TypeScript / Python /
    /// C# / C++ / Go / Java — drops false positives from comments / strings / wrong-
    /// scope same-name refs. Decorator-based dispatch and string-resolved
    /// references (FastAPI `@router.get`, Celery `task.delay`, Uvicorn
    /// factory strings) are invisible to both modes. See
    /// `docs/LIMITATIONS.md` for the full coverage matrix.
    Usages {
        /// Symbol name to find usages of
        name: String,

        /// Max results to return
        #[arg(short, long, default_value = "50")]
        limit: usize,

        /// Filter results by path substring (e.g. "src/api/" or "tests/")
        #[arg(short = 'f', long = "filter")]
        filter_path: Option<String>,

        /// Auto-update index if stale
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        /// Use scope-resolved (binder-backed) refs from the persistent
        /// v5 `reference_edges` section (shipped in v1.8.0 / Phase
        /// 11.1). Drops the false positives of the legacy identifier
        /// FST — comments, doc strings, wrong-scope same-name refs.
        /// Available for Rust / TypeScript / Python / C# / C++ / Go /
        /// Java. Errors
        /// on indexes built before v1.8.0 (re-run `vex index` to
        /// rebuild). For other languages the legacy FST is the only
        /// option and is used by default — see `docs/LIMITATIONS.md`.
        #[arg(long)]
        strict: bool,

        /// Append a JSON usages-trace to stderr after the result list:
        /// `mode` (`strict` = v5 binder edges; `fst_lookup` = refs FST
        /// populated from AST identifier nodes on T1 languages and
        /// line-scan on the rest), `mode_legacy` (back-compat alias
        /// emitting `text_scan` for v1.9.x consumers; removed in
        /// v1.12), hits before/after path filter, "Did you mean"
        /// prefix-suggestion count, filter snapshot. See
        /// `docs/MCP-SCHEMA.md` for the full shape.
        #[arg(long)]
        why: bool,

        /// (non-strict only) Keep the row at the symbol's own definition
        /// line in the results. Pre-v1.20.0 behaviour. By default the
        /// non-strict FST lookup now strips the def-site row because
        /// "find all callers" queries don't want the symbol's own
        /// declaration showing up as a usage. Strict mode never
        /// included the def-site (scope-binder excludes it by
        /// construction), so this flag is a no-op when `--strict` is
        /// set. See `docs/LIMITATIONS.md` §usages-noise.
        #[arg(long)]
        include_self: bool,

        /// (non-strict only) Keep matches whose file is a Markdown /
        /// plain-text / reStructuredText / AsciiDoc document
        /// (`*.md`, `*.markdown`, `*.txt`, `*.rst`, `*.adoc`). Default
        /// strips them: CHANGELOG headings + README mentions of a
        /// symbol are prose, not callers, and they were polluting the
        /// top of every non-strict result list. Strict mode is
        /// unaffected (scope-binder doesn't index docs).
        #[arg(long)]
        include_docs: bool,

        #[command(flatten)]
        scope: ScopeArgs,

        #[command(flatten)]
        diff: DiffFilterArgs,

        /// Find usages in every repo of a `.vex-workspace.toml` (found at
        /// or above cwd), grouped by repo. References are resolved
        /// per-repo — a usage in repo B of a symbol defined in repo A is
        /// NOT seen. `--why` is single-repo only. See docs/MULTIREPO.md.
        #[arg(long, conflicts_with = "why")]
        workspace: bool,
    },

    /// Find code matching a structural AST pattern (like ast-grep)
    Pattern {
        /// Code pattern to match (e.g. 'fn $NAME($$$) -> Result')
        pattern: String,

        /// Language to search in
        #[arg(short, long)]
        lang: String,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Max results to return
        #[arg(long, default_value = "50")]
        limit: usize,

        /// Append a JSON scan-mode trace to stderr after the result list:
        /// whether the indexed prefilter or a live-scan was used, the
        /// inferred root kind, candidate/total file counts, and the
        /// fallback reason when the index path was skipped.
        #[arg(long)]
        why: bool,

        #[command(flatten)]
        scope: ScopeArgs,

        #[command(flatten)]
        diff: DiffFilterArgs,
    },

    /// Incremental update: only re-index changed files
    Update {
        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Generate semantic embeddings for changed files
        #[arg(long)]
        semantic: bool,

        /// Disable semantic embeddings (overrides .vex.toml)
        #[arg(long, conflicts_with = "semantic")]
        no_semantic: bool,

        /// Embedder ID for semantic indexing (default: minilm-l6-v2)
        #[arg(long)]
        embedder: Option<String>,

        /// Worker threads for parallel indexing. Default = 80% of cores, rounded up.
        /// Pass `0` to use all cores; pass N for exactly N workers.
        #[arg(short = 'j', long)]
        jobs: Option<usize>,

        /// Skip the persistent call-graph section. Overrides whatever the
        /// previous build recorded in the manifest. Without this flag,
        /// `update` honours the previous decision.
        #[arg(long)]
        no_call_graph: bool,

        /// Skip the BM25 channel. Overrides whatever the previous build
        /// recorded in the manifest. Without this flag, `update` honours
        /// the previous decision.
        #[arg(long)]
        no_bm25: bool,

        /// Skip the v6 pattern-skeleton side-section. Overrides the
        /// previous build's setting. Without this flag, `update`
        /// honours what the manifest recorded.
        #[arg(long)]
        no_pattern_index: bool,

        /// Exit immediately with a "busy" status if another vex instance
        /// is currently building the same index, instead of waiting for
        /// it to finish. See `--help` on `vex index --no-wait` for output
        /// shape.
        #[arg(long)]
        no_wait: bool,

        /// Re-build the Phase 14.8 `git_history` section incrementally
        /// against the new tip. Detects force-push / rebase via
        /// `git merge-base --is-ancestor` and falls back to full rebuild
        /// when the previous tip is unreachable. Has no effect if no
        /// `git_history` section is present in the prior manifest
        /// (run `vex index --history` first to enable).
        ///
        /// Phase 14.8 Step 3 scaffold — flag is parsed but the
        /// incremental walker lands in Step 5. Today `--history` is a
        /// no-op.
        #[arg(long)]
        history: bool,

        /// Drop the `git_history` section on this update. Mirrors the
        /// `--no-call-graph` / `--no-bm25` sticky-opt-out pattern: once
        /// the manifest records `history_indexed_at = Some(_)`,
        /// subsequent `vex update` runs rebuild the section by default;
        /// `--no-history` is the way to drop it. Conflicts with
        /// `--history`.
        ///
        /// Phase 14.8 Step 3 scaffold — flag is parsed but the drop
        /// path lands in Step 5. Today `--no-history` is a no-op.
        #[arg(long, conflicts_with = "history")]
        no_history: bool,

        /// Use the GPU for embedding generation, if this vex build was compiled
        /// with a gpu-* feature. Falls back to CPU if no GPU is usable. (Mostly
        /// a no-op for incremental updates — few/zero embeddings are recomputed.)
        #[arg(long, conflicts_with = "no_gpu")]
        gpu: bool,

        /// Force CPU embedding even if `gpu = true` is set in .vex.toml.
        #[arg(long)]
        no_gpu: bool,

        /// Advanced: pick a specific embedding execution provider
        /// (cpu | auto | cuda | directml | coreml). Mutually exclusive with --gpu/--no-gpu.
        #[arg(long, value_name = "DEVICE", conflicts_with_all = ["gpu", "no_gpu"])]
        device: Option<String>,

        /// Incrementally update every repo in a `.vex-workspace.toml`
        /// (found at or above `--path`/cwd), reporting per-repo changes.
        /// See docs/MULTIREPO.md.
        #[arg(long)]
        workspace: bool,
    },

    /// Show structure of a file (symbols, kinds, lines)
    Outline {
        /// File to analyze
        file: PathBuf,

        /// Filter by symbol kind. Aliases: fn, method, struct, class, interface, trait, enum, type/type_alias, impl, const/constant, prop/property, pkg/package
        #[arg(short, long)]
        kind: Option<String>,
    },

    /// Watch for file changes and re-index incrementally
    Watch {
        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Generate semantic embeddings
        #[arg(long)]
        semantic: bool,

        /// Disable semantic embeddings (overrides .vex.toml)
        #[arg(long, conflicts_with = "semantic")]
        no_semantic: bool,

        /// Embedder ID for semantic indexing (default: minilm-l6-v2)
        #[arg(long)]
        embedder: Option<String>,

        /// Worker threads for parallel indexing. Default = 80% of cores, rounded up.
        /// Pass `0` to use all cores; pass N for exactly N workers.
        #[arg(short = 'j', long)]
        jobs: Option<usize>,

        /// Skip the persistent call-graph section in both the initial build
        /// and subsequent incremental updates.
        #[arg(long)]
        no_call_graph: bool,

        /// Skip the BM25 channel in both the initial build and subsequent
        /// incremental updates.
        #[arg(long)]
        no_bm25: bool,

        /// Skip the v6 pattern-skeleton side-section in both the initial
        /// build and subsequent incremental updates.
        #[arg(long)]
        no_pattern_index: bool,

        /// Watch every repo declared in a `.vex-workspace.toml` (found at or
        /// above `--path`/cwd): build each member's initial index, then keep
        /// each incrementally fresh, routing a changed file to its owning
        /// member. See docs/MULTIREPO.md.
        #[arg(long)]
        workspace: bool,
    },

    /// Show the full body of a symbol (function, class, struct, etc.)
    Show {
        /// Symbol names to show (one or more)
        #[arg(required = true, num_args = 1..)]
        symbols: Vec<String>,

        /// Max results per symbol if multiple matches
        #[arg(short, long, default_value = "1")]
        limit: usize,

        /// Context lines before/after symbol body
        #[arg(short, long, default_value = "0")]
        context: usize,

        /// Filter results by path substring (e.g. "src/api/" or "tests/")
        #[arg(short = 'f', long = "filter")]
        filter_path: Option<String>,

        /// Boost results matching one or more kinds (repeatable,
        /// comma-separated). Accepts canonical kind names plus aliases:
        /// def, comment, test, ref. See `vex search --help` for details.
        #[arg(short = 'k', long, value_name = "KIND")]
        kind: Vec<String>,

        /// Boost results near this file path (e.g. your current editor file)
        #[arg(long = "context-path")]
        context_path: Option<String>,

        /// Auto-update index if stale before showing
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        // ----- Phase 13.3 — smart truncation flags (mutually exclusive) -----
        /// Print only the signature line(s) up to the first `{` / `:`
        /// (Phase 13.3). Mutually exclusive with `--head`, `--no-body`,
        /// `--collapsed`.
        #[arg(
            long = "signature-only",
            conflicts_with_all = ["head", "no_body", "collapsed"],
        )]
        signature_only: bool,

        /// Print only the first N lines of the symbol body and append a
        /// `... (M more lines)` indicator (Phase 13.3). Mutually
        /// exclusive with `--signature-only`, `--no-body`, `--collapsed`.
        #[arg(
            long = "head",
            value_name = "N",
            conflicts_with_all = ["signature_only", "no_body", "collapsed"],
        )]
        head: Option<usize>,

        /// Print signature + leading docstring/comment block only;
        /// drop the body (Phase 13.3). Mutually exclusive with
        /// `--signature-only`, `--head`, `--collapsed`.
        #[arg(
            long = "no-body",
            conflicts_with_all = ["signature_only", "head", "collapsed"],
        )]
        no_body: bool,

        /// Collapse nested method/function bodies inside a class /
        /// impl / module (Phase 13.3). **v1.9 NO-OP** — flag-shape is
        /// stable but the body is currently emitted unchanged with a
        /// warning on stderr. Mutually exclusive with
        /// `--signature-only`, `--head`, `--no-body`.
        #[arg(
            long = "collapsed",
            conflicts_with_all = ["signature_only", "head", "no_body"],
        )]
        collapsed: bool,

        #[command(flatten)]
        meta: MetadataArgs,

        #[command(flatten)]
        scope: ScopeArgs,
    },

    /// Search file contents by regex pattern (no index needed)
    Grep {
        /// Regex pattern to search in file contents
        pattern: String,

        /// Max results to return
        #[arg(short, long, default_value = "50")]
        limit: usize,

        /// Filter by path substring (e.g. "src/api/" or "tests/")
        #[arg(short = 'f', long = "filter")]
        filter_path: Option<String>,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        #[command(flatten)]
        scope: ScopeArgs,

        #[command(flatten)]
        diff: DiffFilterArgs,

        /// Grep every repo in a `.vex-workspace.toml` (found at or above
        /// `--path`/cwd), grouping matches by repo. See docs/MULTIREPO.md.
        #[arg(long)]
        workspace: bool,
    },

    /// Show index statistics
    Status {
        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Emit an index-coverage diagnostic: files indexed (per language),
        /// files discovered but not indexed (with reason), and files in
        /// the manifest that no longer exist on disk. Useful when
        /// `auto_update` silently misses something or a new file type
        /// appears in the tree.
        #[arg(long)]
        coverage: bool,
    },

    /// Diagnose GPU acceleration: show the execution provider compiled into
    /// this binary, actively probe whether it engages on this machine, and
    /// print targeted setup help if it does not. No index needed.
    #[command(
        long_about = "Diagnose GPU acceleration. Prints the execution provider compiled \
into this binary and actively probes one real inference, so a silent CPU fallback shows as \
FAILED with EP-specific setup help.\n\n\
Environment variables observed by GPU paths (all read-only — `vex gpu` never mutates the \
process env):\n  \
  VEX_DEVICE       Global default device across projects (cpu|auto|cuda|directml|coreml). \
Lower precedence than --device / .vex.toml. A stale pin to an uncompiled EP degrades to the \
compile-time default rather than erroring.\n  \
  VEX_EMBEDDER     Global default embedder (minilm-l6-v2 / jina-code / bge-base-en-v1.5 / \
bge-large-en-v1.5 / mxbai-large). Unknown values fall back to the default with a warning.\n  \
  VEX_GPU_STRICT   `1` turns ORT's silent EP-registration fallback into a hard error — \
useful for benchmarking or for proving the GPU engaged on a given box. `vex gpu` requests \
strict mode internally without setting this variable. Default: unset (lenient).\n  \
  VEX_GPU_ATTN_BUDGET / VEX_GPU_MEM_LIMIT  Advanced batching / VRAM tunables (see \
docs/GPU_SUPPORT.md §11 — heavy embedders / shared GPU only)."
    )]
    Gpu {
        /// Probe only this device (cuda|directml|coreml; `auto` = all). Default:
        /// every execution provider compiled into this binary.
        device: Option<String>,

        /// If a GPU engages, persist it to the VEX_DEVICE environment variable
        /// (user-level) so every project uses it. Applies to new shells. If no
        /// GPU engages, nothing is pinned and the command says so.
        #[arg(long)]
        enable: bool,
    },

    /// Find all types that inherit from / implement a base class, trait, or interface (no index needed)
    Implementations {
        /// Base class, trait, or interface name to search for
        name: String,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Max results to return
        #[arg(short, long, default_value = "50")]
        limit: usize,

        /// Auto-update index if stale (or bootstrap if missing) before searching.
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        #[command(flatten)]
        scope: ScopeArgs,

        #[command(flatten)]
        diff: DiffFilterArgs,
    },

    /// Find all functions that call a given function. Uses the persistent call
    /// graph (fast, ~4ms) when an index is available; falls back to live scan otherwise.
    ///
    /// Known limitation: only call sites *inside* a function/method body are
    /// recorded as callers. Module-level expressions (e.g. `app = create_app()`
    /// at Python module scope) and decorator-based dispatch (e.g.
    /// `@router.get("/foo")`) are invisible to the call graph. Use `vex grep`
    /// as a fallback. See `docs/LIMITATIONS.md` for the full surface.
    Callers {
        /// Function name to find callers of
        name: String,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Max results to return
        #[arg(short, long, default_value = "50")]
        limit: usize,

        /// Auto-update index if stale (or bootstrap if missing) before searching.
        /// Enables the persistent call-graph fast path; live-scan is used otherwise.
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        #[command(flatten)]
        scope: ScopeArgs,

        #[command(flatten)]
        diff: DiffFilterArgs,

        /// Find callers in every repo of a `.vex-workspace.toml` (found at
        /// or above `--path`/cwd), grouped by repo. Call edges resolve
        /// per-repo (a caller in repo B is not seen). See docs/MULTIREPO.md.
        #[arg(long)]
        workspace: bool,
    },

    /// Find all functions called by a given function. Uses the persistent call
    /// graph (fast, ~4ms) when an index is available; falls back to live scan otherwise.
    Callees {
        /// Function name to find callees of
        name: String,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Max results to return
        #[arg(short, long, default_value = "50")]
        limit: usize,

        /// Auto-update index if stale (or bootstrap if missing) before searching.
        /// Enables the persistent call-graph fast path; live-scan is used otherwise.
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        /// v1.15.1: include stdlib / macro callees that the default
        /// post-filter drops. Without this flag, names matching
        /// `std::*`, `__*`, common stdlib container methods (`c_str`,
        /// `push_back`, ...), and short all-uppercase macro-style
        /// identifiers are suppressed so the real callgraph edges
        /// surface. Pass to see the unfiltered list. C/C++ idiom only —
        /// other languages already produce mostly-clean callee sets.
        #[arg(long)]
        include_stdlib: bool,

        #[command(flatten)]
        scope: ScopeArgs,

        #[command(flatten)]
        diff: DiffFilterArgs,

        /// Find callees in every repo of a `.vex-workspace.toml` (found at
        /// or above `--path`/cwd), grouped by repo. Call edges resolve
        /// per-repo. See docs/MULTIREPO.md.
        #[arg(long)]
        workspace: bool,
    },

    /// Symbol-level diff between an arbitrary git revision and the
    /// working tree. Lists added / removed / moved / body-changed
    /// symbols across the files touched on the branch.
    Diff {
        /// Git revision to compare against (e.g. `main`, `HEAD~3`,
        /// `origin/main`). The working tree is the "new" side.
        #[arg(long)]
        base: String,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Max changes to return
        #[arg(short, long, default_value = "500")]
        limit: usize,

        #[command(flatten)]
        scope: ScopeArgs,
    },

    /// Enumerate all caller chains from `from` to `to` in the persistent
    /// call graph. Multi-hop generalisation of `vex callers`.
    Paths {
        /// Starting function (the caller).
        from: String,

        /// Destination function (the callee).
        to: String,

        /// Maximum hops between `from` and `to`. Default 6 catches most
        /// real chains without exploding traversal time.
        #[arg(long, default_value = "6")]
        max_hops: usize,

        /// Maximum number of paths to enumerate. Caps output and aborts
        /// traversal early in dense graphs.
        #[arg(long, default_value = "50")]
        max_paths: usize,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Auto-update index if stale (or bootstrap if missing) before
        /// searching. The call graph fast path requires a v4 index;
        /// `paths` does not have a live-scan fallback.
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        #[command(flatten)]
        scope: ScopeArgs,
    },

    /// Find all symbols whose callees transitively reach `target` in the
    /// persistent call graph. Multi-hop generalisation of `vex callers`.
    Reachable {
        /// Symbol whose callers (direct + transitive) we want.
        target: String,

        /// Maximum hops to walk back from `target`.
        #[arg(long, default_value = "6")]
        max_hops: usize,

        /// Max results to return
        #[arg(short, long, default_value = "200")]
        limit: usize,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Auto-update index if stale (or bootstrap if missing) before
        /// searching.
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        #[command(flatten)]
        scope: ScopeArgs,

        /// Compute the reachable set in every repo of a
        /// `.vex-workspace.toml` (found at or above `--path`/cwd), grouped
        /// by repo. Call edges resolve per-repo. See docs/MULTIREPO.md.
        #[arg(long)]
        workspace: bool,
    },

    /// Find test functions that transitively reach `target` in the
    /// persistent call graph. Post-filters `reachable` by test-path
    /// globs + (optionally) a name heuristic, then stamps a
    /// `framework` label per row.
    TestsFor {
        /// Symbol whose test coverage we want.
        target: String,

        /// Maximum hops to walk back from `target`.
        #[arg(long, default_value = "6")]
        max_hops: usize,

        /// Max results to return
        #[arg(short, long, default_value = "200")]
        limit: usize,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Auto-update index if stale (or bootstrap if missing) before
        /// searching.
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        /// Glob pattern for test paths. Repeatable. When set, REPLACES
        /// the default pattern set (does not append).
        #[arg(long = "test-pattern")]
        test_pattern: Vec<String>,

        /// Include non-test-named helpers (fixtures) that live under
        /// test paths. Default: off — only `test_*` / `*Test` names
        /// are surfaced. When set, ALSO performs a one-hop forward
        /// callee walk from each surviving test row to admit fixtures
        /// the test calls into; this can grow the result set beyond
        /// what plain name-filter weakening would produce (rows are
        /// still capped by `--limit` and constrained to test-path
        /// globs).
        #[arg(long)]
        include_fixtures: bool,

        #[command(flatten)]
        scope: ScopeArgs,
    },

    /// Fast existence check: which of the given symbols exist in the index?
    Check {
        /// Symbol names to check (case-insensitive exact match)
        #[arg(required = true, num_args = 1..)]
        names: Vec<String>,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Auto-update index if stale
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        /// Check the names across every repo in a `.vex-workspace.toml`
        /// (found at or above `--path`/cwd), reporting which repos define
        /// each name. See docs/MULTIREPO.md.
        #[arg(long)]
        workspace: bool,
    },

    /// Find symbols semantically similar to a given symbol (requires --semantic index)
    Similar {
        /// Symbol name to find similar symbols to
        name: String,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Max results to return
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Minimum cosine similarity in 0.0..=1.0. Alias: `--min-score`.
        #[arg(short, long, alias = "min-score", default_value = "0.5")]
        threshold: f32,

        /// Filter results by path substring (e.g. "src/api/" or "tests/")
        #[arg(short = 'f', long = "filter")]
        filter_path: Option<String>,

        /// Show reasoning per result: identifier-set Jaccard overlap +
        /// truncated unified diff between the seed and each match.
        #[arg(long)]
        explain: bool,

        /// Auto-update index if stale
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        /// Append a JSON similar-trace to stderr after the result list:
        /// seed resolution, applied threshold, candidates before/after
        /// path filter, filter snapshot. See `docs/MCP-SCHEMA.md`.
        #[arg(long)]
        why: bool,

        #[command(flatten)]
        scope: ScopeArgs,

        #[command(flatten)]
        diff: DiffFilterArgs,
    },

    /// Find pairs of near-duplicate symbols (requires --semantic index)
    Duplicates {
        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Minimum cosine similarity to consider a duplicate (0.0..=1.0).
        /// Alias: `--min-score`.
        #[arg(short, long, alias = "min-score", default_value = "0.9")]
        threshold: f32,

        /// Max pairs to return
        #[arg(short, long, default_value = "50")]
        limit: usize,

        /// Skip symbols whose body has fewer than this many lines (filters trivial 1-liners)
        #[arg(long, default_value = "5")]
        min_body_lines: usize,

        /// Filter pairs to those involving this path substring
        #[arg(short = 'f', long = "filter")]
        filter_path: Option<String>,

        /// Show reasoning per pair: identifier-set Jaccard overlap +
        /// truncated unified diff between the two symbol bodies.
        #[arg(long)]
        explain: bool,

        /// Auto-update index if stale
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,

        /// Append a JSON duplicates-trace to stderr after the result
        /// list: applied threshold + min_body_lines, pairs before/after
        /// path filter, filter snapshot. See `docs/MCP-SCHEMA.md`.
        #[arg(long)]
        why: bool,

        #[command(flatten)]
        scope: ScopeArgs,

        #[command(flatten)]
        diff: DiffFilterArgs,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },

    /// Create a default .vex.toml config file in the current directory.
    /// With `--agents-md`, also writes a community-convention AGENTS.md
    /// next to it (readable by Cursor / Codex CLI / Aider / Cline / etc.
    /// as a fallback to their own per-tool config files).
    Init {
        /// Also write an AGENTS.md template next to `.vex.toml`. Refuses
        /// to overwrite an existing AGENTS.md.
        #[arg(long)]
        agents_md: bool,

        /// Skip the `.vex.toml` write — only emit AGENTS.md. Useful for
        /// projects that already have a `.vex.toml` but want the agent
        /// instruction file too. Mutually exclusive with the default
        /// behaviour; ignored without `--agents-md`.
        #[arg(long, requires = "agents_md")]
        agents_md_only: bool,
    },

    /// Print machine-readable capabilities matrix (Phase 13.0).
    Capabilities,

    /// v1.16 — `vex history <Symbol>`: walk git log and return every
    /// historical version of the named symbol. Query-time (no
    /// indexing) — works on any repo `vex` hasn't yet indexed.
    /// Dedups by blob SHA so consecutive commits with identical file
    /// content surface as one entry.
    History {
        /// Symbol name to walk through history. Matched whole-word
        /// against file contents via `git grep --word-regexp`, then
        /// filtered post-parse to exact `name == query`.
        symbol: String,

        /// Project root (defaults to cwd). Must be inside a git
        /// worktree.
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Max commits to walk per file. Unbounded by default — bump
        /// down on long-lived repos to keep latency in check.
        #[arg(long, value_name = "N")]
        depth: Option<usize>,

        /// Restrict log walk to this revision (`refs/heads/foo`,
        /// `origin/main`, a SHA). Defaults to `HEAD`.
        #[arg(long, value_name = "REV")]
        branch: Option<String>,

        /// Cap the total result set. The walker stops as soon as the
        /// limit is reached.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,

        /// Force the v1.16 query-time walker even if a `git_history`
        /// section is present in the index. Default (Phase 14.8
        /// `HistoryMode::Auto`) picks the indexed path when available
        /// and falls back to the walker otherwise. Use this for
        /// debugging or regression-checking the walker against the
        /// indexed path.
        ///
        /// Phase 14.8 Step 3 scaffold — flag is parsed but the indexed
        /// reader lands in Step 4c. Today `--no-index` is a no-op
        /// because no index path exists yet.
        #[arg(long)]
        no_index: bool,

        /// Phase 14.9 Tier A.2 — keep only entries whose commit date
        /// is `>= YYYY-MM-DD` (inclusive). Compared lexicographically
        /// against `%cs` / unix-seconds-derived ISO strings; works
        /// on both walker and indexed paths.
        #[arg(long, value_name = "YYYY-MM-DD")]
        since: Option<String>,

        /// Phase 14.9 Tier A.2 — keep only entries whose commit date
        /// is `<= YYYY-MM-DD` (inclusive).
        #[arg(long, value_name = "YYYY-MM-DD")]
        until: Option<String>,

        /// Phase 14.9 Tier A.3 — keep only entries whose commit author
        /// contains this substring (case-insensitive). Walker-only —
        /// the Phase 14.8 sidecar does not record author info, so the
        /// indexed path rejects this flag with a clear error pointing
        /// at `--no-index`. Phase 14.10 (rename tracking) will
        /// retroactively populate author for the indexed path.
        #[arg(long, value_name = "SUBSTR")]
        author: Option<String>,

        /// Phase 14.9 Tier A.4 — keep only entries whose symbol kind
        /// matches exactly. Use the lowercase label
        /// (`function` / `struct` / `impl` / …) — matches the kind
        /// strings the indexed and walker paths both emit.
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,

        /// Phase 14.9 Tier A.1 — render unified diffs between
        /// consecutive historical versions of the same
        /// `(symbol, kind)` pair instead of repeating the full body
        /// for each entry. Cuts output noise dramatically on deep
        /// histories. JSON consumers see `body_diff` in place of
        /// `body`; advertised in the `capabilities` matrix as
        /// `history_diff: true`. Mutually exclusive with
        /// `--exact-presence` — the diff path groups entries by
        /// `(symbol, kind)`, which would break the per-row presence
        /// mapping.
        #[arg(long, conflicts_with = "exact_presence")]
        diff: bool,

        /// Phase 14.9 Tier B.7 — for each entry, list the exact set
        /// of commits where its blob existed in the file. Defeats
        /// the convex-hull span representation (LIMITATIONS §4c #4).
        /// Adds latency: each unique file_path triggers one
        /// `git log` walk + one batched `git cat-file --batch-check`.
        /// Walk depth is capped by `--exact-presence-max-commits`.
        #[arg(long)]
        exact_presence: bool,

        /// Phase 14.9 Tier B.7 — cap on the `git log` walk per file
        /// during `--exact-presence`. Beyond the cap, the entry
        /// falls back to the convex-hull span and signals
        /// `presence_truncated: true` in JSON output. Default 500.
        #[arg(long, value_name = "N", default_value_t = 500)]
        exact_presence_max_commits: usize,
    },

    /// v1.15.0 — manage the `vex-mcp` server entry in your coding
    /// agent's config. `install` adds the entry idempotently;
    /// `uninstall` removes it; `list` shows current entries.
    /// `--agent all` fans out across every supported agent.
    ///
    /// Supported agents: `claude-code`, `cursor`. More land in the
    /// follow-up commits — `codex-cli`, `windsurf`, `cline`,
    /// `continue`, `zed`.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },

    /// Phase 13.2 — unified multi-source bundle primitive. One command,
    /// three modes (`symbol`, `pr-impact`, `project`) that assemble a
    /// structured response from existing v6 index sections. Replaces the
    /// 4-round-trip agent loop `show → callers → callees → similar`.
    ///
    /// Mode-specific args are validated server-side; only `--mode` is
    /// universally required. See `docs/MCP-SCHEMA.md` for the response
    /// envelope shape.
    Bundle {
        /// Bundle mode selector. One of `symbol`, `pr-impact`, `project`.
        #[arg(long, value_enum, value_name = "MODE")]
        mode: crate::cli::cmd_bundle::BundleModeFlag,

        /// (`symbol` mode) Symbol name to resolve.
        #[arg(long, value_name = "NAME")]
        symbol: Option<String>,

        /// (`pr-impact` mode) Git base revision to diff against.
        #[arg(long, value_name = "REV")]
        base: Option<String>,

        /// (`pr-impact` mode) Transitive callers walk depth.
        #[arg(long, default_value = "2", value_name = "N")]
        depth: usize,

        /// (`project` mode) Path glob filter applied to ranked symbols.
        #[arg(long, value_name = "GLOB")]
        path_glob: Option<String>,

        /// (`project` mode) Top-N result cap.
        #[arg(long, default_value = "30", value_name = "N")]
        top_n: usize,

        /// (`project` mode) Emit only the directory-symbol-density tree
        /// in `mode_hints.directory_tree` and an empty `items[]`.
        /// Architecture-orientation use case — saves the indegree work
        /// and keeps the response compact.
        #[arg(long)]
        directory_tree_only: bool,

        /// (`project` mode) Cap on entries in `mode_hints.directory_tree`,
        /// sorted by `recursive_symbol_count` descending.
        #[arg(long, default_value = "30", value_name = "N")]
        directory_tree_top: usize,

        /// (`symbol` mode) Max direct callers in the response.
        #[arg(long, default_value = "10", value_name = "N")]
        callers_max: usize,

        /// (`symbol` mode) Max direct callees in the response.
        #[arg(long, default_value = "10", value_name = "N")]
        callees_max: usize,

        /// (`symbol` mode) Max semantic-similar matches in the response.
        #[arg(long, default_value = "5", value_name = "N")]
        similar_max: usize,

        /// (`pr-impact` mode) Max test-classified results.
        #[arg(long, default_value = "20", value_name = "N")]
        tests_max: usize,

        /// Project root (defaults to cwd).
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Auto-rebuild the index when staleness is detected.
        #[arg(long)]
        auto_update: bool,

        /// Skip the staleness check entirely (faster, may return stale data).
        #[arg(long)]
        no_stale_check: bool,

        /// Path scope filters (`--include`/`--exclude` globs, repeatable).
        #[command(flatten)]
        scope: ScopeArgs,
    },

    /// Run the ranking evaluation harness (Phase 13.12). Loads a
    /// hand-curated golden query set, runs every query against the
    /// current index, and reports nDCG@10 / recall@10 / MRR per
    /// query and aggregated. Intended as a CI regression guard, not
    /// a research benchmark — see `docs/RANKING-EVAL.md`.
    Eval {
        /// Path to the golden set TOML. Defaults to the bundled
        /// `benches/ranking_golden/queries.toml` next to the vex
        /// source tree. Tests and CI override this to a fixture path.
        #[arg(long, value_name = "PATH")]
        bench: Option<PathBuf>,

        /// Fail with non-zero exit if the mean nDCG@10 drops below
        /// this threshold. Default `0.0` — eval always succeeds —
        /// makes the command safe to wire into "first run" workflows
        /// before a baseline is captured. CI sets a recorded floor.
        #[arg(long, default_value = "0.0", value_name = "F64")]
        min_ndcg: f64,

        /// Project root path (defaults to cwd). Eval consumes whatever
        /// index already lives at this root — it never builds one.
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Emit the full report as JSON to stdout instead of the
        /// human-readable summary. The JSON shape matches
        /// `vex::eval::harness::EvalReport`.
        #[arg(long)]
        json: bool,
    },

    /// Update vex to the latest GitHub release. Replaces the running
    /// binary in place. Works on Linux, macOS, and Windows.
    SelfUpdate {
        /// Print the latest release version without modifying anything.
        #[arg(long, conflicts_with = "yes")]
        check: bool,

        /// Skip the interactive confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

/// Sub-action for `vex mcp ...`. Each variant maps 1:1 to a function
/// in [`crate::cli::cmd_mcp`].
#[derive(Subcommand)]
pub enum McpAction {
    /// Register the `vex-mcp` server entry in the agent's MCP config.
    /// Idempotent: skips writes when an entry already matches the
    /// intended shape unless `--force` is set.
    Install {
        /// Target agent. Use `all` to install into every supported
        /// agent at once. See `vex mcp install --help` for the list.
        #[arg(long, value_name = "ID")]
        agent: String,

        /// Name registered in the agent's MCP server table. Defaults
        /// to `vex`. Use distinct names per `VEX_ROOT` for multi-repo
        /// setups (`vex-api` / `vex-client`).
        #[arg(long, value_name = "NAME")]
        server_name: Option<String>,

        /// Absolute path to the `vex-mcp` binary. Defaults to the
        /// sibling of the currently-running `vex` binary; falls back
        /// to bare `vex-mcp` (PATH lookup) otherwise.
        #[arg(long, value_name = "PATH")]
        binary_path: Option<PathBuf>,

        /// Project root passed to the server via `VEX_ROOT`. Defaults
        /// to the current working directory.
        #[arg(long, value_name = "PATH")]
        project_root: Option<PathBuf>,

        /// Don't touch any files — print what *would* be written.
        #[arg(long)]
        dry_run: bool,

        /// Overwrite an existing entry with the same `--server-name`.
        /// Without this, install on an existing matching entry is a
        /// no-op skip; install on an existing *differing* entry also
        /// requires `--force` (deliberate — protects hand-tuned configs).
        #[arg(long)]
        force: bool,
    },

    /// Remove the named server entry from the agent's MCP config.
    /// Idempotent: no error if the entry was already absent.
    Uninstall {
        #[arg(long, value_name = "ID")]
        agent: String,

        #[arg(long, value_name = "NAME")]
        server_name: Option<String>,
    },

    /// Print the MCP server entries currently configured for one
    /// agent (with `--agent <id>`) or all supported agents (default).
    List {
        #[arg(long, value_name = "ID")]
        agent: Option<String>,
    },
}
