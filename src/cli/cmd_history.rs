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
//! Phase 14.9 Tier A: post-filter via [`HistoryFilter`] (date / author
//! / kind) before output; `--author` is walker-only (rejected with a
//! hard error on the indexed path). JSON output uses the standard
//! Phase 13 [`ResponseEnvelope`](crate::protocol::ResponseEnvelope)
//! shape via [`crate::cli::output::print_envelope`]; the legacy
//! `results: { items: [...] }` nesting is gone (breaking change for
//! MCP consumers — documented in v1.16.0 release notes).

use anyhow::{anyhow, Result};
use std::path::PathBuf;

use crate::cli::args::OutputFormat;
use crate::cli::common::CmdCtx;
use crate::cli::output;
use crate::history::{
    find_symbol_history, parse_iso_date, HistoricalSymbol, HistoryFilter, HistoryOpts,
};
use crate::store::git_history::HistoryReader;
use crate::util::config;

/// Flattened CLI arguments for `vex history <Symbol>`. Mirrors the
/// `Commands::History` clap variant 1:1; carved out as a struct so
/// the dispatch site doesn't keep growing positional arguments and
/// so internal call sites (tests, MCP shim) can build an instance
/// without going through clap.
#[derive(Debug, Clone, Default)]
pub struct HistoryArgs {
    pub symbol: String,
    pub path: Option<PathBuf>,
    pub depth: Option<usize>,
    pub branch: Option<String>,
    pub limit: Option<usize>,
    pub no_index: bool,
    pub since: Option<String>,
    pub until: Option<String>,
    pub author: Option<String>,
    pub kind: Option<String>,
    pub diff: bool,
}

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

pub(crate) fn history(ctx: &CmdCtx, args: HistoryArgs) -> Result<()> {
    let HistoryArgs {
        symbol,
        path,
        depth,
        branch,
        limit,
        no_index,
        since,
        until,
        author,
        kind,
        diff,
    } = args;

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

    // Build + validate the filter once. Date parsing surfaces a clean
    // error before any walker / index work happens.
    let filter = build_filter(since.as_deref(), until.as_deref(), author.clone(), kind)?;

    // When a filter is active, don't push `--limit` down to the
    // walker / indexed path — we need the full pre-filter set so the
    // limit caps post-filter output, not raw rows that get filtered
    // away. The trade-off is more walker work; acceptable because
    // `--limit` without filters is the common case and stays fast.
    let inner_limit = if filter.is_active() { None } else { limit };

    // Pre-resolve mode so we can reject --author on the indexed path
    // BEFORE doing any walker work. Otherwise an --author query with a
    // populated sidecar would run, return rows, then error after the
    // fact.
    let mode = resolve_mode(no_index, &root)?;

    if mode == HistoryMode::Indexed && author.is_some() {
        eprintln!(
            "error: `vex history --author` is walker-only — the Phase 14.8 \
             history sidecar does not record commit author. Re-run with \
             `--no-index` to force the walker (slower but author-aware). \
             Phase 14.10 will populate author on the indexed path."
        );
        return Err(anyhow!(
            "--author requires --no-index against Phase 14.8 sidecars"
        ));
    }

    let mut results = match mode {
        HistoryMode::Indexed => {
            // Warn when --branch is passed against the indexed path
            // (same surface as before — section reflects HEAD at index
            // time).
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
            run_indexed(&root, &symbol, inner_limit)?
        }
        HistoryMode::Walker => run_walker(&root, &symbol, depth, branch.as_deref(), inner_limit)?,
    };

    // Apply post-filter. When inactive this is a no-op iterator.
    if filter.is_active() {
        let filtered: Vec<HistoricalSymbol> = filter.apply(&results).cloned().collect();
        results = filtered;
        if let Some(cap) = limit {
            results.truncate(cap);
        }
    }

    match ctx.format {
        OutputFormat::Text => render_text(&symbol, &results, diff),
        OutputFormat::Json => render_json(&root, &symbol, &results, mode, diff)?,
        OutputFormat::Compact => render_compact(&results),
    }

    Ok(())
}

