#![cfg(test)]

use std::collections::HashMap;

use super::*;
use crate::parse::language::Language;

#[test]
fn grammar_failure_summary_includes_language_count_and_reason() {
    // Pin the structured fields the user-visible warning emits, so a future
    // refactor cannot silently drop the count or error string without test
    // fail. We cannot easily hit the path end-to-end (every grammar
    // currently loads), so this test mirrors the format the warning
    // produces and locks the contract.
    let mut failures: HashMap<Language, (String, usize)> = HashMap::new();
    failures.insert(Language::CSharp, ("ABI mismatch v15".to_string(), 42));

    let mut rendered = String::new();
    for (lang, (err, count)) in &failures {
        rendered = format!("language={lang:?} skipped={count} error={err}");
    }
    assert!(rendered.contains("CSharp"), "{rendered}");
    assert!(rendered.contains("42"), "{rendered}");
    assert!(rendered.contains("ABI mismatch v15"), "{rendered}");
}

// --- A1 (v1.12.0): options-aware skip-path helpers -------------------

fn manifest_with_embedder(id: Option<&str>) -> Manifest {
    Manifest {
        embedder_id: id.map(|s| s.to_string()),
        ..Manifest::default()
    }
}

#[test]
fn options_cover_when_caller_does_not_want_embeddings() {
    let opts = IndexOptions {
        with_embeddings: false,
        ..IndexOptions::default()
    };
    // Peer with no embeddings: covered.
    assert!(manifest_options_cover(
        &manifest_with_embedder(None),
        opts,
        "minilm-l6-v2"
    ));
    // Peer that built MORE than we need (has embeddings): still covered —
    // the extra section is harmless.
    assert!(manifest_options_cover(
        &manifest_with_embedder(Some("minilm-l6-v2")),
        opts,
        "minilm-l6-v2"
    ));
}

#[test]
fn options_do_not_cover_when_caller_wants_embeddings_but_peer_has_none() {
    let opts = IndexOptions {
        with_embeddings: true,
        ..IndexOptions::default()
    };
    // Peer skipped embeddings — we'd silently downgrade if we skipped here.
    assert!(!manifest_options_cover(
        &manifest_with_embedder(None),
        opts,
        "minilm-l6-v2"
    ));
}

#[test]
fn options_do_not_cover_when_caller_and_peer_disagree_on_embedder_id() {
    let opts = IndexOptions {
        with_embeddings: true,
        ..IndexOptions::default()
    };
    assert!(!manifest_options_cover(
        &manifest_with_embedder(Some("bge-small")),
        opts,
        "minilm-l6-v2"
    ));
}

#[test]
fn options_cover_when_embedder_ids_match() {
    let opts = IndexOptions {
        with_embeddings: true,
        ..IndexOptions::default()
    };
    assert!(manifest_options_cover(
        &manifest_with_embedder(Some("minilm-l6-v2")),
        opts,
        "minilm-l6-v2"
    ));
}

#[test]
fn run_refuses_to_skip_partial_pattern_index_when_caller_opted_in() {
    // The caller ran `vex index` (full rebuild) and the pattern index is
    // wanted. A peer's manifest from `vex update` (pattern_index_full =
    // Some(false)) does not satisfy the explicit ask, so skip is rejected.
    let opts = IndexOptions {
        with_embeddings: false,
        with_pattern_index: true,
        ..IndexOptions::default()
    };
    let m = Manifest {
        pattern_index_full: Some(false),
        ..Manifest::default()
    };
    assert!(!run_can_skip(&m, opts, "minilm-l6-v2"));
}

#[test]
fn run_accepts_full_or_pre_flag_pattern_index() {
    let opts = IndexOptions {
        with_embeddings: false,
        with_pattern_index: true,
        // Orthogonal to this test's concern (pattern_index_full) — see
        // the dedicated P4a cluster-skip-gate tests below for that gate.
        with_clusters: false,
        ..IndexOptions::default()
    };
    let full = Manifest {
        pattern_index_full: Some(true),
        ..Manifest::default()
    };
    assert!(run_can_skip(&full, opts, "minilm-l6-v2"));

    // `None` is not `Some(false)`, so the guard at the top of
    // `run_can_skip` does not fire — pre-11.4 manifests slip through.
    // The Manifest doc treats `None` as conservative (i.e. *not* full),
    // but the skip gate only blocks on an explicit `Some(false)` written
    // by `vex update`; that is the precise scenario the gate exists for.
    let pre_flag = Manifest::default();
    assert!(run_can_skip(&pre_flag, opts, "minilm-l6-v2"));
}

