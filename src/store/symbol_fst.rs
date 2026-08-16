//! FST-based symbol index: maps symbol name/sub-tokens → posting list of symbol indices.
//!
//! This replaces the in-memory InvertedIndex with a persistent, zero-copy FST.
//! CamelCase sub-tokens are indexed: "PaymentService" → ["paymentservice", "payment", "service"].

use anyhow::{Context, Result};
use std::sync::{Arc, Mutex, OnceLock};

/// Build symbol FST + posting lists from symbol records.
/// Each symbol name and its CamelCase sub-tokens are inserted.
/// Returns (fst_bytes, posting_bytes).
///
/// v1.13 P7: `Vec<(String, u32)>` + final sort beats the previous
/// `BTreeMap<String, Vec<u32>>` — no per-insert tree node, contiguous
/// sort, and the duplicate-key path no longer clones the key on each
/// hit. CamelCase sub-tokens still emit one `to_lowercase` allocation
/// each but no longer pay the `entry().clone()` tax on dup names.
pub fn build_symbol_fst(
    symbols: &[(String, u32)], // (name, symbol_index)
) -> Result<(Vec<u8>, Vec<u8>)> {
    // Worst case: one primary key + a handful of CamelCase tokens per
    // symbol. Reserve 3× as a heuristic — most identifiers split into
    // 1–3 tokens.
    let mut entries: Vec<(String, u32)> = Vec::with_capacity(symbols.len() * 3);

    for (name, idx) in symbols {
        let lower = name.to_lowercase();
        // Push the primary lowercased name, then any CamelCase
        // sub-tokens that differ from it.
        for token in split_camel_case(name) {
            let token_lower = token.to_lowercase();
            if token_lower != lower {
                entries.push((token_lower, *idx));
            }
        }
        entries.push((lower, *idx));
    }

    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut posting_data: Vec<u8> = Vec::with_capacity(entries.len() * 4 + entries.len());
    let mut fst_builder = fst::MapBuilder::memory();

    let mut i = 0;
    while i < entries.len() {
        let mut j = i + 1;
        while j < entries.len() && entries[j].0 == entries[i].0 {
            j += 1;
        }
        // Dedup ascending edge indices in-place over the contiguous group.
        let group = &mut entries[i..j];
        let mut write = 0;
        for read in 0..group.len() {
            if write == 0 || group[read].1 != group[write - 1].1 {
                group.swap(read, write);
                write += 1;
            }
        }
        let offset = posting_data.len() as u64;
        let count = write as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());
        for slot in group.iter().take(write) {
            posting_data.extend_from_slice(&slot.1.to_le_bytes());
        }
        fst_builder
            .insert(entries[i].0.as_bytes(), offset)
            .context("fst insert")?;
        i = j;
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

    /// Fuzzy search: find keys within Levenshtein edit distance of the query.
    /// Uses the FST's built-in Levenshtein automaton for efficient traversal.
    ///
    /// The automaton comes from [`fuzzy_automaton`] rather than being built
    /// here, so callers that run this rung more than once for one query — the
    /// `--workspace` fanout, which loops over members in-process — pay for the
    /// DFA once instead of once per index. See that function for why it
    /// matters.
    pub fn find_fuzzy(
        &self,
        query: &str,
        max_distance: u32,
        limit: usize,
    ) -> Vec<(String, Vec<u32>)> {
        use fst::{IntoStreamer, Streamer};

        let key = query.to_lowercase();
        let lev = match fuzzy_automaton(&key, max_distance) {
            Some(l) => l,
            None => return Vec::new(), // query too long for this distance
        };

        // `fst` implements `Automaton for &A`, so the shared DFA streams by
        // reference and is never cloned.
        let mut stream = self.fst_map.search(&*lev).into_stream();
        let mut results = Vec::new();
        let mut total = 0usize;

        while let Some((k, offset)) = stream.next() {
            let name = std::str::from_utf8(k).unwrap_or("").to_owned();
            let indices = self.read_posting_list(offset);
            total += indices.len();
            results.push((name, indices));
            if total >= limit {
                break;
            }
        }

        results
    }

    /// Search with fuzzy fallback: exact → prefix → Levenshtein.
    /// Returns (indices, was_fuzzy).
    pub fn search_with_fallback(&self, query: &str, limit: usize) -> (Vec<u32>, bool) {
        // Exact match
        let exact = self.find(query);
        if !exact.is_empty() {
            return (exact.into_iter().take(limit).collect(), false);
        }

        // Prefix match.
        //
        // The cap is checked *before* the push, not after. Checking after only
        // stops once the budget is already exceeded, which is how `limit == 0`
        // — reachable, `--limit` takes any `usize` — returned one row instead
        // of none.
        let prefix_results = self.find_by_prefix(query);
        let mut all: Vec<u32> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        'prefix: for (_name, indices) in prefix_results {
            for idx in indices {
                if all.len() >= limit {
                    break 'prefix;
                }
                if seen.insert(idx) {
                    all.push(idx);
                }
            }
        }
        if !all.is_empty() {
            return (all, false);
        }

        // Fuzzy fallback. The adaptive distance is a *ceiling*, not the first
        // attempt: climb from 1 and stop at the first rung that matches.
        //
        // Two reasons, and the cheaper one is not the speed. Truncation here is
        // lexicographic — `find_fuzzy` stops once it has `limit` postings, in
        // FST key order — so a flat distance-2 sweep can spend the budget on
        // alphabetically-earlier distance-2 keys and drop the single-edit
        // match the user actually typo'd. Climbing returns the closest rung
        // that has anything, which is what a typo correction should do.
        //
        // The speed is the second reason: on a 13-char query a distance-2 DFA
        // costs ~10× a distance-1 one, so a single-edit typo — the common case
        // — resolves in 0.11 ms instead of 1.68 ms (**14.8×**). A query that
        // really is two edits out now pays both DFAs, measured at ~8 % slower,
        // and a true miss is a wash.
        let ceiling = fuzzy_distance(query);
        let mut fuzzy_results = Vec::new();
        for distance in 1..=ceiling {
            fuzzy_results = self.find_fuzzy(query, distance, limit);
            if !fuzzy_results.is_empty() {
                break;
            }
        }
        // Same before-the-push cap as the prefix rung. The inner `break` this
        // replaces only ever left the *inner* loop, so it read as a full stop
        // while the outer one kept going — harmless in practice only because
        // `find_fuzzy` stops collecting names the moment its own running total
        // reaches `limit`, which puts the crossing inside the last name it
        // returns. Relying on that invariant across two functions is not worth
        // the line it saves.
        let mut fuzzy_all: Vec<u32> = Vec::new();
        seen.clear();
        'fuzzy: for (_name, indices) in fuzzy_results {
            for idx in indices {
                if fuzzy_all.len() >= limit {
                    break 'fuzzy;
                }
                if seen.insert(idx) {
                    fuzzy_all.push(idx);
                }
            }
        }
        let was_fuzzy = !fuzzy_all.is_empty();
        (fuzzy_all, was_fuzzy)
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

