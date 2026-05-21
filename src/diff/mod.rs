//! Diff-aware mode (11.2): `vex diff --base <rev>`.
//!
//! Lists symbol-level changes between an arbitrary git revision and the
//! current working tree — added, removed, moved-within-file, and
//! body-changed entries — so PR reviewers and agents can answer
//! "what symbols did this branch touch?" without scrolling through
//! a unified diff.
//!
//! The implementation walks `git diff --name-only <base>` to find files
//! that differ, then for each file fetches old content via
//! `git show <base>:<path>` and new content from the filesystem
//! (uncommitted changes included). Both sides are parsed via the
//! regular [`crate::parse::parse_file`] pipeline, then symbols are
//! paired by `(name, kind)`. Body identity is a 64-bit xxh3 hash of
//! the line range from the symbol's def line up to the next symbol's
//! def line in the same file — coarser than a tree-sitter byte-range
//! hash, but stable enough for "did anything around this function
//! change" detection without a second AST pass.
//!
//! Known limitations of this first cut:
//! - Renames are not detected — `git mv old.rs new.rs` shows up as
//!   "all old.rs symbols removed, all new.rs symbols added".
//! - Two symbols sharing `(name, kind)` in the same file (overloads,
//!   duplicate definitions) collapse to per-symbol add/remove rather
//!   than smart pair-up.
//! - Languages without tree-sitter support are silently skipped.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::index::symbols::ParsedSymbol;
use crate::parse::language::Language;

/// The category a symbol falls into when comparing two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Moved,
    BodyChanged,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Moved => "moved",
            Self::BodyChanged => "body_changed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolChange {
    pub kind: ChangeKind,
    pub name: String,
    pub symbol_kind: String,
    pub path: String,
    pub line: usize,
    /// Previous def line for `Moved`. `None` for every other kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_line: Option<usize>,
}

/// Compute symbol-level changes between `base` (a git ref) and the
/// current working tree. Errors when `base` is not resolvable as a
/// git ref or when the root is not a git repository.
pub fn diff_against_base(
    root: &Path,
    base: &str,
    excludes: &[String],
    limit: usize,
) -> Result<Vec<SymbolChange>> {
    // Asymmetry note: `excludes` (substring) gates the per-file work
    // *before* the `git show` round-trip below; `--include` glob from
    // the CLI runs as a post-filter on the result list, so a wide
    // `--include 'src/**'` on a 500-file PR still pays the spawn cost
    // for the 490 excluded files. Worth threading the include set
    // through here later — for v1 the cost is small enough and the
    // semantics are clear in the help text.
    let root = root.canonicalize().context("canonicalize root")?;
    let files = git_changed_files(&root, base)
        .with_context(|| format!("list files changed between `{base}` and the working tree"))?;
    let mut out: Vec<SymbolChange> = Vec::new();
    for rel in &files {
        if !is_path_eligible(rel, excludes) {
            continue;
        }
        let Some(lang) = path_to_lang(rel) else {
            continue;
        };
        // Only swallow the well-defined "file did not exist at base"
        // signal; bubble every other git failure up so the caller sees
        // it instead of misclassifying every symbol as Added.
        let old_content = git_show_at(&root, base, rel)?;
        let abs_path = root.join(rel);
        // Binary files / non-UTF-8 content also fail read_to_string —
        // we treat that as "no source-level symbols on the new side"
        // rather than aborting, which mirrors how the parser would
        // bail out on the same input.
        let new_content = std::fs::read_to_string(&abs_path).ok();
        diff_one_file(
            rel,
            lang,
            old_content.as_deref(),
            new_content.as_deref(),
            &mut out,
        );
        if out.len() >= limit {
            out.truncate(limit);
            return Ok(out);
        }
    }
    out.truncate(limit);
    Ok(out)
}

fn is_path_eligible(rel: &str, excludes: &[String]) -> bool {
    excludes.iter().all(|x| !rel.contains(x.as_str()))
}

fn path_to_lang(rel: &str) -> Option<Language> {
    let ext = std::path::Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())?;
    Language::from_extension(ext)
}

