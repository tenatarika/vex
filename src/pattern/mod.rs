pub mod matcher;
pub mod skeleton;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::parse::language::Language;

/// A match found by the pattern matcher.
#[derive(Debug, Clone)]
pub struct PatternMatch {
    pub path: String,
    pub line: usize,
    pub matched_text: String,
    pub captures: Vec<(String, String)>, // (metavar_name, captured_text)
}

/// Whether the pattern scan used the persisted skeleton index or fell back
/// to a full live walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    Indexed,
    LiveScan,
}

/// Diagnostic trace emitted when `--why` is set.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanTrace {
    /// How files were selected for matching.
    pub mode: ScanMode,
    /// The root node-kind inferred from the pattern's leading keyword,
    /// or `None` when no keyword matched.
    pub root_kind_inferred: Option<String>,
    /// Number of files actually scanned (pre-match). In `Indexed`
    /// mode this is the post-skeleton-narrow candidate set. In
    /// `LiveScan` this equals `total_files` (no narrowing).
    pub candidate_files: usize,
    /// Total files for the language in the project (only set in
    /// `LiveScan`; in `Indexed` mode this is the count of files that
    /// had at least one skeleton of the right language).
    pub total_files: usize,
    /// Why indexed mode was skipped, if applicable.
    /// Values: `"no-index"`, `"index-open-error"`,
    /// `"no-skeleton-section"`, `"empty-section"`, `"grammar-drift"`,
    /// `"partial-section"`.
    pub fallback_reason: Option<String>,
}

/// Scan files in a directory for code matching a structural pattern.
/// Thin wrapper that discards the [`ScanTrace`]. Kept as the stable
/// public entry-point for callers that don't need trace data (Inc 8 MCP
/// tool will call this directly).
#[allow(dead_code)]
pub fn scan(
    root: &Path,
    pattern: &str,
    lang: Language,
    limit: usize,
    excludes: &[String],
) -> Result<Vec<PatternMatch>> {
    let (matches, _trace) = scan_with_mode(root, pattern, lang, limit, excludes)?;
    Ok(matches)
}

/// Scan files and return both the matches and a diagnostic trace.
pub fn scan_with_mode(
    root: &Path,
    pattern: &str,
    lang: Language,
    limit: usize,
    excludes: &[String],
) -> Result<(Vec<PatternMatch>, ScanTrace)> {
    let root = root.canonicalize().context("canonicalize root")?;

    let composite = matcher::parse_composite_pattern(pattern, lang).context("parse pattern")?;

    // 11.4 Inc 7: for OR composition (multiple disjuncts) the disjuncts
    // can target distinct kinds (`interface $N || class $N`), so a
    // single root-kind narrowing would silently drop one side. Skip
    // kind narrowing in that case — language filter still applies.
    // For pure AND (one disjunct, multiple conjuncts), the first
    // conjunct's keyword is the right narrowing signal since every
    // AND-matching file must also contain that shape.
    let root_kind = if composite.has_or() {
        None
    } else {
        infer_root_kind(pattern, lang).map(str::to_owned)
    };

    // --- Try indexed mode ---
    let index_path = crate::util::config::index_path(&root);
    if !index_path.exists() {
        return live_scan(
            &root, &composite, lang, limit, excludes, root_kind, "no-index",
        );
    }

    let reader = match crate::store::reader::IndexReader::open(&index_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "failed to open index for pattern prefilter");
            return live_scan(
                &root,
                &composite,
                lang,
                limit,
                excludes,
                root_kind,
                "index-open-error",
            );
        }
    };

    let skel_reader = match reader.pattern_skeleton_reader() {
        Some(r) => r,
        None => {
            return live_scan(
                &root,
                &composite,
                lang,
                limit,
                excludes,
                root_kind,
                "no-skeleton-section",
            );
        }
    };

    if skel_reader.is_empty() {
        return live_scan(
            &root,
            &composite,
            lang,
            limit,
            excludes,
            root_kind,
            "empty-section",
        );
    }

    // Grammar drift check.
    let live_fp = crate::store::pattern_skeletons::grammar_fingerprint_for_lang(lang);
    match skel_reader.grammar_fingerprint(lang.lang_id()) {
        None => {
            return live_scan(
                &root,
                &composite,
                lang,
                limit,
                excludes,
                root_kind,
                "grammar-drift",
            );
        }
        Some(persisted) if persisted != live_fp => {
            tracing::warn!(
                lang = lang.lang_id(),
                persisted_fp = persisted,
                live_fp,
                "grammar fingerprint mismatch — falling back to live-scan"
            );
            return live_scan(
                &root,
                &composite,
                lang,
                limit,
                excludes,
                root_kind,
                "grammar-drift",
            );
        }
        Some(_) => {} // fingerprints match → proceed indexed
    }

    // 11.4 Inc 5 (post-review): incremental builds leave skeletons empty for
    // unchanged files (mirrors `bound_refs` / `call_edges`). A partial
    // section would silently under-report matches if we ran the prefilter —
    // detect via the manifest flag set by the pipeline (`true` after full
    // `vex index`, `false` after `vex update`) and degrade to live-scan.
    // This check sits AFTER section-presence/empty/drift checks so v5 and
    // empty-section indexes report their specific reasons first.
    let manifest_path = crate::util::config::manifest_path(&root);
    let manifest_full = crate::index::manifest::Manifest::load(&manifest_path)
        .ok()
        .and_then(|m| m.pattern_index_full)
        .unwrap_or(false);
    if !manifest_full {
        return live_scan(
            &root,
            &composite,
            lang,
            limit,
            excludes,
            root_kind,
            "partial-section",
        );
    }

    // --- Indexed mode: collect candidate file paths ---
    let file_paths = reader.file_paths();
    let root_kind_ref: Option<&str> = root_kind.as_deref();

    let candidates: Vec<PathBuf> = skel_reader
        .file_ids()
        .filter_map(|file_id| {
            let path_str = file_paths.get(file_id as usize)?;

            // (a) Extension must match the requested language.
            let ext = std::path::Path::new(path_str)
                .extension()
                .and_then(|e| e.to_str())?;
            if Language::from_extension(ext) != Some(lang) {
                return None;
            }

            // (b) If we inferred a root kind, at least one skeleton must match.
            if let Some(kind) = root_kind_ref {
                let skeletons = skel_reader.skeletons_for_file(file_id);
                if !skeletons.iter().any(|sk| sk.kind == kind) {
                    return None;
                }
            }

            // Resolve to absolute path.
            let abs = if std::path::Path::new(path_str).is_absolute() {
                PathBuf::from(path_str)
            } else {
                root.join(path_str)
            };
            Some(abs)
        })
        .collect();

    let candidate_files = candidates.len();
    // "Total" for indexed mode = files of the matching language in the
    // index (the universe before kind-narrowing). file_paths is already
    // loaded, so this is essentially free.
    let total_files = file_paths
        .iter()
        .filter(|p| {
            std::path::Path::new(p.as_str())
                .extension()
                .and_then(|e| e.to_str())
                .and_then(Language::from_extension)
                == Some(lang)
        })
        .count();

    let matches: Vec<PatternMatch> = candidates
        .par_iter()
        .flat_map(|path| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            matcher::find_matches_composite(&content, &composite, &rel)
        })
        .collect();

    let matches: Vec<PatternMatch> = matches.into_iter().take(limit).collect();

    let trace = ScanTrace {
        mode: ScanMode::Indexed,
        root_kind_inferred: root_kind,
        // `candidate_files` is the count we actually parsed (after
        // skeleton + kind narrowing). `total_files` is the universe of
        // lang-matching files known to the index.
        candidate_files,
        total_files,
        fallback_reason: None,
    };

    Ok((matches, trace))
}

