#![no_main]

//! Fuzz the IndexReader with arbitrary bytes as the index file.
//!
//! Exercises all unsafe code paths in reader.rs:
//! - header() — raw pointer cast to Header
//! - symbol(idx) — raw pointer cast to SymbolRecord
//! - vector(idx) — raw pointer cast to &[f32]
//! - read_string(offset) — string slice from mmap
//! - file_paths() — u32 reads from file table section
//! - ref_edge_count() / ref_edge(idx) — Q4-A MaybeUninit +
//!   copy_nonoverlapping path. Adversarial section headers with
//!   ref_edges_len that isn't a multiple of RefEdge::SIZE, or that
//!   point past mmap end, or that index past the section.
//!
//! Goal: no panics, no UB, no out-of-bounds reads. Errors are fine.

use libfuzzer_sys::fuzz_target;
use std::io::Write;

// Reuse a single temp dir across fuzz iterations to avoid descriptor/inode leak.
static FUZZ_DIR: std::sync::LazyLock<tempfile::TempDir> =
    std::sync::LazyLock::new(|| tempfile::tempdir().unwrap());

fuzz_target!(|data: &[u8]| {
    let path = FUZZ_DIR.path().join("fuzz.vex");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(data).unwrap();
    }

    let reader = match vex::store::reader::IndexReader::open(&path) {
        Ok(r) => r,
        Err(_) => return,
    };

    let count = reader.symbol_count().min(100);
    let _ = reader.has_vectors();
    let _ = reader.has_refs();
    let _ = reader.fst_bytes();
    let _ = reader.posting_bytes();
    let _ = reader.file_paths();

    for i in 0..count {
        if let Some(rec) = reader.symbol(i) {
            let _ = reader.read_string(rec.name_offset);
            let _ = reader.read_string(rec.file_offset);
            let _ = reader.read_string(rec.signature_offset);
            let _ = reader.vector(rec.vector_index);
        }
    }

    let _ = reader.symbol(count);
    let _ = reader.read_string(0);
    let _ = reader.read_string(u32::MAX);
    let _ = reader.vector(0);
    let _ = reader.vector(u32::MAX);

    // Q4-A (Phase 11.1.9): ref_edge reader path. Must not panic on
    // corrupt v5 section headers — len-not-multiple-of-SIZE returns 0
    // (with warn), out-of-bounds idx returns None, unaligned offset
    // is sidestepped by the MaybeUninit + copy_nonoverlapping idiom.
    let _ = reader.has_ref_edges();
    let ref_count = reader.ref_edge_count().min(100);
    for i in 0..ref_count {
        let _ = reader.ref_edge(i);
    }
    // OOB probes — exhaustively cover boundary cases.
    let _ = reader.ref_edge(ref_count);
    let _ = reader.ref_edge(usize::MAX);
    // `find_ref_edges_by_symbol` is the FST-keyed lookup path, currently
    // dead-code in production (#[allow(dead_code)]). The upstream `fst`
    // crate panics on adversarial-but-header-valid bytes at
    // `node.rs:302` during traversal — production code uses
    // `catch_unwind` defense-in-depth, but libfuzzer's panic hook fires
    // BEFORE the unwind can be caught, so this fuzz target excludes the
    // call. Re-enable when production wires the FST lookup or when we
    // switch to a Result-returning FST API.
});
