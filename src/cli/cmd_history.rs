//! `vex history <Symbol>` — CLI surface for the query-time git-log
//! walker in [`crate::history`]. Renders the [`HistoricalSymbol`]
//! sequence as plain text (default), JSON envelope, or compact one-
//! per-line.

use anyhow::Result;
use std::path::PathBuf;

use crate::cli::args::OutputFormat;
use crate::cli::common::CmdCtx;
use crate::history::{find_symbol_history, HistoricalSymbol, HistoryOpts};

pub(crate) fn history(
    ctx: &CmdCtx,
    symbol: String,
    path: Option<PathBuf>,
    depth: Option<usize>,
    branch: Option<String>,
    limit: Option<usize>,
) -> Result<()> {
    let root = match path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };

    let opts = HistoryOpts {
        depth,
        branch: branch.as_deref(),
        limit,
    };

    let results = find_symbol_history(&root, &symbol, &opts)?;

    match ctx.format {
        OutputFormat::Text => render_text(&symbol, &results),
        OutputFormat::Json => render_json(&symbol, &results)?,
        OutputFormat::Compact => render_compact(&results),
    }

    Ok(())
}

fn render_text(symbol: &str, results: &[HistoricalSymbol]) {
    if results.is_empty() {
        println!("No history found for `{symbol}` — not in any indexed file at the chosen tip.");
        return;
    }
    println!("History for `{symbol}` ({} versions):\n", results.len());
    for r in results {
        println!(
            "  {short_sha}  {date}  {author}",
            short_sha = &r.commit_sha[..8.min(r.commit_sha.len())],
            date = r.commit_date,
            author = r.author,
        );
        println!("    {}:{}  {}", r.file_path, r.line, r.kind);
        if !r.signature.is_empty() {
            println!("    {}", r.signature);
        }
        println!("    blob {}", r.blob_sha);
        println!();
    }
}

fn render_compact(results: &[HistoricalSymbol]) {
    // One line per result, tab-separated. Matches the convention from
    // `vex search --format compact`: easy to grep/awk in agent shells.
    for r in results {
        println!(
            "{}\t{}\t{}\t{}:{}\t{}",
            &r.commit_sha[..8.min(r.commit_sha.len())],
            r.commit_date,
            r.kind,
            r.file_path,
            r.line,
            r.signature,
        );
    }
}

fn render_json(symbol: &str, results: &[HistoricalSymbol]) -> Result<()> {
    // v1 envelope shape (protocol_version + capabilities + _meta + results)
    // matches `vex search --format json` / bundle output, so MCP clients
    // can use the same envelope parser.
    let items: Vec<_> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "commit_sha": r.commit_sha,
                "commit_date": r.commit_date,
                "author": r.author,
                "file_path": r.file_path,
                "blob_sha": r.blob_sha,
                "line": r.line,
                "signature": r.signature,
                "kind": r.kind,
            })
        })
        .collect();

    let envelope = serde_json::json!({
        "protocol_version": crate::protocol::PROTOCOL_VERSION,
        "capabilities": crate::protocol::capabilities::current(),
        "_meta": {
            "vex.dev/query_symbol": symbol,
            "vex.dev/result_count": results.len(),
        },
        "results": {
            "items": items,
        }
    });
    println!("{}", serde_json::to_string_pretty(&envelope)?);
    Ok(())
}
