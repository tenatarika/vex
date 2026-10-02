//! `vex modules` (hidden alias `clusters`) — list the symbol clusters
//! ("de-facto modules") stored in the v9 cluster section, or look up the
//! cluster of one symbol (`docs/V9-FORMAT.md` §4.1, §4.2, §13 Q7).
//!
//! The cluster section is read through the lazy
//! [`ClusterSectionReader`](crate::store::cluster_section::ClusterSectionReader):
//!
//! - no COMPUTED section (pre-v9 index, `--no-clusters`) → exit 1,
//!   `empty_reason: clusters_not_built`, stderr hint;
//! - COMPUTED header but the reader refuses it (semantic corruption) → a
//!   handler error, exit 2 — only this command ever pays for that;
//! - structural corruption already fails `IndexReader::open` (exit 2).
//!
//! Scope filters apply to *members*: a cluster is shown iff at least one
//! member is in scope, and its displayed `size` is the live in-scope count
//! (`size_at_build` keeps the frozen build-time count). Cluster staleness
//! is reported as `stale` / `new_since_build` in the payload and a `!` line
//! in text; it is deliberately NOT folded into `_meta.vex.dev/stale`, which
//! means "index older than the working tree".

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use super::args::{ModulesSort, OutputFormat, ScopeArgs};
use super::cmd_implementations::resolve_name_to_indices;
use super::common::{resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use super::scope::PathScope;
use crate::index::symbols::SymbolKind;
use crate::protocol::capabilities;
use crate::store::cluster_section::{ClusterRecordView, ClusterSectionReader, ClusterStatus};
use crate::store::reader::IndexReader;
use crate::util::config::{self, VexConfig};
use crate::workspace;

/// `--members` default when a SYMBOL is given (§4.1).
const SYMBOL_MODE_MEMBERS: usize = 25;

/// Parsed CLI arguments, bundled so the dispatch arm stays short.
pub(crate) struct ModulesArgs {
    pub symbol: Option<String>,
    pub path: Option<PathBuf>,
    pub limit: usize,
    pub min_size: usize,
    pub members: Option<usize>,
    pub sort: ModulesSort,
    pub auto_update: bool,
    pub no_stale_check: bool,
    pub scope: ScopeArgs,
    pub workspace: bool,
}

/// Everything `collect` needs besides the reader.
struct Query<'a> {
    symbol: Option<&'a str>,
    limit: usize,
    min_size: usize,
    members: usize,
    sort: ModulesSort,
    scope: &'a PathScope,
}

#[derive(Debug, Serialize)]
struct HubOut {
    name: String,
    path: String,
    line: u32,
}

#[derive(Debug, Serialize)]
struct MemberOut {
    name: String,
    path: String,
    line: u32,
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct ClusterOut {
    id: u32,
    label: String,
    size: u32,
    size_at_build: u32,
    cohesion: f64,
    internal_weight: u32,
    cut_weight: u32,
    hubs: Vec<HubOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    members: Option<Vec<MemberOut>>,
}

#[derive(Debug, Serialize)]
struct SymbolOut {
    name: String,
    path: String,
    line: u32,
    kind: &'static str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cluster_id: Option<u32>,
}

/// One repo's (or the single project's) answer. Field order is the JSON
/// key order; every key is additive per `docs/PROTOCOL-EVOLUTION.md`.
#[derive(Debug, Serialize)]
struct ModulesReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    algorithm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<String>,
    stale: bool,
    new_since_build: usize,
    /// Clusters in the section (before any filter).
    total_clusters: usize,
    unclustered: usize,
    not_eligible: usize,
    /// Clusters passing `--min-size` and the scope filters, before
    /// `--limit` (list mode only; equals `clusters.len()` in symbol mode).
    matching_clusters: usize,
    clusters: Vec<ClusterOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: Option<Vec<SymbolOut>>,
    /// Symbol mode only: how many symbols matched when `--limit` capped
    /// `symbol` (and hence the clusters shown).
    #[serde(skip_serializing_if = "Option::is_none")]
    symbols_total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_reason: Option<&'static str>,
}

