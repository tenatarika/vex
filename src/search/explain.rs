//! Reasoning artefacts for `vex similar --explain` / `vex duplicates --explain`.
//!
//! `vex similar` / `vex duplicates` already emit a cosine similarity score
//! alongside each match, but that single number gives the user no signal
//! about *why* the embeddings clustered. Two functions can score 0.98
//! cosine because they are line-for-line identical, or because they share
//! a vocabulary while having very different control flow.
//!
//! This module produces a small, cheap-to-render explanation:
//!   * `identifier_jaccard` — Jaccard overlap of identifier-shaped tokens
//!     pulled from the two bodies. Catches "shared vocabulary".
//!   * unified-line diff truncated to a configurable line cap with the
//!     `+`/`-` change counts surfaced separately for compact output.
//!
//! Both parts run on already-extracted body strings, so this module
//! deliberately knows nothing about the index, tree-sitter, or the
//! filesystem.

use std::collections::HashSet;

/// Explanation for one `(a, b)` similarity pair. Cheap to build (linear
/// in body length); safe to compute per-row for the typical CLI result
/// budget of a few dozen matches.
#[derive(Debug, Clone)]
pub struct Explanation {
    /// |tokens(a) ∩ tokens(b)| / |tokens(a) ∪ tokens(b)|. Returns 1.0 when
    /// both bodies are empty (degenerate but symmetric); 0.0 when exactly
    /// one side is empty.
    pub identifier_jaccard: f32,
    /// Truncated unified diff: the leading `max_diff_lines` of `+`/`-`/` `
    /// lines emitted by `similar::TextDiff`. Empty when the bodies are
    /// identical. Suffix `\n... (N more lines truncated)\n` when capped.
    pub diff: String,
    /// Count of inserted lines across the *full* diff (not the truncated
    /// `diff` string). Lets compact output show `+3 -1` cheaply.
    pub added: usize,
    /// Count of removed lines across the full diff.
    pub removed: usize,
}

/// Build an `Explanation` for one pair of symbol bodies.
pub fn explain_pair(a_body: &str, b_body: &str, max_diff_lines: usize) -> Explanation {
    let identifier_jaccard = jaccard(&identifiers(a_body), &identifiers(b_body));
    let (diff, added, removed) = unified_diff(a_body, b_body, max_diff_lines);
    Explanation {
        identifier_jaccard,
        diff,
        added,
        removed,
    }
}

/// Tokenise a body into identifier-shaped tokens for Jaccard comparison.
///
/// Heuristic: split on every char that is not `[A-Za-z0-9_]`. Lowercase the
/// result so `PaymentProcessor` and `payment_processor` cluster. Filter
/// out single-char tokens — they're noise (loop indices, etc).
fn identifiers(body: &str) -> HashSet<String> {
    body.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| t.len() > 1)
        .map(str::to_lowercase)
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f32 / union as f32
}

