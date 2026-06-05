#![no_main]

//! Fuzz the v1.14.1 B1.1 HNSW hash-index sidecar parser
//! (`<index_dir>/index.hashes`, magic `VEXH` v1).
//!
//! The sidecar is loaded at every `vex search --semantic` /
//! `vex similar` / `vex duplicates` invocation by `HnswHandle::open`.
//! A malformed sidecar must NEVER panic — the contract is "any
//! corruption is an `Err`, caller bails to brute-force fallback".
//! Risk class identical to the v1.12.0 bloom sidecar and v1.13.0
//! marker harnesses, both of which surfaced real defects in <60s.
//!
//! Surfaces under test: 4-byte magic comparison, version `u32`
//! parse, count `u32` parse + `MAX_COUNT` bound, body loop over
//! `count × u64` (truncation, partial reads, EOF mid-entry).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    vex::search::hash_index::__fuzz_hash_index_bytes(data);
});
