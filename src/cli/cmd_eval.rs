//! `vex eval` — run a ranking-eval harness against the index.
//! Extracted from `cli/mod.rs` in S1 Group B.

use anyhow::{bail, Context, Result};

use super::args::OutputFormat;
use super::common::resolve_root;
use super::index_management::ensure_index_ready;
use crate::store::reader::IndexReader;
use crate::util::config;

pub(crate) fn cmd_eval(
    path: Option<std::path::PathBuf>,
    bench: Option<std::path::PathBuf>,
    min_ndcg: f64,
    json: bool,
    local_cache_active: bool,
    cfg: &config::VexConfig,
    format: &OutputFormat,
) -> Result<()> {
    let root = resolve_root(path)?.canonicalize()?;

    // Resolve the golden set. Default: `<root>/benches/ranking_golden/queries.toml`
    // when running inside the vex source tree; otherwise the caller must
    // pass `--bench` explicitly. We don't `include_str!` the bundled set
    // because cross-repo callers should always author their own.
    let bench_path = match bench {
        Some(p) => p,
        None => root.join("benches/ranking_golden/queries.toml"),
    };
    if !bench_path.exists() {
        bail!(
            "golden set not found at {}\n\nPass `--bench <PATH>` or run from \
             the vex source tree. See docs/RANKING-EVAL.md for the schema.",
            bench_path.display()
        );
    }

    let set = crate::eval::harness::GoldenSet::from_path(&bench_path)?;

    // Eval consumes whatever index already exists at `root`. We use the
    // same staleness/bootstrap helper every other read-side command
    // uses; passing `false` for both auto-update flags keeps the
    // command non-destructive — eval surfaces "no index" as an error
    // instead of silently rebuilding.
    let index_path = ensure_index_ready(
        &root,
        /*auto_update_flag=*/ false,
        /*no_stale_check=*/ true,
        /*needs_semantic=*/ false,
        local_cache_active,
        cfg,
    )?;
    let reader = IndexReader::open(&index_path).context("open index")?;

    let report = crate::eval::harness::run(&reader, &set)?;

    // JSON mode is opt-in via `--json` so the default --format=text
    // experience stays human-readable. The global `--format json`
    // also flips the switch for tooling consistency with other
    // subcommands.
    let emit_json = json || matches!(format, OutputFormat::Json);
    if emit_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        crate::eval::harness::print_text_report(&report);
    }

    if report.mean_ndcg < min_ndcg {
        // Non-zero exit so CI / shell scripts can branch on the
        // threshold. Use anyhow::bail so the error message surfaces
        // via the standard CLI error formatter.
        bail!(
            "mean nDCG@{} {:.4} dropped below --min-ndcg threshold {:.4}",
            report.k,
            report.mean_ndcg,
            min_ndcg,
        );
    }
    Ok(())
}
