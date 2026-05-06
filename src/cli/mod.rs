pub mod args;
pub mod output;

use anyhow::Result;
use args::{Cli, Commands};

pub fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Index { path } => {
            let root = path.unwrap_or_else(|| std::env::current_dir().unwrap());
            tracing::info!(?root, "indexing");
            // TODO: call index::pipeline::run(&root)
            Ok(())
        }
        Commands::Search { query, limit, semantic } => {
            tracing::info!(%query, %limit, %semantic, "searching");
            // TODO: call search dispatcher
            Ok(())
        }
        Commands::Watch { path } => {
            let root = path.unwrap_or_else(|| std::env::current_dir().unwrap());
            tracing::info!(?root, "watching");
            // TODO: call watch::run(&root)
            Ok(())
        }
        Commands::Status { path } => {
            let root = path.unwrap_or_else(|| std::env::current_dir().unwrap());
            tracing::info!(?root, "status");
            // TODO: print index stats
            Ok(())
        }
    }
}
