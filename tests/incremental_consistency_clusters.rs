//! P4b (`docs/V9-FORMAT.md` §5, §13 R11-R14) incremental-consistency
//! tests for symbol clusters, following the pattern of
//! `tests/incremental_consistency_ref_edges.rs`: full pipeline runs
//! (`vex::index::pipeline::run`/`update`), then assertions against the
//! real on-disk `IndexReader`/`ClusterSectionReader`.

use std::collections::HashSet;
use std::fs;

use tempfile::TempDir;

use vex::index::manifest::Manifest;
use vex::index::pipeline::{self, IndexOptions};
use vex::store::cluster_section::ClusterStatus;
use vex::store::legacy_v8;
use vex::store::reader::IndexReader;
use vex::util::config;

fn open_reader(project_dir: &std::path::Path) -> IndexReader {
    let canonical = project_dir.canonicalize().unwrap();
    IndexReader::open(&config::index_path(&canonical)).unwrap()
}

/// Linear scan for the symbol at `(path, name, line)` — small fixtures
/// only, so O(n) is fine and keeps the test independent of any FST
/// lookup path.
fn find_sym(reader: &IndexReader, path: &str, name: &str, line: u32) -> Option<u32> {
    for i in 0..reader.symbol_count() {
        let rec = reader.symbol(i)?;
        if reader.read_string(rec.file_offset) == path
            && reader.read_string(rec.name_offset) == name
            && rec.line == line
        {
            return Some(i as u32);
        }
    }
    None
}

fn status_of(reader: &IndexReader, path: &str, name: &str, line: u32) -> ClusterStatus {
    let idx = find_sym(reader, path, name, line)
        .unwrap_or_else(|| panic!("symbol {name} @ {path}:{line} not found"));
    reader
        .cluster_section_reader()
        .expect("COMPUTED cluster section")
        .status(idx)
}

/// Two disconnected 4-function call cliques (`mod_a`, `mod_b`) plus one
/// fully isolated function (`mod_c::iso`) — the same shape as the
/// writer-level `four_clique_plus_isolated` fixture (`src/store/writer.rs`
/// `cluster_tests`), proven to produce real clusters at the default
/// γ=1/8. Two disjoint cliques give us two independent cluster ordinals
/// to track across an update, plus an UNCLUSTERED control symbol.
fn write_two_clique_project(root: &std::path::Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/mod_a.rs"),
        "pub fn f0() -> i32 { f1() + f2() }\n\
         pub fn f1() -> i32 { f2() }\n\
         pub fn f2() -> i32 { f3() }\n\
         pub fn f3() -> i32 { f0() }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/mod_b.rs"),
        "pub fn g0() -> i32 { g1() + g2() }\n\
         pub fn g1() -> i32 { g2() }\n\
         pub fn g2() -> i32 { g3() }\n\
         pub fn g3() -> i32 { g0() }\n",
    )
    .unwrap();
    fs::write(root.join("src/mod_c.rs"), "pub fn iso() -> i32 { 42 }\n").unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub mod mod_a;\npub mod mod_b;\npub mod mod_c;\n",
    )
    .unwrap();
}

fn run_default(root: &std::path::Path) {
    pipeline::run(root, IndexOptions::default(), "minilm-l6-v2", &[]).unwrap();
}

fn update_default(root: &std::path::Path) {
    pipeline::update(root, IndexOptions::default(), "minilm-l6-v2", &[]).unwrap();
}

// ── Test: edit one (unrelated) file → STALE set, ordinals preserved ───────

