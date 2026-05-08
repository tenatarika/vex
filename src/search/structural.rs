use crate::search::{MatchType, SearchResult};
use crate::store::reader::IndexReader;

/// Search with fuzzy fallback: exact → prefix → Levenshtein.
/// Returns results tagged with MatchType::Fuzzy when fuzzy matching was used.
pub fn search_with_fuzzy(reader: &IndexReader, query: &str, limit: usize) -> Vec<SearchResult> {
    if let Some(fst_reader) = reader.symbol_fst_reader() {
        let (indices, was_fuzzy) = fst_reader.search_with_fallback(query, limit);
        let match_type = if was_fuzzy {
            MatchType::Fuzzy
        } else {
            MatchType::Structural
        };
        indices_to_results_typed(reader, &indices, match_type)
    } else {
        let inverted = crate::store::inverted::InvertedIndex::from_reader(reader);
        let indices = inverted.search(query, limit);
        indices_to_results(reader, &indices)
    }
}

fn indices_to_results_typed(
    reader: &IndexReader,
    indices: &[u32],
    match_type: MatchType,
) -> Vec<SearchResult> {
    indices
        .iter()
        .filter_map(|&idx| {
            let rec = reader.symbol(idx as usize)?;
            let name = reader.read_string(rec.name_offset).to_string();
            let path = reader.read_string(rec.file_offset).to_string();
            let sig = {
                let s = reader.read_string(rec.signature_offset);
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            };

            Some(SearchResult {
                name,
                kind: symbol_kind_str(rec.kind).to_string(),
                path,
                line: rec.line as usize,
                signature: sig,
                score: 1.0,
                match_type: match_type.clone(),
            })
        })
        .collect()
}

fn indices_to_results(reader: &IndexReader, indices: &[u32]) -> Vec<SearchResult> {
    indices
        .iter()
        .filter_map(|&idx| {
            let rec = reader.symbol(idx as usize)?;
            let name = reader.read_string(rec.name_offset).to_string();
            let path = reader.read_string(rec.file_offset).to_string();
            let sig = {
                let s = reader.read_string(rec.signature_offset);
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            };

            Some(SearchResult {
                name,
                kind: symbol_kind_str(rec.kind).to_string(),
                path,
                line: rec.line as usize,
                signature: sig,
                score: 1.0,
                match_type: MatchType::Structural,
            })
        })
        .collect()
}

pub fn symbol_kind_str(kind: u8) -> &'static str {
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
