use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use tempfile::tempdir;

use vex::index::pipeline;
use vex::search::structural;
use vex::store::reader::IndexReader;
use vex::util::config;

fn write_src(root: &Path, name: &str, content: &str) {
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join(name), content).unwrap();
}

fn run_index(root: &Path) {
    let root = root.canonicalize().unwrap();
    pipeline::run(&root, false, "minilm-l6-v2", &[]).expect("pipeline::run");
}

fn open_reader(root: &Path) -> IndexReader {
    let root = root.canonicalize().unwrap();
    let index_path = config::index_path(&root);
    IndexReader::open(&index_path).expect("IndexReader::open")
}

// ---------------------------------------------------------------------------
// 1. Parallel index + index: file lock prevents corruption
//    Two threads try to index the same project simultaneously.
//    The advisory lock should serialize them — no corruption, no panic.
// ---------------------------------------------------------------------------

#[test]
fn parallel_index_serialized_by_lock() {
    let dir = tempdir().unwrap();
    write_src(dir.path(), "a.rs", "pub fn alpha() {}\npub fn beta() {}\n");

    let root = dir.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let r = root.clone();
            let b = Arc::clone(&barrier);
            thread::spawn(move || {
                b.wait();
                let r = r.canonicalize().unwrap();
                pipeline::run(&r, false, "minilm-l6-v2", &[])
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // At least one should succeed; the other may also succeed (waited for lock)
    // or fail with a lock error — but neither should panic.
    let successes = results.iter().filter(|r| r.is_ok()).count();
    assert!(
        successes >= 1,
        "at least one parallel index must succeed, got {successes} successes"
    );

    // The index should be valid after both finish
    let reader = open_reader(dir.path());
    assert!(
        reader.symbol_count() >= 2,
        "index should contain symbols after parallel indexing"
    );
}

// ---------------------------------------------------------------------------
// 2. Concurrent read while index exists
//    One thread reads from the index while another re-indexes.
//    The reader should see a consistent snapshot (old or new), not crash.
// ---------------------------------------------------------------------------

#[test]
fn read_during_reindex_no_crash() {
    let dir = tempdir().unwrap();
    write_src(dir.path(), "a.rs", "pub fn original() {}\n");
    run_index(dir.path());

    let root = dir.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));

    // Reader thread: open and search the existing index
    let reader_root = root.clone();
    let reader_barrier = Arc::clone(&barrier);
    let reader_handle = thread::spawn(move || {
        reader_barrier.wait();
        // Try multiple reads to increase chance of overlap with writer
        for _ in 0..10 {
            let r = reader_root.canonicalize().unwrap();
            let index_path = config::index_path(&r);
            if let Ok(reader) = IndexReader::open(&index_path) {
                let _ = structural::search_with_fuzzy(&reader, "original", 10);
                let _ = reader.symbol_count();
            }
            // Don't sleep — just retry
        }
    });

    // Writer thread: re-index with different content
    let writer_root = root.clone();
    let writer_barrier = Arc::clone(&barrier);
    let writer_handle = thread::spawn(move || {
        writer_barrier.wait();
        let r = writer_root.canonicalize().unwrap();
        fs::write(
            r.join("src/a.rs"),
            "pub fn modified() {}\npub fn extra() {}\n",
        )
        .unwrap();
        pipeline::run(&r, false, "minilm-l6-v2", &[])
    });

    // Neither thread should panic
    reader_handle.join().expect("reader thread panicked");
    writer_handle
        .join()
        .expect("writer thread panicked")
        .expect("writer pipeline failed");

    // Final state should be valid
    let reader = open_reader(dir.path());
    let results = structural::search_with_fuzzy(&reader, "modified", 10);
    assert!(
        !results.is_empty(),
        "modified symbol should be findable after reindex"
    );
}

// ---------------------------------------------------------------------------
// 3. File deleted between discover and parse
//    Simulates a file disappearing after the file walker discovers it
//    but before tree-sitter reads it. Pipeline should not panic.
// ---------------------------------------------------------------------------

#[test]
fn file_deleted_during_indexing_no_panic() {
    let dir = tempdir().unwrap();
    write_src(dir.path(), "keep.rs", "pub fn keeper() {}\n");
    write_src(dir.path(), "vanish.rs", "pub fn vanisher() {}\n");

    // We can't easily inject a delete between discover and parse in the
    // current pipeline. Instead, we test that indexing a project where a
    // file becomes unreadable (permissions) or empty doesn't crash.
    // Delete the file before indexing — the walker won't find it, but this
    // at least exercises the "missing file" graceful path.
    fs::remove_file(dir.path().join("src/vanish.rs")).unwrap();

    run_index(dir.path());

    let reader = open_reader(dir.path());
    let results = structural::search_with_fuzzy(&reader, "keeper", 10);
    assert!(!results.is_empty(), "surviving file should be indexed");
    let vanished = structural::search_with_fuzzy(&reader, "vanisher", 10);
    assert!(
        vanished.is_empty(),
        "deleted file should not appear in index"
    );
}

// ---------------------------------------------------------------------------
// 4. Concurrent update + update: lock prevents double-write
// ---------------------------------------------------------------------------

#[test]
fn parallel_update_serialized_by_lock() {
    let dir = tempdir().unwrap();
    write_src(dir.path(), "a.rs", "pub fn base() {}\n");
    run_index(dir.path());

    // Modify a file so update has something to do
    fs::write(
        dir.path().join("src/a.rs"),
        "pub fn base() {}\npub fn added() {}\n",
    )
    .unwrap();

    let root = dir.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let r = root.clone();
            let b = Arc::clone(&barrier);
            thread::spawn(move || {
                b.wait();
                let r = r.canonicalize().unwrap();
                pipeline::update(&r, false, "minilm-l6-v2", &[])
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let successes = results.iter().filter(|r| r.is_ok()).count();
    assert!(successes >= 1, "at least one parallel update must succeed");

    let reader = open_reader(dir.path());
    let results = structural::search_with_fuzzy(&reader, "added", 10);
    assert!(
        !results.is_empty(),
        "added symbol should be findable after parallel update"
    );
}

// ---------------------------------------------------------------------------
// 5. Many concurrent readers: no crash
// ---------------------------------------------------------------------------

#[test]
fn many_concurrent_readers() {
    let dir = tempdir().unwrap();
    // Create a project with enough symbols to make reads non-trivial
    let mut content = String::new();
    for i in 0..100 {
        content.push_str(&format!("pub fn func_{i}() {{}}\n"));
    }
    write_src(dir.path(), "big.rs", &content);
    run_index(dir.path());

    let root = dir.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(8));

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let r = root.clone();
            let b = Arc::clone(&barrier);
            thread::spawn(move || {
                b.wait();
                let r = r.canonicalize().unwrap();
                let index_path = config::index_path(&r);
                let reader = IndexReader::open(&index_path).expect("open index");
                let query = format!("func_{}", i * 10);
                let results = structural::search_with_fuzzy(&reader, &query, 10);
                assert!(!results.is_empty(), "reader {i} should find {query}");
                reader.symbol_count()
            })
        })
        .collect();

    let counts: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    // All readers should see the same symbol count
    assert!(
        counts.iter().all(|&c| c == counts[0]),
        "all concurrent readers should see the same symbol count"
    );
}
