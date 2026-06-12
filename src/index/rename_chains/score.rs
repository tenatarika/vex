//! Scoring helpers for rename-chain detection.
//!
//! Two primitives the orchestrator (see `mod.rs::build_rename_chains`)
//! consumes:
//!
//! 1. **Interned-token Jaccard** — body_tokens are already lowercased
//!    by the extractor (see `src/store/body_tokens.rs`); we intern each
//!    unique token into a `u32` id during the build, then compute
//!    Jaccard as a sorted-merge intersection over `&[u32]`. No per-pair
//!    `HashSet` allocation, fully cache-friendly.
//!
//! 2. **MiniLM cosine** — keyed by `context_hash` (see
//!    `src/embed/cache.rs::context_hash`). The orchestrator owns the
//!    `vectors: &[Vec<f32>]` and `hashes: &[u64]` slices produced by
//!    the embedding pipeline; `CosineLookup` builds a `hash → &[f32]`
//!    map once and the hot loop calls `cosine(h_a, h_b)` per candidate
//!    pair without hitting the HNSW.
//!
//! Both primitives are `pub(crate)` only — they are an implementation
//! detail of the `rename_chains` submodule, not part of the crate's
//! public API.

use std::collections::HashMap;

use crate::search::semantic::{cosine_similarity, dot_product};

// =====================================================================
// Token interner
// =====================================================================

/// Per-build token interner. Maps each unique token string to a stable
/// `u32` id so Jaccard becomes a merge-style intersection on sorted
/// `Vec<u32>` slices (no per-pair `HashSet` allocation).
///
/// Build once for an entire `build_rename_chains` invocation, share
/// immutably across the rayon parallel section. The interner is *not*
/// thread-safe for inserts; populate it serially before the parallel
/// section starts.
pub(crate) struct TokenInterner {
    table: HashMap<String, u32>,
    next_id: u32,
}

impl TokenInterner {
    pub(crate) fn new() -> Self {
        Self {
            table: HashMap::new(),
            next_id: 0,
        }
    }

    /// Intern `tok`; subsequent calls with the same `tok` return the
    /// same `u32`.
    pub(crate) fn intern(&mut self, tok: &str) -> u32 {
        if let Some(&id) = self.table.get(tok) {
            return id;
        }
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).expect(
            "TokenInterner overflowed u32::MAX unique tokens — \
             rename-chain corpus is unrealistically large",
        );
        self.table.insert(tok.to_string(), id);
        id
    }

    /// Tokenise `body` (whitespace-split — lowercasing is the
    /// extractor's job, NOT this interner's) and return a sorted-deduped
    /// `Vec<u32>`. `None` or an empty / whitespace-only `body` → empty
    /// `Vec`.
    ///
    /// "Sorted-deduped" is the precondition for [`jaccard_sorted`].
    pub(crate) fn tokenise(&mut self, body: Option<&str>) -> Vec<u32> {
        let Some(text) = body else { return Vec::new() };
        if text.is_empty() {
            return Vec::new();
        }
        let mut ids: Vec<u32> = text
            .split_whitespace()
            .map(|tok| self.intern(tok))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    pub(crate) fn unique_token_count(&self) -> usize {
        self.table.len()
    }
}

// =====================================================================
// Sorted-merge Jaccard
// =====================================================================

/// Jaccard similarity `|A ∩ B| / |A ∪ B|` over two sorted-deduped
/// `&[u32]` slices.
///
/// Linear in `a.len() + b.len()`, allocation-free.
///
/// * Empty ∩ Empty → `1.0` (vacuously identical — two symbols with
///   empty bodies are treated as a perfect match).
/// * Empty vs non-empty → `0.0`.
///
/// **Precondition** (debug-checked): both slices are strictly sorted
/// ascending with no duplicates. Violating this in release builds will
/// silently return a wrong number — populate inputs via
/// [`TokenInterner::tokenise`].
pub(crate) fn jaccard_sorted(a: &[u32], b: &[u32]) -> f32 {
    #[cfg(debug_assertions)]
    {
        debug_assert!(
            a.windows(2).all(|w| w[0] < w[1]),
            "jaccard_sorted: `a` is not strictly sorted",
        );
        debug_assert!(
            b.windows(2).all(|w| w[0] < w[1]),
            "jaccard_sorted: `b` is not strictly sorted",
        );
    }

    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let mut i = 0usize;
    let mut j = 0usize;
    let mut intersection: u32 = 0;
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                intersection += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }

    // |A ∪ B| = |A| + |B| - |A ∩ B|
    let union = a.len() as u32 + b.len() as u32 - intersection;
    intersection as f32 / union as f32
}

