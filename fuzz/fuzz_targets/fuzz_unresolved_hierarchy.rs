#![no_main]

//! Fuzz the v8 `unresolved_hierarchy` section reader (HIERARCHY-EDGES P2,
//! `docs/HIERARCHY-EDGES.md` §3.5) with arbitrary FST, posting, and edge
//! bytes.
//!
//! `UnresolvedHierarchyReader` does bounds-checked reads (posting offsets,
//! edge indices into the records array) — this pins that no adversarial
//! byte soup panics it. Mirrors `fuzz_unresolved_refs` (the resolved-vs-
//! unresolved split is structurally identical: name-keyed FST + posting
//! lists, verbatim case-preserving key here instead of lowercased). The
//! production query path (`IndexReader::find_unresolved_hierarchy_by_name`)
//! additionally wraps traversal in `catch_unwind`, but the RAW reader
//! fuzzed here must be panic-free on its own (defense in depth) — same
//! discipline `fuzz_unresolved_refs` applies to `UnresolvedRefReader`.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

/// Structured input: the three sub-section byte blobs (edges / FST /
/// postings) plus names to look up.
#[derive(Arbitrary, Debug)]
struct UnresolvedHierarchyInput {
    fst_bytes: Vec<u8>,
    posting_bytes: Vec<u8>,
    edge_bytes: Vec<u8>,
    queries: Vec<String>,
}

fuzz_target!(|input: UnresolvedHierarchyInput| {
    let reader = match vex::store::unresolved_hierarchy::UnresolvedHierarchyReader::new(
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
    // carry-forward path (`vex update` P2a, `reconstruct_unchanged`) drives
    // it on every incremental run, so adversarial bytes here must not panic.
    let _ = reader.iter_all();

    // Edge cases mirroring fuzz_unresolved_refs — empty / NUL / huge name.
    // Unlike unresolved_refs, this key is verbatim case (not lowercased),
    // so also probe mixed-case and non-ASCII to catch any accidental
    // case-folding regression in the traversal path.
    let _ = reader.find_by_name("");
    let _ = reader.find_by_name("\x00");
    let _ = reader.find_by_name(&"A".repeat(10000));
    let _ = reader.find_by_name("MixedCaseType");
    let _ = reader.find_by_name("\u{1F600}");
});
