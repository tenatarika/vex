#![no_main]

//! Fuzz `build_hnsw_incremental_at` (v1.15.0 B1.2). The function is
//! invoked on every `vex update --semantic` run with `new_hashes`
//! derived from `compute_hashes_for` over freshly parsed code — input
//! the user doesn't directly control. Risk surfaces this catches:
//!
//!   - HashSet construction on duplicate-heavy `new_hashes`
//!   - tombstone-threshold arithmetic at boundary inputs
//!   - usearch's `add(k, v)` / `remove(k)` reaction to corner cases
//!     (already-removed keys, multi-remove of the same key, adding
//!     a key that was just removed)
//!   - the sidecar-rewrite-after-HNSW-save error path
//!
//! Same survival-as-success contract as the v1.12.0 bloom / v1.13.0
//! marker / v1.14.1 hash-index harnesses: any byte sequence must be
//! handled without panic.
//!
//! **Out of scope.** The shim does NOT exercise:
//!   - `HnswHandle::open` / `search` (query path; no probe is issued
//!     after the mutation)
//!   - `build_hnsw_at` (full rebuild path; only the incremental path)
//!   - the embed pipeline (`generate_embeddings`, ONNX, fastembed) —
//!     the shim feeds synthetic `new_hashes` straight in, bypassing
//!     `compute_hashes_for` and the cache lookup logic
//!   - `index.bodytokens` parsing — covered separately by
//!     `body_tokens::tests` and not in the incremental hot path
//!
//! Coverage of the diff / HashSet / tombstone / usearch mutation
//! surfaces is the explicit goal. Property test
//! `tests/incremental_hnsw_property_test.rs` covers equivalence; this
//! shim covers panic-resistance.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    vex::index::pipeline::__fuzz_incremental_hnsw_bytes(data);
});
