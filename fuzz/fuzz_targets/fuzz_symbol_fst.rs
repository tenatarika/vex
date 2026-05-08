#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

/// Structured input: separate FST bytes and posting bytes,
/// plus queries to run against the reader.
#[derive(Arbitrary, Debug)]
struct SymFstInput {
    fst_bytes: Vec<u8>,
    posting_bytes: Vec<u8>,
    queries: Vec<String>,
}

/// Fuzz SymbolFstReader with arbitrary FST and posting bytes.
///
/// Exercises exact, prefix, fuzzy (Levenshtein), and fallback search.
fuzz_target!(|input: SymFstInput| {
    let reader =
        match vex::store::symbol_fst::SymbolFstReader::new(&input.fst_bytes, &input.posting_bytes)
        {
            Ok(r) => r,
            Err(_) => return,
        };

    for query in &input.queries {
        let _ = reader.find(query);
        let _ = reader.find_by_prefix(query);
        let _ = reader.find_fuzzy(query, 1, 100);
        let _ = reader.find_fuzzy(query, 2, 100);
        let _ = reader.search_with_fallback(query, 100);
    }

    // Edge cases
    let _ = reader.find("");
    let _ = reader.find_by_prefix("");
    let _ = reader.find_fuzzy("", 0, 10);
    let _ = reader.search_with_fallback("", 10);
    let _ = reader.find("\x00\x7f");
    let _ = reader.find_fuzzy(&"x".repeat(200), 2, 10);
});
