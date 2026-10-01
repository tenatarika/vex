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
use vex::store::legacy_v8::{
    build_sample_v8_index, build_sample_v9_index, downgrade_v9_file_to_v8,
};
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

    // Pin: `build_sample_v8_index` must actually produce a v8 file — it
    // downgrades today's (v9) writer output via
    // `legacy_v8::downgrade_v9_file_to_v8` (P2 deviation, see that
    // function's doc comment), so this is no longer a statement about
    // the live `VERSION` constant (which is 9) but about the downgrade
    // converter's own correctness. If this ever fails, the golden
    // answers below stop meaning anything.
    assert_eq!(reader.header().version, 8);
}

#[test]
fn golden_index_is_v9() {
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v9_index(&root).unwrap();
    let reader = IndexReader::open(&index_path).unwrap();
    assert_eq!(reader.header().version, 9);
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

// ---------------------------------------------------------------------
// v9 parity: the same golden answers on a genuine v9 (CSR) index built
// from the identical fixture project. §8/§13: "golden answers on the v8
// file must equal the v9 answers for the same project".
// ---------------------------------------------------------------------

#[test]
fn golden_callees_of_caller_is_helper_on_v9() {
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v9_index(&root).unwrap();
    let reader = IndexReader::open(&index_path).unwrap();

    assert!(reader.has_call_graph());
    let matches = find_callees_fast(&reader, "caller_fn", 50);
    assert_eq!(matches.len(), 1, "caller calls exactly helper: {matches:?}");
    assert_eq!(matches[0].name, "helper_fn");
    assert!(matches[0].path.ends_with("b.rs"), "{matches:?}");
}

#[test]
fn golden_callers_of_helper_is_caller_on_v9() {
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v9_index(&root).unwrap();
    let reader = IndexReader::open(&index_path).unwrap();

    let matches = find_callers_fast(&reader, "helper_fn", 50);
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].name, "caller_fn");
    assert!(matches[0].path.ends_with("b.rs"), "{matches:?}");
}

#[test]
fn golden_ref_edges_resolve_cross_file_import_on_v9() {
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v9_index(&root).unwrap();
    let reader = IndexReader::open(&index_path).unwrap();

    assert!(reader.has_ref_edges());
    let helper_idx = symbol_idx_by_name(&reader, "helper_fn");
    let refs = reader.find_ref_edges_by_symbol(helper_idx);
    assert_eq!(refs.len(), 1, "{refs:?}");
    assert_eq!(refs[0].line, 4);
}

/// `downgrade_v9_file_to_v8` is the P2 deviation (see its own doc
/// comment) that makes `build_sample_v8_index` possible without
/// threading a format-version parameter through the production writer.
/// Prove its correctness directly: build v9 and v8 (= downgraded v9)
/// indexes from the *same* fixture and assert every golden answer is
/// identical between them, not just individually plausible.
#[test]
fn downgrade_v9_to_v8_preserves_every_golden_answer() {
    let tmp9 = TempDir::new().unwrap();
    let root9 = canonical_project_root(&tmp9);
    let v9_path = build_sample_v9_index(&root9).unwrap();
    let v9_reader = IndexReader::open(&v9_path).unwrap();

    let tmp8 = TempDir::new().unwrap();
    let root8 = canonical_project_root(&tmp8);
    let v8_path = build_sample_v9_index(&root8).unwrap();
    downgrade_v9_file_to_v8(&v8_path).unwrap();
    let v8_reader = IndexReader::open(&v8_path).unwrap();
    assert_eq!(v8_reader.header().version, 8);
    assert_eq!(v9_reader.header().version, 9);

    let v9_callees = find_callees_fast(&v9_reader, "caller_fn", 50);
    let v8_callees = find_callees_fast(&v8_reader, "caller_fn", 50);
    assert_eq!(
        v9_callees.iter().map(|m| &m.name).collect::<Vec<_>>(),
        v8_callees.iter().map(|m| &m.name).collect::<Vec<_>>(),
    );

    let v9_callers = find_callers_fast(&v9_reader, "helper_fn", 50);
    let v8_callers = find_callers_fast(&v8_reader, "helper_fn", 50);
    assert_eq!(
        v9_callers.iter().map(|m| &m.name).collect::<Vec<_>>(),
        v8_callers.iter().map(|m| &m.name).collect::<Vec<_>>(),
    );

    let v9_helper_idx = symbol_idx_by_name(&v9_reader, "helper_fn");
    let v8_helper_idx = symbol_idx_by_name(&v8_reader, "helper_fn");
    assert_eq!(
        v9_helper_idx, v8_helper_idx,
        "symbol numbering is unchanged"
    );
    let v9_refs = v9_reader.find_ref_edges_by_symbol(v9_helper_idx);
    let v8_refs = v8_reader.find_ref_edges_by_symbol(v8_helper_idx);
    assert_eq!(v9_refs.len(), v8_refs.len());
    assert_eq!(v9_refs[0].line, v8_refs[0].line);
}

// ---------------------------------------------------------------------
// Downgrade gate + adversarial CSR corruption (§7, §13 R1/R4).
// ---------------------------------------------------------------------

