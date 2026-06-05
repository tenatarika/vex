//! `vex status` — index summary (size, symbols, sections, optional
//! coverage report). Extracted from `cli/mod.rs` in S1 Group B.2.

use anyhow::{Context, Result};

use super::args::OutputFormat;
use super::common::{resolve_root, CmdCtx};
use super::output::print_envelope;
use super::status_coverage;
use crate::index::manifest::Manifest;
use crate::protocol::{capabilities, MetaEnvelope};
use crate::store::reader::IndexReader;
use crate::util::config;

pub(crate) fn status(
    ctx: &CmdCtx<'_>,
    path: Option<std::path::PathBuf>,
    coverage: bool,
) -> Result<()> {
    let root = resolve_root(path)?
        .canonicalize()
        .context("canonicalize root")?;
    let index_path = config::index_path(&root);

    if !index_path.exists() {
        match ctx.format {
            OutputFormat::Json => {
                let payload = serde_json::json!({"error": "no index found"});
                print_envelope(&payload, capabilities::current(), MetaEnvelope::default());
                // ^ no index → no manifest → no index_age; default meta is correct.
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
    // Manifest load is best-effort — a missing/corrupt JSON sidecar must
    // not block `vex status` (the index header alone covers core fields).
    // `Manifest::load` already returns `Manifest::default()` for absent
    // files, so the only failure path here is a corrupt JSON; surface as
    // default in that case so the rest of the report still renders.
    let manifest = Manifest::load(&config::manifest_path(&root)).unwrap_or_default();
    let coverage_report = if coverage {
        Some(status_coverage::collect(&root, &reader, ctx.excludes)?)
    } else {
        None
    };

    match ctx.format {
        OutputFormat::Json => {
            let mut json = serde_json::json!({
                "project": root.to_string_lossy(),
                "index": index_path.to_string_lossy(),
                "size_bytes": meta.len(),
                "symbols": reader.symbol_count(),
                "embeddings": reader.has_vectors(),
                "call_graph": reader.has_call_graph(),
                "bm25": reader.has_bm25(),
                // v1.14 marker — None on pre-1.14 manifests, Some(true)
                // from v1.14+. Serialised as a literal bool so scripts
                // can `jq '.cpp_includes_processed'` without unwrapping.
                "cpp_includes_processed": manifest.cpp_includes_processed.unwrap_or(false),
            });
            if let Some(c) = &coverage_report {
                json["coverage"] = serde_json::to_value(c)?;
            }
            print_envelope(
                &json,
                capabilities::current(),
                super::output::default_meta_for(&root),
            );
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
            // v1.14: surface the C++ include-resolution marker. Pre-v1.14
            // indexes lack the field; we render an actionable hint instead
            // of a blank "no" so users know how to opt in.
            match manifest.cpp_includes_processed {
                Some(true) => println!("C++ includes: yes"),
                Some(false) | None => {
                    println!("C++ includes: no (run `vex index` to enable cross-file C++ refs)")
                }
            }
            if let Some(c) = &coverage_report {
                status_coverage::render_text(c);
            }
        }
    }
    Ok(())
}
