use crate::search::{MatchType, SearchResult};
use crate::store::reader::IndexReader;
use crate::store::symbol_fst::SymbolFstReader;

/// Search by symbol name using persistent FST (zero-copy from mmap).
/// Falls back to in-memory inverted index if FST not available (v2 indexes).
pub fn search(reader: &IndexReader, query: &str, limit: usize) -> Vec<SearchResult> {
    let indices = if let Some(fst_reader) = reader.symbol_fst_reader() {
        fst_reader.search(query, limit)
    } else {
        // Fallback for old indexes without symbol FST
        let inverted = crate::store::inverted::InvertedIndex::from_reader(reader);
        inverted.search(query, limit)
    };

    indices_to_results(reader, &indices)
}

/// Search with an already-loaded FST reader (avoids re-creating per query in MCP).
#[allow(dead_code)] // for MCP server persistent sessions
pub fn search_with_fst(
    reader: &IndexReader,
    fst_reader: &SymbolFstReader<'_>,
    query: &str,
    limit: usize,
) -> Vec<SearchResult> {
    let indices = fst_reader.search(query, limit);
    indices_to_results(reader, &indices)
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