impl ModulesReport {
    fn empty(reason: &'static str) -> Self {
        Self {
            algorithm: None,
            resolution: None,
            stale: false,
            new_since_build: 0,
            total_clusters: 0,
            unclustered: 0,
            not_eligible: 0,
            matching_clusters: 0,
            clusters: Vec::new(),
            symbol: None,
            symbols_total: None,
            empty_reason: Some(reason),
        }
    }
}

#[derive(Debug, Serialize)]
struct RepoReport {
    repo: String,
    #[serde(flatten)]
    report: ModulesReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    stale_reason: Option<String>,
}

pub(crate) fn modules(ctx: &CmdCtx<'_>, args: ModulesArgs) -> Result<()> {
    let path_scope = PathScope::from_args(&args.scope.include, &args.scope.exclude)?;
    let members = args.members.unwrap_or(if args.symbol.is_some() {
        SYMBOL_MODE_MEMBERS
    } else {
        0
    });
    let query = Query {
        symbol: args.symbol.as_deref(),
        limit: args.limit,
        min_size: args.min_size,
        members,
        sort: args.sort,
        scope: &path_scope,
    };

    if args.workspace {
        return modules_workspace(ctx, &args, &query);
    }

    // Canonicalize up front: cache-path writer/reader symmetry (see the
    // matching note in `cmd_subtypes.rs`).
    let root = resolve_root(args.path.clone())?
        .canonicalize()
        .context("canonicalize root")?;
    let report = modules_in_root(
        &root,
        ctx.cfg,
        ctx.local_cache_active,
        &query,
        args.auto_update,
        args.no_stale_check,
    )?;

    if let Some(reason) = report.empty_reason {
        crate::cli::exit_code::signal_no_results();
        eprintln!("{}", hint_for(reason, query.symbol));
    }

    match ctx.format {
        OutputFormat::Json => print_envelope(
            &report,
            capabilities::current(),
            super::output::default_meta_for(&root),
        ),
        OutputFormat::Text => print!(
            "{}",
            render_text(&report, query.min_size, query.symbol.is_some())
        ),
        OutputFormat::Compact => print!("{}", render_compact(&report)),
    }
    Ok(())
}

fn modules_in_root(
    root: &Path,
    cfg: &VexConfig,
    local_cache_active: bool,
    query: &Query<'_>,
    auto_update: bool,
    no_stale_check: bool,
) -> Result<ModulesReport> {
    let index_path = ensure_index_ready(
        root,
        auto_update,
        no_stale_check,
        false,
        local_cache_active,
        cfg,
    )?;
    // A structurally corrupt cluster section fails here (exit 2, §7).
    let reader = IndexReader::open(&index_path).context("open index")?;
    collect(&reader, query)
}

