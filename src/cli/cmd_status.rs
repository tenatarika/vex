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
    // Phase 14.9 Tier B.6: detect submodule presence + git_history
    // size ratio so the text output can surface §4c #5 and #6 from
    // LIMITATIONS without changing the manifest schema. Both checks
    // are best-effort reads — failures fall through silently.
    let has_submodules = root.join(".gitmodules").is_file();
    let git_history_size_bytes = std::fs::metadata(config::git_history_path(&root))
        .ok()
        .map(|m| m.len());
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
                // Phase 14.9 Tier B.6 — surface submodule presence and
                // git_history sidecar size so JSON consumers can
                // compute the §4c #5 ratio themselves.
                "has_submodules": has_submodules,
                "git_history_size_bytes": git_history_size_bytes,
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
            // Phase 14.9 Tier B.6 — submodule + size-ratio warnings,
            // gated on history being indexed (these are facts about
            // the indexed snapshot, not the project at large).
            if manifest.history_indexed_at.is_some() {
                if has_submodules {
                    println!(
                        "            ⚠ this repo has submodules — their history is NOT \
                         in `index.git_history`. Submodule blobs aren't in the parent \
                         repo's git db, so `vex history` against them returns nothing. \
                         Run `vex history` inside each submodule's checkout for \
                         per-submodule history. (LIMITATIONS §4c #6)"
                    );
                }
                if let Some(gh_bytes) = git_history_size_bytes {
                    let ratio = gh_bytes as f64 / meta.len() as f64;
                    if ratio > 2.0 {
                        println!(
                            "            ℹ git_history sidecar is {:.1}× index.vex \
                             ({:.1} KB) — long-lived repos scale by history depth, not \
                             current-symbol-count. Cap with `vex index --history --history-depth N` \
                             if storage is tight. (LIMITATIONS §4c #5)",
                            ratio,
                            gh_bytes as f64 / 1024.0,
                        );
                    }
                }
            }
            if let Some(c) = &coverage_report {
                status_coverage::render_text(c);
            }
        }
    }
    Ok(())
}
