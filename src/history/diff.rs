//! Phase 14.9 Tier A.1 — render unified diffs between consecutive
//! historical versions of the same `(symbol, kind)` pair.
//!
//! Phase 14.9 only stores `signature` (first line of the def) per
//! historical entry — full bodies aren't materialised. So "diff" in
//! v1.16.0 means signature-line diff. JSON wire format names the
//! field `body_diff` so a later phase can graduate to full-body diffs
//! without renaming.

use crate::history::HistoricalSymbol;

/// Group `rows` by `kind`, sorting each group oldest-first by
/// `commit_date`. Returns groups in `kind`-alphabetical order so
/// successive `vex history` invocations produce stable output.
pub fn group_by_kind(rows: &[HistoricalSymbol]) -> Vec<Vec<&HistoricalSymbol>> {
    use std::collections::BTreeMap;
    let mut by_kind: BTreeMap<&str, Vec<&HistoricalSymbol>> = BTreeMap::new();
    for r in rows {
        by_kind.entry(r.kind.as_str()).or_default().push(r);
    }
    for group in by_kind.values_mut() {
        group.sort_by(|a, b| a.commit_date.cmp(&b.commit_date));
    }
    by_kind.into_values().collect()
}

/// Unified diff between `prev` and `curr` signature lines. Header
/// uses short SHAs (8 chars) like `git diff --no-prefix`.
pub fn render_unified_diff(prev: &HistoricalSymbol, curr: &HistoricalSymbol) -> String {
    let prev_text = signature_or_placeholder(&prev.signature);
    let curr_text = signature_or_placeholder(&curr.signature);
    let diff = similar::TextDiff::from_lines(prev_text, curr_text);

    let mut out = format!(
        "--- @{prev_sha}\n+++ @{curr_sha}\n",
        prev_sha = short_sha(&prev.commit_sha),
        curr_sha = short_sha(&curr.commit_sha),
    );
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => "-",
            similar::ChangeTag::Insert => "+",
            similar::ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value());
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// JSON items for `vex history --diff --format json`. Head of each
/// `kind` group carries `signature`; non-head entries carry
/// `body_diff: { from, to, hunks }` where `hunks` is the unified-diff
/// text. Field name is `body_diff` (not `signature_diff`) so a future
/// phase that stores full bodies can graduate the field without a
/// JSON-shape break.
pub fn render_json_items(rows: &[HistoricalSymbol]) -> Vec<serde_json::Value> {
    let groups = group_by_kind(rows);
    let mut out = Vec::with_capacity(rows.len());
    for group in groups {
        for (idx, r) in group.iter().enumerate() {
            let mut obj = serde_json::json!({
                "commit_sha": r.commit_sha,
                "commit_date": r.commit_date,
                "author": r.author,
                "file_path": r.file_path,
                "blob_sha": r.blob_sha,
                "line": r.line,
                "kind": r.kind,
            });
            if idx == 0 {
                obj["signature"] = serde_json::Value::String(r.signature.clone());
            } else {
                let prev = group[idx - 1];
                obj["body_diff"] = serde_json::json!({
                    "from": prev.commit_sha,
                    "to": r.commit_sha,
                    "hunks": render_unified_diff(prev, r),
                });
            }
            out.push(obj);
        }
    }
    out
}

fn signature_or_placeholder(sig: &str) -> &str {
    if sig.is_empty() {
        "<empty signature>"
    } else {
        sig
    }
}

fn short_sha(sha: &str) -> &str {
    &sha[..8.min(sha.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(date: &str, sha: &str, sig: &str, kind: &str) -> HistoricalSymbol {
        HistoricalSymbol {
            commit_sha: sha.into(),
            commit_date: date.into(),
            author: "alice".into(),
            file_path: "lib.rs".into(),
            blob_sha: format!("{sha}b"),
            line: 1,
            signature: sig.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn group_splits_by_kind_and_orders_oldest_first() {
        let rows = vec![
            sym("2026-06-09", "ccc", "fn v3", "function"),
            sym("2026-06-01", "bbb", "fn v2", "function"),
            sym("2026-05-15", "aaa", "fn v1", "function"),
            sym("2026-05-20", "ddd", "struct S { a: u8 }", "struct"),
        ];
        let groups = group_by_kind(&rows);
        assert_eq!(groups.len(), 2);
        // Alphabetical kind order: function < struct
        assert_eq!(groups[0][0].kind, "function");
        assert_eq!(groups[0][0].signature, "fn v1");
        assert_eq!(groups[0][1].signature, "fn v2");
        assert_eq!(groups[0][2].signature, "fn v3");
        assert_eq!(groups[1][0].kind, "struct");
    }

    #[test]
    fn unified_diff_shows_change_between_two_signatures() {
        let prev = sym(
            "2026-05-15",
            "aaaaaaaa",
            "fn parse(input: &str) -> i32",
            "function",
        );
        let curr = sym(
            "2026-06-09",
            "bbbbbbbb",
            "fn parse(input: &str) -> Result<i32, ()>",
            "function",
        );
        let out = render_unified_diff(&prev, &curr);
        assert!(out.contains("--- @aaaaaaaa"));
        assert!(out.contains("+++ @bbbbbbbb"));
        assert!(out.contains("-fn parse(input: &str) -> i32"));
        assert!(out.contains("+fn parse(input: &str) -> Result<i32, ()>"));
    }

    #[test]
    fn json_items_head_has_signature_tail_has_body_diff() {
        let rows = vec![
            sym("2026-05-15", "aaaaaaaa", "fn parse() -> i32", "function"),
            sym(
                "2026-06-09",
                "bbbbbbbb",
                "fn parse() -> Result<i32, ()>",
                "function",
            ),
        ];
        let items = render_json_items(&rows);
        assert_eq!(items.len(), 2);
        // Head: full signature, no body_diff.
        assert!(items[0].get("signature").is_some());
        assert!(items[0].get("body_diff").is_none());
        // Tail: body_diff, no signature.
        assert!(items[1].get("signature").is_none());
        let bd = items[1].get("body_diff").unwrap();
        assert_eq!(bd["from"], "aaaaaaaa");
        assert_eq!(bd["to"], "bbbbbbbb");
        let hunks = bd["hunks"].as_str().unwrap();
        assert!(hunks.contains("-fn parse() -> i32"));
        assert!(hunks.contains("+fn parse() -> Result<i32, ()>"));
    }

    #[test]
    fn empty_signature_renders_placeholder() {
        let prev = sym("2026-05-15", "aaaaaaaa", "", "function");
        let curr = sym("2026-06-09", "bbbbbbbb", "fn parse()", "function");
        let out = render_unified_diff(&prev, &curr);
        assert!(out.contains("-<empty signature>"));
        assert!(out.contains("+fn parse()"));
    }
}
