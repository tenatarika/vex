//! Phase 14.10 — MinHash signature computation.
//!
//! Self-implemented MinHash with `MINHASH_HASHES` (240) seeded
//! `xxh3_64_with_seed` hashes per signature. The signature is used
//! both directly (to estimate Jaccard similarity between two token
//! sets without materialising the sets) and as input to the LSH
//! banding layer (`lsh.rs`) for sub-quadratic candidate pruning.
//!
//! ## Hash family note (rust M3)
//!
//! xxh3 is **not** a universal hash family in the theoretical sense
//! — strict MinHash variance bounds assume universality. At vex's
//! scale (≤ 50k symbols, 240 signature slots, body_tokens ≤ 400 B
//! ≈ ≤ ~50 tokens per symbol) empirical estimator variance stays
//! within the analytic bound for universal MinHash; the design doc
//! cites this and the literature pass agrees. If accuracy
//! regressions surface in the CodeShovel oracle eval, swap for a
//! true universal family (e.g. tabulation hashing) — but for v1,
//! xxh3 is documented-good and saves us a dependency.
//!
//! ## API shape
//!
//! `Signature` is `Vec<u32>` rather than `[u32; MINHASH_HASHES]` so
//! the const doesn't leak through every call site. Changing
//! `MINHASH_HASHES` would be a sidecar-format break regardless of
//! the type signature, so the const-vs-Vec choice is purely
//! ergonomic.

use xxhash_rust::xxh3::xxh3_64_with_seed;

use crate::index::rename_chains::weights::MINHASH_HASHES;

/// MinHash signature — conceptually `[u32; MINHASH_HASHES]`. Stored
/// as `Vec<u32>` so the type signature doesn't drag `MINHASH_HASHES`
/// through every call site (a const bump would still be a sidecar
/// format break, but the type stays the same).
pub(crate) type Signature = Vec<u32>;

/// Compute a MinHash signature over a token set.
///
/// For each slot `i` in `0..MINHASH_HASHES`:
///
/// ```text
/// sig[i] = min over t in tokens of
///          xxh3_64_with_seed(t.as_bytes(), i as u64) as u32
/// ```
///
/// Empty token sets produce a signature of all `u32::MAX` — the
/// identity element for `min`. Token order is irrelevant by
/// construction (set semantics).
pub(crate) fn signature(tokens: &[&str]) -> Signature {
    let mut sig = vec![u32::MAX; MINHASH_HASHES];
    for token in tokens {
        let bytes = token.as_bytes();
        for (slot, out) in sig.iter_mut().enumerate() {
            // Truncating the u64 digest to u32 halves the per-slot
            // cost vs. storing the full 64-bit hash; for J ≥ 0.7
            // estimation with 240 slots the additional collision
            // probability is negligible (≤ 2⁻³² per slot).
            let h = xxh3_64_with_seed(bytes, slot as u64) as u32;
            if h < *out {
                *out = h;
            }
        }
    }
    sig
}

