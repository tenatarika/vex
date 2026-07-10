//! RED-phase integration tests for `find_similar` and `find_duplicates`.
//!
//! All tests are expected to fail with `unimplemented!()` panics until
//! Phase 9.2 GREEN implementation is complete.
//!
//! No `Embedder::new()` is called here — vectors are pre-baked `Vec<f32>`
//! of the correct dimension (384 = `VECTOR_DIM`).

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use vex::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
use vex::search::similar::{find_duplicates, find_similar};
use vex::store::format::VECTOR_DIM;
use vex::store::reader::IndexReader;
use vex::store::writer::write_index_full;

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn make_sym(name: &str, kind: SymbolKind, line: usize) -> ParsedSymbol {
    ParsedSymbol {
        name: name.to_string(),
        kind,
        line,
        signature: Some(format!("fn {name}()")),
        doc: None,
        body_tokens: None,
    }
}

fn make_file(path: &str, symbols: Vec<ParsedSymbol>) -> ParsedFile {
    ParsedFile {
        path: path.to_string(),
        symbols,
        refs: vec![],
        call_edges: vec![],
        bound_refs: vec![],
        skeletons: Vec::new(),
        cpp_includes: Vec::new(),
        trigram_bloom: None,
    }
}

/// Build a small index with optional per-symbol vectors.
///
/// `entries` is a list of `(kind, name, path, line, optional_vector)`.
/// Vectors must all be `VECTOR_DIM`-dimensional or `None`.
/// Symbols without a vector get `vector_index == u32::MAX` in the written record,
/// which requires a small trick: we pass `None` as a sentinel and build the
/// vectors slice only for symbols that have one, assigning `u32::MAX` for the
/// rest by relying on the writer's own behaviour.
///
/// The writer assigns `vector_index = symbol_idx` when `symbol_idx < vectors.len()`
/// and `u32::MAX` otherwise.  We exploit this by padding the vectors slice with
/// a dummy zero-vector for symbols we want to include and then NOT including a
/// vector for symbols we want to skip — but that's still sequential.
///
/// Instead the simplest correct approach: pass a `Vec<Vec<f32>>` where every
/// symbol gets a real vector or we omit vectors entirely.  For the
/// "skip no-vector symbols" tests we put the no-vector symbol LAST and pass
/// a vectors slice shorter than the symbol count.
fn build_index(
    tmp: &Path,
    entries: &[(&str, &str, u32, Option<Vec<f32>>)], // (name, path, line, vector)
    kind: SymbolKind,
) -> PathBuf {
    build_index_mixed(
        tmp,
        &entries
            .iter()
            .map(|(n, p, l, v)| (kind, *n, *p, *l, v.clone()))
            .collect::<Vec<_>>(),
    )
}

/// Build an index from mixed-kind entries.
/// `entries`: (kind, name, path, line, optional_vector)
///
/// The writer assigns `vector_index = symbol_idx` for indices `< vectors.len()`.
/// To give `vector_index == u32::MAX` to a symbol, put it at position >= vectors.len().
/// We honour `None` entries by clipping the vectors slice.
type MixedEntry<'a> = (SymbolKind, &'a str, &'a str, u32, Option<Vec<f32>>);

