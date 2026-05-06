pub mod args;
pub mod output;

use std::time::Instant;

use anyhow::{bail, Context, Result};
use args::{Cli, Commands, OutputFormat};

use crate::embed::Embedder;
use crate::index::pipeline;
use crate::search::{fusion, semantic, structural};
use crate::store::reader::IndexReader;
use crate::util::config;

fn resolve_root(path: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match path {
        Some(p) => Ok(p),
        None => std::env::current_dir().context("get working directory"),
    }
}

pub fn dispatch(cli: Cli) -> Result<()> {
    let format = &cli.format;

    match cli.command {
        Commands::Index { path, semantic } => {
            let root = resolve_root(path)?;
            let start = Instant::now();
            let count = pipeline::run(&root, semantic)?;
            let elapsed = start.elapsed();
            let index_path = config::index_path(&root.canonicalize()?);

            match format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "symbols": count,
                        "elapsed_ms": elapsed.as_millis(),
                        "embeddings": semantic,
                        "index": index_path.to_string_lossy(),
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    println!("Indexed {count} symbols in {elapsed:.2?}");
                    if semantic {
                        println!("Embeddings: enabled");
                    }
                    println!("Index: {}", index_path.display());
                }
            }
            Ok(())
        }
        Commands::Search {
            query,
            limit,
            semantic,
        } => {
            let root = resolve_root(None)?.canonicalize()?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                bail!(
                    "No index found. Run `vex index` first.\nExpected: {}",
                    index_path.display()
                );
            }

            let reader = IndexReader::open(&index_path).context("open index")?;

            let structural_results = structural::search(&reader, &query, limit);

            let results = if semantic && reader.has_vectors() {
                let mut embedder = Embedder::new().context("load embedding model")?;
                let semantic_results =
                    semantic::search_with_embedder(&reader, &mut embedder, &query, limit)?;
                fusion::fuse(structural_results, semantic_results, limit)
            } else {
                if semantic && !reader.has_vectors() {
                    eprintln!("Warning: no embeddings in index. Run `vex index --semantic` first.");
                }
                structural_results
            };

            if results.is_empty() {
                match format {
                    OutputFormat::Json => println!("[]"),
                    OutputFormat::Text | OutputFormat::Compact => {
                        println!("No results for \"{query}\"")
                    }
                }
            } else {
                output::print_results(&results, format);
            }
            Ok(())
        }
        Commands::Usages { name, limit } => {
            let root = resolve_root(None)?.canonicalize()?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                bail!(
                    "No index found. Run `vex index` first.\nExpected: {}",
                    index_path.display()
                );
            }

            let reader = IndexReader::open(&index_path).context("open index")?;
            let ref_reader = reader
                .ref_reader()
                .context("no refs in index — re-run `vex index` to rebuild")?;
            let file_paths = reader.file_paths();

            let entries = ref_reader.find(&name);
            let total = entries.len();
            let entries: Vec<_> = entries.into_iter().take(limit).collect();

            match format {
                OutputFormat::Json => {
                    let json: Vec<serde_json::Value> = entries
                        .iter()
                        .map(|e| {
                            let path = file_paths
                                .get(e.file_id as usize)
                                .map(|s| s.as_str())
                                .unwrap_or("?");
                            serde_json::json!({
                                "path": path,
                                "line": e.line,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    if entries.is_empty() {
                        println!("No usages found for \"{name}\"");

                        let prefix_results = ref_reader.find_by_prefix(&name);
                        if !prefix_results.is_empty() {
                            println!("\nDid you mean:");
                            for (n, refs) in prefix_results.iter().take(5) {
                                println!("  {n} ({} usages)", refs.len());
                            }
                        }
                    } else {
                        println!("{name}: {total} usages (showing {})", entries.len());
                        for e in &entries {
                            let path = file_paths
                                .get(e.file_id as usize)
                                .map(|s| s.as_str())
                                .unwrap_or("?");
                            println!("  {path}:{}", e.line);
                        }
                    }
                }
            }
            Ok(())
        }
        Commands::Update { path, semantic } => {
            let root = resolve_root(path)?;
            let start = Instant::now();
            let (total, changed, deleted) = pipeline::update(&root, semantic)?;
            let elapsed = start.elapsed();

            match format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "symbols": total,
                        "changed": changed,
                        "deleted": deleted,
                        "elapsed_ms": elapsed.as_millis(),
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    if changed == 0 && deleted == 0 {
                        println!("Index up to date ({total} symbols)");
                    } else {
                        println!("Updated in {elapsed:.2?}: {changed} changed, {deleted} deleted, {total} total symbols");
                    }
                }
            }
            Ok(())
        }
        Commands::Outline { file } => cmd_outline(&file, format),
        Commands::Watch { path, semantic } => {
            let root = resolve_root(path)?;
            crate::watch::handler::watch(&root, semantic)?;
            Ok(())
        }
        Commands::Status { path } => {
            let root = resolve_root(path)?
                .canonicalize()
                .context("canonicalize root")?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                match format {
                    OutputFormat::Json => {
                        println!("{}", serde_json::json!({"error": "no index found"}));
                    }
                    OutputFormat::Text | OutputFormat::Compact => {
                        println!("No index found for {}", root.display());
                        println!("Run `vex index` to build one.");
                    }
                }
                return Ok(());
            }

            let meta = std::fs::metadata(&index_path)?;
            let reader = IndexReader::open(&index_path)?;

            match format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "project": root.to_string_lossy(),
                        "index": index_path.to_string_lossy(),
                        "size_bytes": meta.len(),
                        "symbols": reader.symbol_count(),
                        "embeddings": reader.has_vectors(),
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    println!("Project:    {}", root.display());
                    println!("Index:      {}", index_path.display());
                    println!("Size:       {:.1} KB", meta.len() as f64 / 1024.0);
                    println!("Symbols:    {}", reader.symbol_count());
                    println!(
                        "Embeddings: {}",
                        if reader.has_vectors() { "yes" } else { "no" }
                    );
                }
            }
            Ok(())
        }
    }
}

fn cmd_outline(file: &std::path::Path, format: &OutputFormat) -> Result<()> {
    let content =
        std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;

    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .context("file has no extension")?;

    let lang = crate::parse::language::Language::from_extension(ext)
        .with_context(|| format!("unsupported language: .{ext}"))?;

    // Check if we have a tree-sitter query for this language
    if crate::parse::queries::get_query(lang).is_none() {
        bail!(
            "language .{ext} is recognized but has no tree-sitter query yet (Kotlin, TypeScript pending)"
        );
    }

    let rel = file.to_string_lossy().to_string();
    let parsed = crate::parse::parse_file(&rel, &content, lang)?;

    match format {
        OutputFormat::Json => {
            let symbols: Vec<serde_json::Value> = parsed
                .symbols
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "kind": s.kind.as_str(),
                        "line": s.line,
                        "signature": s.signature,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&symbols)?);
        }
        OutputFormat::Text | OutputFormat::Compact => {
            if parsed.symbols.is_empty() {
                println!("No symbols found in {}", file.display());
            } else {
                println!("{}", file.display());
                for s in &parsed.symbols {
                    println!("  {:<12} {:<40} line {}", s.kind.as_str(), s.name, s.line);
                    if let Some(sig) = &s.signature {
                        println!("               {sig}");
                    }
                }
            }
        }
    }
    Ok(())
}
