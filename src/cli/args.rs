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
    },

    /// Find all usages/references of a symbol
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

        #[command(flatten)]
        scope: ScopeArgs,
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

        #[command(flatten)]
        scope: ScopeArgs,
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
    },

    /// Find all functions that call a given function. Uses the persistent call
    /// graph (fast, ~4ms) when an index is available; falls back to live scan otherwise.
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

        #[command(flatten)]
        scope: ScopeArgs,
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

        #[command(flatten)]
        scope: ScopeArgs,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },

    /// Create a default .vex.toml config file in the current directory
    Init,

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
