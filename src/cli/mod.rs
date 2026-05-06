pub mod args;
pub mod output;

use std::time::Instant;

use anyhow::{Context, Result, bail};
use args::{Cli, Commands};

use crate::embed::Embedder;
use crate::index::pipeline;
use crate::search::{fusion, semantic, structural};
use crate::store::inverted::InvertedIndex;
use crate::store::reader::IndexReader;
use crate::util::config;

fn resolve_root(path: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match path {
        Some(p) => Ok(p),
        None => std::env::current_dir().context("get working directory"),
    }
}

pub fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Index { path, semantic } => {
            let root = resolve_root(path)?;
            let start = Instant::now();
            let count = pipeline::run(&root, semantic)?;
            let elapsed = start.elapsed();
            println!("Indexed {count} symbols in {elapsed:.2?}");
            if semantic {
                println!("Embeddings: enabled");
            }
            println!("Index: {}", config::index_path(&root.canonicalize()?).display());
            Ok(())
        }
        Commands::Search { query, limit, semantic } => {
            let root = resolve_root(None)?.canonicalize()?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                bail!("No index found. Run `vex index` first.\nExpected: {}", index_path.display());
            }

            let reader = IndexReader::open(&index_path)
                .context("open index")?;
            let inverted = InvertedIndex::from_reader(&reader);

            let structural_results = structural::search(&reader, &inverted, &query, limit);

            let results = if semantic && reader.has_vectors() {
                let mut embedder = Embedder::new().context("load embedding model")?;
                let semantic_results = semantic::search_with_embedder(&reader, &mut embedder, &query, limit)?;
                fusion::fuse(structural_results, semantic_results, limit)
            } else {
                if semantic && !reader.has_vectors() {
                    eprintln!("Warning: no embeddings in index. Run `vex index --semantic` first.");
                }
                structural_results
            };

            if results.is_empty() {
                println!("No results for \"{query}\"");
            } else {
                output::print_results(&results, &cli.format);
            }
            Ok(())
        }
        Commands::Update { path, semantic } => {
            let root = resolve_root(path)?;
            let start = Instant::now();
            let (total, changed, deleted) = pipeline::update(&root, semantic)?;
            let elapsed = start.elapsed();
            if changed == 0 && deleted == 0 {
                println!("Index up to date ({total} symbols)");
            } else {
                println!("Updated in {elapsed:.2?}: {changed} changed, {deleted} deleted, {total} total symbols");
            }
            Ok(())
        }
        Commands::Watch { path, semantic } => {
            let root = resolve_root(path)?;
            crate::watch::handler::watch(&root, semantic)?;
            Ok(())
        }
        Commands::Status { path } => {
            let root = resolve_root(path)?.canonicalize().context("canonicalize root")?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                println!("No index found for {}", root.display());
                println!("Run `vex index` to build one.");
                return Ok(());
            }

            let meta = std::fs::metadata(&index_path)?;
            let reader = IndexReader::open(&index_path)?;

            println!("Project:    {}", root.display());
            println!("Index:      {}", index_path.display());
            println!("Size:       {:.1} KB", meta.len() as f64 / 1024.0);
            println!("Symbols:    {}", reader.symbol_count());
            println!("Embeddings: {}", if reader.has_vectors() { "yes" } else { "no" });
            Ok(())
        }
    }
}
