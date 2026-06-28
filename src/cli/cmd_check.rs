//! `vex check` — fast symbol-existence probe via the FST.
//! Extracted from `cli/mod.rs` in S1 Group B.2.
//!
//! v1.12.0 T4 — when the bloom sidecar (`index.bloom`) is present, each
//! name is pre-filtered through it: a `may_contain == false` short-
//! circuits the FST lookup to `(name, false)`. A missing or corrupt
//! sidecar is non-fatal; we just skip the optimisation and fall
//! through to the FST as before.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

use super::args::OutputFormat;
use super::common::{resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use crate::protocol::capabilities;
use crate::search::bloom::SymbolBloom;
use crate::store::reader::IndexReader;
use crate::util::config::{self, VexConfig};
use crate::workspace;

pub(crate) fn check(
    ctx: &CmdCtx<'_>,
    names: Vec<String>,
    path: Option<std::path::PathBuf>,
    auto_update: bool,
    no_stale_check: bool,
    workspace: bool,
) -> Result<()> {
    if workspace {
        return check_workspace(ctx, names, path, auto_update, no_stale_check);
    }

    let root = resolve_root(path)?.canonicalize()?;
    let results = check_in_root(
        &root,
        &names,
        auto_update,
        no_stale_check,
        ctx.cfg,
        ctx.local_cache_active,
    )?;

    match ctx.format {
        OutputFormat::Json => {
            let json: serde_json::Value = results
                .iter()
                .map(|(name, found)| serde_json::json!({ "name": name, "exists": found }))
                .collect();
            print_envelope(
                &json,
                capabilities::current(),
                super::output::default_meta_for(&root),
            );
        }
        OutputFormat::Text | OutputFormat::Compact => {
            for (name, found) in &results {
                let mark = if *found { "+" } else { "-" };
                println!("{mark} {name}");
            }
        }
    }
    Ok(())
}

/// Probe `names` against a single repo's index, returning `(name, exists)`
/// in input order. Opens the index (auto-updating per the flags) and uses
/// the bloom sidecar as a pre-filter when present.
fn check_in_root(
    root: &Path,
    names: &[String],
    auto_update: bool,
    no_stale_check: bool,
    cfg: &VexConfig,
    local_cache_active: bool,
) -> Result<Vec<(String, bool)>> {
    let index_path = ensure_index_ready(
        root,
        auto_update,
        no_stale_check,
        false,
        local_cache_active,
        cfg,
    )?;

    let reader = IndexReader::open(&index_path).context("open index")?;

    // Load bloom sidecar if present. `Err` (corruption) is treated the
    // same as `Ok(None)`: silently degrade to direct FST lookups —
    // bloom is an optimisation and a corrupt sidecar must not wedge
    // `vex check`.
    let bloom_path = config::bloom_path(root);
    let bloom = match SymbolBloom::load(&bloom_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!(
                path = %bloom_path.display(),
                error = %e,
                "bloom sidecar unreadable; falling back to direct FST"
            );
            None
        }
    };

    // Case-insensitive exact match: FST candidates filtered by actual
    // name. Pre-filtered by bloom (lowercased query) when the sidecar
    // is loaded — a `may_contain == false` is a guaranteed miss.
    let results: Vec<(String, bool)> = if let Some(fst) = reader.symbol_fst_reader() {
        names
            .iter()
            .map(|n| {
                let lower = n.to_lowercase();
                if let Some(b) = bloom.as_ref() {
                    if !b.may_contain(&lower) {
                        return (n.clone(), false);
                    }
                }
                let found = fst.find(n).iter().any(|&idx| {
                    reader
                        .symbol(idx as usize)
                        .map(|r| reader.read_string(r.name_offset).to_lowercase() == lower)
                        .unwrap_or(false)
                });
                (n.clone(), found)
            })
            .collect()
    } else {
        // Fallback: build lowercased set for consistent case-insensitive matching
        let all_lower: std::collections::HashSet<String> = (0..reader.symbol_count())
            .filter_map(|i| {
                let rec = reader.symbol(i)?;
                let name = reader.read_string(rec.name_offset);
                if name.is_empty() {
                    None
                } else {
                    Some(name.to_lowercase())
                }
            })
            .collect();
        names
            .iter()
            .map(|n| {
                let lower = n.to_lowercase();
                if let Some(b) = bloom.as_ref() {
                    if !b.may_contain(&lower) {
                        return (n.clone(), false);
                    }
                }
                (n.clone(), all_lower.contains(&lower))
            })
            .collect()
    };

    Ok(results)
}

/// `vex check --workspace`: probe each name across every member of the
/// nearest `.vex-workspace.toml`, reporting which repos define it. Each
/// member uses its own `.vex.toml` for staleness/auto-update.
fn check_workspace(
    ctx: &CmdCtx<'_>,
    names: Vec<String>,
    path: Option<std::path::PathBuf>,
    auto_update: bool,
    no_stale_check: bool,
) -> Result<()> {
    let start_dir = resolve_root(path)?;
    let ws_file = workspace::find_workspace_file(&start_dir).ok_or_else(|| {
        anyhow!(
            "no {} found at or above {}",
            workspace::WORKSPACE_FILE,
            start_dir.display()
        )
    })?;
    let ws = workspace::Workspace::load(&ws_file)?;
    let base = ws
        .file
        .parent()
        .expect("canonicalized workspace file has a parent directory")
        .to_path_buf();

    // Per member: (display_name, results). `local_cache_active` is false in
    // workspace mode (the index command rejects hash-less layouts), and the
    // member's own .vex.toml drives staleness/auto-update.
    let mut per_repo: Vec<(String, Vec<(String, bool)>)> = Vec::with_capacity(ws.members.len());
    for m in &ws.members {
        let member_cfg = crate::util::config::load_config(&m.root)?;
        let results = check_in_root(
            &m.root,
            &names,
            auto_update,
            no_stale_check,
            &member_cfg,
            false,
        )?;
        per_repo.push((m.display_name.clone(), results));
    }

    match ctx.format {
        OutputFormat::Json => {
            let repos: Vec<_> = per_repo
                .iter()
                .map(|(repo, results)| {
                    let names: Vec<_> = results
                        .iter()
                        .map(|(name, found)| serde_json::json!({ "name": name, "exists": found }))
                        .collect();
                    serde_json::json!({ "repo": repo, "names": names })
                })
                .collect();
            print_envelope(
                serde_json::json!({
                    "workspace": ws.file.to_string_lossy(),
                    "repos": repos,
                }),
                capabilities::current(),
                super::output::default_meta_for(&base),
            );
        }
        OutputFormat::Text | OutputFormat::Compact => {
            // For each name, list the repos that define it — the "which
            // repo has X?" question is the point of a workspace check.
            for name in &names {
                let hits: Vec<&str> = per_repo
                    .iter()
                    .filter(|(_, results)| results.iter().any(|(n, f)| n == name && *f))
                    .map(|(repo, _)| repo.as_str())
                    .collect();
                if hits.is_empty() {
                    println!("- {name}");
                } else {
                    println!("+ {name}  [{}]", hits.join(", "));
                }
            }
        }
    }
    Ok(())
}
