pub mod body;
pub mod extractor;
pub mod language;
pub(crate) mod parser_pool;
pub mod queries;
pub mod scope;

use anyhow::Result;
use language::Language;

use crate::index::symbols::{ParsedFile, ParsedSymbol, RawCallEdge, SymbolKind};

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
    // 11.4 Inc 4 — extract pattern skeletons while source is hot. T2/T3
    // langs return an empty Vec via the allowlist short-circuit.
    let skeletons = crate::pattern::skeleton::extract_skeletons(content, lang);
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
    })
}