fn diff_one_file(
    rel: &str,
    lang: Language,
    old_content: Option<&str>,
    new_content: Option<&str>,
    out: &mut Vec<SymbolChange>,
) {
    let old_symbols: Vec<ParsedSymbol> = old_content
        .and_then(|c| crate::parse::parse_file(rel, c, lang).ok())
        .map(|p| p.symbols)
        .unwrap_or_default();
    let new_symbols: Vec<ParsedSymbol> = new_content
        .and_then(|c| crate::parse::parse_file(rel, c, lang).ok())
        .map(|p| p.symbols)
        .unwrap_or_default();

    // Files only in one side — pure adds/removes.
    if old_content.is_none() {
        for s in &new_symbols {
            out.push(SymbolChange {
                kind: ChangeKind::Added,
                name: s.name.clone(),
                symbol_kind: s.kind.as_str().to_string(),
                path: rel.to_string(),
                line: s.line,
                old_line: None,
            });
        }
        return;
    }
    if new_content.is_none() {
        for s in &old_symbols {
            out.push(SymbolChange {
                kind: ChangeKind::Removed,
                name: s.name.clone(),
                symbol_kind: s.kind.as_str().to_string(),
                path: rel.to_string(),
                line: s.line,
                old_line: None,
            });
        }
        return;
    }

    let old_content = old_content.unwrap_or_default();
    let new_content = new_content.unwrap_or_default();
    let old_index = group_by_name_kind(&old_symbols, old_content);
    let new_index = group_by_name_kind(&new_symbols, new_content);

    // For each key present in either side: emit per-bucket changes.
    let mut all_keys: std::collections::HashSet<(String, String)> =
        old_index.keys().cloned().collect();
    all_keys.extend(new_index.keys().cloned());
    // Stable iteration order so output is deterministic across runs.
    let mut sorted_keys: Vec<_> = all_keys.into_iter().collect();
    sorted_keys.sort();
    for key in sorted_keys {
        let symbol_kind = key.1.clone();
        let old_entries = old_index.get(&key).cloned().unwrap_or_default();
        let new_entries = new_index.get(&key).cloned().unwrap_or_default();
        match (old_entries.as_slice(), new_entries.as_slice()) {
            ([], []) => {}
            ([], new) => {
                for e in new {
                    out.push(SymbolChange {
                        kind: ChangeKind::Added,
                        name: key.0.clone(),
                        symbol_kind: symbol_kind.clone(),
                        path: rel.to_string(),
                        line: e.line,
                        old_line: None,
                    });
                }
            }
            (old, []) => {
                for e in old {
                    out.push(SymbolChange {
                        kind: ChangeKind::Removed,
                        name: key.0.clone(),
                        symbol_kind: symbol_kind.clone(),
                        path: rel.to_string(),
                        line: e.line,
                        old_line: None,
                    });
                }
            }
            ([o], [n]) => {
                if o.body_hash == n.body_hash && o.line == n.line {
                    // Unchanged — skip.
                } else if o.body_hash == n.body_hash {
                    out.push(SymbolChange {
                        kind: ChangeKind::Moved,
                        name: key.0.clone(),
                        symbol_kind: symbol_kind.clone(),
                        path: rel.to_string(),
                        line: n.line,
                        old_line: Some(o.line),
                    });
                } else {
                    out.push(SymbolChange {
                        kind: ChangeKind::BodyChanged,
                        name: key.0.clone(),
                        symbol_kind: symbol_kind.clone(),
                        path: rel.to_string(),
                        line: n.line,
                        old_line: None,
                    });
                }
            }
            (old, new) => {
                // Overloaded / duplicated symbols — emit as separate
                // removes + adds rather than guessing a pairing. v1
                // accepts the noise to keep the pairing logic simple.
                for e in old {
                    out.push(SymbolChange {
                        kind: ChangeKind::Removed,
                        name: key.0.clone(),
                        symbol_kind: symbol_kind.clone(),
                        path: rel.to_string(),
                        line: e.line,
                        old_line: None,
                    });
                }
                for e in new {
                    out.push(SymbolChange {
                        kind: ChangeKind::Added,
                        name: key.0.clone(),
                        symbol_kind: symbol_kind.clone(),
                        path: rel.to_string(),
                        line: e.line,
                        old_line: None,
                    });
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct SymbolEntry {
    line: usize,
    body_hash: u64,
}

fn group_by_name_kind(
    symbols: &[ParsedSymbol],
    content: &str,
) -> HashMap<(String, String), Vec<SymbolEntry>> {
    // Sort lines from each symbol so we can compute body ranges that
    // run from this symbol's def line up to the next symbol's def line.
    // Coarse but stable — and identical between old and new snapshots.
    let mut sym_lines: Vec<usize> = symbols.iter().map(|s| s.line).collect();
    sym_lines.sort();
    sym_lines.dedup();
    let total_lines = content.lines().count();

    let next_after = |line: usize| -> usize {
        sym_lines
            .iter()
            .copied()
            .find(|&l| l > line)
            .unwrap_or(total_lines + 1)
    };

    // TODO(v2): tighten body identity to tree-sitter node byte_range
    // instead of line-range-to-next-symbol. Today's heuristic produces
    // BodyChanged false-positives when a comment or blank line is
    // edited between two symbols — the change is attributed to the
    // earlier symbol. Acceptable for v1 (call it "did anything around
    // this symbol change"), but a node-bounded hash would be tighter.
    let lines: Vec<&str> = content.lines().collect();
    let mut out: HashMap<(String, String), Vec<SymbolEntry>> = HashMap::new();
    for sym in symbols {
        let next = next_after(sym.line);
        let start = sym.line.saturating_sub(1).min(lines.len());
        let end = next.saturating_sub(1).min(lines.len());
        let slice = if start < end { &lines[start..end] } else { &[] };
        let joined = slice.join("\n");
        let body_hash = xxhash_rust::xxh3::xxh3_64(joined.as_bytes());
        out.entry((sym.name.clone(), sym.kind.as_str().to_string()))
            .or_default()
            .push(SymbolEntry {
                line: sym.line,
                body_hash,
            });
    }
    out
}

/// Files that differ between `<base>` and the working tree, returned
/// as repo-relative POSIX-style paths.
fn git_changed_files(root: &Path, base: &str) -> Result<Vec<String>> {
    // `--no-renames` keeps both sides of a `git mv` in the output so
    // the diff surfaces as remove-from-old + add-to-new, matching the
    // module doc's documented behaviour. With git's default rename
    // detection, a moved file would silently appear under only the
    // new path and the user would lose the "remove" half of the
    // change entirely.
    let output = std::process::Command::new("git")
        .args(["diff", "--no-renames", "--name-only", base])
        .current_dir(root)
        .output()
        .context("invoke git diff --name-only")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff --name-only {base} failed: {stderr}");
    }
    let files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    Ok(files)
}

/// Read the contents of `path` at git revision `base`. Returns Err
/// when the file did not exist at that revision (caller treats this
/// as "file is new in working tree").
///
/// `git show` exits non-zero for two qualitatively different reasons —
/// "this object doesn't exist at this revision" (exit 128, stderr
/// contains `does not exist in`) and unexpected failures (corrupt
/// object store, permission denied, OOM). The former is a normal "new
/// file" signal that callers want to treat as `None`; the latter must
/// bubble up with the stderr context attached so an agent or CI
/// pipeline doesn't silently misclassify every symbol in a corrupt
/// repo as Added. We capture stderr and inspect it before deciding.
fn git_show_at(root: &Path, base: &str, path: &str) -> Result<Option<String>> {
    let spec = format!("{base}:{path}");
    let output = std::process::Command::new("git")
        .args(["show", &spec])
        .current_dir(root)
        .output()
        .context("invoke git show")?;
    if output.status.success() {
        return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Git's missing-object phrasings: "does not exist in" / "exists on
    // disk, but not in" / "Path '...' does not exist in" — all mean
    // the same thing here. Everything else (corrupt object store,
    // permission denied, OOM, …) must propagate so a broken repo
    // doesn't silently get reported as "every symbol was added".
    let is_missing =
        stderr.contains("does not exist in") || stderr.contains("exists on disk, but not in");
    if is_missing {
        return Ok(None);
    }
    anyhow::bail!(
        "git show {spec} failed ({}): {}",
        output.status,
        stderr.trim()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::symbols::SymbolKind;

    fn sym(name: &str, kind: SymbolKind, line: usize) -> ParsedSymbol {
        ParsedSymbol {
            name: name.into(),
            kind,
            line,
            signature: None,
            doc: None,
            body_tokens: None,
        }
    }

    #[test]
    fn added_symbol_in_modified_file() {
        let old = "fn a() {}\n";
        let new = "fn a() {}\nfn b() {}\n";
        let mut out = Vec::new();
        diff_one_file("lib.rs", Language::Rust, Some(old), Some(new), &mut out);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].kind, ChangeKind::Added);
        assert_eq!(out[0].name, "b");
    }

    #[test]
    fn removed_symbol_in_modified_file() {
        let old = "fn a() {}\nfn b() {}\n";
        let new = "fn a() {}\n";
        let mut out = Vec::new();
        diff_one_file("lib.rs", Language::Rust, Some(old), Some(new), &mut out);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].kind, ChangeKind::Removed);
        assert_eq!(out[0].name, "b");
    }

    #[test]
    fn unchanged_symbol_emits_nothing() {
        let src = "fn a() { 1 + 1 }\n";
        let mut out = Vec::new();
        diff_one_file("lib.rs", Language::Rust, Some(src), Some(src), &mut out);
        assert!(
            out.is_empty(),
            "unchanged file must not emit changes: {out:?}"
        );
    }

    #[test]
    fn body_change_detected() {
        let old = "fn a() { 1 + 1 }\n";
        let new = "fn a() { 2 + 2 }\n";
        let mut out = Vec::new();
        diff_one_file("lib.rs", Language::Rust, Some(old), Some(new), &mut out);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].kind, ChangeKind::BodyChanged);
        assert_eq!(out[0].name, "a");
    }

    #[test]
    fn move_within_file_detected() {
        // `b` moves from line 4 to line 1; body unchanged. Hash matches
        // → emitted as Moved, not BodyChanged.
        let old = "fn a() {}\nfn b() {\n  42\n}\n";
        let new = "fn b() {\n  42\n}\nfn a() {}\n";
        let mut out = Vec::new();
        diff_one_file("lib.rs", Language::Rust, Some(old), Some(new), &mut out);
        let by_name: HashMap<&str, &SymbolChange> =
            out.iter().map(|c| (c.name.as_str(), c)).collect();
        // `a` and `b` both moved — the unchanged-body invariant means
        // both register as Moved with old_line set.
        assert_eq!(by_name["a"].kind, ChangeKind::Moved, "{out:?}");
        assert_eq!(by_name["b"].kind, ChangeKind::Moved, "{out:?}");
        assert!(by_name["a"].old_line.is_some());
    }

    #[test]
    fn new_file_marks_everything_added() {
        let mut out = Vec::new();
        diff_one_file(
            "lib.rs",
            Language::Rust,
            None,
            Some("fn a() {}\nfn b() {}\n"),
            &mut out,
        );
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|c| c.kind == ChangeKind::Added));
    }

    #[test]
    fn deleted_file_marks_everything_removed() {
        let mut out = Vec::new();
        diff_one_file(
            "lib.rs",
            Language::Rust,
            Some("fn a() {}\nfn b() {}\n"),
            None,
            &mut out,
        );
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|c| c.kind == ChangeKind::Removed));
    }

    #[test]
    fn duplicate_symbol_name_emits_separate_add_remove() {
        // Two `foo` functions in old, one in new → grouping by (name,
        // kind) sees 2 vs 1 and falls back to flat removes + adds.
        let old = "fn foo() { 1 }\nfn other() {}\nfn foo() { 2 }\n";
        let new = "fn foo() { 1 }\nfn other() {}\n";
        let mut out = Vec::new();
        diff_one_file("lib.rs", Language::Rust, Some(old), Some(new), &mut out);
        let kinds: Vec<&str> = out.iter().map(|c| c.kind.as_str()).collect();
        // Two removes + one add → 3 changes (per fallback rule).
        assert!(
            kinds.contains(&"removed"),
            "expected at least one removed: {out:?}"
        );
        assert!(
            kinds.contains(&"added"),
            "expected at least one added: {out:?}"
        );
    }

    #[test]
    fn whitespace_only_edit_inside_body_changes_hash() {
        // Adding a blank line to a body changes the hash, so the
        // symbol registers as BodyChanged. This is the documented
        // "any line around the symbol counts as a change" trade-off —
        // worth pinning so a future tightening to byte_range-based
        // hashing is a visible behaviour change rather than a silent
        // one.
        let old = "fn a() {\n    1\n}\n";
        let new = "fn a() {\n\n    1\n}\n";
        let mut out = Vec::new();
        diff_one_file("lib.rs", Language::Rust, Some(old), Some(new), &mut out);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].kind, ChangeKind::BodyChanged);
    }

    #[test]
    fn group_by_name_kind_hashes_distinct_bodies() {
        let symbols = vec![
            sym("a", SymbolKind::Function, 1),
            sym("b", SymbolKind::Function, 3),
        ];
        let content = "fn a() {}\n\nfn b() { 42 }\n";
        let map = group_by_name_kind(&symbols, content);
        assert_eq!(map.len(), 2);
        let a_hash = map[&("a".into(), "function".into())][0].body_hash;
        let b_hash = map[&("b".into(), "function".into())][0].body_hash;
        assert_ne!(a_hash, b_hash, "different bodies must hash differently");
    }
}
