//! `vex history <Symbol>` — CLI surface for two paths:
//!
//! 1. **Indexed path** (Phase 14.8 Step 4c): when an
//!    `index.git_history` sidecar exists, [`HistoryReader`] resolves
//!    matches via FST in ~ms. Surfaces symbols whose name no longer
//!    appears at HEAD — the v1.16 walker can't see those because
//!    its `git grep` probe runs against the tip.
//!
//! 2. **Walker path** (v1.16 query-time): the long-form `git log`
//!    walk in [`crate::history`]. Default fallback when no sidecar
//!    exists OR when the user passes `--no-index` to force it.
//!
//! Mode selection (`HistoryMode::Auto`):
//!   - `--no-index` → walker (always)
//!   - sidecar present → indexed
//!   - sidecar absent → walker
//!
//! JSON output advertises which path served the query via
//! `_meta.vex.dev/history_mode = "indexed" | "walker"`.

use anyhow::Result;
use std::path::PathBuf;

use crate::cli::args::OutputFormat;
use crate::cli::common::CmdCtx;
use crate::history::{find_symbol_history, HistoricalSymbol, HistoryOpts};
use crate::store::git_history::HistoryReader;
use crate::util::config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryMode {
    Indexed,
    Walker,
}

impl HistoryMode {
    fn as_str(&self) -> &'static str {
        match self {
            HistoryMode::Indexed => "indexed",
            HistoryMode::Walker => "walker",
        }
    }
}

pub(crate) fn history(
    ctx: &CmdCtx,
    symbol: String,
    path: Option<PathBuf>,
    depth: Option<usize>,
    branch: Option<String>,
    limit: Option<usize>,
    no_index: bool,
) -> Result<()> {
    let raw_root = match path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    // Canonicalize so the cache subdir hash matches what the writer
    // computed in `cmd_index::index` (which calls `.canonicalize()`).
    // Without this, `/tmp/foo` (the user's CLI argument) and
    // `/private/tmp/foo` (what `cmd_index` canonicalized to before
    // hashing) map to different xxh3 cache subdirs on macOS — the
    // reader looks in the wrong dir and silently falls back to the
    // walker even though the sidecar is on disk. `canonicalize` falls
    // back to the raw path on error (non-existent dirs in tests).
    let root = raw_root.canonicalize().unwrap_or(raw_root);

    let (results, mode) = if no_index {
        (
            run_walker(&root, &symbol, depth, branch.as_deref(), limit)?,
            HistoryMode::Walker,
        )
    } else {
        // Phase 14.8: try indexed first; fall back to walker on absence.
        let sidecar_path = config::git_history_path(&root);
        match HistoryReader::open(&sidecar_path)? {
            Some(reader) => {
                // Code-reviewer MUST-FIX #2: the indexed section is
                // always built against `HEAD` at index time. A user
                // passing `--branch other` would silently get
                // HEAD-time results from the indexed path. Surface
                // this mismatch via tracing::warn! so MCP agents
                // tailing logs see it; suggest `--no-index` for the
                // branch-specific path (which actually honours
                // `--branch` via the walker).
                if let Some(b) = branch.as_deref() {
                    if b != "HEAD" {
                        tracing::warn!(
                            requested_branch = %b,
                            "phase 14.8: --branch is ignored by the indexed path \
                             (section reflects HEAD at index time). Pass --no-index \
                             to query the v1.16 walker against the requested branch."
                        );
                    }
                }
                (run_indexed(&reader, &symbol, limit), HistoryMode::Indexed)
            }
            None => (
                run_walker(&root, &symbol, depth, branch.as_deref(), limit)?,
                HistoryMode::Walker,
            ),
        }
    };

    match ctx.format {
        OutputFormat::Text => render_text(&symbol, &results),
        OutputFormat::Json => render_json(&symbol, &results, mode)?,
        OutputFormat::Compact => render_compact(&results),
    }

    Ok(())
}

