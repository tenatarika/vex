//! FST-based symbol index: maps symbol name/sub-tokens → posting list of symbol indices.
//!
//! This replaces the in-memory InvertedIndex with a persistent, zero-copy FST.
//! CamelCase sub-tokens are indexed: "PaymentService" → ["paymentservice", "payment", "service"].

use std::collections::BTreeMap;

use anyhow::{Context, Result};

/// Build symbol FST + posting lists from symbol records.
/// Each symbol name and its CamelCase sub-tokens are inserted.
/// Returns (fst_bytes, posting_bytes).
pub fn build_symbol_fst(
    symbols: &[(String, u32)], // (name, symbol_index)
) -> Result<(Vec<u8>, Vec<u8>)> {
    // Group symbol indices by lowercased name and sub-tokens
    let mut entries: BTreeMap<String, Vec<u32>> = BTreeMap::new();

    for (name, idx) in symbols {
        let lower = name.to_lowercase();
        entries.entry(lower.clone()).or_default().push(*idx);

        // CamelCase sub-tokens
        for token in split_camel_case(name) {
            let token_lower = token.to_lowercase();
            if token_lower != lower {
                entries.entry(token_lower).or_default().push(*idx);
            }
        }
    }

    // Build posting lists
    let capacity: usize = entries.values().map(|v| 4 + v.len() * 4).sum();
    let mut posting_data: Vec<u8> = Vec::with_capacity(capacity);
    let mut fst_builder = fst::MapBuilder::memory();

    for (key, indices) in &mut entries {
        // Deduplicate indices (CamelCase tokens can produce duplicates like "FooFoo")
        indices.sort_unstable();
        indices.dedup();

        let offset = posting_data.len() as u64;

        let count = indices.len() as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());

        for &idx in indices.iter() {
            posting_data.extend_from_slice(&idx.to_le_bytes());
        }

        fst_builder
            .insert(key.as_bytes(), offset)
            .context("fst insert")?;
    }

    let fst_bytes = fst_builder.into_inner().context("finalize fst")?;
    Ok((fst_bytes, posting_data))
}

/// Read symbol indices from persistent FST. Zero-copy from mmap.
pub struct SymbolFstReader<'a> {
    fst_map: fst::Map<&'a [u8]>,
    posting_data: &'a [u8],
}

impl<'a> SymbolFstReader<'a> {
    pub fn new(fst_bytes: &'a [u8], posting_bytes: &'a [u8]) -> Result<Self> {
        let fst_map = fst::Map::new(fst_bytes).map_err(|e| anyhow::anyhow!("fst load: {e}"))?;
        Ok(Self {
            fst_map,
            posting_data: posting_bytes,
        })
    }

    /// Exact lookup: find all symbol indices for a name (lowercased).
    pub fn find(&self, name: &str) -> Vec<u32> {
        let key = name.to_lowercase();
        match self.fst_map.get(key.as_bytes()) {
            Some(offset) => self.read_posting_list(offset),
            None => Vec::new(),
        }
    }

    /// Prefix search: find all matching names and their symbol indices.
    pub fn find_by_prefix(&self, prefix: &str) -> Vec<(String, Vec<u32>)> {
        use fst::automaton::{Automaton, Str};
        use fst::{IntoStreamer, Streamer};

        let key = prefix.to_lowercase();
        let automaton = Str::new(&key).starts_with();
        let mut stream = self.fst_map.search(automaton).into_stream();
        let mut results = Vec::new();

        while let Some((k, offset)) = stream.next() {
            let name = std::str::from_utf8(k).unwrap_or("").to_owned();
            let indices = self.read_posting_list(offset);
            results.push((name, indices));
        }

        results
    }

    /// Search: exact match first, then prefix fallback. Returns deduplicated symbol indices.
    pub fn search(&self, query: &str, limit: usize) -> Vec<u32> {
        // Exact match
        let exact = self.find(query);
        if !exact.is_empty() {
            return exact.into_iter().take(limit).collect();
        }

        // Prefix match — collect and deduplicate
        let prefix_results = self.find_by_prefix(query);
        let mut all: Vec<u32> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for (_name, indices) in prefix_results {
            for idx in indices {
                if seen.insert(idx) {
                    all.push(idx);
                    if all.len() >= limit {
                        return all;
                    }
                }
            }
        }

        all
    }