#[test]
fn update_after_edit_sets_stale_and_preserves_unrelated_ordinals() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    write_two_clique_project(&root);
    run_default(&root);

    let before = open_reader(&root);
    assert!(
        before.has_clusters(),
        "initial full index must compute clusters"
    );
    let summary_before = before.cluster_section_reader().unwrap().summary();
    assert_eq!(
        summary_before.k, 2,
        "two disjoint 4-cliques -> two clusters"
    );
    assert!(!summary_before.stale);

    let ord_f0 = match status_of(&before, "src/mod_a.rs", "f0", 1) {
        ClusterStatus::Clustered(ord) => ord,
        other => panic!("expected f0 clustered, got {other:?}"),
    };
    let ord_g0 = match status_of(&before, "src/mod_b.rs", "g0", 1) {
        ClusterStatus::Clustered(ord) => ord,
        other => panic!("expected g0 clustered, got {other:?}"),
    };
    assert_ne!(
        ord_f0, ord_g0,
        "the two cliques must land in different clusters"
    );
    assert_eq!(
        status_of(&before, "src/mod_c.rs", "iso", 1),
        ClusterStatus::Unclustered
    );

    // Record every mod_a/mod_b member's ordinal by (path, name, line)
    // before the edit, to compare after.
    let members_before: Vec<(&str, &str, u32, ClusterStatus)> = vec![
        (
            "src/mod_a.rs",
            "f0",
            1,
            status_of(&before, "src/mod_a.rs", "f0", 1),
        ),
        (
            "src/mod_a.rs",
            "f1",
            2,
            status_of(&before, "src/mod_a.rs", "f1", 2),
        ),
        (
            "src/mod_a.rs",
            "f2",
            3,
            status_of(&before, "src/mod_a.rs", "f2", 3),
        ),
        (
            "src/mod_a.rs",
            "f3",
            4,
            status_of(&before, "src/mod_a.rs", "f3", 4),
        ),
        (
            "src/mod_b.rs",
            "g0",
            1,
            status_of(&before, "src/mod_b.rs", "g0", 1),
        ),
        (
            "src/mod_b.rs",
            "g1",
            2,
            status_of(&before, "src/mod_b.rs", "g1", 2),
        ),
        (
            "src/mod_b.rs",
            "g2",
            3,
            status_of(&before, "src/mod_b.rs", "g2", 3),
        ),
        (
            "src/mod_b.rs",
            "g3",
            4,
            status_of(&before, "src/mod_b.rs", "g3", 4),
        ),
    ];
    drop(before);

    // Edit mod_c.rs only: keep `iso` (the surviving symbol of the edited
    // file — exercises the re-parsed 1:1 key-match rule, R12) and add a
    // brand-new eligible symbol `iso2` (-> NEW) plus a new ineligible
    // Markdown heading elsewhere (-> NOT_ELIGIBLE, not NEW).
    fs::write(
        root.join("src/mod_c.rs"),
        "pub fn iso() -> i32 { 42 }\npub fn iso2() -> i32 { 7 }\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs/NOTE.md"), "# Hello\n").unwrap();

    update_default(&root);

    let after = open_reader(&root);
    let summary_after = after.cluster_section_reader().unwrap().summary();
    assert!(summary_after.stale, "a carry-update must set STALE");
    assert_eq!(summary_after.k, 2, "the table is carried, not recomputed");
    assert_eq!(summary_after.resolution, (1, 8));
    assert_eq!(summary_after.algo_version, 1);

    // Every unchanged-file symbol (mod_a.rs, mod_b.rs were never touched)
    // keeps its EXACT ordinal/status, compared by (path, name, line).
    for (path, name, line, before_status) in &members_before {
        assert_eq!(
            status_of(&after, path, name, *line),
            *before_status,
            "{path}::{name} must keep its pre-update cluster status"
        );
    }

    // The edited file's surviving symbol `iso` keeps its OLD assignment
    // (carried via the 1:1 key-match rule, not recomputed).
    assert_eq!(
        status_of(&after, "src/mod_c.rs", "iso", 1),
        ClusterStatus::Unclustered
    );

    // A brand-new eligible symbol with no old counterpart -> NEW.
    assert_eq!(
        status_of(&after, "src/mod_c.rs", "iso2", 2),
        ClusterStatus::New
    );

    // A brand-new symbol excluded by kind+language (Markdown heading) ->
    // NOT_ELIGIBLE, never NEW (§13 R13).
    assert_eq!(
        status_of(&after, "docs/NOTE.md", "Hello", 1),
        ClusterStatus::NotEligible
    );
}

// ── Test: overload/duplicate case -> NEW, never many-to-one ───────────────