fn build_index_mixed(tmp: &Path, entries: &[MixedEntry<'_>]) -> PathBuf {
    let index_path = tmp.join("index.vex");

    // Group symbols by path while preserving declaration order.
    let mut files: std::collections::BTreeMap<&str, Vec<ParsedSymbol>> =
        std::collections::BTreeMap::new();
    let mut order: Vec<&str> = Vec::new();

    for (kind, name, path, line, _) in entries {
        if files.insert(*path, vec![]).is_none() {
            // first time we saw this path
        }
        let entry = files.entry(*path).or_default();
        // Avoid duplicates from the is_none check above
        if entry.is_empty() {
            order.push(*path);
        }
        entry.push(make_sym(name, *kind, *line as usize));
    }

    // Rebuild in insertion order.
    let mut files_ordered: std::collections::BTreeMap<&str, Vec<ParsedSymbol>> =
        std::collections::BTreeMap::new();
    for &p in &order {
        files_ordered.entry(p).or_default();
    }

    // Simpler: build the parsed files list in entry order (same as entries order, grouped by path).
    let mut parsed: Vec<ParsedFile> = Vec::new();
    let mut seen_paths: Vec<&str> = Vec::new();
    for (kind, name, path, line, _) in entries {
        if !seen_paths.contains(path) {
            seen_paths.push(*path);
            parsed.push(make_file(path, vec![]));
        }
        let file = parsed.iter_mut().find(|f| f.path == *path).unwrap();
        file.symbols.push(make_sym(name, *kind, *line as usize));
    }

    // Build vectors: writer uses sequential index — symbol 0 gets vectors[0], etc.
    // For None entries we clip the slice so the symbol gets u32::MAX.
    // Strategy: find the last Some entry; everything up to and including it must
    // have a real vector (use zero-vec as placeholder for any intermediate None slots).
    let last_some = entries.iter().rposition(|(_, _, _, _, v)| v.is_some());
    let vectors: Vec<Vec<f32>> = if let Some(last) = last_some {
        entries[..=last]
            .iter()
            .map(|(_, _, _, _, v)| {
                v.clone()
                    .unwrap_or_else(|| vec![0.0_f32; VECTOR_DIM as usize])
            })
            .collect()
    } else {
        vec![]
    };

    write_index_full(&parsed, &vectors, 384, &index_path).expect("write_index_full");
    index_path
}

/// Nonexistent path used as the hnsw_path — implementation falls back to brute-force.
fn no_hnsw(tmp: &Path) -> PathBuf {
    tmp.join("no.hnsw")
}

/// All-ones unit vector (dim 384).
fn ones() -> Vec<f32> {
    vec![1.0_f32; VECTOR_DIM as usize]
}

/// All-zeros vector (dim 384).  Cosine(ones, zeros) = 0.
fn zeros() -> Vec<f32> {
    vec![0.0_f32; VECTOR_DIM as usize]
}

/// A vector nearly identical to `ones()` — cosine similarity > 0.999.
fn near_ones() -> Vec<f32> {
    let mut v = vec![1.0_f32; VECTOR_DIM as usize];
    v[0] = 0.999;
    v
}

// ---------------------------------------------------------------------------
// find_similar tests
// ---------------------------------------------------------------------------

/// The target symbol itself must not appear in the result list.
#[test]
fn similar_excludes_self() {
    let tmp = TempDir::new().unwrap();
    let path = build_index(
        tmp.path(),
        &[
            ("Foo", "a.rs", 1, Some(ones())),
            ("Bar", "a.rs", 10, Some(near_ones())),
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let results = find_similar(&reader, &no_hnsw(tmp.path()), "Foo", 10, 0.0, false).unwrap();
    assert!(
        results.iter().all(|m| m.name != "Foo"),
        "find_similar must not return the target symbol itself"
    );
}

/// `limit` caps the number of returned results.
#[test]
fn similar_respects_limit() {
    let tmp = TempDir::new().unwrap();
    // 5 symbols all with identical vectors — after excluding self, 4 candidates.
    let path = build_index(
        tmp.path(),
        &[
            ("A", "a.rs", 1, Some(ones())),
            ("B", "a.rs", 10, Some(ones())),
            ("C", "a.rs", 20, Some(ones())),
            ("D", "a.rs", 30, Some(ones())),
            ("E", "a.rs", 40, Some(ones())),
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let results = find_similar(&reader, &no_hnsw(tmp.path()), "A", 3, 0.0, false).unwrap();
    assert!(
        results.len() <= 3,
        "find_similar with limit=3 should return at most 3 results, got {}",
        results.len()
    );
}

/// Every result must have `similarity >= threshold`.
/// With threshold=0.99 and an orthogonal vector as target, no match should pass.
#[test]
fn similar_respects_threshold() {
    let tmp = TempDir::new().unwrap();
    // Foo has ones(); Bar has zeros() — cosine(ones, zeros) = 0.
    let path = build_index(
        tmp.path(),
        &[
            ("Foo", "a.rs", 1, Some(ones())),
            ("Bar", "a.rs", 10, Some(zeros())),
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let results = find_similar(&reader, &no_hnsw(tmp.path()), "Foo", 10, 0.99, false).unwrap();
    assert!(
        results.iter().all(|m| m.similarity >= 0.99),
        "all results must have similarity >= threshold"
    );
}

/// Orthogonal vectors yield similarity ≈ 0 and must be filtered out by any threshold > 0.
#[test]
fn similar_orthogonal_vectors_have_zero_similarity() {
    let tmp = TempDir::new().unwrap();
    let path = build_index(
        tmp.path(),
        &[
            ("Foo", "a.rs", 1, Some(ones())),
            ("Bar", "a.rs", 10, Some(zeros())),
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    // Threshold just above 0 — orthogonal vector should not appear.
    let results = find_similar(&reader, &no_hnsw(tmp.path()), "Foo", 10, 1e-6, false).unwrap();
    assert!(
        results.iter().all(|m| m.name != "Bar"),
        "Bar with orthogonal vector should not appear when threshold > 0"
    );
}

/// Two symbols with the same vector must have similarity ≈ 1.0 (within 1e-5).
#[test]
fn similar_identical_vectors_have_similarity_1() {
    let tmp = TempDir::new().unwrap();
    let path = build_index(
        tmp.path(),
        &[
            ("Foo", "a.rs", 1, Some(ones())),
            ("Bar", "a.rs", 10, Some(ones())),
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let results = find_similar(&reader, &no_hnsw(tmp.path()), "Foo", 10, 0.0, false).unwrap();
    assert!(!results.is_empty(), "should find Bar");
    let bar = results
        .iter()
        .find(|m| m.name == "Bar")
        .expect("Bar missing");
    assert!(
        (bar.similarity - 1.0_f32).abs() < 1e-5,
        "identical vectors should yield similarity ≈ 1.0, got {}",
        bar.similarity
    );
}

/// Querying a symbol name that does not exist in the index must return `Err`.
/// The error message must contain the missing symbol name.
#[test]
fn similar_unknown_symbol_errors() {
    let tmp = TempDir::new().unwrap();
    let path = build_index(
        tmp.path(),
        &[("Foo", "a.rs", 1, Some(ones()))],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let result = find_similar(
        &reader,
        &no_hnsw(tmp.path()),
        "NonexistentSymbol",
        10,
        0.0,
        false,
    );
    assert!(result.is_err(), "should return Err for unknown symbol");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("NonexistentSymbol"),
        "error message should contain the symbol name, got: {msg}"
    );
}

/// Calling `find_similar` on an index that has no vectors must return `Err`
/// mentioning `--semantic`.
#[test]
fn similar_no_vectors_errors() {
    let tmp = TempDir::new().unwrap();
    // write_index (no vectors variant)
    let path = tmp.path().join("index.vex");
    let parsed = vec![make_file(
        "a.rs",
        vec![make_sym("Foo", SymbolKind::Function, 1)],
    )];
    write_index_full(&parsed, &[], 384, &path).unwrap();
    let reader = IndexReader::open(&path).unwrap();

    let result = find_similar(&reader, &no_hnsw(tmp.path()), "Foo", 10, 0.0, false);
    assert!(
        result.is_err(),
        "should return Err when index has no vectors"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("semantic") || msg.contains("--semantic"),
        "error should mention --semantic, got: {msg}"
    );
}

/// A symbol with `vector_index == u32::MAX` must not appear in results.
/// Set up: two symbols in index; the second has no vector (shorter vectors slice).
/// Query the first — the second must be absent from results.
#[test]
fn similar_skips_symbols_without_vector() {
    let tmp = TempDir::new().unwrap();
    // Foo has vector, Baz has none (None => writer assigns u32::MAX).
    let path = build_index(
        tmp.path(),
        &[
            ("Foo", "a.rs", 1, Some(ones())),
            ("Baz", "a.rs", 10, None), // no vector
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let results = find_similar(&reader, &no_hnsw(tmp.path()), "Foo", 10, 0.0, false).unwrap();
    assert!(
        results.iter().all(|m| m.name != "Baz"),
        "symbol without vector (Baz) must not appear in results"
    );
}

/// Results must be sorted by `similarity` in descending order.
#[test]
fn similar_descending_by_similarity() {
    let tmp = TempDir::new().unwrap();
    // Create three vectors with different similarities to Foo (ones):
    //   near_ones ≈ 1.0, partial ≈ 0.5ish, zeros ≈ 0.0
    let partial: Vec<f32> = {
        let mut v = vec![0.0_f32; VECTOR_DIM as usize];
        // Fill only the first half with 1.0 → moderate similarity to all-ones.
        for x in v.iter_mut().take(VECTOR_DIM as usize / 2) {
            *x = 1.0;
        }
        v
    };
    let path = build_index(
        tmp.path(),
        &[
            ("Foo", "a.rs", 1, Some(ones())),
            ("VeryClose", "a.rs", 10, Some(near_ones())),
            ("Partial", "a.rs", 20, Some(partial)),
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let results = find_similar(&reader, &no_hnsw(tmp.path()), "Foo", 10, 0.0, false).unwrap();
    assert!(results.len() >= 2, "should find at least 2 results");
    for w in results.windows(2) {
        assert!(
            w[0].similarity >= w[1].similarity,
            "results must be sorted descending by similarity: {} < {}",
            w[0].similarity,
            w[1].similarity
        );
    }
}

// ---------------------------------------------------------------------------
// find_duplicates tests
// ---------------------------------------------------------------------------

/// No pair `(X, X)` should ever appear.
#[test]
fn duplicates_no_self_pairs() {
    let tmp = TempDir::new().unwrap();
    let path = build_index(
        tmp.path(),
        &[
            ("A", "a.rs", 1, Some(ones())),
            ("B", "a.rs", 20, Some(ones())),
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let pairs = find_duplicates(&reader, &no_hnsw(tmp.path()), 0.0, 0, 100, false).unwrap();
    for (a, b) in &pairs {
        assert_ne!(
            (&a.name, &a.path, a.line),
            (&b.name, &b.path, b.line),
            "self-pair ({}) found in duplicates output",
            a.name
        );
    }
}

/// Pairs must be canonical: `(A, B)` and `(B, A)` must not both appear.
#[test]
fn duplicates_pairs_are_canonical() {
    let tmp = TempDir::new().unwrap();
    let path = build_index(
        tmp.path(),
        &[
            ("A", "a.rs", 1, Some(ones())),
            ("B", "b.rs", 1, Some(ones())),
            ("C", "c.rs", 1, Some(ones())),
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let pairs = find_duplicates(&reader, &no_hnsw(tmp.path()), 0.0, 0, 100, false).unwrap();

    // Build a set of (sorted name pair) to detect both orderings.
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for (a, b) in &pairs {
        let key = if a.name <= b.name {
            (a.name.clone(), b.name.clone())
        } else {
            (b.name.clone(), a.name.clone())
        };
        assert!(
            seen.insert(key.clone()),
            "duplicate pair ({}, {}) found — pairs must be canonical",
            key.0,
            key.1
        );
    }
}

/// Every returned pair must have similarity >= threshold.
#[test]
fn duplicates_respects_threshold() {
    let tmp = TempDir::new().unwrap();
    let path = build_index(
        tmp.path(),
        &[
            ("A", "a.rs", 1, Some(ones())),
            ("B", "b.rs", 1, Some(ones())),
            ("C", "c.rs", 1, Some(zeros())), // orthogonal to A and B
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let threshold = 0.99_f32;
    let pairs = find_duplicates(&reader, &no_hnsw(tmp.path()), threshold, 0, 100, false).unwrap();
    for (a, b) in &pairs {
        assert!(
            a.similarity >= threshold,
            "pair ({}, {}) has similarity {} < threshold {}",
            a.name,
            b.name,
            a.similarity,
            threshold
        );
    }
    // C must not appear — it is orthogonal to everything else.
    for (a, b) in &pairs {
        assert!(
            a.name != "C" && b.name != "C",
            "C should be filtered by threshold"
        );
    }
}

/// Pairs must be sorted by similarity in descending order.
#[test]
fn duplicates_descending_by_similarity() {
    let tmp = TempDir::new().unwrap();
    // near_ones is closer to ones than partial is.
    let partial: Vec<f32> = {
        let mut v = vec![0.0_f32; VECTOR_DIM as usize];
        for x in v.iter_mut().take(VECTOR_DIM as usize / 2) {
            *x = 1.0;
        }
        v
    };
    let path = build_index(
        tmp.path(),
        &[
            ("A", "a.rs", 1, Some(ones())),
            ("B", "b.rs", 1, Some(near_ones())),
            ("C", "c.rs", 1, Some(partial)),
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let pairs = find_duplicates(&reader, &no_hnsw(tmp.path()), 0.0, 0, 100, false).unwrap();
    for w in pairs.windows(2) {
        assert!(
            w[0].0.similarity >= w[1].0.similarity,
            "pairs must be sorted descending by similarity"
        );
    }
}

/// `limit` caps the number of returned pairs.
#[test]
fn duplicates_respects_limit() {
    let tmp = TempDir::new().unwrap();
    // 4 identical vectors → 6 possible pairs (4 choose 2); limit=2 should return ≤ 2.
    let path = build_index(
        tmp.path(),
        &[
            ("A", "a.rs", 1, Some(ones())),
            ("B", "b.rs", 1, Some(ones())),
            ("C", "c.rs", 1, Some(ones())),
            ("D", "d.rs", 1, Some(ones())),
        ],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let pairs = find_duplicates(&reader, &no_hnsw(tmp.path()), 0.0, 0, 2, false).unwrap();
    assert!(
        pairs.len() <= 2,
        "find_duplicates with limit=2 should return at most 2 pairs, got {}",
        pairs.len()
    );
}

/// Calling `find_duplicates` on an index without vectors must return `Err`
/// with a message mentioning `--semantic`.
#[test]
fn duplicates_no_vectors_errors() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("index.vex");
    let parsed = vec![make_file(
        "a.rs",
        vec![
            make_sym("Foo", SymbolKind::Function, 1),
            make_sym("Bar", SymbolKind::Function, 10),
        ],
    )];
    write_index_full(&parsed, &[], 384, &path).unwrap();
    let reader = IndexReader::open(&path).unwrap();

    let result = find_duplicates(&reader, &no_hnsw(tmp.path()), 0.9, 0, 100, false);
    assert!(
        result.is_err(),
        "should return Err when index has no vectors"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("semantic") || msg.contains("--semantic"),
        "error should mention --semantic, got: {msg}"
    );
}

/// Symbols whose body is shorter than `min_body_lines` must not appear in any pair.
///
/// Body length approximation: `next_symbol.line - this_symbol.line` within the same file.
/// We place "Short" at line 1 with "Short2" at line 2 (body = 1 line),
/// and "Long" at line 2 with the end at line 50 (body = many lines — it's last in file
/// so the approximation falls back, but we also add another symbol far away).
#[test]
fn duplicates_skips_short_bodies() {
    let tmp = TempDir::new().unwrap();

    // File layout:
    //   line  1: ShortA  (body ~4 lines: next is at line 5)
    //   line  5: LongA   (body ~45 lines: next is at line 50 or last-in-file)
    // min_body_lines = 10 → ShortA is skipped, LongA passes.
    // We put ShortA and ShortB (in separate files) as identical vectors
    // so they would form a pair if not filtered.
    // LongA and LongB are in different files with identical vectors too.
    let path = build_index_mixed(
        tmp.path(),
        &[
            // file_a.rs: ShortA at line 1, LongA at line 5
            (SymbolKind::Function, "ShortA", "file_a.rs", 1, Some(ones())),
            (SymbolKind::Function, "LongA", "file_a.rs", 5, Some(ones())),
            // file_b.rs: ShortB at line 1, LongB at line 5
            (SymbolKind::Function, "ShortB", "file_b.rs", 1, Some(ones())),
            (SymbolKind::Function, "LongB", "file_b.rs", 5, Some(ones())),
        ],
    );
    let reader = IndexReader::open(&path).unwrap();

    // min_body_lines = 10 → ShortA (body ≈ 4 lines) and ShortB (body ≈ 4 lines) are skipped.
    let pairs = find_duplicates(&reader, &no_hnsw(tmp.path()), 0.0, 10, 100, false).unwrap();

    for (a, b) in &pairs {
        assert!(
            a.name != "ShortA" && a.name != "ShortB" && b.name != "ShortA" && b.name != "ShortB",
            "short-body symbols (ShortA, ShortB) must not appear in duplicate pairs, got ({}, {})",
            a.name,
            b.name
        );
    }
}

/// An index with zero symbols must return `Ok(vec![])`.
#[test]
fn duplicates_empty_index_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("index.vex");
    // Write index with vectors flag but zero symbols.
    write_index_full(&[], &[], 384, &path).unwrap();
    let reader = IndexReader::open(&path).unwrap();

    // The index has no vectors (write_index_full with empty slices), but
    // the implementation must handle the zero-symbol case before the
    // no-vectors check and return Ok(vec![]).
    let result = find_duplicates(&reader, &no_hnsw(tmp.path()), 0.9, 0, 100, false);
    // Either Ok(empty) or Err(no vectors) are acceptable for truly empty index.
    // The spec says Ok(vec![]) — if it returns Err, the test will fail once GREEN.
    match result {
        Ok(pairs) => assert!(pairs.is_empty(), "empty index must return empty pairs"),
        Err(e) => {
            // If the implementation checks vectors first and this is an Err,
            // the test will fail at GREEN time — that's expected behaviour for RED.
            panic!("expected Ok(vec![]) for empty index, got Err: {e}");
        }
    }
}

/// An index with exactly one symbol cannot form any pair; must return `Ok(vec![])`.
#[test]
fn duplicates_single_symbol_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let path = build_index(
        tmp.path(),
        &[("OnlyOne", "a.rs", 1, Some(ones()))],
        SymbolKind::Function,
    );
    let reader = IndexReader::open(&path).unwrap();

    let pairs = find_duplicates(&reader, &no_hnsw(tmp.path()), 0.0, 0, 100, false).unwrap();
    assert!(
        pairs.is_empty(),
        "single-symbol index cannot form a pair, expected empty, got {} pairs",
        pairs.len()
    );
}
