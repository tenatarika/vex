//! Reference extraction — turn source text into `ParsedRef` entries.
//!
//! Two entry points: [`extract_references`] does a cheap line-based
//! identifier scan (FST fallback when AST extraction is impossible),
//! and [`extract_references_ast`] runs the proper tree-sitter walker
//! that skips comments and string interiors. The walker logic lives in
//! [`walk_for_refs`]; the per-language node-kind classifiers
//! (`is_comment_kind`, `is_plain_string_kind`, etc.) sit alongside.
//!
//! Isolated from `mod.rs` so the AST-walker concerns don't share
//! screen real estate with symbol/import extraction; classifiers
//! grouped together so adding a new language's interpolation form
//! requires touching one file.

use anyhow::Result;

use crate::index::symbols::ParsedRef;
use crate::parse::language::Language;
use crate::parse::parser_pool::parse_text;

use super::is_meaningful_identifier;

/// Extract references (symbol usages) via simple identifier scanning.
pub fn extract_references(content: &str) -> Vec<ParsedRef> {
    let mut refs = Vec::new();
    for (line_num, line) in content.lines().enumerate() {
        for cap in scan_identifiers(line) {
            refs.push(ParsedRef {
                name: cap.to_string(),
                line: line_num + 1,
                context: Some(line.trim().to_string()),
            });
        }
    }
    refs
}

/// Extract references via an AST walk, skipping identifiers that live
/// inside comment or string-literal nodes. For languages without an AST
/// filter (see [`Language::has_ast_ref_filter`]) this falls back to the
/// line-based scanner so the rest of the indexer is unchanged.
///
/// This is the 11.1.1 entry point — it deletes the loudest class of
/// false positives from `vex usages` (idents matched inside doc
/// comments and string literals) without touching the binary format.
/// Performance cost on the binder languages is one extra tree-sitter
/// parse per file; 11.1.2 will fuse the parses.
pub fn extract_references_ast(content: &str, lang: Language) -> Result<Vec<ParsedRef>> {
    if !lang.has_ast_ref_filter() {
        return Ok(extract_references(content));
    }

    // v1.12.0 P3 — pooled per-thread parser; v1.23.0 — guarded by the
    // shared `parse_text` budget (see `super::parser_pool`).
    let tree = parse_text(lang, content)?;

    let mut refs = Vec::new();
    // v1.12.0 P4 — collect line slices once, then pass an O(1)-indexable
    // view to the recursive walker. The old code did
    // `content.lines().nth(n)` per identifier node; on identifier-dense
    // files that compounded to O(line_count × ident_count).
    let line_slices: Vec<&str> = content.lines().collect();
    walk_for_refs(tree.root_node(), content, &line_slices, lang, &mut refs);
    Ok(refs)
}

fn walk_for_refs(
    node: tree_sitter::Node,
    content: &str,
    lines: &[&str],
    lang: Language,
    refs: &mut Vec<ParsedRef>,
) {
    let kind = node.kind();

    // Comments — drop the whole subtree.
    if is_comment_kind(kind, lang) {
        return;
    }

    // Plain strings — drop the whole subtree. Anything that looks like
    // an identifier inside a string is prose, not a real usage.
    if is_plain_string_kind(kind, lang) {
        return;
    }

    // Interpolatable strings (TS template literals, Python f-strings):
    // descend only into the interpolation child kind so real code refs
    // inside `${...}` / `{...}` survive while the literal text around
    // them does not.
    if let Some(interp_kind) = interpolation_child_kind(kind, lang) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == interp_kind {
                walk_for_refs(child, content, lines, lang, refs);
            }
        }
        return;
    }

    if is_identifier_kind(kind) {
        let text = node.utf8_text(content.as_bytes()).unwrap_or_default();
        if is_meaningful_identifier(text) {
            let line = node.start_position().row + 1;
            let context = lines.get(line - 1).map(|l| l.trim().to_string());
            refs.push(ParsedRef {
                name: text.to_string(),
                line,
                context,
            });
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_refs(child, content, lines, lang, refs);
    }
}

fn is_comment_kind(kind: &str, lang: Language) -> bool {
    match lang {
        Language::Rust => matches!(kind, "line_comment" | "block_comment"),
        Language::TypeScript | Language::Python | Language::CSharp | Language::Cpp => {
            kind == "comment"
        }
        Language::Go => kind == "comment",
        Language::Java => matches!(kind, "line_comment" | "block_comment"),
        Language::Kotlin => matches!(kind, "line_comment" | "block_comment"),
        _ => false,
    }
}

