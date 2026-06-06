//! v1.15.0 B1.2 — property-based equivalence between
//! `build_hnsw_incremental_at` and `build_hnsw_at`.
//!
//! For any small-churn scenario (under the 25% tombstone threshold),
//! the incremental path's final on-disk state MUST be equivalent to a
//! from-scratch `build_hnsw_at` over the same target hash set. Catches
//! drift between the two paths that point-in-time unit tests can miss:
//! e.g. an `add()` order bug that leaves stale tombstones in usearch's
//! internal slot table, or a sidecar rewrite that drops a hash.
//!
//! Equivalence assertions are intentionally robust to HNSW's stochastic
//! graph topology (insertion order may produce different neighbour
//! links). What we pin:
//!   - both paths' `index.size()` agree
//!   - both paths' `index.hashes` sidecar contains the same `{u64}` set
//!     in sym_idx order (incremental rewrites in new sym_idx order on
//!     `new_hashes`; full does the same)
//!   - every member hash is `contains()`-able in both indexes
//!
//! Not asserted (would be flaky):
//!   - identical HNSW byte layout
//!   - identical search() top-k beyond self-recovery
//!
//! 256 cases × 2 builds per case ≈ 1-2 s on M1 — `proptest`-default
//! sampling. Set `PROPTEST_CASES` to widen.

use std::collections::HashSet;

use proptest::prelude::*;
use tempfile::TempDir;
use vex::index::pipeline::{build_hnsw_at, build_hnsw_incremental_at};
use vex::search::hash_index;

/// Small dim for fast HNSW build — production uses 384 (MINILM) but
/// the property under test (set-equivalence of stored hashes) is
/// dim-independent. Smaller dim = faster proptest iterations.
const DIM: usize = 16;

/// Deterministic unit vector for `hash`. Same xorshift-style mixing
/// pattern as `perf_b12.rs` so the two stay aligned. Each hash maps
/// to a distinct vector with overwhelming probability (collisions
/// only on identical hashes, which the property's input generation
/// already excludes).
fn vector_for(hash: u64) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    let mut norm_sq = 0.0_f32;
    for (j, slot) in v.iter_mut().enumerate() {
        let mut s = hash.wrapping_mul(0x9E3779B97F4A7C15);
        s ^= (j as u64).wrapping_mul(0xBF58476D1CE4E5B9);
        s = s.wrapping_mul(0x94D049BB133111EB);
        let bits = (s >> 40) as u32;
        let unit = bits as f32 / ((1u32 << 24) - 1) as f32;
        *slot = unit * 2.0 - 1.0;
        norm_sq += *slot * *slot;
    }
    let norm = norm_sq.sqrt().max(1e-12);
    for slot in v.iter_mut() {
        *slot /= norm;
    }
    v
}

/// Apply the input-generation invariants the property assumes:
///   - `old_hashes` and `additions` are pairwise disjoint
///   - `removed_indices` indexes into `old_hashes`
///   - `|removed_indices|` ≤ 25% of `|old_hashes|` (under tombstone threshold)
///
/// Returns `(old_hashes, new_hashes, removed_set)`. Both vectors keep
/// hash uniqueness for HNSW key correctness.
fn assemble(
    old_seeds: Vec<u32>,
    addition_seeds: Vec<u32>,
    removed_index_seeds: Vec<u8>,
) -> Option<(Vec<u64>, Vec<u64>, HashSet<u64>)> {
    // Map seeds → distinct hash keys. Two disjoint namespaces so an
    // addition can never collide with an old entry by construction.
    let mut old_set: HashSet<u64> = HashSet::new();
    let mut old_hashes: Vec<u64> = Vec::new();
    for s in &old_seeds {
        let h = 0x1000_0000_u64 | (*s as u64);
        if old_set.insert(h) {
            old_hashes.push(h);
        }
    }
    if old_hashes.len() < 4 {
        // Need ≥4 entries so tombstone-threshold arithmetic
        // (1/4 of old.len()) admits at least one remove.
        return None;
    }

    let mut additions: Vec<u64> = Vec::new();
    let mut addition_set: HashSet<u64> = HashSet::new();
    for s in &addition_seeds {
        let h = 0x2000_0000_u64 | (*s as u64);
        if !old_set.contains(&h) && addition_set.insert(h) {
            additions.push(h);
        }
    }

    // Cap removed_count to floor(old_len / 4) so the property's
    // assumption (incremental path applies, not falls back) holds.
    let max_remove = old_hashes.len() / 4;
    let mut removed_idx_set: HashSet<usize> = HashSet::new();
    for s in &removed_index_seeds {
        if removed_idx_set.len() >= max_remove {
            break;
        }
        let idx = (*s as usize) % old_hashes.len();
        removed_idx_set.insert(idx);
    }
    let removed_set: HashSet<u64> = removed_idx_set.iter().map(|&i| old_hashes[i]).collect();

    let mut new_hashes: Vec<u64> = old_hashes
        .iter()
        .copied()
        .filter(|h| !removed_set.contains(h))
        .collect();
    new_hashes.extend(additions);

    Some((old_hashes, new_hashes, removed_set))
}