#[test]
fn duplicate_name_in_edited_file_becomes_new_not_many_to_one() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir_all(root.join("src")).unwrap();
    // `dup` joins the clique (called by f0, calls f1) so its OLD cluster
    // assignment is a real, non-default ordinal — a many-to-one bug would
    // let one of the two new `dup`s silently inherit it.
    fs::write(
        root.join("src/mod_a.rs"),
        "pub fn f0() -> i32 { f1() + f2() + dup() }\n\
         pub fn f1() -> i32 { f2() }\n\
         pub fn f2() -> i32 { f3() }\n\
         pub fn f3() -> i32 { f0() }\n\
         pub fn dup() -> i32 { f1() }\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub mod mod_a;\n").unwrap();
    run_default(&root);

    let before = open_reader(&root);
    let dup_status_before = status_of(&before, "src/mod_a.rs", "dup", 5);
    assert!(
        matches!(dup_status_before, ClusterStatus::Clustered(_)),
        "fixture must give the original `dup` a real cluster assignment, got {dup_status_before:?}"
    );
    drop(before);

    // Edit: append a SECOND `dup` function. The new file now has two
    // symbols keyed (name="dup", kind=Function) — ambiguous on the NEW
    // side, so neither may inherit the old single `dup`'s assignment.
    fs::write(
        root.join("src/mod_a.rs"),
        "pub fn f0() -> i32 { f1() + f2() + dup() }\n\
         pub fn f1() -> i32 { f2() }\n\
         pub fn f2() -> i32 { f3() }\n\
         pub fn f3() -> i32 { f0() }\n\
         pub fn dup() -> i32 { f1() }\n\
         pub fn dup() -> i32 { f2() }\n",
    )
    .unwrap();
    update_default(&root);

    let after = open_reader(&root);
    assert_eq!(
        status_of(&after, "src/mod_a.rs", "dup", 5),
        ClusterStatus::New
    );
    assert_eq!(
        status_of(&after, "src/mod_a.rs", "dup", 6),
        ClusterStatus::New
    );
    // The rest of the clique, unaffected by the ambiguity, still carries.
    assert!(matches!(
        status_of(&after, "src/mod_a.rs", "f0", 1),
        ClusterStatus::Clustered(_)
    ));
}

// ── Test: deleted file with many clusters — reps/hubs lost, index still opens ──

#[test]
fn deleted_file_loses_rep_and_hubs_but_index_and_status_still_work() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    write_two_clique_project(&root);
    run_default(&root);

    let before = open_reader(&root);
    let ord_a = match status_of(&before, "src/mod_a.rs", "f0", 1) {
        ClusterStatus::Clustered(ord) => ord,
        other => panic!("expected f0 clustered, got {other:?}"),
    };
    let ord_b = match status_of(&before, "src/mod_b.rs", "g0", 1) {
        ClusterStatus::Clustered(ord) => ord,
        other => panic!("expected g0 clustered, got {other:?}"),
    };
    let rec_b_before = before
        .cluster_section_reader()
        .unwrap()
        .record(ord_b as usize)
        .unwrap();
    assert!(
        rec_b_before.rep_sym_idx.is_some(),
        "mod_b's cluster must have a live rep before the update"
    );
    drop(before);

    // Delete mod_a.rs entirely — its whole 4-member cluster vanishes from
    // the symbol table. This is the regression case for the old
    // `k <= symbol_count/2` brick (§13 R3): the live symbol_count shrinks
    // by 4 while the frozen table still claims k=2.
    fs::remove_file(root.join("src/mod_a.rs")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub mod mod_b;\npub mod mod_c;\n").unwrap();
    update_default(&root);

    let after = open_reader(&root);
    assert!(
        find_sym(&after, "src/mod_a.rs", "f0", 1).is_none(),
        "mod_a's symbols must be gone"
    );

    // The index must still OPEN and `vex status`-equivalent reads
    // (summary/cluster_section_reader) must still WORK — the historical
    // bug bricked exactly this path.
    let csr = after
        .cluster_section_reader()
        .expect("cluster section must still open after a file deletion");
    let summary = csr.summary();
    assert_eq!(
        summary.k, 2,
        "the table is carried record-by-record, same k"
    );

    // mod_a's ordinal now has a LOST rep/hubs — every member vanished, so
    // §5 rule 5's remap must degrade them to `None` (on-disk `u32::MAX`),
    // never a stale or out-of-range index.
    let rec_a_after = csr.record(ord_a as usize).unwrap();
    assert!(
        rec_a_after.rep_sym_idx.is_none(),
        "mod_a's rep must be lost (None/u32::MAX) once every member is deleted"
    );
    assert!(
        rec_a_after.hubs.iter().all(Option::is_none),
        "mod_a's hubs must all be lost once every member is deleted"
    );
    // Frozen size/weights survive even though the members are gone.
    assert_eq!(rec_a_after.size, 4);

    // mod_b was untouched by the deletion — its rep/hubs remap to valid
    // NEW indices (not lost).
    let rec_b_after = csr.record(ord_b as usize).unwrap();
    assert!(
        rec_b_after.rep_sym_idx.is_some(),
        "mod_b's rep must remap to a live NEW sym_idx, not be lost"
    );
}

