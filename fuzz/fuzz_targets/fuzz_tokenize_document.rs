#![no_main]

//! Fuzz `vex::store::bm25::tokenize_document` (v1.13.0 P8 refactor).
//!
//! The tokenizer is on the `vex index` hot path — every symbol body
//! flows through it. v1.13.0 restructured the inner loop to share an
//! upfront-lowered owning `String` and dedup via `HashSet<&str>`. The
//! refactor preserves byte-level semantics; a parity table + invariant
//! tests pin that. This harness is a paranoia layer against unicode
//! edge cases the table doesn't enumerate — combining marks, RTL,
//! noncharacters, surrogate-shaped sequences (already filtered by
//! UTF-8 validity), and bytes near the `is_alphanumeric` predicate
//! boundary.
//!
//! Goal: no panics on any UTF-8 string. Non-UTF-8 inputs are dropped
//! (matches production: term bags arrive as `String`).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    vex::store::bm25::__fuzz_tokenize_bytes(data);
});
