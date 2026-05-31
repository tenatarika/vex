//! `vex status` — index summary (size, symbols, sections, optional
//! coverage report). Extracted from `cli/mod.rs` in S1 Group B.2.

use anyhow::{Context, Result};

use super::args::OutputFormat;
use super::common::resolve_root;
use super::status_coverage;
use crate::store::reader::IndexReader;
use crate::util::config;

pub(crate) fn status(
    path: Option<std::path::PathBuf>,
    coverage: bool,
    format: &OutputFormat,
    excludes: &[String],
) -> Result<()> {
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
    let coverage_report = if coverage {
        Some(status_coverage::collect(&root, &reader, excludes)?)
    } else {
        None
    };

    match format {
        OutputFormat::Json => {
            let mut json = serde_json::json!({
                "project": root.to_string_lossy(),
                "index": index_path.to_string_lossy(),
                "size_bytes": meta.len(),
                "symbols": reader.symbol_count(),
                "embeddings": reader.has_vectors(),
                "call_graph": reader.has_call_graph(),
                "bm25": reader.has_bm25(),
            });
            if let Some(c) = &coverage_report {
                json["coverage"] = serde_json::to_value(c)?;
            }
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
            println!(
                "Call graph: {}",
                if reader.has_call_graph() { "yes" } else { "no" }
            );
            println!(
                "BM25:       {}",
                if reader.has_bm25() { "yes" } else { "no" }
            );
            if let Some(c) = &coverage_report {
                status_coverage::render_text(c);
            }
        }
    }
    Ok(())
}