// ── Test: `vex index` after a carry-update clears STALE and recomputes ────

#[test]
fn full_index_after_update_clears_stale_and_is_semantically_equivalent_to_a_fresh_index() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    write_two_clique_project(&root);
    run_default(&root);

    fs::write(
        root.join("src/mod_c.rs"),
        "pub fn iso() -> i32 { 42 }\npub fn iso2() -> i32 { 7 }\n",
    )
    .unwrap();
    update_default(&root);

    let after_update = open_reader(&root);
    assert!(
        after_update
            .cluster_section_reader()
            .unwrap()
            .summary()
            .stale
    );
    drop(after_update);

    // A full `vex index` on the now-updated tree must recompute and
    // clear STALE.
    run_default(&root);
    let after_reindex = open_reader(&root);
    let summary = after_reindex.cluster_section_reader().unwrap().summary();
    assert!(!summary.stale, "a full `vex index` must clear STALE");
    assert_eq!(summary.new_count, 0, "a full index leaves no NEW sentinels");

    // Cross-check against a FRESH index of the identical final tree
    // state, built from scratch in an independent project directory (a
    // separate cache subdir, keyed by its own canonical path). We
    // compare by canonical (path, name) identity rather than raw mmap
    // bytes: `vex`'s file walk does not sort entries (`docs/V9-FORMAT.md`
    // §13 R6 — sym_idx order is walk-order-dependent, deliberately out of
    // scope), so two independently-walked directories are not guaranteed
    // byte-identical even though the writer itself is fully deterministic
    // given the same input order (see `two_cold_full_indexes_of_the_same_input_are_byte_identical`
    // in `src/store/writer.rs`). Semantic equivalence is the real
    // invariant §5/§13 promise here.
    let tmp2 = TempDir::new().unwrap();
    let root2 = tmp2.path().join("project");
    fs::create_dir_all(root2.join("src")).unwrap();
    for name in ["mod_a.rs", "mod_b.rs", "mod_c.rs", "lib.rs"] {
        fs::copy(root.join("src").join(name), root2.join("src").join(name)).unwrap();
    }
    run_default(&root2);
    let fresh = open_reader(&root2);
    let fresh_summary = fresh.cluster_section_reader().unwrap().summary();
    assert_eq!(fresh_summary.k, summary.k);
    assert_eq!(fresh_summary.resolution, summary.resolution);

    // Member sets, by (path, name) — independent of sym_idx order.
    let cluster_members_by_name = |reader: &IndexReader, ord: usize| -> HashSet<(String, String)> {
        reader
            .cluster_section_reader()
            .unwrap()
            .members(ord)
            .into_iter()
            .map(|idx| {
                let rec = reader.symbol(idx as usize).unwrap();
                (
                    reader.read_string(rec.file_offset).to_string(),
                    reader.read_string(rec.name_offset).to_string(),
                )
            })
            .collect()
    };
    let ord_a_reindexed = match status_of(&after_reindex, "src/mod_a.rs", "f0", 1) {
        ClusterStatus::Clustered(ord) => ord as usize,
        other => panic!("expected f0 clustered after reindex, got {other:?}"),
    };
    let ord_a_fresh = match status_of(&fresh, "src/mod_a.rs", "f0", 1) {
        ClusterStatus::Clustered(ord) => ord as usize,
        other => panic!("expected f0 clustered in the fresh index, got {other:?}"),
    };
    assert_eq!(
        cluster_members_by_name(&after_reindex, ord_a_reindexed),
        cluster_members_by_name(&fresh, ord_a_fresh),
        "the reindexed-after-update tree and a fresh index of the same tree \
         must agree on cluster membership"
    );
}

