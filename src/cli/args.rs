use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "vex",
    version,
    about = "Fast hybrid structural + semantic code search"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Output format
    #[arg(long, global = true, default_value = "text")]
    pub format: OutputFormat,
}

#[derive(Clone, clap::ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Build index for a project directory
    Index {
        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Generate semantic embeddings (slower but enables semantic search)
        #[arg(long, default_value = "false")]
        semantic: bool,
    },

    /// Search symbols by name or semantics
    Search {
        /// Search query
        query: String,

        /// Max results to return
        #[arg(short, long, default_value = "20")]
        limit: usize,

        /// Enable semantic (vector) search
        #[arg(long, default_value = "false")]
        semantic: bool,
    },

    /// Find all usages/references of a symbol
    Usages {
        /// Symbol name to find usages of
        name: String,

        /// Max results to return
        #[arg(short, long, default_value = "50")]
        limit: usize,
    },

    /// Incremental update: only re-index changed files
    Update {
        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Generate semantic embeddings for changed files
        #[arg(long, default_value = "false")]
        semantic: bool,
    },

    /// Show structure of a file (symbols, kinds, lines)
    Outline {
        /// File to analyze
        file: PathBuf,
    },

    /// Watch for file changes and re-index incrementally
    Watch {
        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,

        /// Generate semantic embeddings
        #[arg(long, default_value = "false")]
        semantic: bool,
    },

    /// Show index statistics
    Status {
        /// Project root path (defaults to cwd)
        #[arg(short, long)]
        path: Option<PathBuf>,
    },
}
