pub mod extractor;
pub mod language;
pub mod queries;

use anyhow::Result;
use language::Language;

use crate::index::symbols::ParsedFile;

/// Parse a single file and extract symbols + references.
pub fn parse_file(path: &str, content: &str, lang: Language) -> Result<ParsedFile> {
    let symbols = extractor::extract_symbols(content, lang)?;
    let refs = extractor::extract_references(content, lang);
    Ok(ParsedFile {
        path: path.to_string(),
        symbols,
        refs,
    })
}
