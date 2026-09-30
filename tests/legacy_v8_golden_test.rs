//! P0 (`docs/V9-FORMAT.md` §9/§13 R20) — library-level golden answers for
//! the v8 on-disk format, built programmatically (no checked-in binary
//! fixture) via `vex::store::legacy_v8::build_sample_v8_index`.
//!
//! These assertions are what the v9 CSR migration (P1 onward) must keep
//! passing: once the callees FST and `ref_edges` FST are replaced by
//! `store::csr`, a fresh index built from the same project must still
//! answer `callees` / `callers` / ref-edges lookups identically.
//!
//! Library-level on purpose (R20): calls `vex::index::pipeline::run`
//! directly, never spawns the `vex` binary, so there is no auto-update
//! step that could silently rewrite the index to a newer format before
//! the query runs.
//!
//! `set_cache_override` isolates every index this file builds into a
//! TempDir shared by this test binary, so repeated runs (and the CI
//! matrix) never leave orphaned cache dirs under the user's real cache
//! — mirrors `tests/grep_trigram_test.rs` / `tests/trigram_sidecar_test.rs`.

use std::sync::OnceLock;

use tempfile::TempDir;
use vex::store::call_graph::{find_callees_fast, find_callers_fast};
use vex::store::format::VERSION;
use vex::store::legacy_v8::build_sample_v8_index;
use vex::store::reader::IndexReader;

static CACHE_TMP: OnceLock<TempDir> = OnceLock::new();

fn shared_cache_root() {
    CACHE_TMP.get_or_init(|| {
        let tmp = TempDir::new().expect("create cache TempDir");
        let root = tmp.path().canonicalize().expect("canonicalize cache dir");
        vex::util::config::set_cache_override(root, false);
        tmp
    });
}

/// Canonicalize on macOS: `/var` symlinks to `/private/var`, and the
/// pipeline + `util::config::index_path` must agree on the canonical
/// project root (cache-path writer/reader-symmetry note).
fn canonical_project_root(tmp: &TempDir) -> std::path::PathBuf {
    shared_cache_root();
    tmp.path().canonicalize().unwrap()
}

fn symbol_idx_by_name(reader: &IndexReader, name: &str) -> u32 {
    for i in 0..reader.symbol_count() {
        if let Some(rec) = reader.symbol(i) {
            if reader.read_string(rec.name_offset) == name {
                return i as u32;
            }
        }
    }
    panic!("symbol {name} not found in index");
}

#[test]
fn golden_index_is_v8() {
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v8_index(&root).unwrap();
    let reader = IndexReader::open(&index_path).unwrap();

    // Pin: the programmatic builder must actually produce today's
    // format (v8) — if this ever fails, the golden answers below stop
    // meaning anything, because a later VERSION would already carry the
    // v9 CSR sections instead of the legacy FSTs being pinned here.
    assert_eq!(VERSION, 8, "this golden fixture assumes VERSION == 8");
    assert_eq!(reader.header().version, 8);
}

#[test]
fn golden_callees_of_caller_is_helper() {
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v8_index(&root).unwrap();
    let reader = IndexReader::open(&index_path).unwrap();

    assert!(reader.has_call_graph());
    let matches = find_callees_fast(&reader, "caller_fn", 50);
    assert_eq!(matches.len(), 1, "caller calls exactly helper: {matches:?}");
    assert_eq!(matches[0].name, "helper_fn");
    assert!(
        matches[0].path.ends_with("b.rs"),
        "callee call site is attributed to b.rs: {matches:?}"
    );
}

#[test]
fn golden_callers_of_helper_is_caller() {
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v8_index(&root).unwrap();
    let reader = IndexReader::open(&index_path).unwrap();

    let matches = find_callers_fast(&reader, "helper_fn", 50);
    assert_eq!(
        matches.len(),
        1,
        "helper has exactly one caller: {matches:?}"
    );
    assert_eq!(matches[0].name, "caller_fn");
    assert!(
        matches[0].path.ends_with("b.rs"),
        "caller is defined in b.rs: {matches:?}"
    );
}

#[test]
fn golden_ref_edges_resolve_cross_file_import() {
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v8_index(&root).unwrap();
    let reader = IndexReader::open(&index_path).unwrap();

    assert!(
        reader.has_ref_edges(),
        "the binder must have produced at least one cross-file ref"
    );

    let helper_idx = symbol_idx_by_name(&reader, "helper_fn");
    let refs = reader.find_ref_edges_by_symbol(helper_idx);
    assert_eq!(
        refs.len(),
        1,
        "helper has exactly one binder-resolved reference (b.rs's call site): {refs:?}"
    );

    let edge = &refs[0];
    assert_eq!(edge.line, 4, "the call site is on line 4 of b.rs");
    // kind encoding (col_and_kind high byte): the Rust binder emits bare
    // identifier references — including call-site callees — as
    // RefKind::Value (1), not RefKind::Call (2); see
    // `parse::scope::rust`'s `"identifier" => w.emit_ref(node, scope,
    // RefKind::Value)`.
    let kind = (edge.col_and_kind >> 24) as u8;
    assert_eq!(kind, 1, "the reference kind is Value, per the Rust binder");

    let file_paths = reader.file_paths();
    let from_path = &file_paths[edge.from_file_id as usize];
    assert!(
        from_path.ends_with("b.rs"),
        "the reference originates in b.rs: {from_path}"
    );
}
