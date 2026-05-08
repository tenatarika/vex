#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

/// Structured input: separate FST bytes and posting bytes,
/// plus queries to run against the reader.
#[derive(Arbitrary, Debug)]
struct RefsInput {
    fst_bytes: Vec<u8>,
    posting_bytes: Vec<u8>,
    queries: Vec<String>,
}

/// Fuzz RefReader with arbitrary FST and posting bytes.
///
/// The reader does bounds-checked reads from posting lists,
/// but we want to ensure no panics on malformed data.
fuzz_target!(|input: RefsInput| {
    let reader = match vex::store::refs_fst::RefReader::new(&input.fst_bytes, &input.posting_bytes)
    {
        Ok(r) => r,
        Err(_) => return,
    };

    for query in &input.queries {
        let _ = reader.find(query);
        let _ = reader.find_by_prefix(query);
    }

    // Edge cases
    let _ = reader.find("");
    let _ = reader.find_by_prefix("");
    let _ = reader.find("\x00");
    let _ = reader.find(&"A".repeat(10000));
});
