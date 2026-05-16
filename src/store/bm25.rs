//! BM25 inverted index — third channel in the hybrid search fusion.
//!
//! At index time we build a term → posting list mapping where each posting
//! is `(sym_idx, tf)`. Per-document length is stored alongside `doc_count`
//! and `avg_doc_len` in a separate stats section so we can compute Okapi
//! BM25 at query time without scanning all postings.
//!
//! Tokenization currently relies on `ParsedSymbol.body_tokens` (already
//! deduplicated per symbol by the extractor). This means TF is effectively
//! binary for terms drawn from the body — IDF still discriminates rare
//! vs. common terms across the corpus. A future iteration could re-extract
//! without per-symbol dedup to get true TF counts.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};

/// BM25 parameters (Okapi standard).
pub const K1: f32 = 1.2;
pub const B: f32 = 0.75;

/// Minimal English stop-word list applied to QUERIES (not documents). Kept
/// small on purpose — body_tokens already filters short/uninformative
/// noise via the extractor, and over-aggressive stopword removal hurts
/// recall on code identifiers (`as`, `is` appear in real symbol names).
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "of", "to", "in", "for", "on", "with", "it", "this", "that",
    "and", "or", "but", "if", "as", "at", "by", "from",
];

/// Build a BM25 index from per-symbol term bags.
pub struct Bm25IndexBuilder {
    /// Length (in terms) of each document, indexed by sym_idx.
    doc_lens: Vec<u32>,
    /// Term → list of `(sym_idx, tf)` postings.
    postings: BTreeMap<String, Vec<(u32, u32)>>,
}

impl Bm25IndexBuilder {
    /// Pre-allocate doc-length tracking for `doc_count` symbols.
    pub fn new(doc_count: usize) -> Self {
        Self {
            doc_lens: vec![0; doc_count],
            postings: BTreeMap::new(),
        }
    }

    /// Add a symbol's term bag. `terms` is the pre-tokenized,
    /// pre-lowercased list of unique terms present in the symbol's body.
    /// `sym_idx` must be < `doc_count` from `new`.
    pub fn add_document(&mut self, sym_idx: u32, terms: &[String]) {
        if (sym_idx as usize) >= self.doc_lens.len() {
            tracing::warn!(
                sym_idx,
                doc_count = self.doc_lens.len(),
                "bm25 add_document called with out-of-range sym_idx; terms dropped \
                 (pipeline doc_count miscounted)"
            );
            return;
        }
        let mut term_freq: BTreeMap<&str, u32> = BTreeMap::new();
        for t in terms {
            *term_freq.entry(t.as_str()).or_default() += 1;
        }
        self.doc_lens[sym_idx as usize] = term_freq.values().sum();
        for (term, tf) in term_freq {
            self.postings
                .entry(term.to_string())
                .or_default()
                .push((sym_idx, tf));
        }
    }

    /// Finalize. Returns `(fst_bytes, postings_bytes, stats_bytes)`.
    ///
    /// Posting layout per term:
    /// `[count: u32] [ (sym_idx: u32, tf: u32) ; count ]`
    ///
    /// Stats layout: `[doc_count: u32] [avg_doc_len: f32] [doc_len: u16; doc_count]`.
    pub fn build(self) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        let mut posting_data: Vec<u8> = Vec::new();
        let mut fst_builder = fst::MapBuilder::memory();

        for (term, mut entries) in self.postings {
            entries.sort_unstable_by_key(|(sym, _)| *sym);
            let offset = posting_data.len() as u64;
            let count = entries.len() as u32;
            posting_data.extend_from_slice(&count.to_le_bytes());
            for (sym, tf) in entries {
                posting_data.extend_from_slice(&sym.to_le_bytes());
                posting_data.extend_from_slice(&tf.to_le_bytes());
            }
            fst_builder
                .insert(term.as_bytes(), offset)
                .context("bm25 fst insert")?;
        }

        let fst_bytes = fst_builder.into_inner().context("finalize bm25 fst")?;

        let doc_count = self.doc_lens.len();
        let total_len: u64 = self.doc_lens.iter().map(|&n| n as u64).sum();
        let avg_doc_len = if doc_count == 0 {
            0.0
        } else {
            (total_len as f32) / (doc_count as f32)
        };