/// Pick which path will service the query without doing the work.
/// Probes the sidecar path with `HistoryReader::open` (cheap — mmap
/// header read only) and returns `Walker` on absence.
fn resolve_mode(no_index: bool, root: &std::path::Path) -> Result<HistoryMode> {
    if no_index {
        return Ok(HistoryMode::Walker);
    }
    let sidecar_path = config::git_history_path(root);
    match HistoryReader::open(&sidecar_path)? {
        Some(_) => Ok(HistoryMode::Indexed),
        None => Ok(HistoryMode::Walker),
    }
}

fn build_filter(
    since: Option<&str>,
    until: Option<&str>,
    author: Option<String>,
    kind: Option<String>,
) -> Result<HistoryFilter> {
    let since_iso = since
        .map(parse_iso_date)
        .transpose()
        .map_err(|e| anyhow!(e))?;
    let until_iso = until
        .map(parse_iso_date)
        .transpose()
        .map_err(|e| anyhow!(e))?;
    Ok(HistoryFilter {
        since_iso,
        until_iso,
        author,
        kind: kind.map(|k| k.to_ascii_lowercase()),
    })
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
    root: &std::path::Path,
    symbol: &str,
    limit: Option<usize>,
) -> Result<Vec<HistoricalSymbol>> {
    let sidecar_path = config::git_history_path(root);
    let reader = HistoryReader::open(&sidecar_path)?
        .ok_or_else(|| anyhow!("history sidecar disappeared between mode probe and run"))?;

    // Phase 14.9 Tier B.8: prefix-FST fallback when exact lookup
    // misses on an identifier-shaped query. Cap at 50 distinct FST
    // keys to bound worst-case work; lexicographic order, not
    // relevance.
    let entry_idxs = reader.find_by_name_or_prefix(symbol, 50);
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
    Ok(out)
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

fn render_text(symbol: &str, results: &[HistoricalSymbol], diff: bool) {
    if results.is_empty() {
        println!("No history found for `{symbol}` — not in any indexed file at the chosen tip.");
        return;
    }
    println!("History for `{symbol}` ({} versions):\n", results.len());

    if diff {
        // Group consecutive entries by (symbol_name, kind), render
        // entry[0] full + entry[1..N] as unified diff against
        // entry[i-1]. The slice is in newest-first order — render
        // oldest-first within each group so the diff reads naturally.
        let groups = crate::history::diff::group_by_kind(results);
        for group in groups {
            for (idx, entry) in group.iter().enumerate() {
                print_entry_header(entry);
                if idx == 0 {
                    if !entry.signature.is_empty() {
                        println!("    {}", entry.signature);
                    }
                } else {
                    let prev = group[idx - 1];
                    let diff_text = crate::history::diff::render_unified_diff(prev, entry);
                    for line in diff_text.lines() {
                        println!("    {line}");
                    }
                }
                println!("    blob {}", entry.blob_sha);
                println!();
            }
        }
        return;
    }

    for r in results {
        print_entry_header(r);
        if !r.signature.is_empty() {
            println!("    {}", r.signature);
        }
        println!("    blob {}", r.blob_sha);
        println!();
    }
}

fn print_entry_header(r: &HistoricalSymbol) {
    println!(
        "  {short_sha}  {date}  {author}",
        short_sha = &r.commit_sha[..8.min(r.commit_sha.len())],
        date = r.commit_date,
        author = r.author,
    );
    println!("    {}:{}  {}", r.file_path, r.line, r.kind);
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

fn render_json(
    root: &std::path::Path,
    symbol: &str,
    results: &[HistoricalSymbol],
    mode: HistoryMode,
    diff: bool,
) -> Result<()> {
    // Phase 14.9 Tier A.5: port from the hand-rolled `json!({...})`
    // wrapper to the standard Phase 13 envelope. BREAKING for any MCP
    // consumer that read `results.items[*]` instead of `results[*]`.
    let items: Vec<serde_json::Value> = if diff {
        crate::history::diff::render_json_items(results)
    } else {
        results
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
            .collect()
    };

    let _ = symbol; // The symbol used to live in `vex.dev/query_symbol`; dropped in the port (caller already knows what it queried).

    let mut meta = output::default_meta_for(root);
    meta.history_mode = Some(mode.as_str());

    output::print_envelope(items, crate::protocol::capabilities::current(), meta);
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