/// Estimate Jaccard similarity between two token sets from their
/// MinHash signatures.
///
/// Returns `# matching slots / MINHASH_HASHES`. Unbiased estimator
/// for J(A, B); standard deviation under the universal-hash
/// assumption is `sqrt(J(1-J)/MINHASH_HASHES)`, so at J = 0.78 and
/// 240 slots, σ ≈ 0.027 — comfortably tight enough for the
/// `GATE_JACCARD = 0.70` pre-filter.
///
/// Returns 0.0 if either signature is empty (degenerate input);
/// signatures of mismatched lengths are treated as fully disjoint
/// rather than panicking, on the principle that a length mismatch
/// is a sidecar-version skew the caller should have caught earlier.
pub(crate) fn estimate_jaccard(a: &Signature, b: &Signature) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let matches = a.iter().zip(b.iter()).filter(|(x, y)| x == y).count();
    matches as f32 / a.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_token_set_produces_all_max_signature() {
        // Identity-element check: `min` over an empty set leaves
        // every slot at its initial `u32::MAX`. Downstream LSH
        // hashing must still see a valid (if degenerate) sig.
        let sig = signature(&[]);
        assert_eq!(sig.len(), MINHASH_HASHES);
        assert!(sig.iter().all(|&v| v == u32::MAX));
    }

    #[test]
    fn signature_length_is_exactly_minhash_hashes() {
        // Pins the API contract — any token set produces a
        // signature of exactly MINHASH_HASHES slots so the LSH
        // banding layer's `b * r` indexing stays in bounds.
        for tokens in [
            vec![],
            vec!["a"],
            vec!["a", "b"],
            vec!["a", "b", "c", "d", "e", "f", "g", "h"],
        ] {
            let sig = signature(&tokens);
            assert_eq!(sig.len(), MINHASH_HASHES);
        }
    }

    #[test]
    fn identical_token_sets_produce_identical_signatures() {
        let tokens = ["fn", "foo", "let", "x", "return"];
        let a = signature(&tokens);
        let b = signature(&tokens);
        assert_eq!(a, b);
    }

    #[test]
    fn deterministic_across_runs() {
        // Same input → byte-identical signature across calls. xxh3
        // is deterministic but a future refactor that introduced a
        // randomised seed (e.g. `RandomState` for HashMap) would
        // silently break the LSH band fingerprints.
        let tokens = ["alpha", "beta", "gamma"];
        let a = signature(&tokens);
        let b = signature(&tokens);
        let c = signature(&tokens);
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn order_independent() {
        // Set semantics: token order must not affect the signature.
        // If the impl ever switches to a hash that depends on
        // insertion order (e.g. xxh3 streaming), this would catch
        // the regression.
        let forward = ["fn", "foo", "bar", "baz"];
        let backward = ["baz", "bar", "foo", "fn"];
        assert_eq!(signature(&forward), signature(&backward));
    }

    #[test]
    fn disjoint_token_sets_produce_near_zero_matches() {
        // Two fully disjoint sets share no tokens → expected
        // Jaccard 0. With 240 slots and u32-truncated hashes,
        // accidental collisions are bounded by 240 × 2⁻³² ≈ 5×10⁻⁸
        // per pair, so a strict `< 0.05` upper bound is safe.
        let a_tokens = ["alpha", "beta", "gamma", "delta", "epsilon"];
        let b_tokens = ["one", "two", "three", "four", "five"];
        let a = signature(&a_tokens);
        let b = signature(&b_tokens);
        let j = estimate_jaccard(&a, &b);
        assert!(j < 0.05, "disjoint sets should estimate near 0, got {j}");
    }

    #[test]
    fn overlapping_token_sets_estimate_jaccard_within_tolerance() {
        // |A ∩ B| = 7, |A ∪ B| = 9 → true Jaccard = 7/9 ≈ 0.778.
        // Under universal MinHash with 240 slots, σ ≈ 0.027; ±0.15
        // is a generous 5σ band that catches gross regressions
        // without flaking on the non-universal-hash variance bump.
        let a_tokens = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let b_tokens = ["a", "b", "c", "d", "e", "f", "g", "z"];
        let a = signature(&a_tokens);
        let b = signature(&b_tokens);
        let j = estimate_jaccard(&a, &b);
        let expected = 7.0 / 9.0;
        assert!(
            (j - expected).abs() < 0.15,
            "estimator out of band: got {j}, expected {expected} ± 0.15",
        );
    }

    #[test]
    fn estimate_jaccard_self_is_one() {
        // J(A, A) must be exactly 1.0 for any signature — every
        // slot matches itself. A regression here would shift the
        // composite-score gate's anchor.
        let tokens = ["one", "two", "three"];
        let s = signature(&tokens);
        assert!((estimate_jaccard(&s, &s) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn estimate_jaccard_handles_empty_signatures() {
        // Both empty and length-mismatch cases must not panic.
        // Returning 0.0 lets the caller treat the pair as
        // non-matching without a separate error path.
        let empty: Signature = Vec::new();
        let nonempty = signature(&["a"]);
        assert_eq!(estimate_jaccard(&empty, &empty), 0.0);
        assert_eq!(estimate_jaccard(&empty, &nonempty), 0.0);
        assert_eq!(estimate_jaccard(&nonempty, &empty), 0.0);
    }

    #[test]
    fn estimate_jaccard_returns_zero_on_length_mismatch() {
        // A length mismatch can only happen if the caller crosses
        // sidecar versions — treat as fully disjoint rather than
        // panicking, on the principle that the corruption is
        // upstream and we shouldn't add a separate error path here.
        let short = vec![0u32; MINHASH_HASHES - 1];
        let normal = signature(&["a"]);
        assert_eq!(estimate_jaccard(&short, &normal), 0.0);
    }
}
