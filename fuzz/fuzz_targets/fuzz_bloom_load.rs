#![no_main]

//! Fuzz the bloom-sidecar load path (`SymbolBloom::load`) introduced
//! in v1.12.0 T4. Feeds arbitrary bytes through the parser via the
//! `vex::search::bloom::__fuzz_load_bytes` shim, which writes to a
//! tmp file, calls `load`, and on success drives `may_contain` to
//! exercise the `bloomfilter::Bloom::check` internals.
//!
//! Goal: no panics, no UB, no out-of-bounds reads. The load contract
//! is "any `Err` is acceptable; only `Err` or `Ok(Some(_))` results
//! must not panic on subsequent `may_contain` calls". The pre-load
//! consistency guards (`MAX_BITMAP_LEN`, `n_bits == bitmap_len * 8`)
//! were added explicitly to keep `bloomfilter::check` from panicking
//! on a tampered sidecar — this harness pins that.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    vex::search::bloom::__fuzz_load_bytes(data);
});
