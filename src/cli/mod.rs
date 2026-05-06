pub mod args;
pub mod output;

use std::time::Instant;

use anyhow::{Context, Result, bail};
use args::{Cli, Commands};

use crate::index::pipeline;
use crate::search::structural;
use crate::store::inverted::InvertedIndex;
use crate::store::reader::IndexReader;
use crate::util::config;

pub fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Index { path } => {
            let root = path.unwrap_or_else(|| std::env::current_dir().unwrap());
            let start = Instant::now();
            let count = pipeline::run(&root)?;
            let elapsed = start.elapsed();
            println!("Indexed {count} symbols in {elapsed:.2?}");
            println!("Index: {}", config::index_path(&root.canonicalize()?).display());
            Ok(())
        }
        Commands::Search { query, limit, semantic: _ } => {
            let root = std::env::current_dir()?;
            let root = root.canonicalize()?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                bail!("No index found. Run `vex index` first.\nExpected: {}", index_path.display());
            }

            let reader = IndexReader::open(&index_path)
                .context("open index")?;
            let inverted = InvertedIndex::from_reader(&reader);

            let results = structural::search(&reader, &inverted, &query, limit);

            if results.is_empty() {
                println!("No results for \"{query}\"");
            } else {
                output::print_results(&results, &cli.format);
            }
            Ok(())
        }
        Commands::Watch { path } => {
            let root = path.unwrap_or_else(|| std::env::current_dir().unwrap());
            println!("Watch mode not yet implemented (Phase 3)");
            println!("Root: {}", root.display());
            Ok(())
        }
        Commands::Status { path } => {
            let root = path.unwrap_or_else(|| std::env::current_dir().unwrap());
            let root = root.canonicalize().context("canonicalize root")?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                println!("No index found for {}", root.display());
                println!("Run `vex index` to build one.");
                return Ok(());
            }

            let meta = std::fs::metadata(&index_path)?;
            let reader = IndexReader::open(&index_path)?;

            println!("Project:  {}", root.display());
            println!("Index:    {}", index_path.display());
            println!("Size:     {:.1} KB", meta.len() as f64 / 1024.0);
            println!("Symbols:  {}", reader.symbol_count());
            Ok(())
        }
    }
}
