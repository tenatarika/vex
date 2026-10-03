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

/// Candidate cap [`search_ranked`] applies to prefix / fuzzy fallbacks
/// (never below the caller's limit). Exact-name hits are never capped.
/// Cutting to the caller's limit *before* the rerank made a 1-result lookup
/// return the first posting in index order — filesystem walk (`readdir`)
/// order — so the same tree resolved differently on NTFS than on
/// APFS/ext4, and the test-path demotion never ran.
pub const RERANK_POOL: usize = 64;

/// Candidates for `query`, ranked independently of index (file discovery)
/// order:
///
/// 1. An exact (case-insensitive) name hit takes **every** posting for the
///    name — a capped pool would be the first N in walk order for names
///    like `new` with hundreds of definitions. Otherwise fall back to
///    [`search_with_fuzzy`] capped at `pool` (prefix / fuzzy fallbacks).
/// 2. Sort into canonical `(lowercased name, path, line)` order. The
///    lowercased name is the FST key, so prefix / fuzzy candidates keep
///    their name order; the path / line part replaces index order inside a
///    name.
/// 3. Rerank. Its sort is stable, so equal-score ties keep the canonical
///    order rather than index order.
pub fn search_ranked(
    reader: &IndexReader,
    query: &str,
    pool: usize,
    ctx: &crate::search::rerank::RerankContext<'_>,
) -> Vec<SearchResult> {
    let exact = reader
        .symbol_fst_reader()
        .map(|fst| fst.find(query))
        .unwrap_or_default();
    let mut candidates = if exact.is_empty() {
        search_with_fuzzy(reader, query, pool)
    } else {
        indices_to_results(reader, &exact, MatchType::Structural)
    };
    candidates.sort_by_cached_key(|r| (r.name.to_lowercase(), r.path.clone(), r.line));
    crate::search::rerank::rerank(query, ctx, candidates)
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

#[cfg(test)]
mod tests {
    //! [`search_ranked`] must not depend on index order, which in production
    //! is filesystem walk (`readdir`) order. Each test writes the index in
    //! explicit orders so the guard holds on every OS.
    use super::*;
    use crate::index::symbols::{ParsedFile, ParsedSymbol};
    use crate::search::rerank::RerankContext;
    use crate::store::writer::write_index;

    fn def(path: &str, name: &str) -> ParsedFile {
        ParsedFile {
            path: path.to_string(),
            symbols: vec![ParsedSymbol {
                name: name.to_string(),
                kind: SymbolKind::Function,
                line: 1,
                signature: Some(format!("pub fn {name}() {{}}")),
                doc: None,
                body_tokens: None,
            }],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
            hierarchy_captures: Vec::new(),
        }
    }

    /// `(name, path)` of every ranked candidate for `query` over an index
    /// written from `files` in the given order.
    fn ranked(files: &[ParsedFile], query: &str, pool: usize) -> Vec<(String, String)> {
        let tmp = tempfile::TempDir::new().unwrap();
        let out = tmp.path().join("index.vex");
        write_index(files, &out).unwrap();
        let reader = IndexReader::open(&out).unwrap();
        let ctx = RerankContext {
            kind_hints: Vec::new(),
            context_path: None,
        };
        search_ranked(&reader, query, pool, &ctx)
            .into_iter()
            .map(|r| (r.name, r.path))
            .collect()
    }

    #[test]
    fn exact_hits_beyond_the_pool_are_all_ranked_in_either_order() {
        // RERANK_POOL + 6 test-file definitions sort before the one
        // production definition when the index is test-first. A capped pool
        // would hold only test files then, and the production one would
        // never reach the rerank.
        let mut files: Vec<ParsedFile> = (0..RERANK_POOL + 6)
            .map(|i| def(&format!("tests/t{i:03}.rs"), "shared_new_fn"))
            .collect();
        files.push(def("src/lib.rs", "shared_new_fn"));
        let test_first = ranked(&files, "shared_new_fn", RERANK_POOL);
        files.reverse();
        let prod_first = ranked(&files, "shared_new_fn", RERANK_POOL);

        assert_eq!(test_first.len(), RERANK_POOL + 7);
        assert_eq!(test_first[0].1, "src/lib.rs");
        assert_eq!(test_first, prod_first);
    }

    #[test]
    fn prefix_matches_stay_in_name_order() {
        // No exact hit, so every candidate scores the same; the result must
        // follow the FST name order, not path order (src/a.rs holds
        // `fetch_gamma`) and not index order.
        let mut files = vec![
            def("src/c.rs", "fetch_alpha_fn"),
            def("src/a.rs", "fetch_gamma_fn"),
            def("src/b.rs", "fetch_beta_fn"),
        ];
        let expected: Vec<(String, String)> = [
            ("fetch_alpha_fn", "src/c.rs"),
            ("fetch_beta_fn", "src/b.rs"),
            ("fetch_gamma_fn", "src/a.rs"),
        ]
        .iter()
        .map(|(n, p)| (n.to_string(), p.to_string()))
        .collect();
        assert_eq!(ranked(&files, "fetch_", RERANK_POOL), expected);
        files.reverse();
        assert_eq!(ranked(&files, "fetch_", RERANK_POOL), expected);
    }
}