        let mut stats_bytes: Vec<u8> = Vec::with_capacity(8 + doc_count * 2);
        stats_bytes.extend_from_slice(&(doc_count as u32).to_le_bytes());
        stats_bytes.extend_from_slice(&avg_doc_len.to_le_bytes());
        let mut clamped = 0usize;
        for &dl in &self.doc_lens {
            // Clamp to u16::MAX — symbols with 65k+ unique terms are
            // pathological (10K-line generated files); we'd rather mis-score
            // them slightly than blow up the stats section size.
            if dl > u16::MAX as u32 {
                clamped += 1;
            }
            let dl16 = dl.min(u16::MAX as u32) as u16;
            stats_bytes.extend_from_slice(&dl16.to_le_bytes());
        }
        if clamped > 0 {
            tracing::warn!(
                clamped,
                "bm25 stats clamped doc_len to u16::MAX for {clamped} symbol(s) \
                 with > 65,535 unique terms — BM25 scores for those docs are \
                 slightly inflated. Likely generated/minified files."
            );
        }

        Ok((fst_bytes, posting_data, stats_bytes))
    }
}

/// Zero-copy reader over the three BM25 sections.
pub struct Bm25Reader<'a> {
    fst: fst::Map<&'a [u8]>,
    postings: &'a [u8],
    stats: &'a [u8],
    doc_count: u32,
    avg_doc_len: f32,
}

impl<'a> Bm25Reader<'a> {
    pub fn new(fst_bytes: &'a [u8], postings: &'a [u8], stats: &'a [u8]) -> Result<Self> {
        let fst = fst::Map::new(fst_bytes).map_err(|e| anyhow::anyhow!("bm25 fst load: {e}"))?;
        if stats.len() < 8 {
            bail!("bm25 stats section too small ({} bytes)", stats.len());
        }
        let doc_count = u32::from_le_bytes(stats[0..4].try_into().unwrap());
        let avg_doc_len = f32::from_le_bytes(stats[4..8].try_into().unwrap());
        let expected = 8 + (doc_count as usize).saturating_mul(2);
        if stats.len() < expected {
            bail!(
                "bm25 stats section truncated (need {expected} bytes, have {})",
                stats.len()
            );
        }
        Ok(Self {
            fst,
            postings,
            stats,
            doc_count,
            avg_doc_len,
        })
    }

    /// Read doc_len for a single document from the stats section.
    fn doc_len(&self, sym_idx: u32) -> u32 {
        if sym_idx >= self.doc_count {
            return 0;
        }
        let off = 8 + (sym_idx as usize) * 2;
        if off + 2 > self.stats.len() {
            return 0;
        }
        u16::from_le_bytes(self.stats[off..off + 2].try_into().unwrap_or([0; 2])) as u32
    }

    /// Read the posting list for a term. Returns `(sym_idx, tf)` pairs.
    fn posting(&self, term: &str) -> Vec<(u32, u32)> {
        let Some(offset) = self.fst.get(term.as_bytes()) else {
            return Vec::new();
        };
        let offset = offset as usize;
        if offset + 4 > self.postings.len() {
            return Vec::new();
        }
        let count = u32::from_le_bytes(
            self.postings[offset..offset + 4]
                .try_into()
                .unwrap_or([0; 4]),
        ) as usize;
        let mut out = Vec::with_capacity(count);
        let mut pos = offset + 4;
        for _ in 0..count {
            if pos + 8 > self.postings.len() {
                break;
            }
            let sym = u32::from_le_bytes(self.postings[pos..pos + 4].try_into().unwrap_or([0; 4]));
            let tf =
                u32::from_le_bytes(self.postings[pos + 4..pos + 8].try_into().unwrap_or([0; 4]));
            out.push((sym, tf));
            pos += 8;
        }
        out
    }