// =====================================================================
// CosineLookup
// =====================================================================

/// Holds `context_hash -> &[f32]` so the rename-chain builder's hot
/// loop can compute cosine for arbitrary pairs without hitting the
/// HNSW.
///
/// Lifetimes: borrows the vectors from the embedding pipeline; the
/// lookup is alive for the duration of `build_rename_chains`.
///
/// Duplicate-hash policy: keep first (matches `HnswHandle::open`
/// semantics — see `src/search/semantic.rs:179`). Second and later
/// vectors at the same hash are unreachable.
#[doc(hidden)] pub struct CosineLookup<'a> {
    by_hash: HashMap<u64, &'a [f32]>,
    normalized: bool,
}

impl<'a> CosineLookup<'a> {
    /// `vectors[i]` is paired with `hashes[i]` (same order the
    /// `build_hnsw_at` pipeline uses). Panics in dev builds (and is
    /// an unrecoverable caller bug in release) if the two slices have
    /// different lengths.
    pub(crate) fn from_hashed_vectors(
        vectors: &'a [Vec<f32>],
        hashes: &'a [u64],
        normalized: bool,
    ) -> Self {
        assert_eq!(
            vectors.len(),
            hashes.len(),
            "CosineLookup::from_hashed_vectors: vectors / hashes length mismatch",
        );
        let mut by_hash: HashMap<u64, &'a [f32]> = HashMap::with_capacity(vectors.len());
        for (vec, &hash) in vectors.iter().zip(hashes.iter()) {
            // keep-first: matches HnswHandle::open's
            // `entry().or_insert` semantics.
            by_hash.entry(hash).or_insert_with(|| vec.as_slice());
        }
        Self {
            by_hash,
            normalized,
        }
    }

