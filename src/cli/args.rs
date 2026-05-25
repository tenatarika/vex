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

#[derive(Parser)]
#[command(
    name = "vex",
    version = env!("VEX_VERSION"),
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

#[derive(Clone, clap::ValueEnum)]
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
    },

    /// Find all usages/references of a symbol.
    ///
    /// Default mode reads from the legacy refs FST (identifier mentions on
    /// 5 T1 languages via AST walk; line-scan fallback elsewhere). Use
    /// `--strict` for binder-resolved refs on Rust / TypeScript / Python /
    /// C# / C++ — drops false positives from comments / strings / wrong-
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
        /// Available for Rust / TypeScript / Python / C# / C++. Errors
        /// on indexes built before v1.8.0 (re-run `vex index` to
        /// rebuild). For other languages the legacy FST is the only
        /// option and is used by default — see `docs/LIMITATIONS.md`.
        #[arg(long)]
        strict: bool,

        /// Append a JSON usages-trace to stderr after the result list:
        /// `mode` (`strict` = v5 binder edges; `text_scan` = legacy
        /// refs FST — note: name is historical, the FST itself is
        /// populated from AST identifier nodes on T1 languages and
        /// line-scan on the rest), hits before/after path filter,
        /// "Did you mean" prefix-suggestion count, filter snapshot.
        /// See `docs/MCP-SCHEMA.md` for the full shape.
        #[arg(long)]
        why: bool,

        #[command(flatten)]
        scope: ScopeArgs,

        #[command(flatten)]
        diff: DiffFilterArgs,
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
    },

    /// Show index statistics
    Status {
        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,
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

        #[command(flatten)]
        scope: ScopeArgs,

        #[command(flatten)]
        diff: DiffFilterArgs,
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

    /// Create a default .vex.toml config file in the current directory
    Init,

    /// Print machine-readable capabilities matrix (Phase 13.0).
    Capabilities,

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
