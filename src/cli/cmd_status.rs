//! `vex status` — index summary (size, symbols, sections, optional
//! coverage report). Extracted from `cli/mod.rs` in S1 Group B.2.

use anyhow::{Context, Result};

use super::args::OutputFormat;
use super::common::{resolve_root, CmdCtx};
use super::output::print_envelope;
use super::status_coverage;
use crate::index::manifest::Manifest;
use crate::index::rename_chains::weights as chain_weights;
use crate::protocol::{capabilities, MetaEnvelope};
use crate::store::reader::IndexReader;
use crate::store::rename_chains as store_rc;
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
    // Phase 14.9 Tier B.6: detect submodule presence + git_history
    // size ratio so the text output can surface §4c #5 and #6 from
    // LIMITATIONS without changing the manifest schema. Both checks
    // are best-effort reads — failures fall through silently.
    let has_submodules = root.join(".gitmodules").is_file();
    let git_history_size_bytes = std::fs::metadata(config::git_history_path(&root))
        .ok()
        .map(|m| m.len());
    // Phase 14.10 — rename-chains sidecar diagnostics. Best-effort:
    // a corrupt / version-mismatched sidecar is reported as absent.
    // No tip-SHA or body-hash check here — `vex status` reports what's
    // on disk, not whether the chain data is fresh (use `vex history`
    // to surface staleness via fallback).
    let chain_header = store_rc::read_header(&config::index_dir(&root))
        .ok()
        .flatten();
    let chain_sidecar_size_bytes = std::fs::metadata(config::rename_chains_path(&root))
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
                "cpp_includes_processed": manifest.state.cpp_includes_processed.unwrap_or(false),
                // v1.15.0 B1.2 marker — None on pre-1.15 manifests means
                // the body_tokens sidecar isn't on disk, so the next
                // `vex update` will fall back to full HNSW rebuild.
                // Same `jq`-friendly bool projection.
                "body_tokens_persisted": manifest.state.body_tokens_persisted.unwrap_or(false),
                // v1.24+ grep trigram skip-index marker — false on
                // pre-trigram manifests / when the sidecar save failed.
                "trigram_persisted": manifest.trigram_persisted.unwrap_or(false),
                // v1.17 Phase 14.8 — sticky sentinel + counts. ISO date
                // when section is present, null otherwise. Agents can
                // `jq '.history_indexed_at // empty'` to branch on
                // section presence without unwrapping.
                "history_indexed_at": manifest.state.history_indexed_at,
                "history": manifest.state.history,
                // Phase 14.9 Tier B.6 — surface submodule presence and
                // git_history sidecar size so JSON consumers can
                // compute the §4c #5 ratio themselves.
                "has_submodules": has_submodules,
                "git_history_size_bytes": git_history_size_bytes,
                // Phase 14.10 — rename-chains sidecar diagnostics.
                // `null` when the sidecar is absent / corrupt / pre-v1.17;
                // an object with chain counts + active thresholds + weights
                // when present. Fields are additive — agents can `jq
                // '.rename_chains.chain_count // 0'` without unwrapping.
                "rename_chains": chain_header.as_ref().map(|h| serde_json::json!({
                    "chain_count": h.chain_count,
                    "forward_count": h.forward_count,
                    "member_count": h.member_count,
                    "sidecar_size_bytes": chain_sidecar_size_bytes,
                    "thresholds": {
                        "score": chain_weights::GATE_SCORE,
                        "jaccard": chain_weights::GATE_JACCARD,
                        "len_ratio": chain_weights::GATE_LEN_RATIO,
                    },
                    "weights": {
                        "body_with_cos": chain_weights::W_BODY_WITH_COS,
                        "sig_with_cos":  chain_weights::W_SIG_WITH_COS,
                        "cos":           chain_weights::W_COS,
                        "body_no_cos":   chain_weights::W_BODY_NO_COS,
                        "sig_no_cos":    chain_weights::W_SIG_NO_COS,
                    },
                    // MiniLM tiebreaker hit count, sourced from the
                    // manifest (build-time stat, not persisted in the
                    // sidecar header). `null` = pre-14.10 manifest OR
                    // build ran without semantic embeddings; `0` =
                    // cosine path active but no decisions hinged on
                    // it; `>0` = number of accepted links whose
                    // cosine contribution was strictly required to
                    // clear GATE_SCORE. See manifest.rs docstring on
                    // `rename_chains_minilm_tiebreak_hits` for the
                    // exact predicate.
                    "minilm_tiebreak_hits":
                        manifest.rename_chains_minilm_tiebreak_hits,
                })),
                // Phase 14.10 — manifest provenance for the rename_chains
                // sidecar. Distinguishes "tried and failed" (Some(false),
                // surfaced as `false`) from "never tried" (None, surfaced
                // as `null`). Co-rendered with the `rename_chains` block
                // above so agents that see a populated chain header but a
                // `false` flag here know the manifest is out of sync with
                // disk (rare, but useful for debugging crash-recovery).
                "rename_chains_built": manifest.rename_chains_built,
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
            match manifest.state.cpp_includes_processed {
                Some(true) => println!("C++ includes: yes"),
                Some(false) | None => {
                    println!("C++ includes: no (run `vex index` to enable cross-file C++ refs)")
                }
            }
            // v1.15.0 B1.2: surface the body_tokens sidecar marker. Pre-v1.15
            // indexes lack the file → the next `vex update` falls back to a
            // full HNSW rebuild instead of the incremental path.
            match manifest.state.body_tokens_persisted {
                Some(true) => println!("Body tokens: yes (incremental HNSW update enabled)"),
                Some(false) | None => {
                    println!("Body tokens: no (run `vex index` to enable incremental HNSW update)")
                }
            }
            // v1.24+ grep trigram skip-index sidecar. Absent (pre-trigram
            // index or a failed save) → `vex grep` full-walks every file.
            match manifest.trigram_persisted {
                Some(true) => {
                    println!("Trigram skip-index: yes (`vex grep` skips non-matching files)")
                }
                Some(false) | None => {
                    println!("Trigram skip-index: no (run `vex index` to speed up `vex grep`)")
                }
            }
            // v1.17 Phase 14.8 — git_history section surface. Three
            // shapes: present + stats (the typical post-build case),
            // present-but-no-stats (defensive — should not happen
            // since Step 5 always pairs them), and absent.
            match (&manifest.state.history_indexed_at, &manifest.state.history) {
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
            // Phase 14.10 — rename-chains surface, gated on history
            // being indexed (the sidecar is co-written with
            // index.git_history). Three shapes: present with chains,
            // present-but-empty (history indexed, no renames detected
            // — common on small / young repos), and absent.
            match (&manifest.state.history_indexed_at, &chain_header) {
                (Some(_), Some(h)) if h.chain_count > 0 => {
                    println!(
                        "Rename chains: {} chains, {} members (threshold {:.2}, body-Jaccard ≥ {:.2})",
                        h.chain_count,
                        h.member_count,
                        chain_weights::GATE_SCORE,
                        chain_weights::GATE_JACCARD,
                    );
                    // Phase 14.10 — surface tie-breaker hits only when
                    // the cosine path actually ran (`Some(_)`). `Some(0)`
                    // gets its own line so users can see the path was
                    // active even without decisions hinging on it; `None`
                    // (structural-only build, e.g. `--no-semantic`) stays
                    // silent so the typical `--no-semantic` user isn't
                    // told about a feature they didn't opt into.
                    if let Some(hits) = manifest.rename_chains_minilm_tiebreak_hits {
                        if hits > 0 {
                            println!(
                                "               MiniLM tie-break decided {} link{}",
                                hits,
                                if hits == 1 { "" } else { "s" },
                            );
                        } else {
                            println!("               MiniLM tie-break: active, 0 decisive");
                        }
                    }
                }
                (Some(_), Some(_)) => {
                    println!(
                        "Rename chains: 0 (no renames detected at threshold {:.2})",
                        chain_weights::GATE_SCORE,
                    );
                }
                (Some(_), None) => {
                    // History indexed but no rename_chains sidecar.
                    // Three distinguishable causes:
                    //   - `rename_chains_built = Some(false)`: the build
                    //     attempted the write and failed (disk full,
                    //     permission, rename race). Logs carry the reason.
                    //   - `rename_chains_built = Some(true)`: build
                    //     succeeded but the sidecar has since gone missing
                    //     (manual rm, filesystem hiccup, external cleanup).
                    //     Different action — the manifest disagrees with
                    //     disk, a re-index will reconcile.
                    //   - `rename_chains_built = None`: a pre-Phase-14.10
                    //     index, or chain detection wasn't reached.
                    //     Re-index opts in.
                    match manifest.rename_chains_built {
                        Some(false) => println!(
                            "Rename chains: ⚠ build attempted but write failed; \
                             chain expansion in `vex history` is disabled. Re-run \
                             `vex index --history` after checking logs for the \
                             cause (disk full / permission / rename race)."
                        ),
                        Some(true) => println!(
                            "Rename chains: ⚠ manifest records a successful build \
                             but the sidecar is missing on disk. Likely manual \
                             removal or external cleanup; re-run `vex index --history` \
                             to reconcile."
                        ),
                        None => println!(
                            "Rename chains: no (re-run `vex index --history` to enable \
                             Phase 14.10 chain detection)"
                        ),
                    }
                }
                (None, _) => {
                    // History not indexed — chain detection is gated on it.
                    // The "History: no" line above already prompted the user;
                    // skip a redundant chain-specific hint.
                }
            }
            // Phase 14.9 Tier B.6 — submodule + size-ratio warnings,
            // gated on history being indexed (these are facts about
            // the indexed snapshot, not the project at large).
            if manifest.state.history_indexed_at.is_some() {
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