// ── Test: v8 -> update computes clusters once ──────────────────────────────

#[test]
fn v8_index_update_computes_clusters_once() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    // Canonicalize BEFORE calling into `legacy_v8` — `pipeline::run`
    // canonicalizes its root internally, and `index_path`/`manifest_path`
    // must agree with that canonical form (cache-path writer/reader
    // symmetry; macOS `/tmp` -> `/private/tmp`), exactly like
    // `tests/legacy_v8_golden_test.rs`'s `canonical_project_root`.
    let root = root.canonicalize().unwrap();
    // `build_sample_v8_index` runs a real `pipeline::run` (default
    // options — clusters wanted) on its own small fixture, then rewrites
    // the file as v8 in place, stripping the v9-only cluster section
    // entirely (and the manifest's `clusters_opt_out` stays `Some(false)`
    // from that original run — not opted out).
    legacy_v8::build_sample_v8_index(&root).unwrap();

    let before = open_reader(&root);
    assert_eq!(before.header().version, 8);
    assert!(
        !before.has_clusters(),
        "a v8 file has no cluster header at all"
    );
    drop(before);

    // Even a NO-CHANGE update must converge v8 -> v9 (R5) and, per R14,
    // compute clusters once since the manifest never recorded an opt-out.
    update_default(&root);

    let after = open_reader(&root);
    assert_eq!(after.header().version, vex::store::format::VERSION);
    assert!(
        after.has_clusters(),
        "R14: update must compute clusters once for a v8 upgrade"
    );
    let summary = after.cluster_section_reader().unwrap().summary();
    assert!(
        !summary.stale,
        "a fresh compute-once is not a carry — never STALE"
    );
    assert_eq!(summary.algo_version, 1);

    let manifest = Manifest::load(&config::manifest_path(&root.canonicalize().unwrap())).unwrap();
    assert_eq!(
        manifest.clusters_full,
        Some(true),
        "clusters_full reflects a freshly-COMPUTED section, update's R14 compute-once included"
    );
}

// ── Test: `--no-clusters` then update does NOT compute ─────────────────────

#[test]
fn no_clusters_opt_out_is_respected_by_a_later_update() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    write_two_clique_project(&root);

    let no_clusters_opts = IndexOptions {
        with_clusters: false,
        ..IndexOptions::default()
    };
    pipeline::run(&root, no_clusters_opts, "minilm-l6-v2", &[]).unwrap();

    let before = open_reader(&root);
    assert!(!before.has_clusters());
    drop(before);
    let manifest = Manifest::load(&config::manifest_path(&root.canonicalize().unwrap())).unwrap();
    assert_eq!(manifest.clusters_opt_out, Some(true));

    // A real edit (not a no-change update) so the writer actually runs
    // and makes the compute-once-vs-opt-out decision.
    fs::write(root.join("src/mod_c.rs"), "pub fn iso() -> i32 { 43 }\n").unwrap();
    update_default(&root);

    let after = open_reader(&root);
    assert!(
        !after.has_clusters(),
        "an explicit --no-clusters opt-out must survive a later `vex update`"
    );
    let manifest_after =
        Manifest::load(&config::manifest_path(&root.canonicalize().unwrap())).unwrap();
    assert_eq!(
        manifest_after.clusters_opt_out,
        Some(true),
        "the opt-out marker itself must carry forward across the update"
    );
}

// ── Test: no-change `vex update` still skips ───────────────────────────────

#[test]
fn no_change_update_still_skips_with_clusters_enabled() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    write_two_clique_project(&root);
    run_default(&root);

    let before = open_reader(&root);
    let summary_before = before.cluster_section_reader().unwrap().summary();
    drop(before);

    let (_total, changed, deleted) =
        pipeline::update(&root, IndexOptions::default(), "minilm-l6-v2", &[]).unwrap();
    assert_eq!(changed, 0);
    assert_eq!(deleted, 0);

    let after = open_reader(&root);
    let summary_after = after.cluster_section_reader().unwrap().summary();
    assert_eq!(
        summary_after, summary_before,
        "a no-change update must skip entirely — the cluster section is untouched"
    );
}

