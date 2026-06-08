//! Regression tests for the v1.15.1 duplicate-hash bug.
//!
//! **Bug**: `vex index --semantic` aborts with
//! `Error: add vector to HNSW index: Duplicate keys not allowed in high-level
//! wrappers` when two symbols produce the same `embed::cache::context_hash`.
//! The HNSW index is opened with `multi: false` so inserting the same key
//! twice is a fatal error at the usearch layer.
//!
//! **Fix contract tested here**: `build_hnsw_at` and
//! `build_hnsw_incremental_at` MUST deduplicate on the hash before inserting
//! into usearch — keeping the first occurrence and skipping duplicates with
//! a `tracing::warn!` — so the build never aborts. The on-disk hash sidecar
//! stays **sym_idx-aligned** (one entry per symbol, duplicates preserved)
//! because `src/search/semantic.rs:156` checks `hashes.len() ==
//! expected_symbols` at query open and bails to brute force on mismatch.
//! The reader at `semantic.rs:175-193` already handles duplicate sidecar
//! entries by keeping the first sym_idx per hash (with a `collisions`
//! warn-log), so end-to-end the collision becomes "second symbol invisible
//! to semantic search" rather than "whole build aborts".

use std::collections::HashSet;

use tempfile::TempDir;
use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};
use vex::embed::MINILM_DIM;
use vex::index::pipeline::{build_hnsw_at, build_hnsw_incremental_at};
use vex::search::hash_index;

// ---------------------------------------------------------------------------
// Helpers shared across the dedup test cases
// ---------------------------------------------------------------------------

/// Return the usearch dimension constant as a plain `usize`.
fn dim() -> usize {
    MINILM_DIM as usize
}

/// Build a unit vector with `1.0` at position `slot` (orthogonal to any
/// other slot so cosine similarity between distinct slots is zero).
fn one_hot(slot: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; dim()];
    // Clamp slot to the dimension so the helper never panics, even for
    // large slot values used in stress tests.
    v[slot % dim()] = 1.0;
    v
}

/// Canonicalize a `TempDir` path so the macOS `/private/tmp` → `/tmp`
/// symlink doesn't create an asymmetry between the path passed to writer
/// functions and the path used to read back the artifacts.
fn canon(tmp: &TempDir) -> std::path::PathBuf {
    tmp.path().canonicalize().expect("canonicalize TempDir")
}

/// Open a mutable usearch `Index` sized for MiniLM cosine search and load
/// the saved file at `path`. Used to assert on the on-disk vector count
/// without going through `HnswHandle` (which has a size-match guard we don't
/// want to fight in edge-case tests).
fn load_index_at(path: &std::path::Path) -> usearch::Index {
    let options = IndexOptions {
        dimensions: dim(),
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    };
    let index = new_index(&options).expect("new_index for verification");
    let path_str = path.to_str().expect("HNSW path is valid UTF-8");
    index.load(path_str).expect("load HNSW for verification");
    index
}

// ---------------------------------------------------------------------------
// Test 1 — control: all-unique hashes, happy path unchanged
// ---------------------------------------------------------------------------