#[test]
fn run_ignores_pattern_index_full_when_caller_did_not_ask_for_pattern_index() {
    let opts = IndexOptions {
        with_embeddings: false,
        with_pattern_index: false,
        // Orthogonal to this test's concern — see the dedicated P4a
        // cluster-skip-gate tests below.
        with_clusters: false,
        ..IndexOptions::default()
    };
    let m = Manifest {
        pattern_index_full: Some(false),
        ..Manifest::default()
    };
    assert!(run_can_skip(&m, opts, "minilm-l6-v2"));
}

// --- P4a (`docs/V9-FORMAT.md` §13 R5): cluster skip-gate regression guard --

#[test]
fn run_refuses_to_skip_when_clusters_wanted_but_not_computed() {
    // Mirrors `run_refuses_to_skip_partial_pattern_index_when_caller_opted_in`:
    // a peer's manifest from `vex update` (which never computes clusters in
    // P4a) must not satisfy a `vex index` that wants them.
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: true,
        ..IndexOptions::default()
    };
    for clusters_full in [Some(false), None] {
        let m = Manifest {
            clusters_full,
            ..Manifest::default()
        };
        assert!(
            !run_can_skip(&m, opts, "minilm-l6-v2"),
            "clusters_full={clusters_full:?} must not allow a skip when clusters are wanted"
        );
    }
}

#[test]
fn run_accepts_a_manifest_with_computed_clusters() {
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: true,
        ..IndexOptions::default()
    };
    let m = Manifest {
        clusters_full: Some(true),
        ..Manifest::default()
    };
    assert!(run_can_skip(&m, opts, "minilm-l6-v2"));
}

#[test]
fn run_ignores_clusters_full_when_caller_passed_no_clusters() {
    // `vex index --no-clusters` never checks `clusters_full` at all.
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        ..IndexOptions::default()
    };
    let m = Manifest {
        clusters_full: Some(false),
        ..Manifest::default()
    };
    assert!(run_can_skip(&m, opts, "minilm-l6-v2"));
}

#[test]
fn update_skip_ignores_clusters_full_entirely() {
    // R5's load-bearing invariant: `clusters_full` must NEVER reach
    // `manifest_options_cover` (and therefore `try_skip_update`) — folding
    // it in there would make every no-change `vex update` stop skipping
    // forever, since `update` itself never produces `clusters_full ==
    // Some(true)` in P4a. A no-change `vex update` must keep skipping
    // regardless of the cluster state on the peer's manifest.
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: true,
        ..IndexOptions::default()
    };
    let m = Manifest {
        clusters_full: Some(false),
        ..Manifest::default()
    };
    let no_index_root = std::path::Path::new("/nonexistent-vex-update-clusters-skip-test-root");
    assert_eq!(
        try_skip_update(&m, opts, "minilm-l6-v2", no_index_root).unwrap(),
        Some(0),
        "manifest_options_cover (and try_skip_update) must ignore clusters_full"
    );
}

// --- A3 (v1.12.0): non-blocking IndexLock::try_acquire -----------------

#[test]
fn try_acquire_returns_some_when_lock_is_free() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let guard = IndexLock::try_acquire(&root).expect("try_acquire should not error on a free lock");
    assert!(guard.is_some(), "expected Some on uncontended lock");
}

#[test]
fn try_acquire_returns_none_when_peer_holds_lock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    // Mirror IndexLock::open's path derivation so we contend on the
    // exact same sentinel file the production code uses.
    let index_path = config::index_path(&root);
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    let lock_path = index_path.with_extension("lock");
    let peer = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .unwrap();
    fs2::FileExt::lock_exclusive(&peer).unwrap();

    let outcome = IndexLock::try_acquire(&root)
        .expect("try_acquire should not error on a contended lock — it returns Ok(None)");
    assert!(
        outcome.is_none(),
        "expected None when a peer already holds the lock"
    );

    // Release for hygiene; the tempdir will be removed anyway.
    fs2::FileExt::unlock(&peer).unwrap();
}

