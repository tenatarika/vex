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

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    vex::index::pipeline::__fuzz_incremental_hnsw_bytes(data);
});