/// `vex modules --workspace`: per-member answer grouped by repo, like
/// `reachable_workspace`. Clusters never span members (cross-member edges
/// live in no single index); `--limit` applies per member.
fn modules_workspace(ctx: &CmdCtx<'_>, args: &ModulesArgs, query: &Query<'_>) -> Result<()> {
    let start_dir = resolve_root(args.path.clone())?;
    let ws = workspace::Workspace::find_and_load(&start_dir)?;
    let base = ws.base().to_path_buf();

    crate::cli::stale_signal::reset();
    let mut per_repo: Vec<RepoReport> = Vec::with_capacity(ws.members.len());
    let mut any = false;
    for m in &ws.members {
        let member_cfg = config::load_config(&m.root)?;
        let report = modules_in_root(
            &m.root,
            &member_cfg,
            config::skip_hash_for(&m.root),
            query,
            args.auto_update,
            args.no_stale_check,
        )?;
        let stale_reason = crate::cli::stale_signal::take();
        match report.empty_reason {
            Some(reason) => eprintln!("[{}] {}", m.display_name, hint_for(reason, query.symbol)),
            None => any = true,
        }
        per_repo.push(RepoReport {
            repo: m.display_name.clone(),
            report,
            stale_reason,
        });
    }
    if !any {
        crate::cli::exit_code::signal_no_results();
    }

    match ctx.format {
        OutputFormat::Json => print_envelope(
            serde_json::json!({
                "workspace": ws.file.to_string_lossy(),
                "repos": per_repo,
            }),
            capabilities::current(),
            super::output::default_meta_for(&base),
        ),
        OutputFormat::Text => {
            for r in &per_repo {
                println!("── {} ──", r.repo);
                if let Some(reason) = &r.stale_reason {
                    eprintln!("  (stale: {reason})");
                }
                print!(
                    "{}",
                    render_text(&r.report, query.min_size, query.symbol.is_some())
                );
            }
        }
        OutputFormat::Compact => {
            for r in &per_repo {
                println!("repo {}", r.repo);
                print!("{}", render_compact(&r.report));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------

/// Path/line/kind of a symbol, resolved from its record.
struct SymView<'r> {
    name: &'r str,
    path: &'r str,
    line: u32,
    kind: &'static str,
}

fn sym_view(reader: &IndexReader, sym_idx: u32) -> Option<SymView<'_>> {
    let rec = reader.symbol(sym_idx as usize)?;
    Some(SymView {
        name: reader.read_string(rec.name_offset),
        path: reader.read_string(rec.file_offset),
        line: rec.line,
        kind: SymbolKind::try_from(rec.kind).map_or("unknown", |k| k.as_str()),
    })
}

/// Memoised "is this symbol's file in scope" check (one glob match per file).
struct ScopeMemo<'a> {
    scope: &'a PathScope,
    by_file: HashMap<u32, bool>,
}

impl<'a> ScopeMemo<'a> {
    fn new(scope: &'a PathScope) -> Self {
        Self {
            scope,
            by_file: HashMap::new(),
        }
    }

    fn accepts(&mut self, reader: &IndexReader, sym_idx: u32) -> bool {
        if self.scope.is_empty() {
            return true;
        }
        let Some(rec) = reader.symbol(sym_idx as usize) else {
            return false;
        };
        *self
            .by_file
            .entry(rec.file_offset)
            .or_insert_with(|| self.scope.accept(reader.read_string(rec.file_offset)))
    }
}

fn collect(reader: &IndexReader, q: &Query<'_>) -> Result<ModulesReport> {
    if !reader.has_clusters() {
        return Ok(ModulesReport::empty("clusters_not_built"));
    }
    let Some(cr) = reader.cluster_section_reader() else {
        bail!(
            "the cluster section of this index is corrupted (failed validation). \
             Re-run `vex index` to rebuild it."
        );
    };
    let summary = cr.summary();
    let mut report = ModulesReport {
        algorithm: Some(format!("leiden-cpm/{}", summary.algo_version)),
        resolution: Some(format!("{}/{}", summary.resolution.0, summary.resolution.1)),
        stale: summary.stale,
        new_since_build: summary.new_count,
        total_clusters: summary.k,
        unclustered: summary.unclustered,
        not_eligible: summary.not_eligible,
        matching_clusters: 0,
        clusters: Vec::new(),
        symbol: None,
        symbols_total: None,
        empty_reason: None,
    };
    match q.symbol {
        Some(name) => collect_symbol(reader, &cr, q, name, &mut report),
        None => collect_list(reader, &cr, q, &mut report),
    }
    Ok(report)
}

/// One pass over `assign`: live in-scope member lists, indexed by cluster
/// ordinal (length k). `wanted` (length k) restricts which ordinals are
/// tracked (`None` = all).
fn scan_members(
    reader: &IndexReader,
    cr: &ClusterSectionReader<'_>,
    memo: &mut ScopeMemo<'_>,
    wanted: Option<&[bool]>,
) -> Vec<Vec<u32>> {
    let mut out: Vec<Vec<u32>> = vec![Vec::new(); cr.k()];
    for s in 0..reader.symbol_count() as u32 {
        let ClusterStatus::Clustered(ord) = cr.status(s) else {
            continue;
        };
        if wanted.is_some_and(|w| !w.get(ord as usize).copied().unwrap_or(false)) {
            continue;
        }
        if memo.accepts(reader, s) {
            out[ord as usize].push(s);
        }
    }
    out
}

fn collect_list(
    reader: &IndexReader,
    cr: &ClusterSectionReader<'_>,
    q: &Query<'_>,
    report: &mut ModulesReport,
) {
    let mut memo = ScopeMemo::new(q.scope);
    let live = scan_members(reader, cr, &mut memo, None);
    let mut rows: Vec<(u32, ClusterRecordView<'_>, Vec<u32>)> = live
        .into_iter()
        .enumerate()
        .filter(|(_, members)| !members.is_empty() && members.len() >= q.min_size)
        .filter_map(|(ord, members)| Some((ord as u32, cr.record(ord)?, members)))
        .collect();
    match q.sort {
        ModulesSort::Size => rows.sort_by(|a, b| b.2.len().cmp(&a.2.len()).then(a.0.cmp(&b.0))),
        ModulesSort::Cohesion => rows.sort_by(|a, b| {
            cohesion_desc(
                (a.1.internal_weight, a.1.cut_weight),
                (b.1.internal_weight, b.1.cut_weight),
            )
            .then(a.0.cmp(&b.0))
        }),
    }
    report.matching_clusters = rows.len();
    report.clusters = rows
        .into_iter()
        .take(q.limit)
        .map(|(ord, rec, members)| cluster_out(reader, &mut memo, ord, &rec, members, q.members))
        .collect();
    if report.matching_clusters == 0 {
        report.empty_reason = Some(if report.total_clusters == 0 {
            "no_clusters_found"
        } else {
            "filtered_all"
        });
    }
}

fn collect_symbol(
    reader: &IndexReader,
    cr: &ClusterSectionReader<'_>,
    q: &Query<'_>,
    name: &str,
    report: &mut ModulesReport,
) {
    let mut idxs = resolve_name_to_indices(reader, name);
    idxs.sort_unstable();
    idxs.dedup();
    if idxs.is_empty() {
        report.symbol = Some(Vec::new());
        report.empty_reason = Some("symbol_not_found");
        return;
    }
    let mut memo = ScopeMemo::new(q.scope);
    idxs.retain(|&i| memo.accepts(reader, i));
    if idxs.is_empty() {
        report.symbol = Some(Vec::new());
        report.empty_reason = Some("filtered_all");
        return;
    }
    if idxs.len() > q.limit {
        report.symbols_total = Some(idxs.len());
        idxs.truncate(q.limit);
    }

    let mut symbols = Vec::with_capacity(idxs.len());
    let mut ords: Vec<u32> = Vec::new();
    let mut wanted = vec![false; cr.k()];
    for &i in &idxs {
        let Some(v) = sym_view(reader, i) else {
            continue;
        };
        let (status, cluster_id) = match cr.status(i) {
            ClusterStatus::Clustered(ord) => {
                if let Some(w) = wanted.get_mut(ord as usize) {
                    if !*w {
                        *w = true;
                        ords.push(ord);
                    }
                }
                ("clustered", Some(ord))
            }
            ClusterStatus::Unclustered => ("unclustered", None),
            ClusterStatus::NotEligible => ("not_eligible", None),
            ClusterStatus::New => ("new_since_build", None),
        };
        symbols.push(SymbolOut {
            name: v.name.to_string(),
            path: v.path.to_string(),
            line: v.line,
            kind: v.kind,
            status,
            cluster_id,
        });
    }

    let mut live = scan_members(reader, cr, &mut memo, Some(&wanted));
    report.clusters = ords
        .iter()
        .filter_map(|&ord| {
            let rec = cr.record(ord as usize)?;
            let members = std::mem::take(&mut live[ord as usize]);
            Some(cluster_out(
                reader, &mut memo, ord, &rec, members, q.members,
            ))
        })
        .collect();
    report.matching_clusters = report.clusters.len();
    if report.clusters.is_empty() {
        report.empty_reason = Some("symbol_unclustered");
    }
    report.symbol = Some(symbols);
}

fn cluster_out(
    reader: &IndexReader,
    memo: &mut ScopeMemo<'_>,
    ord: u32,
    rec: &ClusterRecordView<'_>,
    members: Vec<u32>,
    want_members: usize,
) -> ClusterOut {
    let size = members.len() as u32;
    // Hubs go through the same scope as members.
    let hubs = rec
        .hubs
        .iter()
        .flatten()
        .filter(|&&h| memo.accepts(reader, h))
        .filter_map(|&h| sym_view(reader, h))
        .map(|v| HubOut {
            name: v.name.to_string(),
            path: v.path.to_string(),
            line: v.line,
        })
        .collect();
    let members_out = (want_members > 0).then(|| {
        // Borrowed sort keys; only the top-N are materialised.
        let mut keyed: Vec<(SymView<'_>, u32)> = members
            .iter()
            .filter_map(|&s| sym_view(reader, s).map(|v| (v, s)))
            .collect();
        keyed.sort_by(|(a, ai), (b, bi)| {
            (a.path, a.line, a.name, ai).cmp(&(b.path, b.line, b.name, bi))
        });
        keyed
            .into_iter()
            .take(want_members)
            .map(|(v, _)| MemberOut {
                name: v.name.to_string(),
                path: v.path.to_string(),
                line: v.line,
                kind: v.kind,
            })
            .collect()
    });
    ClusterOut {
        id: ord,
        label: rec.label.to_string(),
        size,
        size_at_build: rec.size,
        cohesion: round4(cohesion(rec.internal_weight, rec.cut_weight)),
        internal_weight: rec.internal_weight,
        cut_weight: rec.cut_weight,
        hubs,
        members: members_out,
    }
}

// ---------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------

/// `internal / (internal + cut)`; 0 when both are 0.
fn cohesion(internal: u32, cut: u32) -> f64 {
    let total = u64::from(internal) + u64::from(cut);
    if total == 0 {
        0.0
    } else {
        f64::from(internal) / total as f64
    }
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// Descending cohesion order by exact integer cross-multiplication, so the
/// order never depends on float rounding.
fn cohesion_desc(a: (u32, u32), b: (u32, u32)) -> std::cmp::Ordering {
    let frac = |(i, c): (u32, u32)| {
        let t = u128::from(i) + u128::from(c);
        if t == 0 {
            (0u128, 1u128)
        } else {
            (u128::from(i), t)
        }
    };
    let (an, ad) = frac(a);
    let (bn, bd) = frac(b);
    (bn * ad).cmp(&(an * bd))
}

fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn hint_for(reason: &str, symbol: Option<&str>) -> String {
    let sym = symbol.unwrap_or("the symbol");
    match reason {
        "clusters_not_built" => "hint: this index has no symbol clusters — run `vex index` \
             (older indexes and `vex index --no-clusters` skip them)"
            .to_string(),
        "symbol_not_found" => {
            format!("hint: symbol \"{sym}\" not found in the index — try `vex search {sym}`")
        }
        "symbol_unclustered" => format!(
            "hint: \"{sym}\" is in no cluster (isolated, not eligible, or added since the \
             last `vex index`)"
        ),
        "no_clusters_found" => {
            "hint: the index has a cluster section but it holds no clusters".to_string()
        }
        _ => "hint: no cluster matches the filters — try a lower --min-size or different \
              --include/--exclude"
            .to_string(),
    }
}

// ---------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------

fn hub_names(c: &ClusterOut, sep: &str) -> String {
    c.hubs
        .iter()
        .map(|h| h.name.as_str())
        .collect::<Vec<_>>()
        .join(sep)
}

fn stale_line(report: &ModulesReport) -> String {
    if report.new_since_build > 0 {
        format!(
            "! clusters are frozen at the last `vex index`; {} symbols added since are not \
             clustered — run `vex index` to recompute",
            thousands(report.new_since_build)
        )
    } else {
        "! clusters are frozen at the last `vex index`; edits since are not reflected — run \
         `vex index` to recompute"
            .to_string()
    }
}

fn render_text(report: &ModulesReport, min_size: usize, symbol_mode: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let (Some(algo), Some(res)) = (&report.algorithm, &report.resolution) else {
        return out; // no cluster data: the stderr hint carries the message
    };

    if let Some(symbols) = &report.symbol {
        for s in symbols {
            let tail = match s.cluster_id {
                Some(id) => format!("clustered in #{id}"),
                None => s.status.to_string(),
            };
            let _ = writeln!(
                out,
                "{} ({})  {}:{}  — {tail}",
                s.name, s.kind, s.path, s.line
            );
        }
    } else if !symbol_mode {
        let shown = report.clusters.len();
        let showing = if shown < report.matching_clusters {
            format!(", showing {shown}")
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "Modules — {algo} γ={res} · {} clusters (≥{min_size}{showing}) · {} unclustered · {} not eligible",
            thousands(report.matching_clusters),
            thousands(report.unclustered),
            thousands(report.not_eligible),
        );
    }

    let id_w = report
        .clusters
        .iter()
        .map(|c| c.id.to_string().len())
        .max()
        .unwrap_or(1);
    let label_w = report
        .clusters
        .iter()
        .map(|c| c.label.chars().count())
        .max()
        .unwrap_or(0);
    let size_w = report
        .clusters
        .iter()
        .map(|c| c.size.to_string().len())
        .max()
        .unwrap_or(1);
    for c in &report.clusters {
        let _ = writeln!(
            out,
            "  #{:<id_w$} {:<label_w$} {:>size_w$} symbols  cohesion {:.2}  hubs: {}",
            c.id,
            c.label,
            c.size,
            c.cohesion,
            hub_names(c, ", "),
        );
        if let Some(members) = &c.members {
            for m in members {
                let _ = writeln!(out, "      {} ({})  {}:{}", m.name, m.kind, m.path, m.line);
            }
            if (members.len() as u32) < c.size {
                let _ = writeln!(out, "      … {} more", c.size as usize - members.len());
            }
        }
    }
    if report.stale {
        let _ = writeln!(out, "{}", stale_line(report));
    }
    out
}

fn render_compact(report: &ModulesReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if let (Some(algo), Some(res), None) = (&report.algorithm, &report.resolution, &report.symbol) {
        let _ = writeln!(
            out,
            "modules {algo} {res} matching={} total={} unclustered={} not_eligible={}",
            report.matching_clusters,
            report.total_clusters,
            report.unclustered,
            report.not_eligible,
        );
    }
    if let Some(symbols) = &report.symbol {
        for s in symbols {
            let cid = s.cluster_id.map_or(String::new(), |c| format!(" {c}"));
            let _ = writeln!(
                out,
                "symbol {} {}:{} {}{cid}",
                s.name, s.path, s.line, s.status
            );
        }
    }
    for c in &report.clusters {
        let _ = writeln!(
            out,
            "cluster {} {} {:.2} {} hubs={}",
            c.id,
            c.size,
            c.cohesion,
            c.label,
            hub_names(c, ","),
        );
        for m in c.members.iter().flatten() {
            let _ = writeln!(out, "member {} {} {}:{}", c.id, m.name, m.path, m.line);
        }
    }
    if report.stale {
        let _ = writeln!(out, "{}", stale_line(report));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn cohesion_is_zero_when_no_weight_at_all() {
        assert_eq!(cohesion(0, 0), 0.0);
        assert_eq!(cohesion(3, 1), 0.75);
        assert_eq!(cohesion(5, 0), 1.0);
        assert_eq!(cohesion(0, 5), 0.0);
    }

    #[test]
    fn cohesion_order_is_exact_and_descending() {
        // 3/4 > 2/3 > 0/0 == 0/9
        assert_eq!(cohesion_desc((3, 1), (2, 1)), Ordering::Less);
        assert_eq!(cohesion_desc((2, 1), (3, 1)), Ordering::Greater);
        assert_eq!(cohesion_desc((0, 0), (0, 9)), Ordering::Equal);
        assert_eq!(cohesion_desc((1, 0), (u32::MAX, u32::MAX)), Ordering::Less);
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_203), "1,203");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn empty_report_renders_nothing_and_omits_optional_keys() {
        let r = ModulesReport::empty("clusters_not_built");
        assert_eq!(render_text(&r, 3, false), "");
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["empty_reason"], "clusters_not_built");
        assert!(j.get("symbol").is_none());
        assert_eq!(j["clusters"], serde_json::json!([]));
    }
}