fn run_walker(
    root: &std::path::Path,
    symbol: &str,
    depth: Option<usize>,
    branch: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<HistoricalSymbol>> {
    find_symbol_history(
        root,
        symbol,
        &HistoryOpts {
            depth,
            branch,
            limit,
        },
    )
}

fn run_indexed(
    reader: &HistoryReader,
    symbol: &str,
    limit: Option<usize>,
) -> Vec<HistoricalSymbol> {
    let entry_idxs = reader.find_by_name(symbol);
    let mut out = Vec::with_capacity(entry_idxs.len());
    for entry_idx in entry_idxs {
        let entry = match reader.entry(entry_idx) {
            Some(e) => e,
            None => continue,
        };
        // Pick the "newest" commit in the span as the representative
        // commit for the entry, matching the walker's contract that
        // dedup'd entries surface as the newer occurrence.
        let commit = match reader.commit(entry.last_commit_idx) {
            Some(c) => c,
            None => continue,
        };
        let blob = match reader.blob(entry.blob_idx) {
            Some(b) => b,
            None => continue,
        };

        let file_path = reader.string(entry.file_offset).to_string();
        let signature = reader.string(entry.signature_offset).to_string();
        let author = reader.string(commit.author_offset).to_string();
        let commit_date = unix_seconds_to_iso_date(commit.date_unix_seconds);

        out.push(HistoricalSymbol {
            commit_sha: hex_string(&commit.sha),
            commit_date,
            author,
            file_path,
            blob_sha: hex_string(&blob.sha),
            line: entry.line,
            signature,
            kind: kind_label(entry.kind),
        });
    }

    if let Some(cap) = limit {
        out.truncate(cap);
    }
    out
}

fn hex_string(sha: &[u8; 20]) -> String {
    let mut s = String::with_capacity(40);
    for b in sha {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Howard Hinnant's civil-date algorithm — unix seconds → "YYYY-MM-DD".
/// Pure arithmetic, no dependencies. Matches the walker's `%cs` format.
fn unix_seconds_to_iso_date(ts: u32) -> String {
    let days = (ts / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = z - era * 146_097; // already i64
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y_civil = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (if m <= 2 { y_civil + 1 } else { y_civil }) as i32;
    format!("{:04}-{:02}-{:02}", y, m, d)
}

fn kind_label(kind_byte: u8) -> String {
    use crate::index::symbols::SymbolKind;
    // Phase 14.8 H4 contract: `entry.kind` is a `SymbolKind`
    // discriminant. Renumbering variants requires bumping
    // HISTORY_SECTION_VERSION + CACHE_FORMAT_VERSION (see
    // docs/RELEASING.md). Code-reviewer MUST-FIX #5: delegate to the
    // canonical `TryFrom<u8>` impl so the indexed path produces
    // labels identical to the walker (and to every other vex command
    // that round-trips a kind). Out-of-range bytes fall back to
    // "unknown" — the v17 v1 contract pins 0..=13 only.
    SymbolKind::try_from(kind_byte)
        .map(|k| k.as_str().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
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

fn render_json(symbol: &str, results: &[HistoricalSymbol], mode: HistoryMode) -> Result<()> {
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
            "vex.dev/history_mode": mode.as_str(),
        },
        "results": {
            "items": items,
        }
    });
    println!("{}", serde_json::to_string_pretty(&envelope)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_seconds_to_iso_date_known_values() {
        // 2024-03-12 00:00:00 UTC = 1710201600
        assert_eq!(unix_seconds_to_iso_date(1_710_201_600), "2024-03-12");
        // 1970-01-01 = epoch
        assert_eq!(unix_seconds_to_iso_date(0), "1970-01-01");
        // 1_780_000_000s = 2026-05-28 20:26:40 UTC → date is 28th
        assert_eq!(unix_seconds_to_iso_date(1_780_000_000), "2026-05-28");
        // Leap-year boundary: 2024-02-29 23:59:59 UTC
        assert_eq!(unix_seconds_to_iso_date(1_709_251_199), "2024-02-29");
    }
}
