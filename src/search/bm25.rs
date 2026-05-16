//! BM25 search adapter: runs `Bm25Reader::search`, hydrates symbol records
//! into `SearchResult`, and tags them with `MatchType::Bm25` so the fusion
//! layer can label hybrid hits correctly.

use crate::index::symbols::SymbolKind;
use crate::search::{MatchType, SearchResult};
use crate::store::bm25::Bm25Reader;
use crate::store::reader::IndexReader;

/// Run BM25 search and convert raw `(sym_idx, score)` pairs into
/// `SearchResult`. Returns an empty vec when:
/// - the index has no BM25 section
/// - the query is empty or stopwords-only
/// - no documents match any query term
pub fn search(reader: &IndexReader, query: &str, top_k: usize) -> Vec<SearchResult> {
    if !reader.has_bm25() {
        return Vec::new();
    }
    let Ok(bm25) = Bm25Reader::new(
        reader.bm25_fst_bytes(),
        reader.bm25_posting_bytes(),
        reader.bm25_stats_bytes(),
    ) else {
        return Vec::new();
    };

    bm25.search(query, top_k)
        .into_iter()
        .filter_map(|(sym_idx, score)| {
            let rec = reader.symbol(sym_idx as usize)?;
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
                kind: SymbolKind::try_from(rec.kind)
                    .map_or("unknown", |k| k.as_str())
                    .to_string(),
                path,
                line: rec.line as usize,
                signature: sig,
                score: score as f64,
                match_type: MatchType::Bm25,
            })
        })
        .collect()
}