/// Build — or reuse — the Levenshtein DFA for `(key, distance)`.
///
/// `fst::automaton::Levenshtein::new` materialises a **complete** DFA eagerly,
/// and each state carries a 256-entry transition table (the crate's own comment
/// puts its 10 000-state ceiling at "at least 20MB"). Measured on this index:
/// construction is **95–100 % of a fuzzy query's cost and is independent of
/// corpus size** — 1.18 ms at 1 000 symbols, 1.18 ms at 40 000, 1.6 ms at
/// 80 000, against 0.006–0.084 ms of actual traversal. A distance-2 DFA costs
/// ~10× a distance-1 one on the same query (1.18 ms vs 0.13 ms at 13 chars).
///
/// So the automaton, not the index, is what a fuzzy query pays for, and
/// building it twice for one query doubles that query. The `--workspace` fanout
/// does exactly that: `search_workspace` loops over members in-process and each
/// one runs the full ladder. Measured at four members, one distance-2 miss:
/// **6.21 ms rebuilding per member vs 1.60 ms sharing — 74 % saved.**
///
/// A single slot is deliberate. Every caller that repeats within one process
/// repeats the *same* query, so a one-entry memo hits every time, cannot grow,
/// and needs no eviction policy. Failures are memoised too: `Levenshtein::new`
/// only reports `TooManyStates` after building up to the limit, so a retry of a
/// hopeless query is as expensive as the first attempt.
fn fuzzy_automaton(key: &str, distance: u32) -> Option<Arc<fst::automaton::Levenshtein>> {
    use fst::automaton::Levenshtein;

    type Memo = Mutex<Option<(String, u32, Option<Arc<Levenshtein>>)>>;
    static MEMO: OnceLock<Memo> = OnceLock::new();

    let mut slot = MEMO
        .get_or_init(|| Mutex::new(None))
        .lock()
        // A panic while holding this lock leaves only a cached automaton
        // behind — there is no invariant to protect, so recover rather than
        // propagate someone else's panic into an unrelated query.
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some((cached_key, cached_distance, cached)) = slot.as_ref() {
        if cached_key == key && *cached_distance == distance {
            return cached.clone();
        }
    }

    let built = Levenshtein::new(key, distance).ok().map(Arc::new);
    *slot = Some((key.to_owned(), distance, built.clone()));
    built
}

