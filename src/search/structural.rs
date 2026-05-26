use crate::index::symbols::SymbolKind;
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
        indices_to_results(reader, &indices, match_type)
    } else {
        let inverted = crate::store::inverted::InvertedIndex::from_reader(reader);
        let indices = inverted.search(query, limit);
        indices_to_results(reader, &indices, MatchType::Structural)
    }
}

fn indices_to_results(
    reader: &IndexReader,
    indices: &[u32],
    match_type: MatchType,
) -> Vec<SearchResult> {
    indices
        .iter()
        .filter_map(|&idx| {
            let rec = reader.symbol(idx as usize)?;
            // Phase 14.1 — belt-and-braces against a stale FST that still
            // points at a Module record (e.g. an index written by a
            // pre-14.1 build but read by a 14.1 binary). The writer
            // exclusion is the primary defence; this guards the
            // inverted-index fallback path at line 17 too.
            if SymbolKind::try_from(rec.kind) == Ok(SymbolKind::Module) {
                return None;
            }
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
            let kind = SymbolKind::try_from(rec.kind)
                .map_or("unknown", |k| k.as_str())
                .to_string();

            Some(SearchResult {
                name,
                kind,
                path,
                line: rec.line as usize,
                signature: sig,
                score: 1.0,
                match_type: match_type.clone(),
            })
        })
        .collect()
}