fn is_plain_string_kind(kind: &str, lang: Language) -> bool {
    match lang {
        // tree-sitter-rust 0.24 parses `b"..."` as `string_literal` —
        // no separate `byte_string_literal` kind exists.
        Language::Rust => matches!(
            kind,
            "string_literal" | "raw_string_literal" | "char_literal"
        ),
        Language::TypeScript => matches!(kind, "string" | "regex"),
        // Python `string` is handled below via `interpolation_child_kind`
        // because f-strings carry real refs inside `{...}`.
        Language::Python => false,
        // C# `interpolated_string_expression` is handled below — every
        // other string-shaped literal kind is dropped wholesale.
        Language::CSharp => matches!(
            kind,
            "string_literal"
                | "verbatim_string_literal"
                | "raw_string_literal"
                | "character_literal"
        ),
        Language::Cpp => matches!(
            kind,
            "string_literal" | "raw_string_literal" | "char_literal"
        ),
        Language::Go => matches!(
            kind,
            "interpreted_string_literal" | "raw_string_literal" | "rune_literal"
        ),
        Language::Java => matches!(kind, "string_literal" | "character_literal" | "text_block"),
        // Kotlin `string_literal` is interpolatable (`${...}` carries real
        // refs) — handled below via `interpolation_child_kind`. Only the
        // char literal is dropped wholesale.
        Language::Kotlin => kind == "character_literal",
        _ => false,
    }
}

/// If `kind` is a string node whose subtree may contain interpolated
/// code (TS template literals, Python f-strings), return the child kind
/// representing the interpolated code so the walker can descend into
/// only those children.
fn interpolation_child_kind(kind: &str, lang: Language) -> Option<&'static str> {
    match (lang, kind) {
        (Language::TypeScript, "template_string") => Some("template_substitution"),
        (Language::Python, "string") => Some("interpolation"),
        (Language::CSharp, "interpolated_string_expression") => Some("interpolation"),
        (Language::Kotlin, "string_literal") => Some("interpolation"),
        _ => None,
    }
}

fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "shorthand_field_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
    )
}

/// Scan a single line for identifier tokens that look like deliberate
/// symbol names rather than incidental words.
///
/// Accepts: PascalCase (`PaymentGateway`), camelCase (`processOrder`),
/// snake_case (`process_order`), and SCREAMING_SNAKE_CASE (`MAX_RETRIES`).
///
/// Rejects: plain lowercase words (`total`, `amount` — too noisy across
/// natural language and trivial locals), single-letter and very short
/// identifiers (length < 3), and language keywords (see
/// [`super::is_keyword`]).
///
/// The shape filter — "contains `_` OR has mixed case" — is what keeps
/// the refs FST from exploding on prose-heavy comments while still
/// catching every Python/Rust/Go function name the user might want
/// `vex usages` to find.
pub(super) fn scan_identifiers(line: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            if is_meaningful_identifier(word) {
                results.push(word);
            }
        } else {
            i += 1;
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(content: &str, lang: Language) -> Vec<String> {
        extract_references_ast(content, lang)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect()
    }

    #[test]
    fn go_ast_filter_drops_comment_and_string_idents() {
        let src = "package main\n// MentionInComment is prose\nfunc Run() {\n\ts := \"AlsoInString\"\n\t_ = s\n}\n";
        let got = names(src, Language::Go);
        assert!(
            !got.iter().any(|n| n == "MentionInComment"),
            "comment ident must be dropped: {got:?}"
        );
        assert!(
            !got.iter().any(|n| n == "AlsoInString"),
            "string ident must be dropped: {got:?}"
        );
        // A real code ref still survives.
        assert!(got.iter().any(|n| n == "Run"), "got: {got:?}");
    }

    #[test]
    fn java_ast_filter_drops_comment_and_string_idents() {
        let src = "class Widget {\n  // MentionInComment prose\n  void run() {\n    String s = \"AlsoInString\";\n  }\n}\n";
        let got = names(src, Language::Java);
        assert!(
            !got.iter().any(|n| n == "MentionInComment"),
            "comment ident must be dropped: {got:?}"
        );
        assert!(
            !got.iter().any(|n| n == "AlsoInString"),
            "string ident must be dropped: {got:?}"
        );
        assert!(got.iter().any(|n| n == "Widget"), "got: {got:?}");
    }
}