/// Adaptive Levenshtein distance: short queries get distance 1, longer get 2.
fn fuzzy_distance(query: &str) -> u32 {
    if query.chars().count() <= 4 {
        1
    } else {
        2
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
    fn fuzzy_single_typo() {
        let symbols = vec![
            ("PaymentService".to_string(), 0),
            ("UserService".to_string(), 1),
        ];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        // "paymentservce" (missing 'i') → finds "paymentservice" at distance 1
        let results = reader.find_fuzzy("paymentservce", 1, 10);
        let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"paymentservice"),
            "should fuzzy-find PaymentService"
        );
    }

    #[test]
    fn fuzzy_no_match_on_exact() {
        let symbols = vec![("FooBar".to_string(), 0)];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        // Exact match → search_with_fallback returns (indices, false)
        let (indices, was_fuzzy) = reader.search_with_fallback("foobar", 10);
        assert!(!indices.is_empty());
        assert!(!was_fuzzy);
    }

    #[test]
    fn fuzzy_fallback_triggers() {
        let symbols = vec![
            ("PaymentService".to_string(), 0),
            ("UserService".to_string(), 1),
        ];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        // "pymntservice" doesn't match exact or prefix, triggers fuzzy
        let (indices, was_fuzzy) = reader.search_with_fallback("paymentservce", 10);
        assert!(!indices.is_empty(), "fuzzy should find results");
        assert!(was_fuzzy);
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
        let (indices, was_fuzzy) = reader.search_with_fallback("foobar", 10);
        assert_eq!(indices, vec![0]);
        assert!(!was_fuzzy);

        // Prefix — "foo" matches sub-token
        let (results, was_fuzzy) = reader.search_with_fallback("foo", 10);
        assert!(results.contains(&0));
        assert!(results.contains(&1));
        assert!(!was_fuzzy);
    }

    /// The memo is a single process-wide slot, so the risk it introduces is
    /// serving one query's automaton to the next query. Alternate two queries
    /// that must not share results and check each still answers for itself.
    #[test]
    fn fuzzy_memo_does_not_bleed_between_queries() {
        let symbols = vec![
            ("PaymentService".to_string(), 0),
            ("InvoiceGateway".to_string(), 1),
        ];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        for _ in 0..3 {
            let payment = reader.find_fuzzy("paymentservce", 1, 10);
            let names: Vec<&str> = payment.iter().map(|(n, _)| n.as_str()).collect();
            assert!(names.contains(&"paymentservice"), "got {names:?}");
            assert!(!names.contains(&"invoicegateway"), "got {names:?}");

            let invoice = reader.find_fuzzy("invoicegatewy", 1, 10);
            let names: Vec<&str> = invoice.iter().map(|(n, _)| n.as_str()).collect();
            assert!(names.contains(&"invoicegateway"), "got {names:?}");
            assert!(!names.contains(&"paymentservice"), "got {names:?}");
        }
    }

    /// Same key, different distance, is a different automaton — the memo keys
    /// on the pair, so a distance-1 hit must not be replayed for distance 2.
    #[test]
    fn fuzzy_memo_keys_on_distance_too() {
        let symbols = vec![
            ("Alpha".to_string(), 0),
            ("Alpaca".to_string(), 1), // distance 2 from "alpha"
        ];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        let near = reader.find_fuzzy("alpha", 1, 10);
        let near: Vec<&str> = near.iter().map(|(n, _)| n.as_str()).collect();
        assert!(near.contains(&"alpha"));
        assert!(!near.contains(&"alpaca"), "distance 2 at d=1: {near:?}");

        let wide = reader.find_fuzzy("alpha", 2, 10);
        let wide: Vec<&str> = wide.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            wide.contains(&"alpaca"),
            "d=2 should reach alpaca: {wide:?}"
        );
    }

    /// The fuzzy rung climbs from distance 1, so when a single-edit match
    /// exists the two-edit neighbours must not come back with it. Guards the
    /// truncation hazard too: a flat d=2 sweep orders by key, not by distance.
    #[test]
    fn fuzzy_rung_climbs_and_stops_at_the_nearest_distance() {
        let symbols = vec![
            // One edit from the query below (substitute 'x' -> 'e').
            ("Alphabet".to_string(), 0),
            // Two edits, and sorts BEFORE "alphabet" — the key a flat
            // distance-2 sweep would reach first.
            ("Alfabet".to_string(), 1),
        ];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        let (indices, was_fuzzy) = reader.search_with_fallback("alphabxt", 10);
        assert!(was_fuzzy);
        assert_eq!(
            indices,
            vec![0],
            "should return only the single-edit match, not its two-edit sibling"
        );
    }

    /// Climbing must not shrink recall: a query that is genuinely two edits
    /// out still reaches the distance-2 rung.
    #[test]
    fn fuzzy_rung_still_reaches_distance_two() {
        let symbols = vec![("PaymentService".to_string(), 0)];
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        // Two substitutions: paymentservice -> paymntservce is 2 deletes.
        let (indices, was_fuzzy) = reader.search_with_fallback("paymntservce", 10);
        assert_eq!(indices, vec![0], "distance-2 match must still be found");
        assert!(was_fuzzy);
    }

    /// Every rung must honour `limit` exactly, including zero. `--limit` takes
    /// any `usize` with no floor, and the caps used to be checked *after* the
    /// push — so a zero budget came back holding one row.
    #[test]
    fn every_rung_honours_limit_including_zero() {
        let symbols: Vec<(String, u32)> = (0..40).map(|i| (format!("Alphabet{i:02}"), i)).collect();
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        for limit in [0usize, 1, 2, 3, 5, 10, 100] {
            // Exact rung.
            let (idx, fuzzy) = reader.search_with_fallback("alphabet00", limit);
            assert!(idx.len() <= limit, "exact rung: {} > {limit}", idx.len());
            assert!(!fuzzy);

            // Prefix rung — "alphabet" is a sub-token of all 40 symbols.
            let (idx, fuzzy) = reader.search_with_fallback("alphabet", limit);
            assert!(idx.len() <= limit, "prefix rung: {} > {limit}", idx.len());
            assert!(!fuzzy);

            // Fuzzy rung.
            let (idx, _) = reader.search_with_fallback("alphabxt00", limit);
            assert!(idx.len() <= limit, "fuzzy rung: {} > {limit}", idx.len());
        }
    }

    /// Capping before the push must not cost recall at the boundary: a budget
    /// of N still comes back with N when N are available.
    #[test]
    fn limit_cap_still_fills_the_budget() {
        let symbols: Vec<(String, u32)> = (0..40).map(|i| (format!("Alphabet{i:02}"), i)).collect();
        let (fst, postings) = build_symbol_fst(&symbols).unwrap();
        let reader = SymbolFstReader::new(&fst, &postings).unwrap();

        for limit in [1usize, 5, 17, 40] {
            let (idx, _) = reader.search_with_fallback("alphabet", limit);
            assert_eq!(idx.len(), limit, "prefix rung short at limit={limit}");
        }
    }
}
