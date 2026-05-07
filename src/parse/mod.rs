pub mod extractor;
pub mod language;
pub mod queries;

use anyhow::Result;
use language::Language;

use crate::index::symbols::ParsedFile;

/// Parse a single file and extract symbols + references.
pub fn parse_file(path: &str, content: &str, lang: Language) -> Result<ParsedFile> {
    let (symbols, imports) = extractor::extract_symbols_and_imports(content, lang)?;
    let mut refs = extractor::extract_references(content);
    refs.extend(imports);
    Ok(ParsedFile {
        path: path.to_string(),
        symbols,
        refs,
    })
}