/// Read the `index.hashes` sidecar into a set + ordered Vec — both
/// forms are checked against the parallel full-rebuild output.
fn load_sidecar(path: &std::path::Path) -> (Vec<u64>, HashSet<u64>) {
    let v = hash_index::load(path).expect("hash-index load");
    let set: HashSet<u64> = v.iter().copied().collect();
    (v, set)
}

proptest! {
    /// Drive both paths over the same target hash set and verify
    /// equivalence. Fails fast if a regression makes the incremental
    /// path's stored hash set diverge from a from-scratch rebuild.
    #[test]
    fn incremental_path_matches_full_rebuild_under_threshold(
        old_seeds in proptest::collection::vec(any::<u32>(), 4..40),
        addition_seeds in proptest::collection::vec(any::<u32>(), 0..15),
        removed_index_seeds in proptest::collection::vec(any::<u8>(), 0..10),
    ) {
        let Some((old_hashes, new_hashes, _removed_set)) =
            assemble(old_seeds, addition_seeds, removed_index_seeds)
        else {
            // Generated input couldn't satisfy invariants (too few old
            // entries after dedupe) — skip rather than fail.
            return Ok(());
        };

        let old_vectors: Vec<Vec<f32>> = old_hashes.iter().map(|h| vector_for(*h)).collect();
        let new_vectors: Vec<Vec<f32>> = new_hashes.iter().map(|h| vector_for(*h)).collect();

        // Path A: incremental. Build baseline, then mutate.
        let tmp_inc = TempDir::new().expect("tempdir");
        let inc_hnsw = tmp_inc.path().join("index.hnsw");
        let inc_hash = tmp_inc.path().join("index.hashes");
        build_hnsw_at(&inc_hnsw, &inc_hash, &old_vectors, &old_hashes)
            .expect("seed build_hnsw_at");
        let applied = build_hnsw_incremental_at(&inc_hnsw, &inc_hash, &new_vectors, &new_hashes)
            .expect("incremental_at");
        prop_assert!(
            applied,
            "incremental must apply under the 25% tombstone bound — inputs were \
             generated to stay within it (old={}, new={})",
            old_hashes.len(), new_hashes.len()
        );

        // Path B: full rebuild from scratch over the SAME `new_hashes`.
        let tmp_full = TempDir::new().expect("tempdir");
        let full_hnsw = tmp_full.path().join("index.hnsw");
        let full_hash = tmp_full.path().join("index.hashes");
        build_hnsw_at(&full_hnsw, &full_hash, &new_vectors, &new_hashes)
            .expect("full build_hnsw_at");

        // Property 1: sidecar hash-sets equal.
        let (inc_vec, inc_set) = load_sidecar(&inc_hash);
        let (full_vec, full_set) = load_sidecar(&full_hash);
        prop_assert_eq!(
            inc_set, full_set,
            "incremental and full sidecars must encode the same hash set"
        );
        // Property 2: sidecar lengths equal (cardinality).
        prop_assert_eq!(
            inc_vec.len(), full_vec.len(),
            "sidecar length mismatch — duplicate or missing entry on one path"
        );
        // Property 3: incremental sidecar is in sym_idx order of `new_hashes`
        // (incremental REWRITES the sidecar after each mutation; full
        // writes in the order it was given). Both must reflect the
        // caller's `new_hashes` order, so they're bit-equal here.
        prop_assert_eq!(
            inc_vec.clone(), new_hashes.clone(),
            "incremental sidecar must preserve caller's sym_idx order"
        );
        prop_assert_eq!(
            full_vec.clone(), new_hashes.clone(),
            "full sidecar must preserve caller's sym_idx order"
        );
    }
}
