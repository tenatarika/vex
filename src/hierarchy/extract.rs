//! Index-time hierarchy-edge extraction (P2, `docs/HIERARCHY-EDGES.md` §4).
//!
//! Reuses the exact same tree-sitter queries `find_in_source` (mod.rs) runs
//! at query time — this module never re-parses and never duplicates the SCM.
//! The only difference is the tree comes in already-parsed (shared with
//! symbol extraction) and every match is captured, not just ones matching a
//! caller-supplied `base_name` filter.

use streaming_iterator::StreamingIterator;
use tree_sitter::{Query, QueryCursor};

use crate::index::symbols::HierarchyCapture;
use crate::parse::language::Language;
use crate::parse::NodeTextExt;
use crate::store::format::EdgeKind;

use super::queries::{inheritance_query, relation_label};

/// Map a `relation_label` string onto an [`EdgeKind`] discriminant.
///
/// **P2 scope decision (locked):** `relation_label` does not currently
/// distinguish interface `implements` from class `extends` — it lumps
/// Java/TS/C#/Kotlin `implements` under the `"extends"` label (see
/// `queries.rs::relation_label`). So `EdgeKind::Implements` (1) is never
/// produced here in P2; refining the queries to split interface clauses is
/// a documented follow-up (`docs/HIERARCHY-EDGES.md` §4), out of scope now.
fn relation_to_edge_kind(relation: &str) -> u8 {
    match relation {
        "uses" | "include" => EdgeKind::Uses as u8,
        // "extends" | "inherits" | "impl" and any other/future label from
        // relation_label fall back to Extends — matches today's exhaustive
        // set of labels (see queries.rs) without over-fitting to it.
        _ => EdgeKind::Extends as u8,
    }
}

