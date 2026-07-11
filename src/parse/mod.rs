pub mod body;
pub mod extractor;
pub mod language;
pub(crate) mod parser_pool;
pub mod queries;
pub mod scope;

use anyhow::Result;
use language::Language;

use crate::index::symbols::{ParsedFile, ParsedSymbol, RawCallEdge, SymbolKind};

/// Bounds-safe text extraction for tree-sitter nodes.
///
/// tree-sitter's GLR error recovery can emit nodes whose byte range runs past
/// the end of the source — `fuzz_kotlin_binder` found malformed Kotlin that
/// yields a node starting one byte past EOF. `Node::utf8_text` slices
/// `&source[range]` internally and **panics** on such a range; a trailing
/// `.unwrap_or(...)` / `.ok()` does not help because the panic fires before the
/// `Result` is produced. Every text read off a parsed node MUST go through
/// these bounds-checked accessors instead of calling `utf8_text` directly.
pub(crate) trait NodeTextExt {
    /// Node text, or `""` when the byte range is out of bounds or the bytes are
    /// not valid UTF-8.
    fn node_text<'s>(&self, source: &'s [u8]) -> &'s str;
    /// Node text, or `None` when the byte range is out of bounds or the bytes
    /// are not valid UTF-8.
    fn node_text_opt<'s>(&self, source: &'s [u8]) -> Option<&'s str>;
}

impl NodeTextExt for tree_sitter::Node<'_> {
    fn node_text<'s>(&self, source: &'s [u8]) -> &'s str {
        self.node_text_opt(source).unwrap_or("")
    }

    fn node_text_opt<'s>(&self, source: &'s [u8]) -> Option<&'s str> {
        let r = self.byte_range();
        // `r.end > source.len()` is the observed failure — a node whose range
        // runs past EOF. `r.start > r.end` additionally guards a structurally
        // inverted range. Either would panic the `&source[r]` slice below.
        if r.end > source.len() || r.start > r.end {
            return None;
        }
        std::str::from_utf8(&source[r]).ok()
    }
}

/// Parse a single file and extract symbols + references + call edges.
pub fn parse_file(path: &str, content: &str, lang: Language) -> Result<ParsedFile> {
    let (mut symbols, imports) = extractor::extract_symbols_and_imports(content, lang)?;
    let mut refs = extractor::extract_references_ast(content, lang)?;
    refs.extend(imports);
    // Call-edge extraction is cheap (one extra tree-sitter query pass) and
    // gives the persistent call graph the data it needs. Languages without
    // a call-graph query return an empty vec.
    let call_edges: Vec<RawCallEdge> = crate::callgraph::extract_call_edges(content, lang)
        .into_iter()
        .map(
            |(caller_fn_name, caller_fn_line, callee_name, line)| RawCallEdge {
                caller_fn_name,
                caller_fn_line,
                callee_name,
                line,
            },
        )
        .collect();
    // Bind refs BEFORE injecting the synthetic Module symbol — binders
    // treat their `file_symbols` arg as legitimate local definitions, and
    // `<module:path>` is not a real definition.
    let bound_refs = scope::bind_refs(content, lang, &symbols)?;
    // v1.14 — C++ `#include "…"` directives. Only quoted includes; system
    // headers and macro-named includes are skipped at extract time.
    // Transient: the Pass-2 ref resolver consumes these in the writer and
    // they never reach disk. Non-C++ files get an empty vec for free.
    let cpp_includes = if matches!(lang, Language::Cpp) {
        scope::cpp::extract_cpp_includes(content)
    } else {
        Vec::new()
    };
    // 11.4 Inc 4 — extract pattern skeletons while source is hot. T2/T3
    // langs return an empty Vec via the allowlist short-circuit.
    let skeletons = crate::pattern::skeleton::extract_skeletons(content, lang);
    // P2 (`docs/HIERARCHY-EDGES.md` §4) — raw extends/implements/uses
    // captures, reusing the same `hierarchy::queries` SCM the live
    // `vex implementations` walk uses. Uses the pooled per-thread parser
    // like every other extraction phase above/below (this file has no
    // single shared `Tree` threaded through parse_file — see
    // callgraph::extract_call_edges / scope::bind_refs /
    // pattern::skeleton::extract_skeletons for the same pattern), so this
    // is not a second cold parse, just another pass over the pooled tree.
    let hierarchy_captures = match crate::parse::parser_pool::parse_text(lang, content) {
        Ok(tree) => crate::hierarchy::capture_hierarchy_edges(&tree, content, lang),
        Err(_) => Vec::new(),
    };
    // Phase 14.1: inject a synthetic per-file `<module:path>` symbol when the
    // file produces any sentinel edge (module-scope call site). The sentinel
    // is `caller_fn_name.is_empty() && caller_fn_line == 0`; pipeline
    // resolves it to this symbol via `(path, "<module:path>", 1)` lookup.
    if call_edges
        .iter()
        .any(|e| e.caller_fn_name.is_empty() && e.caller_fn_line == 0)
    {
        symbols.insert(
            0,
            ParsedSymbol {
                name: format!("<module:{path}>"),
                kind: SymbolKind::Module,
                line: 1,
                signature: None,
                doc: None,
                body_tokens: None,
            },
        );
    }
    Ok(ParsedFile {
        path: path.to_string(),
        symbols,
        refs,
        call_edges,
        bound_refs,
        skeletons,
        cpp_includes,
        // Built by the pipeline (`parse_files`) from the file bytes, not
        // here — `parse_file` is language-layer and doesn't own the
        // read/cache decision. Left `None` for the parse layer's own
        // callers (tests, direct parses).
        trigram_bloom: None,
        hierarchy_captures,
    })
}

#[cfg(test)]
mod node_text_tests {
    use super::*;
    use crate::parse::parser_pool::parse_text;

    #[test]
    fn node_text_is_bounds_safe_when_range_exceeds_source() {
        // Regression for the `fuzz_kotlin_binder` finding: tree-sitter can
        // emit a node whose byte range runs past the source, and
        // `Node::utf8_text` then panics on the out-of-range slice. Simulate it
        // deterministically by parsing normally, then reading the node against
        // a TRUNCATED source shorter than the node's range. Must yield ""/None,
        // never panic.
        let src = "fn alpha() {}";
        let tree = parse_text(Language::Rust, src).expect("parse");
        let root = tree.root_node();
        let truncated = &src.as_bytes()[..3];
        // Precondition: the node's own range must exceed the (truncated) source
        // — this is the out-of-bounds condition the guard exists to catch.
        assert!(
            root.byte_range().end > truncated.len(),
            "test precondition: node range must exceed the truncated source"
        );
        assert_eq!(root.node_text(truncated), "");
        assert_eq!(root.node_text_opt(truncated), None);

        // Against the full source the accessors still return the text.
        assert!(!root.node_text(src.as_bytes()).is_empty());
        assert!(root.node_text_opt(src.as_bytes()).is_some());
    }
}
