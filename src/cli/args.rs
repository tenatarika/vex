use clap::{Parser, Subcommand};
use clap_complete::Shell;
use std::path::PathBuf;

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

        /// Boost results matching this symbol kind (e.g. fn, struct, trait)
        #[arg(short = 'k', long)]
        kind: Option<String>,

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

        /// Boost results matching this symbol kind (e.g. fn, struct, trait)
        #[arg(short = 'k', long)]
        kind: Option<String>,

        /// Boost results near this file path (e.g. your current editor file)
        #[arg(long = "context-path")]
        context_path: Option<String>,

        /// Auto-update index if stale before showing
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,
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
    },

    /// Find all functions that call a given function (no index needed)
    Callers {
        /// Function name to find callers of
        name: String,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Max results to return
        #[arg(short, long, default_value = "50")]
        limit: usize,
    },

    /// Find all functions called by a given function (no index needed)
    Callees {
        /// Function name to find callees of
        name: String,

        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Max results to return
        #[arg(short, long, default_value = "50")]
        limit: usize,
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

        /// Minimum cosine similarity in 0.0..=1.0
        #[arg(short, long, default_value = "0.5")]
        threshold: f32,

        /// Filter results by path substring (e.g. "src/api/" or "tests/")
        #[arg(short = 'f', long = "filter")]
        filter_path: Option<String>,

        /// Auto-update index if stale
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,
    },

    /// Find pairs of near-duplicate symbols (requires --semantic index)
    Duplicates {
        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Minimum cosine similarity to consider a duplicate (0.0..=1.0)
        #[arg(short, long, default_value = "0.9")]
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

        /// Auto-update index if stale
        #[arg(long)]
        auto_update: bool,

        /// Skip staleness check entirely
        #[arg(long)]
        no_stale_check: bool,
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
        #[arg(long)]
        check: bool,

        /// Skip the interactive confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}
