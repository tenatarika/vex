#![no_main]

//! Fuzz the v7 `unresolved_refs` section reader (multi-repo Phase 6) with
//! arbitrary FST, posting, and edge bytes.
//!
//! `UnresolvedRefReader` does bounds-checked reads (posting offsets, edge
//! indices into the records array) — this pins that no adversarial byte
//! soup panics it. Mirrors `fuzz_refs_fst` (the v5 `RefReader` analog).
//! The production query path (`IndexReader::find_unresolved_refs_by_name`)
//! additionally wraps traversal in `catch_unwind`, but the RAW reader
//! fuzzed here must be panic-free on its own (defense in depth).

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

/// Structured input: the three sub-section byte blobs (edges / FST /
/// postings) plus names to look up.
#[derive(Arbitrary, Debug)]
struct UnresolvedInput {
    fst_bytes: Vec<u8>,
    posting_bytes: Vec<u8>,
    edge_bytes: Vec<u8>,
    queries: Vec<String>,
}

fuzz_target!(|input: UnresolvedInput| {
    let reader = match vex::store::unresolved_refs::UnresolvedRefReader::new(
        &input.fst_bytes,
        &input.posting_bytes,
        &input.edge_bytes,
    ) {
        Ok(r) => r,
        Err(_) => return,
    };

    for query in &input.queries {
        let _ = reader.find_by_name(query);
    }

    // `iter_all` streams the whole FST → posting lists → edge records; the
    // carry-forward path (`vex update`) drives it on every incremental run,
    // so adversarial bytes here must not panic.
    let _ = reader.iter_all();

    // Edge cases mirroring fuzz_refs_fst.
    let _ = reader.find_by_name("");
    let _ = reader.find_by_name("\x00");
    let _ = reader.find_by_name(&"A".repeat(10000));
});