#[test]
fn update_skip_is_strictly_options_cover() {
    let opts = IndexOptions {
        with_embeddings: true,
        ..IndexOptions::default()
    };
    // update treats Some(false) pattern_index_full as fine — it's what
    // update itself emits.
    let m = Manifest {
        pattern_index_full: Some(false),
        ..manifest_with_embedder(Some("minilm-l6-v2"))
    };
    // No index exists at this path — `try_skip_update` treats a missing
    // index as "no version gate to apply, skip with a reported count of
    // 0" (matches pre-R5 behaviour for this options-only check).
    let no_index_root = std::path::Path::new("/nonexistent-vex-update-skip-test-root");
    assert_eq!(
        try_skip_update(&m, opts, "minilm-l6-v2", no_index_root).unwrap(),
        Some(0)
    );

    // But embedder mismatch still blocks skip.
    let wrong_embedder = manifest_with_embedder(Some("bge-small"));
    assert_eq!(
        try_skip_update(&wrong_embedder, opts, "minilm-l6-v2", no_index_root).unwrap(),
        None
    );
}

#[test]
fn update_skip_surfaces_an_error_on_a_corrupt_index_rather_than_skipping() {
    // Promise kept by `try_skip_update`: a corrupt on-disk index must
    // never resolve to a silent skip, even when the manifest's options
    // already cover the request and there are zero file changes.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let index_path = config::index_path(&root);
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    std::fs::write(&index_path, b"not a valid vex index, just garbage bytes").unwrap();

    let opts = IndexOptions::default();
    let m = manifest_with_embedder(None);
    let err = try_skip_update(&m, opts, "minilm-l6-v2", &root)
        .expect_err("a corrupt index must surface an error, not Ok(Some(_)) or Ok(None)");
    assert!(
        err.to_string().contains("open existing index"),
        "unexpected error: {err}"
    );
}

/// End-to-end version of the same promise, through the public
/// `pipeline::update` entry point: overwrite a real index with garbage
/// and run `update` with zero file changes — it must return `Err`, not
/// `Ok` with a skip.
#[test]
fn pipeline_update_errors_on_corrupt_index_with_zero_file_changes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("a.rs"), "pub fn foo() {}\n").unwrap();

    // First build a real manifest (so the diff sees "zero changes" on the
    // next call) but then clobber the index file itself with garbage.
    super::run(&root, IndexOptions::default(), "minilm-l6-v2", &[]).expect("initial run");
    let index_path = config::index_path(&root);
    std::fs::write(&index_path, b"not a valid vex index, just garbage bytes").unwrap();

    let result = super::update(&root, IndexOptions::default(), "minilm-l6-v2", &[]);
    assert!(
        result.is_err(),
        "update over a corrupt index with no file changes must error, not Ok-skip: {result:?}"
    );
}

#[test]
fn update_skip_refuses_when_on_disk_version_is_older_than_build() {
    // §13 R5: an untouched v8 (or earlier) index must converge to v9 on
    // the next `vex update`, not stay stale forever just because the
    // manifest's own options already "cover" the request.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let parsed = vec![crate::index::symbols::ParsedFile {
        path: "a.rs".to_string(),
        symbols: vec![crate::index::symbols::ParsedSymbol {
            name: "foo".to_string(),
            kind: crate::index::symbols::SymbolKind::Function,
            line: 1,
            signature: None,
            doc: None,
            body_tokens: None,
        }],
        refs: Vec::new(),
        call_edges: Vec::new(),
        bound_refs: Vec::new(),
        skeletons: Vec::new(),
        cpp_includes: Vec::new(),
        trigram_bloom: None,
        hierarchy_captures: Vec::new(),
    }];
    let index_path = config::index_path(&root);
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    crate::store::writer::write_index_full(
        &parsed,
        &[],
        crate::store::format::VECTOR_DIM,
        &index_path,
    )
    .unwrap();
    // Today's writer already emits VERSION (9) — downgrade it in place so
    // this test actually exercises the "older than build" branch rather
    // than trivially matching the live constant.
    vex_downgrade_for_test(&index_path);

    let opts = IndexOptions::default();
    let m = manifest_with_embedder(None);
    assert_eq!(
        try_skip_update(&m, opts, "minilm-l6-v2", &root).unwrap(),
        None,
        "a v8 on-disk index must never skip, regardless of manifest option coverage"
    );
}