    /// Cosine similarity ∈ `[-1.0, 1.0]` for two hashes, or `0.0` if
    /// either hash is missing (e.g. duplicate-key skipped at build).
    ///
    /// Picks [`dot_product`] when `normalized` (the fast path the
    /// brute-force search uses for L2-normalized vectors), otherwise
    /// [`cosine_similarity`].
    pub(crate) fn cosine(&self, h_a: u64, h_b: u64) -> f32 {
        let (Some(&a), Some(&b)) = (self.by_hash.get(&h_a), self.by_hash.get(&h_b)) else {
            return 0.0;
        };
        if self.normalized {
            dot_product(a, b)
        } else {
            cosine_similarity(a, b)
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.by_hash.len()
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ----- TokenInterner / tokenise ----------------------------------

    #[test]
    fn interner_returns_same_id_for_repeated_tokens() {
        let mut interner = TokenInterner::new();
        let a = interner.intern("foo");
        let b = interner.intern("foo");
        assert_eq!(a, b);
    }

    #[test]
    fn interner_assigns_new_id_for_new_token() {
        let mut interner = TokenInterner::new();
        let foo = interner.intern("foo");
        let bar = interner.intern("bar");
        assert_ne!(foo, bar);
    }

    #[test]
    fn tokenise_none_returns_empty() {
        let mut interner = TokenInterner::new();
        assert!(interner.tokenise(None).is_empty());
    }

    #[test]
    fn tokenise_empty_string_returns_empty() {
        let mut interner = TokenInterner::new();
        assert!(interner.tokenise(Some("")).is_empty());
    }

    #[test]
    fn tokenise_whitespace_only_returns_empty() {
        let mut interner = TokenInterner::new();
        assert!(interner.tokenise(Some("   \t\n  ")).is_empty());
    }

    #[test]
    fn tokenise_sorts_and_dedups() {
        let mut interner = TokenInterner::new();
        let ids = interner.tokenise(Some("fn foo bar foo"));
        assert_eq!(ids.len(), 3, "expected 3 unique tokens, got {ids:?}");
        // Strictly ascending.
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "tokenise output is not strictly sorted: {ids:?}",
        );
    }

    #[test]
    fn tokenise_does_not_lowercase() {
        // The extractor is responsible for lowercasing — the interner
        // must trust the caller. "Foo" and "foo" must be distinct.
        let mut interner = TokenInterner::new();
        let upper = interner.intern("Foo");
        let lower = interner.intern("foo");
        assert_ne!(upper, lower);
    }

    #[test]
    fn two_interners_are_independent() {
        let mut a = TokenInterner::new();
        let mut b = TokenInterner::new();
        let _ = a.intern("alpha");
        let _ = a.intern("beta");
        let id_in_b = b.intern("gamma");
        // `b` starts from id=0, regardless of `a`'s state.
        assert_eq!(id_in_b, 0);
    }

    #[test]
    fn unique_token_count_tracks_distinct_intern_calls() {
        let mut interner = TokenInterner::new();
        let _ = interner.intern("a");
        let _ = interner.intern("b");
        let _ = interner.intern("a"); // repeat, no new id
        assert_eq!(interner.unique_token_count(), 2);
    }

    // ----- jaccard_sorted -------------------------------------------

    #[test]
    fn jaccard_empty_empty_is_one() {
        assert_eq!(jaccard_sorted(&[], &[]), 1.0);
    }

    #[test]
    fn jaccard_empty_vs_non_empty_is_zero() {
        assert_eq!(jaccard_sorted(&[], &[1, 2, 3]), 0.0);
        assert_eq!(jaccard_sorted(&[1, 2, 3], &[]), 0.0);
    }

    #[test]
    fn jaccard_identical_is_one() {
        assert_eq!(jaccard_sorted(&[1, 2, 3], &[1, 2, 3]), 1.0);
    }

    #[test]
    fn jaccard_disjoint_is_zero() {
        assert_eq!(jaccard_sorted(&[1, 2, 3], &[4, 5, 6]), 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        // |∩| = {2,3,4} = 3, |∪| = {1,2,3,4,5} = 5 → 0.6
        let got = jaccard_sorted(&[1, 2, 3, 4], &[2, 3, 4, 5]);
        assert!((got - 0.6).abs() < 1e-6, "expected ~0.6, got {got}");
    }

    #[test]
    fn jaccard_single_element_intersection() {
        assert_eq!(jaccard_sorted(&[1], &[1]), 1.0);
    }

    #[test]
    fn jaccard_subset() {
        // |∩| = 2, |∪| = 4 → 0.5
        let got = jaccard_sorted(&[1, 2], &[1, 2, 3, 4]);
        assert!((got - 0.5).abs() < 1e-6, "expected 0.5, got {got}");
    }

    #[test]
    fn jaccard_integrates_with_tokenise() {
        let mut interner = TokenInterner::new();
        let a = interner.tokenise(Some("fn foo bar baz"));
        let b = interner.tokenise(Some("fn foo bar qux"));
        // 3 shared (fn/foo/bar) / 5 union (fn/foo/bar/baz/qux) = 0.6
        let got = jaccard_sorted(&a, &b);
        assert!((got - 0.6).abs() < 1e-6, "expected ~0.6, got {got}");
    }

    // ----- CosineLookup ---------------------------------------------

    #[test]
    fn cosine_lookup_empty_construction() {
        let vectors: Vec<Vec<f32>> = Vec::new();
        let hashes: Vec<u64> = Vec::new();
        let lookup = CosineLookup::from_hashed_vectors(&vectors, &hashes, false);
        assert_eq!(lookup.len(), 0);
        // Any lookup misses → 0.0.
        assert_eq!(lookup.cosine(1, 2), 0.0);
    }

    #[test]
    #[should_panic(expected = "vectors / hashes length mismatch")]
    fn cosine_lookup_length_mismatch_panics() {
        let vectors = vec![vec![1.0_f32, 0.0]];
        let hashes: Vec<u64> = vec![]; // mismatch
        let _ = CosineLookup::from_hashed_vectors(&vectors, &hashes, false);
    }

    #[test]
    fn cosine_lookup_duplicate_hash_keeps_first() {
        // Two distinct vectors at the same hash; the second is
        // unreachable. We verify by computing self-cosine on the
        // duplicate hash — must match the *first* vector's norm.
        let vectors = vec![
            vec![3.0_f32, 4.0], // norm 5
            vec![1.0_f32, 0.0], // unreachable second
        ];
        let hashes = vec![42u64, 42u64];
        let lookup = CosineLookup::from_hashed_vectors(&vectors, &hashes, false);
        assert_eq!(lookup.len(), 1);
        // self-cosine of [3,4] = 1.0 (any non-zero vector against
        // itself).
        let got = lookup.cosine(42, 42);
        assert!((got - 1.0).abs() < 1e-6, "expected 1.0, got {got}");
    }

    #[test]
    fn cosine_self_lookup_is_one_for_non_zero_vector() {
        let vectors = vec![vec![1.0_f32, 2.0, 3.0]];
        let hashes = vec![7u64];
        let lookup = CosineLookup::from_hashed_vectors(&vectors, &hashes, false);
        let got = lookup.cosine(7, 7);
        assert!((got - 1.0).abs() < 1e-6, "expected 1.0, got {got}");
    }

    #[test]
    fn cosine_missing_hash_returns_zero() {
        let vectors = vec![vec![1.0_f32, 0.0]];
        let hashes = vec![1u64];
        let lookup = CosineLookup::from_hashed_vectors(&vectors, &hashes, false);
        assert_eq!(lookup.cosine(1, 999), 0.0);
        assert_eq!(lookup.cosine(999, 1), 0.0);
        assert_eq!(lookup.cosine(999, 998), 0.0);
    }

    #[test]
    fn cosine_orthogonal_vectors_is_zero() {
        let vectors = vec![vec![1.0_f32, 0.0], vec![0.0_f32, 1.0]];
        let hashes = vec![10u64, 20u64];
        let lookup = CosineLookup::from_hashed_vectors(&vectors, &hashes, false);
        let got = lookup.cosine(10, 20);
        assert!(got.abs() < 1e-6, "expected ~0.0, got {got}");
    }

    #[test]
    fn cosine_normalized_and_unnormalized_match_for_unit_vectors() {
        // Two L2-normalized vectors. Both code paths should agree.
        // [3/5, 4/5] and [4/5, 3/5] — both already L2-normalized.
        let a = vec![0.6_f32, 0.8];
        let b = vec![0.8_f32, 0.6];
        let vectors = vec![a, b];
        let hashes = vec![100u64, 200u64];

        let normalized = CosineLookup::from_hashed_vectors(&vectors, &hashes, true);
        let unnormalized = CosineLookup::from_hashed_vectors(&vectors, &hashes, false);

        let fast = normalized.cosine(100, 200);
        let slow = unnormalized.cosine(100, 200);

        // Expected: 0.6*0.8 + 0.8*0.6 = 0.96
        assert!(
            (fast - 0.96).abs() < 1e-6,
            "fast path: expected 0.96, got {fast}"
        );
        assert!(
            (slow - 0.96).abs() < 1e-6,
            "slow path: expected 0.96, got {slow}"
        );
        assert!(
            (fast - slow).abs() < 1e-6,
            "fast vs slow path disagree: {fast} vs {slow}",
        );
    }

    #[test]
    fn cosine_normalized_path_uses_dot_product() {
        // Construct a case where dot_product != cosine_similarity to
        // confirm the `normalized` switch is actually live. Vectors
        // are NOT unit-length: dot=2, cosine=2/(sqrt(2)*sqrt(2))=1.0.
        let vectors = vec![vec![1.0_f32, 1.0], vec![1.0_f32, 1.0]];
        let hashes = vec![1u64, 2u64];

        let normalized = CosineLookup::from_hashed_vectors(&vectors, &hashes, true);
        let unnormalized = CosineLookup::from_hashed_vectors(&vectors, &hashes, false);

        // Fast path: dot = 1*1 + 1*1 = 2.0
        let fast = normalized.cosine(1, 2);
        // Slow path: cosine = 2 / (sqrt(2) * sqrt(2)) = 1.0
        let slow = unnormalized.cosine(1, 2);

        assert!(
            (fast - 2.0).abs() < 1e-6,
            "fast path: expected 2.0, got {fast}"
        );
        assert!(
            (slow - 1.0).abs() < 1e-6,
            "slow path: expected 1.0, got {slow}"
        );
    }
}
