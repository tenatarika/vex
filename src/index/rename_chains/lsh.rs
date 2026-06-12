//! Phase 14.10 — LSH banding over MinHash signatures.
//!
//! Sub-quadratic candidate pruning for the per-commit-pair link
//! discovery in `build_rename_chains`. The banding layer turns the
//! 240-slot MinHash signature into 20 bands × 12-row slices; two
//! signatures are LSH-candidates iff at least one band slice
//! hashes to the same `u64` fingerprint. By the standard LSH
//! probability analysis, this targets recall ≈ 1 - (1 - J^r)^b for
//! Jaccard J — with b=20 and r=12, the probability of being chosen
//! as a candidate is essentially 1 above J ≈ 0.7 and drops below
//! 0.1 by J ≈ 0.4. That matches the `GATE_JACCARD = 0.70`
//! downstream filter.
//!
//! ## Fingerprint widening (rust H3)
//!
//! Earlier design called for a u32 band fingerprint. At 1M
//! entries × 20 bands, the birthday bound is √(2³²) ≈ 65k —
//! comfortably collidable. Widened to u64 (xxh3_64 over the
//! `ROWS_PER_BAND * 4 = 48` byte slice), giving a birthday bound
//! around √(2⁶⁴) ≈ 4×10⁹, comfortably above any plausible repo.
//!
//! ## Endianness
//!
//! Each u32 in a band is serialised via `to_le_bytes()` into a
//! stack-allocated buffer before hashing. This keeps band
//! fingerprints byte-identical between little-endian and
//! big-endian targets — important because the rename_chains
//! sidecar might be persisted on one host and read back on another
//! across CI matrices. `bytemuck::cast_slice` would have been
//! shorter but inherits the host's endianness, which we explicitly
//! don't want.

use std::collections::{HashMap, HashSet};

use xxhash_rust::xxh3::xxh3_64;

use crate::index::rename_chains::minhash::Signature;
use crate::index::rename_chains::weights::{NUM_BANDS, ROWS_PER_BAND};

/// LSH band table. Indexed by band number, each band maps a u64
/// fingerprint to the set of entry indices whose signature
/// produced that fingerprint at that band.
///
/// The Vec<u32> per fingerprint is intentionally not deduped — same
/// entry being inserted twice would be a caller bug, and the
/// `candidates` query collapses duplicates via HashSet anyway.
pub(crate) struct BandTable {
    bands: Vec<HashMap<u64, Vec<u32>>>,
}

impl BandTable {
    /// Create an empty band table sized for the configured
    /// `NUM_BANDS`. All bands start with empty HashMaps; capacity
    /// grows lazily as `insert` is called.
    pub(crate) fn new() -> Self {
        let mut bands = Vec::with_capacity(NUM_BANDS);
        for _ in 0..NUM_BANDS {
            bands.push(HashMap::new());
        }
        Self { bands }
    }

    /// Insert an entry's signature into every band's fingerprint
    /// table. Panics on length-mismatch only via the indexing in
    /// `band_fingerprint` — callers are expected to feed signatures
    /// produced by `minhash::signature`, which always returns
    /// `MINHASH_HASHES` slots.
    pub(crate) fn insert(&mut self, entry_idx: u32, sig: &Signature) {
        for (b, table) in self.bands.iter_mut().enumerate() {
            let fp = band_fingerprint(sig, b);
            table.entry(fp).or_default().push(entry_idx);
        }
    }

    /// Return all entry indices that share at least one band
    /// fingerprint with `query_sig`. Dedup via HashSet so an entry
    /// matching in multiple bands appears once.
    ///
    /// The query signature is **not** required to be a member of
    /// the table — `BandTable` is purely a lookup index. Callers
    /// use this to ask "what entries in commit C+1 look similar to
    /// this entry from commit C?" after building the table over
    /// only the candidate side.
    pub(crate) fn candidates(&self, query_sig: &Signature) -> HashSet<u32> {
        let mut out = HashSet::new();
        for (b, table) in self.bands.iter().enumerate() {
            let fp = band_fingerprint(query_sig, b);
            if let Some(matches) = table.get(&fp) {
                out.extend(matches.iter().copied());
            }
        }
        out
    }

    /// Number of bands the table is configured for. Diagnostic
    /// only — pinned to `NUM_BANDS` by construction.
    pub(crate) fn band_count(&self) -> usize {
        self.bands.len()
    }
}

