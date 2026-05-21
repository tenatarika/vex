pub mod body;
pub mod extractor;
pub mod language;
pub mod queries;
pub mod scope;

use anyhow::Result;
use language::Language;

use crate::index::symbols::{ParsedFile, RawCallEdge};

/// Parse a single file and extract symbols + references + call edges.
pub fn parse_file(path: &str, content: &str, lang: Language) -> Result<ParsedFile> {
    let (symbols, imports) = extractor::extract_symbols_and_imports(content, lang)?;
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
    let bound_refs = scope::bind_refs(content, lang, &symbols)?;
    Ok(ParsedFile {
        path: path.to_string(),
        symbols,
        refs,
        call_edges,
        bound_refs,
    })
}
