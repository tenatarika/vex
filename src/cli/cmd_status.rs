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
                // GPU fields are compile-time properties of the binary, so
                // they're meaningful (and most useful) BEFORE the first index
                // exists — an MCP agent deciding whether to pass --gpu to the
                // initial `vex index` gates on exactly this branch.
                let payload = serde_json::json!({
                    "error": "no index found",
                    "gpu_support": crate::embed::device::gpu_support_str(),
                    "default_device": crate::embed::device::DEFAULT_DEVICE.as_str(),
                });
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
                // GPU support is a compile-time property of THIS binary; the
                // default device is what an unflagged `vex index` would use.
                // Mirrors the text branch so MCP agents can gate on the JSON
                // (e.g. decide whether to pass --gpu) instead of scraping
                // human-readable text. See docs/GPU_SUPPORT.md §5.7.
                "gpu_support": crate::embed::device::gpu_support_str(),
                "default_device": crate::embed::device::DEFAULT_DEVICE.as_str(),
                // v1.14 marker — None on pre-1.14 manifests, Some(true)
                // from v1.14+. Serialised as a literal bool so scripts
                // can `jq '.cpp_includes_processed'` without unwrapping.
                "cpp_includes_processed": manifest.cpp_includes_processed.unwrap_or(false),
                // v1.15.0 B1.2 marker — None on pre-1.15 manifests means
                // the body_tokens sidecar isn't on disk, so the next
                // `vex update` will fall back to full HNSW rebuild.
                // Same `jq`-friendly bool projection.
                "body_tokens_persisted": manifest.body_tokens_persisted.unwrap_or(false),
                // v1.17 Phase 14.8 — sticky sentinel + counts. ISO date
                // when section is present, null otherwise. Agents can
                // `jq '.history_indexed_at // empty'` to branch on
                // section presence without unwrapping.
                "history_indexed_at": manifest.history_indexed_at,
                "history": manifest.history,
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
            // GPU support is a compile-time property of THIS binary; the
            // default device is what an unflagged `vex index` would use (the
            // actual device per-run still depends on --gpu/--device/.vex.toml/
            // $VEX_DEVICE). See docs/GPU_SUPPORT.md §5.7.
            println!(
                "GPU:        {} · default {}",
                crate::embed::device::gpu_support_str(),
                crate::embed::device::DEFAULT_DEVICE.as_str()
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
            // v1.15.0 B1.2: surface the body_tokens sidecar marker. Pre-v1.15
            // indexes lack the file → the next `vex update` falls back to a
            // full HNSW rebuild instead of the incremental path.
            match manifest.body_tokens_persisted {
                Some(true) => println!("Body tokens: yes (incremental HNSW update enabled)"),
                Some(false) | None => {
                    println!("Body tokens: no (run `vex index` to enable incremental HNSW update)")
                }
            }
            // v1.17 Phase 14.8 — git_history section surface. Three
            // shapes: present + stats (the typical post-build case),
            // present-but-no-stats (defensive — should not happen
            // since Step 5 always pairs them), and absent.
            match (&manifest.history_indexed_at, &manifest.history) {
                (Some(date), Some(stats)) => {
                    println!(
                        "History:    indexed at {date} ({} commits, {} blobs, {} entries)",
                        stats.commit_count, stats.blob_count, stats.entry_count
                    );
                    if stats.depth_capped == Some(true) {
                        // Step 6 polish: depth-capped is non-obvious
                        // and impacts which symbols `vex history`
                        // can find — surface on its own line so it's
                        // hard to miss.
                        println!(
                            "            ⚠ section is partial: --history-depth cap stopped \
                             walking before the root commit. Symbols introduced before the cap \
                             are NOT indexed; re-run `vex index --history` without the cap to \
                             cover full history."
                        );
                    }
                }
                (Some(date), None) => {
                    println!("History:    indexed at {date}");
                }
                (None, _) => {
                    println!(
                        "History:    no (run `vex index --history` to enable indexed `vex history`)"
                    );
                }
            }
            if let Some(c) = &coverage_report {
                status_coverage::render_text(c);
            }
        }
    }
    Ok(())
}