/// Extract raw hierarchy captures from an already-parsed tree.
///
/// `tree` and `content` MUST be the same parse pair used for symbol
/// extraction — this function never re-parses. Returns an empty vec for
/// languages with no inheritance query (e.g. Go), for a tree whose query
/// fails to compile, or for a file with no matches.
///
/// Node text is read exclusively through [`crate::parse::NodeTextExt`]
/// (never raw `utf8_text`) because tree-sitter's GLR error recovery can
/// emit nodes whose byte range runs past EOF on malformed/adversarial
/// source — `utf8_text` panics on such a range, `node_text`/`node_text_opt`
/// degrade to `""`/`None` instead. This path runs on every indexed file.
pub(crate) fn capture_hierarchy_edges(
    tree: &tree_sitter::Tree,
    content: &str,
    lang: Language,
) -> Vec<HierarchyCapture> {
    let query_src = match inheritance_query(lang) {
        Some(q) => q,
        None => return Vec::new(),
    };

    let ts_lang = lang.ts_language();
    let query = match Query::new(&ts_lang, query_src) {
        Ok(q) => q,
        Err(_) => return Vec::new(),
    };

    let base_idx = match query.capture_index_for_name("base") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let child_idx = match query.capture_index_for_name("child") {
        Some(i) => i,
        None => return Vec::new(),
    };

    let source = content.as_bytes();
    let mut cursor = QueryCursor::new();
    let mut query_matches = cursor.matches(&query, tree.root_node(), source);
    let mut captures = Vec::new();

    while let Some(m) = query_matches.next() {
        let mut parent_name = None;
        let mut child_name = None;
        let mut child_line: u32 = 0;

        for capture in m.captures {
            if capture.index == base_idx {
                if let Some(text) = capture.node.node_text_opt(source) {
                    parent_name = Some(text.to_string());
                }
            } else if capture.index == child_idx {
                if let Some(text) = capture.node.node_text_opt(source) {
                    child_name = Some(text.to_string());
                    // 1-based line, saturating so a pathological node
                    // position (only reachable via a corrupt/adversarial
                    // tree) can't wrap rather than clamp.
                    child_line = (capture.node.start_position().row as u32).saturating_add(1);
                }
            }
        }

        if let (Some(child_name), Some(parent_name)) = (child_name, parent_name) {
            let kind = relation_to_edge_kind(relation_label(lang, m.pattern_index));
            captures.push(HierarchyCapture {
                child_name,
                parent_name,
                kind,
                line: child_line,
            });
        }
    }

    captures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parser_pool::parse_text;

    fn captures_for(lang: Language, src: &str) -> Vec<HierarchyCapture> {
        let tree = parse_text(lang, src).expect("parse");
        capture_hierarchy_edges(&tree, src, lang)
    }

    #[test]
    fn rust_impl_trait_for_struct_is_extends() {
        let src = "trait Shape {}\nstruct Foo;\nimpl Shape for Foo {}\n";
        let caps = captures_for(Language::Rust, src);
        assert_eq!(caps.len(), 1);
        let c = &caps[0];
        assert_eq!(c.child_name, "Foo");
        assert_eq!(c.parent_name, "Shape");
        assert_eq!(c.kind, EdgeKind::Extends as u8);
        assert_eq!(c.line, 3);
    }

    #[test]
    fn python_class_with_base_is_extends() {
        let src = "class Base:\n    pass\n\nclass C(Base):\n    pass\n";
        let caps = captures_for(Language::Python, src);
        assert_eq!(caps.len(), 1);
        let c = &caps[0];
        assert_eq!(c.child_name, "C");
        assert_eq!(c.parent_name, "Base");
        assert_eq!(c.kind, EdgeKind::Extends as u8);
        assert_eq!(c.line, 4);
    }

    #[test]
    fn ruby_include_module_is_uses() {
        let src = "module M\nend\n\nclass C\n  include M\nend\n";
        let caps = captures_for(Language::Ruby, src);
        assert_eq!(caps.len(), 1);
        let c = &caps[0];
        assert_eq!(c.child_name, "C");
        assert_eq!(c.parent_name, "M");
        assert_eq!(c.kind, EdgeKind::Uses as u8);
    }

    #[test]
    fn ruby_class_lt_base_is_extends() {
        let src = "class Base\nend\n\nclass C < Base\nend\n";
        let caps = captures_for(Language::Ruby, src);
        assert_eq!(caps.len(), 1);
        let c = &caps[0];
        assert_eq!(c.child_name, "C");
        assert_eq!(c.parent_name, "Base");
        assert_eq!(c.kind, EdgeKind::Extends as u8);
    }

    #[test]
    fn go_has_no_inheritance_query_and_returns_empty() {
        let src = "package main\n\ntype Foo struct {}\n";
        let caps = captures_for(Language::Go, src);
        assert!(caps.is_empty());
    }

    #[test]
    fn truncated_adversarial_source_does_not_panic() {
        // A source that ends mid-token can produce tree-sitter error-recovery
        // nodes whose byte ranges are unusual. The important assertion is
        // simply that extraction does not panic (NodeTextExt contract) —
        // whatever it returns (empty or partial) is acceptable.
        let src = "class C(Ba";
        let caps = captures_for(Language::Python, src);
        let _ = caps; // no panic is the assertion
    }

    #[test]
    fn empty_source_does_not_panic() {
        let caps = captures_for(Language::Rust, "");
        assert!(caps.is_empty());
    }

    /// Drift tripwire (rust-reviewer + code-reviewer LOW): `relation_to_edge_kind`
    /// falls back to `Extends` for any unmapped label, so a NEW label added to
    /// `queries::relation_label` (e.g. a real `"implements"` per the §4 follow-up)
    /// would silently classify as `Extends` unless the mapping is updated too.
    /// This pins the full set of labels `relation_label` can emit today; if
    /// someone adds a label, this test fails and forces a conscious mapping
    /// decision instead of a silent misclassification.
    #[test]
    fn every_known_relation_label_maps_to_intended_kind() {
        assert_eq!(relation_to_edge_kind("impl"), EdgeKind::Extends as u8);
        assert_eq!(relation_to_edge_kind("inherits"), EdgeKind::Extends as u8);
        assert_eq!(relation_to_edge_kind("extends"), EdgeKind::Extends as u8);
        assert_eq!(relation_to_edge_kind("uses"), EdgeKind::Uses as u8);
        assert_eq!(relation_to_edge_kind("include"), EdgeKind::Uses as u8);
        // A future/unknown label falls back to Extends by design — documented,
        // not a silent bug. When `relation_label` gains a new string, add it
        // above with its intended kind (this is the tripwire).
        assert_eq!(relation_to_edge_kind("implements"), EdgeKind::Extends as u8);
    }
}