    fn read_posting_list(&self, offset: u64) -> Vec<u32> {
        let offset = offset as usize;
        if offset + 4 > self.posting_data.len() {
            tracing::warn!(offset, "symbol posting list offset out of bounds");
            return Vec::new();
        }

        let count = u32::from_le_bytes(
            self.posting_data[offset..offset + 4]
                .try_into()
                .unwrap_or([0; 4]),
        ) as usize;

        let entry_size = 4; // u32 symbol_idx
        let data_start = offset + 4;

        let data_end = match count
            .checked_mul(entry_size)
            .and_then(|n| data_start.checked_add(n))
        {
            Some(end) => end,
            None => {
                tracing::warn!(count, "symbol posting list count overflow");
                return Vec::new();
            }
        };

        if data_end > self.posting_data.len() {
            tracing::warn!(count, "symbol posting list truncated");
            return Vec::new();
        }

        let mut indices = Vec::with_capacity(count);
        for i in 0..count {
            let base = data_start + i * entry_size;
            let idx = u32::from_le_bytes(
                self.posting_data[base..base + 4]
                    .try_into()
                    .unwrap_or([0; 4]),
            );
            indices.push(idx);
        }

        indices
    }
}

/// Split CamelCase identifiers into sub-tokens.
/// Handles lowercase→Uppercase and Uppercase→Uppercase+Lowercase transitions.
/// "PaymentService" → ["Payment", "Service"]
/// "HTTPSClient" → ["HTTPS", "Client"]
/// "getHTTPResponse" → ["get", "HTTP", "Response"]
fn split_camel_case(s: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0;

    for i in 1..bytes.len() {
        let prev_lower = bytes[i - 1].is_ascii_lowercase();
        let curr_upper = bytes[i].is_ascii_uppercase();

        // lowercase → Uppercase: "payment|Service"
        if prev_lower && curr_upper {
            tokens.push(&s[start..i]);
            start = i;
        }

        // Uppercase → Uppercase + Lowercase: "HTTPS|Client" (split before the last uppercase)
        if i + 1 < bytes.len()
            && bytes[i - 1].is_ascii_uppercase()
            && bytes[i].is_ascii_uppercase()
            && bytes[i + 1].is_ascii_lowercase()
        {
            tokens.push(&s[start..i]);
            start = i;
        }
    }
    if start < s.len() {
        tokens.push(&s[start..]);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_search() {
        let symbols = vec![
            ("PaymentService".to_string(), 0),
            ("PaymentGateway".to_string(), 1),
            ("UserService".to_string(), 2),
        ];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        assert_eq!(reader.find("paymentservice"), vec![0]);
        assert_eq!(reader.find("userservice"), vec![2]);
        assert!(reader.find("nonexistent").is_empty());
    }

    #[test]
    fn camel_case_sub_token_search() {
        let symbols = vec![
            ("PaymentService".to_string(), 0),
            ("PaymentGateway".to_string(), 1),
            ("UserService".to_string(), 2),
        ];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        // "payment" sub-token matches both Payment* symbols
        let results = reader.find("payment");
        assert!(results.contains(&0));
        assert!(results.contains(&1));
        assert!(!results.contains(&2));

        // "service" sub-token matches all *Service symbols
        let results = reader.find("service");
        assert!(results.contains(&0));
        assert!(results.contains(&2));
    }

    #[test]
    fn prefix_search() {
        let symbols = vec![
            ("PaymentService".to_string(), 0),
            ("PaymentGateway".to_string(), 1),
        ];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        let results = reader.find_by_prefix("payment");
        assert!(results.len() >= 2); // "payment", "paymentgateway", "paymentservice"
    }

    #[test]
    fn acronym_split() {
        let symbols = vec![
            ("HTTPSClient".to_string(), 0),
            ("getHTTPResponse".to_string(), 1),
        ];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        // "https" sub-token from HTTPSClient
        let results = reader.find("https");
        assert!(results.contains(&0), "should find HTTPSClient via 'https'");

        // "http" sub-token from getHTTPResponse
        let results = reader.find("http");
        assert!(
            results.contains(&1),
            "should find getHTTPResponse via 'http'"
        );

        // "client" sub-token
        let results = reader.find("client");
        assert!(results.contains(&0), "should find HTTPSClient via 'client'");
    }

    #[test]
    fn search_exact_then_prefix() {
        let symbols = vec![
            ("FooBar".to_string(), 0),
            ("FooBaz".to_string(), 1),
            ("Qux".to_string(), 2),
        ];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        // Exact match
        assert_eq!(reader.search("foobar", 10), vec![0]);

        // Prefix — "foo" matches sub-token
        let results = reader.search("foo", 10);
        assert!(results.contains(&0));
        assert!(results.contains(&1));
    }
}