// ── Test: corrupt prior cluster section must not fail `vex update` ────────
//
// Code-review follow-up: §13 R3 says cluster corruption may only break
// cluster *features*, never other commands — and `vex update` runs on
// the auto-update path in front of every query, so it must be
// especially forgiving. A literal mid-table `record(ord) == None` for
// `ord < k` is NOT reachable through a real on-disk file (see
// `build_old_table_from_records`'s doc comment in `src/index/pipeline/mod.rs`:
// `k` is derived from `table_len`, and `ClusterSectionReader::new` only
// succeeds when the mmap slice is exactly `table_len` bytes). Corrupting
// the header so `cluster_section_reader()` returns `None` entirely is
// the realistically-reachable form of "the prior cluster section is
// corrupt", and exercises the exact same downstream contract this item
// protects: `vex update` must not fail, and must recompute fresh
// clusters (R14) rather than propagate an error.

fn corrupt_cluster_resolution_den(index_path: &std::path::Path) {
    let mut bytes = fs::read(index_path).unwrap();
    let cluster_header_offset = vex::store::format::Header::SIZE
        + vex::store::format::CallGraphHeader::SIZE
        + vex::store::format::V5SectionHeader::SIZE
        + vex::store::format::PatternSkeletonHeader::SIZE
        + vex::store::format::UnresolvedRefsHeader::SIZE
        + vex::store::format::HierarchyHeader::SIZE
        + vex::store::format::UnresolvedHierarchyHeader::SIZE;
    // `resolution_den` is at byte 36 within ClusterHeader (u32) —
    // `ClusterSectionReader::new` treats `resolution_den == 0` as
    // corrupt and returns `None`.
    let den_offset = cluster_header_offset + 36;
    assert_ne!(
        u32::from_le_bytes(bytes[den_offset..den_offset + 4].try_into().unwrap()),
        0,
        "fixture must have a real (non-zero) resolution_den before corrupting it"
    );
    bytes[den_offset..den_offset + 4].copy_from_slice(&0u32.to_le_bytes());
    fs::write(index_path, &bytes).unwrap();
}

#[test]
fn corrupted_prior_cluster_section_does_not_fail_update_and_recomputes() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    write_two_clique_project(&root);
    run_default(&root);

    let canonical = root.canonicalize().unwrap();
    let index_path = config::index_path(&canonical);
    corrupt_cluster_resolution_den(&index_path);

    let corrupted = IndexReader::open(&index_path).unwrap();
    assert!(
        corrupted.cluster_section_reader().is_none(),
        "the corruption must defeat ClusterSectionReader::new's own validation"
    );
    drop(corrupted);

    // A real edit so `update` does real work (not the no-change skip
    // path), reaching the cluster-carry decision.
    fs::write(
        canonical.join("src/mod_c.rs"),
        "pub fn iso() -> i32 { 43 }\n",
    )
    .unwrap();

    // Must succeed — a corrupted prior cluster section must never fail
    // `vex update` (§13 R3).
    update_default(&canonical);

    let after = open_reader(&canonical);
    assert!(
        after.has_clusters(),
        "R14 must recompute fresh clusters since the prior section was unreadable \
         and nothing opted out"
    );
    let summary = after.cluster_section_reader().unwrap().summary();
    assert!(!summary.stale, "a fresh compute-once is never STALE");
}

#[test]
fn corrupted_prior_cluster_section_stays_absent_when_opted_out() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    write_two_clique_project(&root);
    run_default(&root);

    let canonical = root.canonicalize().unwrap();
    let index_path = config::index_path(&canonical);
    corrupt_cluster_resolution_den(&index_path);

    // Simulate a user who previously ran `vex index --no-clusters` — the
    // manifest's opt-out marker is independent of the binary cluster
    // section we just corrupted.
    let manifest_path = config::manifest_path(&canonical);
    let mut manifest = Manifest::load(&manifest_path).unwrap();
    manifest.clusters_opt_out = Some(true);
    manifest.save(&manifest_path).unwrap();

    fs::write(
        canonical.join("src/mod_c.rs"),
        "pub fn iso() -> i32 { 43 }\n",
    )
    .unwrap();
    update_default(&canonical);

    let after = open_reader(&canonical);
    assert!(
        !after.has_clusters(),
        "an opted-out user must not get clusters even when the prior section was corrupt"
    );
}
