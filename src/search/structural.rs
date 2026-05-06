use crate::search::{MatchType, SearchResult};
use crate::store::inverted::InvertedIndex;
use crate::store::reader::IndexReader;

/// Search by symbol name using the inverted index.
pub fn search(
    reader: &IndexReader,
    inverted: &InvertedIndex,
    query: &str,
    limit: usize,
) -> Vec<SearchResult> {
    let indices = inverted.search(query, limit);

    indices
        .into_iter()
        .filter_map(|idx| {
            let rec = reader.symbol(idx as usize)?;
            let name = reader.read_string(rec.name_offset).to_string();
            let path = reader.read_string(rec.file_offset).to_string();
            let sig = {
                let s = reader.read_string(rec.signature_offset);
                if s.is_empty() { None } else { Some(s.to_string()) }
            };

            Some(SearchResult {
                name,
                kind: symbol_kind_str(rec.kind).to_string(),
                path,
                line: rec.line as usize,
                signature: sig,
                score: 1.0, // exact match score
                match_type: MatchType::Structural,
            })
        })
        .collect()
}

fn symbol_kind_str(kind: u8) -> &'static str {
    match kind {
        0 => "function",
        1 => "method",
        2 => "struct",
        3 => "class",
        4 => "interface",
        5 => "trait",
        6 => "enum",
        7 => "type_alias",
        8 => "impl",
        9 => "constant",
        10 => "property",
        11 => "package",
        _ => "unknown",
    }
}