/// Guard that the dedup fix does not regress the common case where every
/// hash is distinct. Asserts that `build_hnsw_at` still writes N vectors
/// when given N unique hashes.
#[test]
fn build_hnsw_at_all_unique_hashes_writes_all_vectors() {
    let tmp = TempDir::new().unwrap();
    let root = canon(&tmp);
    let hnsw_path = root.join("index.hnsw");
    let hash_path = root.join("index.hashes");

    let vectors: Vec<Vec<f32>> = (0..5).map(one_hot).collect();
    let hashes: Vec<u64> = (0..5).map(|i| 0xABCD_0000_u64 + i as u64).collect();

    build_hnsw_at(&hnsw_path, &hash_path, &vectors, &hashes)
        .expect("build_hnsw_at with unique hashes must succeed");

    // The on-disk HNSW must contain exactly 5 vectors.
    let index = load_index_at(&hnsw_path);
    assert_eq!(
        index.size(),
        5,
        "all-unique: HNSW must contain all 5 vectors"
    );

    // The sidecar must contain exactly the 5 unique hashes, in insertion order.
    let saved = hash_index::load(&hash_path).expect("load hash sidecar");
    assert_eq!(
        saved, hashes,
        "all-unique: sidecar must contain all 5 hashes"
    );

    // Sanity: no duplicates in the sidecar.
    let unique: HashSet<u64> = saved.iter().copied().collect();
    assert_eq!(
        unique.len(),
        saved.len(),
        "all-unique: sidecar must have no duplicate hash values"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — duplicate hashes in `build_hnsw_at` → must not abort
// ---------------------------------------------------------------------------

/// **Regression test for the v1.15.1 bug.**
///
/// When two or more symbols share the same `context_hash` (e.g. two C++
/// functions with identical signatures in the same file), `build_hnsw_at`
/// previously aborted with "Duplicate keys not allowed in high-level
/// wrappers". After the fix it must return `Ok`, write a valid HNSW with
/// only the first-occurrence vector inserted, and write the FULL hashes
/// slice to the sidecar (duplicates preserved — sym_idx alignment is a
/// hard invariant of the query path; see semantic.rs:156 size check).
#[test]
fn build_hnsw_at_with_duplicate_hashes_returns_ok_and_deduplicates() {
    let tmp = TempDir::new().unwrap();
    let root = canon(&tmp);
    let hnsw_path = root.join("index.hnsw");
    let hash_path = root.join("index.hashes");

    // Simulate two distinct vectors (different C++ overloads) whose
    // `context_hash` happens to collide — they both map to the same u64.
    let collision_hash: u64 = 0xDEAD_BEEF_CAFE_1234;
    let vectors = vec![one_hot(0), one_hot(1)];
    let hashes = vec![collision_hash, collision_hash];

    // Before the fix this call aborts with the usearch duplicate-key error.
    let result = build_hnsw_at(&hnsw_path, &hash_path, &vectors, &hashes);
    assert!(
        result.is_ok(),
        "build_hnsw_at must not abort on duplicate hashes; got: {:?}",
        result.err()
    );

    // HNSW: only 1 vector inserted (dedup at insert; first-occurrence wins).
    let index = load_index_at(&hnsw_path);
    assert_eq!(
        index.size(),
        1,
        "dedup: HNSW must contain exactly 1 vector after deduplication"
    );

    // Sidecar: full sym_idx-aligned length (2), duplicates preserved. The
    // reader (`src/search/semantic.rs:175-193`) dedups at query time via
    // `hash_to_sym_idx` map; truncating the sidecar would break the
    // `hashes.len() == expected_symbols` size check at semantic.rs:156.
    let saved = hash_index::load(&hash_path).expect("load hash sidecar after dedup");
    assert_eq!(
        saved.len(),
        2,
        "dedup: sidecar must be sym_idx-aligned (length matches input)"
    );
    assert_eq!(
        saved,
        vec![collision_hash, collision_hash],
        "dedup: sidecar must preserve every symbol's hash for sym_idx alignment"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — all-duplicate edge case: N identical hashes → 1 vector kept
// ---------------------------------------------------------------------------

/// Edge case: 10 vectors all sharing the same hash. The build must succeed,
/// write exactly 1 vector to the HNSW, and write 10 sidecar entries (all
/// equal to `single_hash`) — sym_idx alignment preserved.
#[test]
fn build_hnsw_at_all_identical_hashes_keeps_one_vector() {
    let tmp = TempDir::new().unwrap();
    let root = canon(&tmp);
    let hnsw_path = root.join("index.hnsw");
    let hash_path = root.join("index.hashes");

    let single_hash: u64 = 0x1111_1111_1111_1111;
    // Use different vector contents so only the hash is the duplicated
    // quantity — stress-tests that the dedup key really is the hash, not
    // the vector bytes.
    let vectors: Vec<Vec<f32>> = (0..10).map(one_hot).collect();
    let hashes: Vec<u64> = vec![single_hash; 10];

    let result = build_hnsw_at(&hnsw_path, &hash_path, &vectors, &hashes);
    assert!(
        result.is_ok(),
        "all-identical: build_hnsw_at must not abort with 10 duplicate hashes; got: {:?}",
        result.err()
    );

    let index = load_index_at(&hnsw_path);
    assert_eq!(
        index.size(),
        1,
        "all-identical: HNSW must keep exactly 1 vector (9 skipped)"
    );

    let saved = hash_index::load(&hash_path).expect("load sidecar");
    assert_eq!(
        saved.len(),
        10,
        "all-identical: sidecar stays sym_idx-aligned (10 entries)"
    );
    assert!(
        saved.iter().all(|&h| h == single_hash),
        "all-identical: every sidecar entry must equal single_hash"
    );
}

// ---------------------------------------------------------------------------
// Test 4 — mixed: some duplicates, some unique
// ---------------------------------------------------------------------------

/// Realistic case: a 5-symbol batch where 2 pairs collide and 1 is unique.
/// HNSW gets 3 vectors (dedup). Sidecar keeps full 5-entry sym_idx alignment.
#[test]
fn build_hnsw_at_partial_duplicates_deduplicates_only_collisions() {
    let tmp = TempDir::new().unwrap();
    let root = canon(&tmp);
    let hnsw_path = root.join("index.hnsw");
    let hash_path = root.join("index.hashes");

    let hash_a: u64 = 0xAAAA_AAAA_AAAA_AAAA;
    let hash_b: u64 = 0xBBBB_BBBB_BBBB_BBBB;
    let hash_c: u64 = 0xCCCC_CCCC_CCCC_CCCC;

    // Layout: [A, B, A(dup), C, B(dup)]  → unique set = {A, B, C}
    let hashes = vec![hash_a, hash_b, hash_a, hash_c, hash_b];
    let vectors: Vec<Vec<f32>> = (0..5).map(one_hot).collect();

    let result = build_hnsw_at(&hnsw_path, &hash_path, &vectors, &hashes);
    assert!(
        result.is_ok(),
        "partial-dedup: build_hnsw_at must succeed; got: {:?}",
        result.err()
    );

    let index = load_index_at(&hnsw_path);
    assert_eq!(
        index.size(),
        3,
        "partial-dedup: HNSW must contain 3 unique vectors"
    );

    let saved = hash_index::load(&hash_path).expect("load sidecar");
    assert_eq!(
        saved.len(),
        5,
        "partial-dedup: sidecar stays sym_idx-aligned (full 5 entries)"
    );
    assert_eq!(
        saved,
        vec![hash_a, hash_b, hash_a, hash_c, hash_b],
        "partial-dedup: sidecar must preserve every symbol's hash in order"
    );

    // Cross-check that the unique set in the sidecar still covers {A, B, C}.
    let unique: HashSet<u64> = saved.iter().copied().collect();
    assert_eq!(unique.len(), 3, "partial-dedup: 3 unique hashes in sidecar");
    assert!(unique.contains(&hash_a), "hash_a must be present");
    assert!(unique.contains(&hash_b), "hash_b must be present");
    assert!(unique.contains(&hash_c), "hash_c must be present");
}

// ---------------------------------------------------------------------------
// Test 5 — `build_hnsw_incremental_at` with colliding new hashes
// ---------------------------------------------------------------------------

/// **Regression test for the incremental path.**
///
/// Seed the HNSW with 2 unique vectors, then call `build_hnsw_incremental_at`
/// with 3 new vectors whose hashes include a pair that collides with each
/// other (but neither collides with the existing seeds). Before the fix the
/// incremental path hits the same "Duplicate keys not allowed" error on the
/// `index.add` call inside `to_add_indices` loop. After the fix it must
/// return `Ok(true)` and write only unique hashes.
#[test]
fn build_hnsw_incremental_at_with_new_duplicate_hashes_returns_ok_true() {
    let tmp = TempDir::new().unwrap();
    let root = canon(&tmp);
    let hnsw_path = root.join("index.hnsw");
    let hash_path = root.join("index.hashes");

    // Seed: 2 distinct vectors.
    let seed_hashes = vec![0x5EED_0001_u64, 0x5EED_0002_u64];
    let seed_vectors: Vec<Vec<f32>> = vec![one_hot(0), one_hot(1)];
    build_hnsw_at(&hnsw_path, &hash_path, &seed_vectors, &seed_hashes).expect("seed build_hnsw_at");

    // New corpus: keep both seeds, then add 3 new symbols where two share
    // a hash collision.
    let collision_hash: u64 = 0xC011_C011_C011_0000;
    let unique_new_hash: u64 = 0xA1B2_C3D4_E5F6_0001;

    let new_hashes = vec![
        seed_hashes[0],  // unchanged seed
        seed_hashes[1],  // unchanged seed
        collision_hash,  // new symbol — first occurrence
        collision_hash,  // new symbol — duplicate (same context_hash)
        unique_new_hash, // new symbol — distinct
    ];
    let new_vectors: Vec<Vec<f32>> = (0..5).map(|i| one_hot(i + 2)).collect();

    // tombstone: 0 removes out of 2 seeds = 0% < 25% → incremental applies.
    let result = build_hnsw_incremental_at(&hnsw_path, &hash_path, &new_vectors, &new_hashes);
    assert!(
        result.is_ok(),
        "incremental-dedup: build_hnsw_incremental_at must not abort on duplicate new hashes; got: {:?}",
        result.err()
    );
    assert!(
        result.unwrap(),
        "incremental-dedup: must return Ok(true) when incremental path succeeds"
    );

    // Sidecar stays sym_idx-aligned (5 entries == new_hashes.len()) with
    // duplicates preserved. The query path dedups via `hash_to_sym_idx`.
    let saved = hash_index::load(&hash_path).expect("load sidecar after incremental");
    assert_eq!(
        saved.len(),
        new_hashes.len(),
        "incremental-dedup: sidecar must be sym_idx-aligned to new_hashes"
    );
    assert_eq!(
        saved, new_hashes,
        "incremental-dedup: sidecar must equal new_hashes (no truncation)"
    );

    let unique: HashSet<u64> = saved.iter().copied().collect();
    assert_eq!(
        unique.len(),
        4,
        "incremental-dedup: 4 distinct hashes expected (2 seeds + collision_hash + unique_new_hash)"
    );
}

// ---------------------------------------------------------------------------
// Test 6 — incremental: new hashes that collide with *existing* hashes
// ---------------------------------------------------------------------------

/// When a new symbol's `context_hash` matches a hash that already exists in
/// the seeded HNSW, the incremental diff computes "0 removes, 0 adds" for
/// that symbol (the existing entry stays). This test verifies the function
/// returns `Ok(true)` and the sidecar does not grow.
#[test]
fn build_hnsw_incremental_at_new_vector_colliding_with_existing_is_skipped() {
    let tmp = TempDir::new().unwrap();
    let root = canon(&tmp);
    let hnsw_path = root.join("index.hnsw");
    let hash_path = root.join("index.hashes");

    let seed_hash: u64 = 0x00EA_57ED_DEAD_BEEF_u64;
    let seed_vector = one_hot(0);
    build_hnsw_at(
        &hnsw_path,
        &hash_path,
        std::slice::from_ref(&seed_vector),
        &[seed_hash],
    )
    .expect("seed build_hnsw_at");

    // New corpus: same hash as the seed (simulates a symbol that hasn't
    // changed) plus a genuinely new symbol.
    let new_hash: u64 = 0xFEED_C0DE_CAFE_BABE;
    let new_hashes = vec![seed_hash, new_hash];
    let new_vectors = vec![one_hot(0), one_hot(5)];

    let result = build_hnsw_incremental_at(&hnsw_path, &hash_path, &new_vectors, &new_hashes);
    assert!(
        result.is_ok(),
        "existing-collision: incremental must succeed; got: {:?}",
        result.err()
    );
    assert!(result.unwrap(), "existing-collision: must return Ok(true)");

    // Sidecar must contain exactly the 2 hashes — no duplicate u64 values.
    let saved = hash_index::load(&hash_path).expect("load sidecar");
    let unique: HashSet<u64> = saved.iter().copied().collect();
    assert_eq!(
        unique.len(),
        saved.len(),
        "existing-collision: sidecar must not contain duplicate hash values"
    );
    assert!(unique.contains(&seed_hash), "existing hash must be present");
    assert!(unique.contains(&new_hash), "new hash must be present");
}