/// Execute a full live-scan and package the result with a trace.
#[allow(clippy::too_many_arguments)] // dispatch fan-out — each arg has a distinct role
fn live_scan(
    root: &Path,
    composite: &matcher::CompositePattern,
    lang: Language,
    limit: usize,
    excludes: &[String],
    root_kind: Option<String>,
    reason: &'static str,
) -> Result<(Vec<PatternMatch>, ScanTrace)> {
    let files = discover_lang_files(root, lang, excludes)?;
    let total_files = files.len();

    let matches: Vec<PatternMatch> = files
        .par_iter()
        .flat_map(|path| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            matcher::find_matches_composite(&content, composite, &rel)
        })
        .collect();

    let matches: Vec<PatternMatch> = matches.into_iter().take(limit).collect();
    let candidate_files = total_files;

    let trace = ScanTrace {
        mode: ScanMode::LiveScan,
        root_kind_inferred: root_kind,
        candidate_files,
        total_files,
        fallback_reason: Some(reason.to_owned()),
    };

    Ok((matches, trace))
}

/// Infer a T1-language root node-kind from the leading literal keyword in
/// `pattern` (the text before the first `$`). Returns `None` when no
/// keyword is recognisable. Stays intentionally minimal — Inc 6/7 will
/// extend when block metavars and composition arrive.
///
/// Skips leading **modifier** tokens that don't change the shape of the
/// declaration (Rust visibility `pub` / `pub(crate)` / `pub(super)`, TS
/// `export` / `export default`, Python / TS `async`). Without this,
/// `pub fn $NAME` would silently disable the prefilter.
pub(crate) fn infer_root_kind(pattern: &str, lang: Language) -> Option<&'static str> {
    // Isolate the prefix before the first `$`.
    let prefix = pattern.split('$').next().unwrap_or("").trim();

    // Strip language-specific modifier tokens off the front. We accept
    // up to two modifiers (e.g. `export default async function`) — more
    // than that is exotic enough to not warrant prefilter coverage.
    let modifiers: &[&str] = match lang {
        Language::Rust => &["async", "unsafe", "const"],
        Language::TypeScript => &[
            "export", "default", "async", "abstract", "public", "private",
        ],
        Language::Python => &["async"],
        _ => &[],
    };

    let mut rest = prefix;
    for _ in 0..2 {
        let first = rest.split_whitespace().next().unwrap_or("");
        // Rust visibility: `pub`, `pub(crate)`, `pub(super)`, `pub(self)`, `pub(in path)`.
        if lang == Language::Rust && (first == "pub" || first.starts_with("pub(")) {
            // Find the end of the modifier — either next whitespace
            // (`pub`) or the closing paren (`pub(crate)`).
            let cut = if let Some(close) = rest.find(')') {
                close + 1
            } else {
                first.len()
            };
            rest = rest[cut..].trim_start();
            continue;
        }
        if modifiers.contains(&first) {
            rest = rest[first.len()..].trim_start();
            continue;
        }
        break;
    }

    let keyword = rest.split_whitespace().next()?;

    match lang {
        Language::Rust => match keyword {
            "fn" => Some("function_item"),
            "struct" => Some("struct_item"),
            "enum" => Some("enum_item"),
            "impl" => Some("impl_item"),
            "trait" => Some("trait_item"),
            "mod" => Some("mod_item"),
            "type" => Some("type_item"),
            "const" => Some("const_item"),
            "static" => Some("static_item"),
            _ => None,
        },
        Language::TypeScript => match keyword {
            "function" => Some("function_declaration"),
            "class" => Some("class_declaration"),
            "interface" => Some("interface_declaration"),
            "type" => Some("type_alias_declaration"),
            "enum" => Some("enum_declaration"),
            _ => None,
        },
        Language::Python => match keyword {
            "def" => Some("function_definition"),
            "class" => Some("class_definition"),
            _ => None,
        },
        _ => None,
    }
}