/// Run line-level unified diff and emit at most `max_lines` of context.
/// Returns `(diff_text, added_lines, removed_lines)` where the line counts
/// reflect the full diff, not the truncation.
fn unified_diff(a: &str, b: &str, max_lines: usize) -> (String, usize, usize) {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(a, b);

    // Single pass: accumulate added/removed counts while rendering the
    // truncated text. Avoids materialising a `Vec` of all changes for the
    // typical case where bodies are small.
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut total = 0usize;
    let mut rendered = 0usize;
    let mut out = String::new();
    for change in diff.iter_all_changes() {
        total += 1;
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
        if rendered < max_lines {
            let sign = match change.tag() {
                ChangeTag::Insert => '+',
                ChangeTag::Delete => '-',
                ChangeTag::Equal => ' ',
            };
            out.push(sign);
            out.push_str(change.value().trim_end_matches('\n'));
            out.push('\n');
            rendered += 1;
        }
    }

    if added == 0 && removed == 0 {
        return (String::new(), 0, 0);
    }
    if total > rendered {
        let remaining = total - rendered;
        out.push_str(&format!("... ({remaining} more lines truncated)\n"));
    }

    (out, added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_bodies_yield_identical_explanation() {
        let body = "fn foo() {\n    let x = 1;\n}\n";
        let ex = explain_pair(body, body, 10);
        assert!(
            (ex.identifier_jaccard - 1.0).abs() < f32::EPSILON,
            "expected jaccard 1.0 on identical bodies, got {}",
            ex.identifier_jaccard
        );
        assert!(
            ex.diff.is_empty(),
            "expected empty diff, got: {:?}",
            ex.diff
        );
        assert_eq!(ex.added, 0);
        assert_eq!(ex.removed, 0);
    }

    #[test]
    fn disjoint_vocab_yields_zero_jaccard() {
        let a = "fn alpha() { beta() }";
        let b = "class Zeta { method epsilon }";
        let ex = explain_pair(a, b, 10);
        assert_eq!(ex.identifier_jaccard, 0.0);
    }

    #[test]
    fn shared_identifiers_clusters_camel_and_snake() {
        // `PaymentProcessor` is one token after split-on-non-alphanum, and
        // it lowercases to `paymentprocessor` — same as the matching token
        // in the second body. This catches "shared vocabulary" without
        // tokenising camelCase boundaries.
        let a = "fn run(PaymentProcessor p) { audit() }";
        let b = "fn invoke(PaymentProcessor q) { log() }";
        let ex = explain_pair(a, b, 10);
        assert!(
            ex.identifier_jaccard > 0.0,
            "shared identifier should yield non-zero jaccard, got {}",
            ex.identifier_jaccard
        );
    }

    #[test]
    fn single_char_tokens_ignored() {
        // Single-character tokens (loop indices, single-letter params)
        // would dominate Jaccard with noise. Verify they're filtered.
        let a = "for x in items {}";
        let b = "for y in items {}";
        let ex = explain_pair(a, b, 10);
        // Both bodies tokenise to {"for", "in", "items"} — single chars
        // dropped.
        assert!(
            (ex.identifier_jaccard - 1.0).abs() < f32::EPSILON,
            "expected jaccard 1.0 after filtering single chars, got {}",
            ex.identifier_jaccard
        );
    }

    #[test]
    fn diff_truncated_at_cap_with_marker() {
        // Force a diff longer than the cap so the truncation suffix fires.
        let a = (0..20).map(|i| format!("line_{i}_a\n")).collect::<String>();
        let b = (0..20).map(|i| format!("line_{i}_b\n")).collect::<String>();
        let ex = explain_pair(&a, &b, 5);
        assert!(
            ex.diff.contains("more lines truncated"),
            "expected truncation marker, got: {}",
            ex.diff
        );
        assert_eq!(ex.added, 20, "every line should be an insert");
        assert_eq!(ex.removed, 20, "every original line should be a delete");
    }

    #[test]
    fn one_line_change_records_one_added_one_removed() {
        let a = "fn foo() { return 1 }";
        let b = "fn foo() { return 2 }";
        let ex = explain_pair(a, b, 10);
        assert_eq!(ex.added, 1);
        assert_eq!(ex.removed, 1);
        assert!(ex.diff.contains("-fn foo() { return 1 }"));
        assert!(ex.diff.contains("+fn foo() { return 2 }"));
    }

    #[test]
    fn both_empty_yields_jaccard_one_and_empty_diff() {
        let ex = explain_pair("", "", 10);
        assert!((ex.identifier_jaccard - 1.0).abs() < f32::EPSILON);
        assert!(ex.diff.is_empty());
    }

    #[test]
    fn one_side_empty_yields_jaccard_zero() {
        let ex = explain_pair("", "fn foo() { return 1 }", 10);
        assert_eq!(ex.identifier_jaccard, 0.0);
        assert_eq!(ex.removed, 0);
        assert!(ex.added > 0);
    }
}