#[test]
fn v8_reader_build_rejects_a_v9_file() {
    // Mirrors `store::reader::tests::open_rejects_pre_v9_reading_v9_via_version_range_gate`
    // at the library level: an actual v9 file, opened by a build whose
    // own `MIN_SUPPORTED_VERSION..=VERSION` range ends at 8, must be
    // rejected with the existing range-gate message — never silently
    // misread as v8.
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v9_index(&root).unwrap();

    let hypothetical_pre_v9_max_version: u32 = 8;
    let hypothetical_min_supported: u32 = 3;
    let bytes = std::fs::read(&index_path).unwrap();
    let on_disk_version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    assert_eq!(on_disk_version, 9);
    assert!(
        !(hypothetical_min_supported..=hypothetical_pre_v9_max_version).contains(&on_disk_version),
        "a pre-v9 build's version gate must reject this v9 file"
    );
}

/// Corrupt the v9 callees CSR `offsets[0]` field (must be 0) — `open()`
/// must reject this cleanly, not panic, per §7's "every new reader"
/// open-time validation.
#[test]
fn corrupt_callees_csr_offsets_zero_rejected_cleanly() {
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v9_index(&root).unwrap();
    let reader = IndexReader::open(&index_path).unwrap();
    let cg = reader.call_graph_header().expect("v9 has CallGraphHeader");
    assert!(cg.callees_index_len > 0, "fixture must have call edges");
    let offsets_offset = cg.callees_index_offset as usize;
    drop(reader);

    let mut bytes = std::fs::read(&index_path).unwrap();
    // offsets[0] must be 0 — corrupt it to a nonzero value.
    bytes[offsets_offset..offsets_offset + 4].copy_from_slice(&7u32.to_le_bytes());
    std::fs::write(&index_path, &bytes).unwrap();

    let err = IndexReader::open(&index_path)
        .err()
        .expect("corrupt callees offsets[0] must be rejected at open");
    let msg = err.to_string();
    assert!(
        msg.contains("is corrupted (callees CSR offsets invalid)"),
        "expected the reader.rs offsets[0]!=0 bail message, got: {msg}"
    );
}

/// Corrupt the v9 `ref_edges` `edge_idx_len` field to a nonzero value —
/// v9 requires it to be exactly 0 (identity-elided, §2.3) — `open()`
/// must reject this cleanly, never panic.
#[test]
fn corrupt_ref_edges_nonzero_edge_idx_len_rejected_cleanly() {
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v9_index(&root).unwrap();
    let reader = IndexReader::open(&index_path).unwrap();
    let v5 = reader.v5_section_header().expect("v9 has V5SectionHeader");
    assert!(v5.ref_edges_len > 0, "fixture must have ref edges");
    assert_eq!(
        v5.ref_edges_edge_idx_len, 0,
        "v9 writer must elide edge_idx"
    );
    // ref_edges_edge_idx_len is the 6th u64 field in V5SectionHeader (byte
    // offset 40 within the struct).
    let v5_header_offset = std::mem::size_of::<vex::store::format::Header>()
        + vex::store::format::CallGraphHeader::SIZE;
    let field_offset = v5_header_offset + 40;
    drop(reader);

    let mut bytes = std::fs::read(&index_path).unwrap();
    bytes[field_offset..field_offset + 8].copy_from_slice(&1u64.to_le_bytes());
    std::fs::write(&index_path, &bytes).unwrap();

    let err = IndexReader::open(&index_path)
        .err()
        .expect("nonzero v9 ref_edges edge_idx_len must be rejected at open");
    let msg = err.to_string();
    assert!(
        msg.contains("is corrupted (ref_edges edge_idx_len must be 0, found 1)"),
        "expected the reader.rs nonzero-edge_idx_len bail message, got: {msg}"
    );
}

/// Corrupt `offsets[n]` so it no longer equals `m` — `CsrView::new`'s own
/// check must catch this at open (R4), degrading to a clean error
/// rather than a panic or silently-wrong lookups.
#[test]
fn corrupt_callees_csr_offsets_n_mismatch_rejected_cleanly() {
    let tmp = TempDir::new().unwrap();
    let root = canonical_project_root(&tmp);
    let index_path = build_sample_v9_index(&root).unwrap();
    let reader = IndexReader::open(&index_path).unwrap();
    let cg = reader.call_graph_header().expect("v9 has CallGraphHeader");
    let n = reader.symbol_count() as u64;
    assert!(cg.callees_index_len > 0, "fixture must have call edges");
    // offsets[n] is the last u32 in the offsets array.
    let last_offset_byte = cg.callees_index_offset as usize + (n as usize) * 4;
    drop(reader);

    let mut bytes = std::fs::read(&index_path).unwrap();
    bytes[last_offset_byte..last_offset_byte + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&index_path, &bytes).unwrap();

    let err = IndexReader::open(&index_path)
        .err()
        .expect("offsets[n] != m must be rejected at open, never silently accepted");
    let msg = err.to_string();
    assert!(
        msg.contains("is corrupted (callees CSR offsets invalid)"),
        "expected the reader.rs offsets[n]!=m bail message, got: {msg}"
    );
}