fn discover_lang_files(root: &Path, lang: Language, excludes: &[String]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for entry in crate::util::walk::walk_builder(root, excludes)?.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();

        let matches_lang = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension)
            .is_some_and(|l| l == lang);

        if matches_lang {
            files.push(path);
        }
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ScanMode JSON serialization stability (review H3 aqa) ──────────────

    #[test]
    fn scan_mode_json_strings_are_stable() {
        // Public consumers (vex pattern --why, MCP --why in Inc 8/10.10)
        // parse these literal strings. A rename of a variant must not
        // silently change the on-wire shape.
        assert_eq!(
            serde_json::to_string(&ScanMode::Indexed).unwrap(),
            "\"indexed\""
        );
        assert_eq!(
            serde_json::to_string(&ScanMode::LiveScan).unwrap(),
            "\"live_scan\""
        );
    }

    // ── infer_root_kind edge cases (review H2 aqa, M3 rust) ────────────────

    #[test]
    fn infer_root_kind_handles_leading_whitespace() {
        assert_eq!(
            infer_root_kind("   fn $NAME()", Language::Rust),
            Some("function_item")
        );
    }

    #[test]
    fn infer_root_kind_strips_rust_visibility() {
        assert_eq!(
            infer_root_kind("pub fn $NAME()", Language::Rust),
            Some("function_item")
        );
        assert_eq!(
            infer_root_kind("pub(crate) fn $NAME()", Language::Rust),
            Some("function_item")
        );
        assert_eq!(
            infer_root_kind("pub(super) struct $S", Language::Rust),
            Some("struct_item")
        );
    }

    #[test]
    fn infer_root_kind_strips_rust_modifiers() {
        assert_eq!(
            infer_root_kind("async fn $NAME()", Language::Rust),
            Some("function_item")
        );
        // `pub` + `async` — both stripped (modifier loop runs twice).
        assert_eq!(
            infer_root_kind("pub async fn $NAME()", Language::Rust),
            Some("function_item")
        );
    }

    #[test]
    fn infer_root_kind_strips_typescript_export_and_async() {
        assert_eq!(
            infer_root_kind("export function $NAME($$$)", Language::TypeScript),
            Some("function_declaration")
        );
        assert_eq!(
            infer_root_kind("export default class $C", Language::TypeScript),
            Some("class_declaration")
        );
        assert_eq!(
            infer_root_kind("export async function $NAME()", Language::TypeScript),
            Some("function_declaration")
        );
    }

    #[test]
    fn infer_root_kind_strips_python_async() {
        assert_eq!(
            infer_root_kind("async def $NAME(self)", Language::Python),
            Some("function_definition")
        );
    }

    #[test]
    fn infer_root_kind_returns_none_when_no_leading_keyword() {
        // Pattern starts with a metavar — no kind to infer.
        assert_eq!(infer_root_kind("$X.then($Y)", Language::Rust), None);
        // Pattern starts with non-keyword identifier.
        assert_eq!(infer_root_kind("foo($X)", Language::Rust), None);
    }

    #[test]
    fn infer_root_kind_returns_none_for_non_t1_keyword() {
        // `func` is Go's function keyword, not in the Rust table.
        assert_eq!(infer_root_kind("func main()", Language::Rust), None);
        // Same keyword used with a language whose allowlist is empty (T2).
        assert_eq!(infer_root_kind("func main()", Language::Go), None);
    }

    #[test]
    fn infer_root_kind_returns_none_for_t3_language() {
        // T3 langs (Markdown, SQL, YAML, …) have no allowlist.
        assert_eq!(infer_root_kind("def $X", Language::Markdown), None);
    }
}
