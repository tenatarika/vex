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
///
/// # The shared tree
///
/// `content` is parsed **once**, here, and the resulting tree is threaded into
/// every extractor that has been migrated to accept one
/// (`.claude/Task/PERF-parse-once-shared-tree.md`). Extractors still on the
/// `(content, lang)` shape parse again internally; each migration commit moves
/// one of them over. `parse_count_tests` below pins the current number of full
/// parses per language, so the remaining duplication is asserted rather than
/// guessed.
pub fn parse_file(path: &str, content: &str, lang: Language) -> Result<ParsedFile> {
    // Probe the symbol query BEFORE parsing. `extract_symbols_and_imports`
    // does the same (`extractor/symbols.rs`: `try_get_query` precedes its
    // parse), which is what makes a grammar/query-load failure surface as
    // `GrammarLoadError` — the pipeline downcasts on that type and aggregates
    // it per language instead of logging a per-file "parse failed"
    // (`index/pipeline/parse_files.rs`). Parsing first would invert that
    // priority for any language where both would fail. `try_get_query` is a
    // `LazyLock` deref, not a compile, so this costs nothing.
    if let Err(reason) = queries::try_get_query(lang) {
        return Err(extractor::GrammarLoadError {
            lang,
            reason: reason.to_string(),
        }
        .into());
    }
    // THE parse. A failure here is identical in effect to the pre-shared-tree
    // behaviour: `extract_symbols_and_imports` ran first and propagated its
    // parse error with `?`, so the file was dropped from the index entirely.
    let tree = parser_pool::parse_text(lang, content)?;
    let (mut symbols, imports) = extractor::extract_symbols_and_imports(content, lang)?;
    // Reads the shared tree. Languages without `has_ast_ref_filter` still get
    // the line-based scanner — the core keeps that short-circuit, see its docs.
    let mut refs = extractor::extract_references_ast_with_tree(&tree, content, lang);
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
    // 11.4 Inc 4 — pattern skeletons off the shared tree. T2/T3 langs return an
    // empty Vec via the allowlist short-circuit, which the core keeps.
    let skeletons = crate::pattern::skeleton::extract_skeletons_with_tree(&tree, content, lang);
    // P2 (`docs/HIERARCHY-EDGES.md` §4) — raw extends/implements/uses
    // captures, reusing the same `hierarchy::queries` SCM the live
    // `vex implementations` walk uses. First consumer of the shared tree.
    //
    // This used to parse for itself and swallow a parse failure as an empty
    // vec. That arm was dead: `extract_symbols_and_imports` above parses the
    // same bytes with the same grammar and propagates failure with `?`, so
    // `parse_file` never reached this line on an input that fails to parse.
    // (An earlier comment here claimed the repeated `parse_text` calls were
    // "not a second cold parse, just another pass over the pooled tree" —
    // that was wrong. The pool caches a `Parser`, not a `Tree`; every call was
    // a full re-parse. Hence this refactor.)
    let hierarchy_captures = crate::hierarchy::capture_hierarchy_edges(&tree, content, lang);
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

/// Tripwire for the shared-tree refactor
/// (`.claude/Task/PERF-parse-once-shared-tree.md`).
///
/// [`parse_file`] used to run a full tree-sitter parse of the same content once
/// per extractor. This pins the remaining count for **every** language so the
/// migration's progress is asserted rather than assumed, and so a future
/// extractor cannot silently reintroduce a parse.
///
/// **The expected counts move with every migration commit** — a stale
/// expectation fails RED, which is the intended behaviour. The target is 1 for
/// every language.
///
/// State after commit 3 (hierarchy, refs and skeletons migrated):
///
/// | Count | Languages | Sites that still parse for themselves |
/// |---|---|---|
/// | 5 | C++ | the four below + `scope::cpp::extract_cpp_includes` |
/// | 4 | Rust, Kotlin, TypeScript, Python, Go, Java, C# | the shared parse + symbols + call edges + binder |
/// | 2 | Ruby, Swift, PHP, SQL, Markdown, CSS, HTML | the shared parse + symbols |
/// | 2 | Bash, Lua, YAML, TOML | the shared parse + symbols |
///
/// The two 2-rows have different *compositions* even though the totals match:
/// the first group runs the skeleton walker over the shared tree, the second
/// short-circuits on an empty allowlist. Both then read hierarchy off the shared
/// tree. Only `symbols` still re-parses for them, which commit 6 removes.
#[cfg(test)]
mod parse_count_tests {
    use super::*;
    use crate::parse::parser_pool::{parse_call_count, reset_parse_call_count};

    /// `(language, fixture extension, expected `parse_text` calls)`.
    /// Exhaustive over `Language::ALL` — enforced by
    /// `every_language_has_a_pinned_parse_count`, so adding a 20th language
    /// fails until its count is pinned here.
    const EXPECTED: &[(Language, &str, u64)] = &[
        (Language::Rust, "rs", 4),
        (Language::Kotlin, "kt", 4),
        (Language::TypeScript, "ts", 4),
        (Language::Python, "py", 4),
        (Language::Go, "go", 4),
        (Language::Java, "java", 4),
        (Language::CSharp, "cs", 4),
        (Language::Cpp, "cpp", 5),
        (Language::Ruby, "rb", 2),
        (Language::Swift, "swift", 2),
        (Language::Php, "php", 2),
        (Language::Sql, "sql", 2),
        (Language::Markdown, "md", 2),
        (Language::Css, "css", 2),
        (Language::Html, "html", 2),
        (Language::Bash, "sh", 2),
        (Language::Lua, "lua", 2),
        (Language::Yaml, "yaml", 2),
        (Language::Toml, "toml", 2),
    ];

    fn parse_count_for(rel: &str, lang: Language) -> u64 {
        let abs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        let content = std::fs::read_to_string(&abs)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", abs.display()));

        // Same thread throughout: the counter is thread-local.
        reset_parse_call_count();
        parse_file(rel, &content, lang).expect("parse fixture");
        parse_call_count()
    }

    #[test]
    fn parse_file_parses_each_file_once_per_extractor() {
        for &(lang, ext, expected) in EXPECTED {
            let rel = format!("tests/fixtures/sample.{ext}");
            assert_eq!(
                parse_count_for(&rel, lang),
                expected,
                "{lang:?}: unexpected number of full tree-sitter parses in parse_file"
            );
        }
    }

    #[test]
    fn every_language_has_a_pinned_parse_count() {
        for &lang in Language::ALL {
            assert!(
                EXPECTED.iter().any(|&(l, _, _)| l == lang),
                "{lang:?} has no pinned parse count — add it to EXPECTED \
                 (and a tests/fixtures/sample.* fixture) so the shared-tree \
                 migration cannot regress it silently"
            );
        }
    }
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
