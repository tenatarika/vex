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
use crate::parse::NodeTextExt;

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
// Bin-target artifact: see the `#[allow]` note on the re-exports in
// `extractor/mod.rs`. No in-crate caller remains — the equivalence test and the
// doctest are what exercise this path; it stays as the documented public API.
#[allow(dead_code)]
pub fn extract_references_ast(content: &str, lang: Language) -> Result<Vec<ParsedRef>> {
    if !lang.has_ast_ref_filter() {
        return Ok(extract_references(content));
    }

    // v1.12.0 P3 — pooled per-thread parser; v1.23.0 — guarded by the
    // shared `parse_text` budget (see `super::parser_pool`).
    //
    // The `has_ast_ref_filter` check above stays HERE, before the parse, so a
    // non-filter language never pays one — the core re-checks it for callers
    // that already hold a tree.
    let tree = parse_text(lang, content)?;
    Ok(extract_references_ast_with_tree(&tree, content, lang))
}

/// [`extract_references_ast`] over a tree the caller already parsed.
///
/// Infallible: the only failure the self-parsing entry point can report is its
/// own parse, which has moved out.
///
/// **The `has_ast_ref_filter` short-circuit is repeated here deliberately.**
/// `parse_file` hands this a tree for every language, including the 11 that
/// must keep using the line-based [`extract_references`] scanner; walking the
/// supplied tree for those would quietly change the refs FST (comment and
/// string identifiers vanish — on the Ruby fixture, 16 refs collapse to 4).
/// The duplication is what makes the tree an *optimisation* rather than a
/// behaviour switch. Pinned by
/// `with_tree_matches_the_self_parsing_entry_point_for_every_language`.
pub(crate) fn extract_references_ast_with_tree(
    tree: &tree_sitter::Tree,
    content: &str,
    lang: Language,
) -> Vec<ParsedRef> {
    if !lang.has_ast_ref_filter() {
        return extract_references(content);
    }

    let mut refs = Vec::new();
    // v1.12.0 P4 — collect line slices once, then pass an O(1)-indexable
    // view to the recursive walker. The old code did
    // `content.lines().nth(n)` per identifier node; on identifier-dense
    // files that compounded to O(line_count × ident_count).
    let line_slices: Vec<&str> = content.lines().collect();
    walk_for_refs(tree.root_node(), content, &line_slices, lang, &mut refs);
    refs
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
        let text = node.node_text(content.as_bytes());
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

    /// The gate-#1 guard for the shared-tree refactor
    /// (`.claude/Task/PERF-parse-once-shared-tree.md`).
    ///
    /// `extract_references_ast` short-circuits to the **line-based**
    /// [`extract_references`] scanner for the 11 languages without
    /// [`Language::has_ast_ref_filter`] — including their comments and string
    /// interiors. `extract_references_ast_with_tree` is handed a tree by
    /// `parse_file` regardless of language, so a core that just walks that tree
    /// would silently re-route those 11 languages onto the AST walker: a
    /// different refs FST and a real recall change for `vex usages` on
    /// Ruby/PHP/Bash/…
    ///
    /// This asserts the two entry points agree for **every** language, which is
    /// what pins the short-circuit inside the core.
    #[test]
    fn with_tree_matches_the_self_parsing_entry_point_for_every_language() {
        let fixtures: &[(Language, &str)] = &[
            // has_ast_ref_filter() == true — AST walker on both sides.
            (Language::Rust, "rs"),
            (Language::Kotlin, "kt"),
            (Language::TypeScript, "ts"),
            (Language::Python, "py"),
            (Language::Go, "go"),
            (Language::Java, "java"),
            (Language::CSharp, "cs"),
            (Language::Cpp, "cpp"),
            // has_ast_ref_filter() == false — line-based scanner on both sides.
            (Language::Ruby, "rb"),
            (Language::Swift, "swift"),
            (Language::Php, "php"),
            (Language::Sql, "sql"),
            (Language::Markdown, "md"),
            (Language::Css, "css"),
            (Language::Html, "html"),
            (Language::Bash, "sh"),
            (Language::Lua, "lua"),
            (Language::Yaml, "yaml"),
            (Language::Toml, "toml"),
        ];
        assert_eq!(
            fixtures.len(),
            Language::ALL.len(),
            "every language must be covered"
        );

        for &(lang, ext) in fixtures {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/fixtures/sample.{ext}"));
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));

            let want = extract_references_ast(&content, lang).expect("self-parsing entry point");
            let tree = parse_text(lang, &content).expect("parse fixture");
            let got = extract_references_ast_with_tree(&tree, &content, lang);

            let want_names: Vec<_> = want.iter().map(|r| (&r.name, r.line)).collect();
            let got_names: Vec<_> = got.iter().map(|r| (&r.name, r.line)).collect();
            assert_eq!(
                got_names,
                want_names,
                "{lang:?}: shared-tree core disagrees with extract_references_ast \
                 (has_ast_ref_filter = {})",
                lang.has_ast_ref_filter()
            );
        }
    }

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