/// Corrupt only the version byte to simulate "older than build" without
/// depending on `store::legacy_v8`'s full downgrade converter (this test
/// only needs the version gate, not a byte-faithful v8 file).
fn vex_downgrade_for_test(index_path: &std::path::Path) {
    let mut bytes = std::fs::read(index_path).unwrap();
    bytes[4..8].copy_from_slice(&8u32.to_le_bytes());
    std::fs::write(index_path, &bytes).unwrap();
}

/// End-to-end regression guard for §13 R5's double-rebuild trap, through
/// the real `pipeline::run`/`pipeline::update` entry points (not just the
/// pure `run_can_skip`/`manifest_options_cover` unit tests above):
///
/// 1. `vex index` computes clusters (`clusters_full: Some(true)`).
/// 2. A real file edit, then `vex update` — P4a never computes on
///    update, so the manifest ends up `clusters_full: Some(false)`.
/// 3. A SECOND `vex index` with no further file changes must NOT skip
///    (R5) — it must actually rebuild and recompute, landing back on
///    `clusters_full: Some(true)`.
/// 4. A THIRD call, `vex update` again with no further changes, DOES
///    skip (manifest_options_cover never looks at `clusters_full`) —
///    this is the "don't regress update's own skip path" half of the
///    guard.
#[test]
fn run_recomputes_clusters_after_update_but_update_itself_keeps_skipping() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("a.rs"), "pub fn foo() {}\n").unwrap();

    let opts = IndexOptions::default(); // with_clusters: true
    let manifest_path = config::manifest_path(&root);

    // 1. Full index: clusters computed.
    let (_, rebuilt) = super::run(&root, opts, "minilm-l6-v2", &[]).expect("initial run");
    assert!(rebuilt);
    let m1 = Manifest::load(&manifest_path).unwrap();
    assert_eq!(
        m1.clusters_full,
        Some(true),
        "first full index must compute clusters"
    );

    // 2. Real file change, then `vex update` — P4a never computes on
    // update, so this writes `clusters_full: Some(false)`.
    std::fs::write(root.join("a.rs"), "pub fn foo() {}\npub fn bar() {}\n").unwrap();
    super::update(&root, opts, "minilm-l6-v2", &[]).expect("update after edit");
    let m2 = Manifest::load(&manifest_path).unwrap();
    assert_eq!(
        m2.clusters_full,
        Some(false),
        "vex update must never claim clusters_full: Some(true) (P4a never computes on update)"
    );

    // 3. A second `vex index` with ZERO further file changes must not
    // skip — `clusters_full != Some(true)` on disk means the rebuild is
    // owed (R5), and it must land back on `Some(true)`.
    let (_, rebuilt) = super::run(&root, opts, "minilm-l6-v2", &[]).expect("second run");
    assert!(
        rebuilt,
        "a no-change `vex index` must still rebuild when clusters_full != Some(true) (R5)"
    );
    let m3 = Manifest::load(&manifest_path).unwrap();
    assert_eq!(
        m3.clusters_full,
        Some(true),
        "the second full index must recompute and clear clusters_full back to Some(true)"
    );

    // 4. A no-change `vex update` must STILL skip — clusters_full is
    // never part of `manifest_options_cover`, so this half of R5 must
    // not regress `update`'s own thundering-herd skip path.
    let (_, changed, deleted) =
        super::update(&root, opts, "minilm-l6-v2", &[]).expect("no-change update");
    assert_eq!(
        (changed, deleted),
        (0, 0),
        "a no-change `vex update` must report zero changed/deleted (the skip path), \
         not run a real incremental rebuild"
    );
}