    /// BM25 search. Returns up to `top_k` `(sym_idx, score)` pairs sorted
    /// descending by score. Empty query or query consisting of stopwords →
    /// empty result.
    pub fn search(&self, query: &str, top_k: usize) -> Vec<(u32, f32)> {
        let terms = tokenize_query(query);
        if terms.is_empty() || self.doc_count == 0 {
            return Vec::new();
        }
        let mut scores: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
        let n = self.doc_count as f32;
        for term in &terms {
            let posting = self.posting(term);
            if posting.is_empty() {
                continue;
            }
            let df = posting.len() as f32;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for (sym, tf) in posting {
                let dl = self.doc_len(sym) as f32;
                let norm = if self.avg_doc_len > 0.0 {
                    1.0 - B + B * (dl / self.avg_doc_len)
                } else {
                    1.0
                };
                let tf = tf as f32;
                let term_score = idf * (tf * (K1 + 1.0)) / (tf + K1 * norm);
                *scores.entry(sym).or_insert(0.0) += term_score;
            }
        }
        let mut ranked: Vec<(u32, f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(top_k);
        ranked
    }
}

/// Tokenize a query: lowercase, split on non-alnum/`_`, drop empties +
/// stopwords + 1-char tokens.
pub fn tokenize_query(query: &str) -> Vec<String> {
    let lower = query.to_lowercase();
    let mut out = Vec::new();
    for raw in lower.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.len() < 2 {
            continue;
        }
        if STOPWORDS.contains(&raw) {
            continue;
        }
        out.push(raw.to_string());
    }
    out
}

/// Tokenize a document's term bag (from `body_tokens` + name + signature +
/// doc). Returns lowercased unique terms in stable order. NO stopword
/// filtering at index time so the stopword list can evolve later without
/// re-indexing.
pub fn tokenize_document(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.len() < 2 {
            continue;
        }
        let lower = raw.to_lowercase();
        if seen.insert(lower.clone()) {
            out.push(lower);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_small_index() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut b = Bm25IndexBuilder::new(3);
        // doc 0: rare term "timeout" + common "config"
        b.add_document(0, &["timeout".to_string(), "config".to_string()]);
        // doc 1: "retry" + "config"
        b.add_document(1, &["retry".to_string(), "config".to_string()]);
        // doc 2: just "config" (most common term)
        b.add_document(2, &["config".to_string()]);
        b.build().unwrap()
    }

    #[test]
    fn empty_query_returns_empty() {
        let (fst, posts, stats) = build_small_index();
        let r = Bm25Reader::new(&fst, &posts, &stats).unwrap();
        assert!(r.search("", 10).is_empty());
        assert!(r.search("   ", 10).is_empty());
        assert!(r.search("the", 10).is_empty()); // stopword only
    }

    #[test]
    fn rare_term_finds_only_relevant() {
        let (fst, posts, stats) = build_small_index();
        let r = Bm25Reader::new(&fst, &posts, &stats).unwrap();
        let hits = r.search("timeout", 10);
        assert_eq!(hits.len(), 1, "got {hits:?}");
        assert_eq!(hits[0].0, 0);
    }

    #[test]
    fn common_term_ranks_shorter_doc_higher() {
        let (fst, posts, stats) = build_small_index();
        let r = Bm25Reader::new(&fst, &posts, &stats).unwrap();
        let hits = r.search("config", 10);
        // doc 2 has length 1, docs 0 and 1 have length 2.
        // BM25 normalization should rank the shorter doc higher.
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].0, 2, "shortest doc should rank first: {hits:?}");
    }

    #[test]
    fn idf_penalises_term_in_all_docs() {
        // "config" appears in every doc → low IDF.
        // "timeout" appears in 1 doc → high IDF.
        // Score for "timeout" on doc 0 should beat score for "config" on
        // ANY doc.
        let (fst, posts, stats) = build_small_index();
        let r = Bm25Reader::new(&fst, &posts, &stats).unwrap();
        let timeout_score = r.search("timeout", 1)[0].1;
        let config_score = r.search("config", 1)[0].1;
        assert!(
            timeout_score > config_score,
            "timeout (rare) score={timeout_score} should beat config (common) score={config_score}"
        );
    }

    #[test]
    fn tokenize_query_drops_short_and_stopwords() {
        assert_eq!(tokenize_query(""), Vec::<String>::new());
        assert_eq!(tokenize_query("the a is"), Vec::<String>::new());
        assert_eq!(
            tokenize_query("Handle Timeout Retry"),
            vec!["handle", "timeout", "retry"]
        );
    }

    #[test]
    fn tokenize_document_dedupes() {
        let toks = tokenize_document("config config config retry retry");
        assert_eq!(toks, vec!["config", "retry"]);
    }

    #[test]
    fn empty_index_search_returns_empty() {
        let b = Bm25IndexBuilder::new(0);
        let (fst, posts, stats) = b.build().unwrap();
        let r = Bm25Reader::new(&fst, &posts, &stats).unwrap();
        assert!(r.search("anything", 10).is_empty());
    }
}