impl Default for BandTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Hash the `ROWS_PER_BAND` u32 slots of band `band_idx` of `sig`
/// into a single u64 fingerprint. Serialises each u32 via
/// `to_le_bytes` so the fingerprint is endian-independent — a
/// little-endian host and a big-endian host produce the same
/// fingerprint for the same signature, which matters when the
/// rename_chains sidecar crosses CI hosts.
///
/// `band_idx` must be `< NUM_BANDS`; out-of-range indices panic
/// via the slice indexing. Same precondition as the design doc's
/// pseudocode.
fn band_fingerprint(sig: &Signature, band_idx: usize) -> u64 {
    let start = band_idx * ROWS_PER_BAND;
    let end = start + ROWS_PER_BAND;
    let slice = &sig[start..end];
    // Stack buffer sized for the maximum band width we'd ever use
    // (256 bytes = 64 u32s). Avoids a heap allocation per
    // fingerprint while staying small enough not to bloat the
    // caller's frame. `ROWS_PER_BAND * 4` bytes are written.
    let mut buf = [0u8; 256];
    let byte_len = ROWS_PER_BAND * 4;
    for (i, slot) in slice.iter().enumerate() {
        let off = i * 4;
        buf[off..off + 4].copy_from_slice(&slot.to_le_bytes());
    }
    xxh3_64(&buf[..byte_len])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::rename_chains::minhash::signature;
    use crate::index::rename_chains::weights::MINHASH_HASHES;

    #[test]
    fn empty_table_returns_empty_candidate_set() {
        // Sanity: with no inserts, no fingerprint is registered,
        // so any query returns the empty set rather than panicking
        // on missing-band lookup.
        let table = BandTable::new();
        let sig = signature(&["foo"]);
        let cands = table.candidates(&sig);
        assert!(cands.is_empty());
    }

    #[test]
    fn band_count_equals_num_bands() {
        // Pin the contract — `band_count` must reflect
        // `NUM_BANDS`, not whatever the impl happened to allocate.
        let table = BandTable::new();
        assert_eq!(table.band_count(), NUM_BANDS);
    }

    #[test]
    fn default_matches_new() {
        // Default impl must be equivalent to ::new(); a divergence
        // (e.g. `derive(Default)` accidentally producing an empty
        // bands Vec) would silently break callers.
        let from_new = BandTable::new();
        let from_default = BandTable::default();
        assert_eq!(from_new.band_count(), from_default.band_count());
    }

    #[test]
    fn inserted_entry_is_its_own_candidate() {
        // Self-similarity is the strongest LSH signal — an entry
        // inserted into the table must surface when its own
        // signature is queried, in every band.
        let mut table = BandTable::new();
        let sig = signature(&["hello", "world", "lorem", "ipsum"]);
        table.insert(42, &sig);
        let cands = table.candidates(&sig);
        assert!(cands.contains(&42));
    }

    #[test]
    fn near_identical_signatures_are_mutual_candidates() {
        // Two large, mostly-overlapping token sets ⇒ MinHash
        // signatures share most slots ⇒ at least one band
        // fingerprint should match ⇒ they LSH-collide. This is
        // the recall side of the LSH contract — near-duplicates
        // must surface as candidates.
        let mut table = BandTable::new();
        let a_tokens: Vec<&str> = [
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
        ]
        .to_vec();
        let mut b_tokens = a_tokens.clone();
        // Swap one token only. With 9/10 overlap, MinHash slot-match
        // rate is high and at least one band of 12 contiguous slots
        // is very likely to fully match.
        b_tokens[9] = "lambda";
        let sig_a = signature(&a_tokens);
        let sig_b = signature(&b_tokens);
        table.insert(1, &sig_a);
        let cands = table.candidates(&sig_b);
        assert!(
            cands.contains(&1),
            "near-identical signatures should be mutual LSH candidates",
        );
    }

    #[test]
    fn dissimilar_signatures_rarely_collide() {
        // The precision side of the LSH contract: two largely
        // disjoint token sets should usually NOT surface each
        // other. LSH false-positive rate is non-zero, so we assert
        // a soft bound rather than an exact-zero — at b=20, r=12
        // the collision probability for J ≈ 0.1 is ≈ 1 - (1 -
        // 0.1^12)^20 ≈ 2×10⁻¹¹, comfortably "essentially never".
        let mut table = BandTable::new();
        let a_tokens = ["one", "two", "three", "four", "five"];
        let b_tokens = ["alpha", "beta", "gamma", "delta", "epsilon"];
        let sig_a = signature(&a_tokens);
        let sig_b = signature(&b_tokens);
        table.insert(7, &sig_a);
        let cands = table.candidates(&sig_b);
        assert!(
            !cands.contains(&7),
            "dissimilar signatures should not LSH-collide",
        );
    }

    #[test]
    fn band_fingerprint_is_deterministic() {
        // xxh3 is deterministic; this guards against a future
        // refactor that introduced a random seed or
        // host-dependent input ordering.
        let sig = signature(&["alpha", "beta", "gamma"]);
        let a = band_fingerprint(&sig, 0);
        let b = band_fingerprint(&sig, 0);
        let c = band_fingerprint(&sig, 0);
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn band_fingerprint_differs_across_bands_for_same_signature() {
        // Different band indices slice different windows of the
        // signature, so fingerprints across bands MUST differ for
        // any non-degenerate signature. A regression that
        // accidentally always sliced the first band would
        // collapse the LSH bands and tank precision.
        let sig = signature(&["alpha", "beta", "gamma", "delta", "epsilon"]);
        let fp0 = band_fingerprint(&sig, 0);
        let fp1 = band_fingerprint(&sig, 1);
        let fp_last = band_fingerprint(&sig, NUM_BANDS - 1);
        assert_ne!(fp0, fp1);
        assert_ne!(fp0, fp_last);
        assert_ne!(fp1, fp_last);
    }

    #[test]
    fn band_fingerprint_uses_only_its_own_slice() {
        // Two signatures that share band 0's 12 slots but differ
        // elsewhere must produce the same band-0 fingerprint.
        // This is the LSH invariant — slot-level changes outside
        // a band MUST NOT affect that band's fingerprint, or the
        // banding probability analysis falls over.
        let mut sig_a = vec![0u32; MINHASH_HASHES];
        for (i, slot) in sig_a.iter_mut().enumerate() {
            *slot = (i as u32).wrapping_mul(0x9E37_79B9);
        }
        let mut sig_b = sig_a.clone();
        // Mutate only slots outside band 0 (slots ROWS_PER_BAND..).
        for slot in sig_b.iter_mut().skip(ROWS_PER_BAND) {
            *slot = slot.wrapping_add(1);
        }
        assert_eq!(band_fingerprint(&sig_a, 0), band_fingerprint(&sig_b, 0));
        assert_ne!(band_fingerprint(&sig_a, 1), band_fingerprint(&sig_b, 1));
    }
}
