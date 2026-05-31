//! `vex check` — fast symbol-existence probe via the FST.
//! Extracted from `cli/mod.rs` in S1 Group B.2.

use anyhow::{Context, Result};

use super::args::OutputFormat;
use super::common::resolve_root;
use super::index_management::ensure_index_ready;
use crate::store::reader::IndexReader;
use crate::util::config;

#[allow(clippy::too_many_arguments)]
pub(crate) fn check(
    names: Vec<String>,
    path: Option<std::path::PathBuf>,
    auto_update: bool,
    no_stale_check: bool,
    local_cache_active: bool,
    cfg: &config::VexConfig,
    format: &OutputFormat,
) -> Result<()> {
    let root = resolve_root(path)?.canonicalize()?;
    let index_path = ensure_index_ready(
        &root,
        auto_update,
        no_stale_check,
        false,
        local_cache_active,
        cfg,
    )?;

    let reader = IndexReader::open(&index_path).context("open index")?;

    // Case-insensitive exact match: FST candidates filtered by actual name
    let results: Vec<(String, bool)> = if let Some(fst) = reader.symbol_fst_reader() {
        names
            .iter()
            .map(|n| {
                let lower = n.to_lowercase();
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
            .map(|n| (n.clone(), all_lower.contains(&n.to_lowercase())))
            .collect()
    };

    match format {
        OutputFormat::Json => {
            let json: serde_json::Value = results
                .iter()
                .map(|(name, found)| serde_json::json!({ "name": name, "exists": found }))
                .collect();
            println!("{}", serde_json::to_string_pretty(&json)?);
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
