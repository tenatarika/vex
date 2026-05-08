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
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },

    /// Create a default .vex.toml config file in the current directory
    Init,
}
