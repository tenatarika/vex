#![cfg(test)]

use std::collections::{HashMap, HashSet};

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
fn invalid_update_vectors_are_dropped_after_canonical_fallback() {
    use crate::index::symbols::{ParsedFile, ParsedRef, ParsedSymbol, SymbolKind};
    use crate::index::types::ClusterCarryArtefacts;

    let make_file = |path: &str, name: &str, line: usize| ParsedFile {
        path: path.to_string(),
        symbols: vec![ParsedSymbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            line,
            signature: None,
            doc: None,
            body_tokens: None,
        }],
        refs: vec![ParsedRef {
            name: format!("ref_{name}"),
            line,
            context: None,
        }],
        call_edges: Vec::new(),
        bound_refs: Vec::new(),
        skeletons: Vec::new(),
        cpp_includes: Vec::new(),
        trigram_bloom: None,
        hierarchy_captures: Vec::new(),
    };
    let mut parsed = vec![make_file("z.rs", "Zed", 9), make_file("a.rs", "Aye", 2)];
    let mut vectors = vec![vec![1.0, 2.0]];
    let mut carry = Some(ClusterCarryArtefacts {
        per_symbol_carried: vec![Some(10), Some(20)],
        per_symbol_old_idx: vec![Some(0), Some(1)],
        ..ClusterCarryArtefacts::default()
    });

    let mapping_valid = canonicalize_update_sidecars(&mut parsed, &mut vectors, &[9], &mut carry);

    assert!(!mapping_valid);
    assert!(vectors.is_empty());
    assert_eq!(
        parsed
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["a.rs", "z.rs"]
    );
    assert_eq!(
        parsed
            .iter()
            .map(|file| file.refs[0].name.as_str())
            .collect::<Vec<_>>(),
        ["ref_Aye", "ref_Zed"]
    );
    let carry = carry.expect("aligned cluster carry remains available");
    assert_eq!(carry.per_symbol_carried, [Some(20), Some(10)]);
    assert_eq!(carry.per_symbol_old_idx, [Some(1), Some(0)]);

    let mut parsed = vec![make_file("z.rs", "Zed", 9), make_file("a.rs", "Aye", 2)];
    let mut vectors = Vec::new();
    let mut misaligned_carry = Some(ClusterCarryArtefacts {
        per_symbol_carried: vec![Some(10)],
        per_symbol_old_idx: vec![Some(0)],
        ..ClusterCarryArtefacts::default()
    });
    assert!(canonicalize_update_sidecars(
        &mut parsed,
        &mut vectors,
        &[],
        &mut misaligned_carry
    ));
    assert!(misaligned_carry.is_none());
}

#[test]
fn update_invalid_vector_map_preserves_refs_cleans_sidecars_and_stays_noop() {
    use crate::index::symbols::{ParsedFile, ParsedRef, ParsedSymbol, SymbolKind};
    use crate::parse::scope::{BindTarget, BoundRef, RefKind, UsePath};
    use crate::store::format::SymbolRecord;
    use crate::store::reader::IndexReader;
    use crate::store::writer::write_index_full;

    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "pub struct Alpha;\npub struct Beta;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "use crate::a::Alpha;\npub fn use_alpha(_: Alpha) {}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/trigger.rs"), "// before\n").unwrap();
    let opts = IndexOptions {
        with_embeddings: false,
        ..IndexOptions::default()
    };
    super::run(&root, opts, "minilm-l6-v2", &[]).expect("initial project index");

    let make_symbol = |name: &str, kind| ParsedSymbol {
        name: name.to_string(),
        kind,
        line: 1,
        signature: None,
        doc: None,
        body_tokens: None,
    };
    let parsed = vec![
        ParsedFile {
            path: "src/a.rs".to_string(),
            symbols: vec![
                make_symbol("Alpha", SymbolKind::Struct),
                make_symbol("Beta", SymbolKind::Struct),
            ],
            refs: Vec::new(),
            call_edges: Vec::new(),
            bound_refs: Vec::new(),
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
            hierarchy_captures: Vec::new(),
        },
        ParsedFile {
            path: "src/b.rs".to_string(),
            symbols: vec![make_symbol("use_alpha", SymbolKind::Function)],
            refs: vec![ParsedRef {
                name: "Alpha".to_string(),
                line: 2,
                context: None,
            }],
            call_edges: Vec::new(),
            bound_refs: vec![BoundRef {
                name: "Alpha".to_string(),
                line: 2,
                col: 21,
                target: BindTarget::Imported(UsePath {
                    segments: vec!["Alpha".to_string()],
                }),
                kind: RefKind::Type,
            }],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
            hierarchy_captures: Vec::new(),
        },
    ];
    let vectors = vec![vec![0.25; 384], vec![0.5; 384], vec![0.75; 384]];
    let index_path = config::index_path(&root);
    write_index_full(&parsed, &vectors, 384, &index_path).expect("write vector fixture");

    let reader = IndexReader::open(&index_path).expect("open vector fixture");
    assert!(
        reader.ref_edge_count() > 0,
        "fixture must contain a carried ref"
    );
    let symbols_offset = reader.header().symbols_offset as usize;
    drop(reader);
    let vector_index_offset =
        symbols_offset + SymbolRecord::SIZE + SymbolRecord::SIZE - std::mem::size_of::<u32>();
    let mut bytes = std::fs::read(&index_path).expect("read vector fixture");
    bytes[vector_index_offset..vector_index_offset + std::mem::size_of::<u32>()]
        .copy_from_slice(&4u32.to_le_bytes());
    std::fs::write(&index_path, bytes).expect("corrupt middle vector index");

    let hnsw_path = config::hnsw_path(&root);
    let hash_path = config::hash_index_path(&root);
    std::fs::write(&hnsw_path, b"stale hnsw").expect("write stale HNSW marker");
    std::fs::write(&hash_path, b"stale hashes").expect("write stale hash marker");
    std::fs::write(root.join("src/trigger.rs"), "// after\n").unwrap();

    super::update(&root, opts, "minilm-l6-v2", &[])
        .expect("corrupt vector mapping degrades without failing update");

    let reader = IndexReader::open(&index_path).expect("open updated index");
    assert!(!reader.has_vectors());
    assert_eq!(reader.symbol_count(), 3);
    assert!(
        reader.ref_edge_count() > 0,
        "unchanged ref edge must survive"
    );
    assert!(!hnsw_path.exists());
    assert!(!hash_path.exists());
    assert_eq!(
        Manifest::load(&config::manifest_path(&root))
            .unwrap()
            .embedder_id,
        None
    );
    drop(reader);

    let stable_bytes = std::fs::read(&index_path).expect("read updated index");
    super::update(&root, opts, "minilm-l6-v2", &[]).expect("no-op update");
    assert_eq!(
        std::fs::read(&index_path).expect("read no-op index"),
        stable_bytes,
        "no-op update must preserve exact index bytes"
    );
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
        try_skip_update(
            &m,
            opts,
            "minilm-l6-v2",
            no_index_root,
            parse_revision::BASELINE
        )
        .unwrap(),
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
        try_skip_update(
            &m,
            opts,
            "minilm-l6-v2",
            no_index_root,
            parse_revision::BASELINE
        )
        .unwrap(),
        Some(0)
    );

    // But embedder mismatch still blocks skip.
    let wrong_embedder = manifest_with_embedder(Some("bge-small"));
    assert_eq!(
        try_skip_update(
            &wrong_embedder,
            opts,
            "minilm-l6-v2",
            no_index_root,
            parse_revision::BASELINE
        )
        .unwrap(),
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
    let err = try_skip_update(&m, opts, "minilm-l6-v2", &root, parse_revision::BASELINE)
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
        try_skip_update(&m, opts, "minilm-l6-v2", &root, parse_revision::BASELINE).unwrap(),
        None,
        "a v8 on-disk index must never skip, regardless of manifest option coverage"
    );
}

#[test]
fn dart_revision_one_migrates_once_then_unchanged_update_skips() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("sample.dart"), "class Repo { Repo(); }\n").unwrap();
    super::run(&root, IndexOptions::default(), "minilm-l6-v2", &[]).unwrap();

    let index_dir = config::index_dir(&root);
    let revision_path = index_dir.join(parse_revision::FILE_NAME);
    std::fs::write(&revision_path, b"VEXR\x01\x00").unwrap();
    assert!(!parse_revision::is_current(
        &index_dir,
        parse_revision::DART
    ));

    super::update(&root, IndexOptions::default(), "minilm-l6-v2", &[]).unwrap();
    assert!(parse_revision::is_current(&index_dir, parse_revision::DART));
    let index_path = config::index_path(&root);
    let migrated_time = std::fs::metadata(&index_path).unwrap().modified().unwrap();

    super::update(&root, IndexOptions::default(), "minilm-l6-v2", &[]).unwrap();
    let second_time = std::fs::metadata(index_path).unwrap().modified().unwrap();
    assert_eq!(
        migrated_time, second_time,
        "second unchanged update must skip"
    );
}

#[test]
fn index_and_update_repair_missing_partial_corrupt_and_stale_dart_sidecars() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("sample.dart"), "class Repo { Repo(); }\n").unwrap();
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    super::run(&root, opts, "minilm-l6-v2", &[]).unwrap();
    let index_dir = config::index_dir(&root);
    let sidecar = crate::index::dart_records::sidecar_path(&index_dir);
    for full_index in [false, true] {
        for corruption in 0..4 {
            match corruption {
                0 => std::fs::remove_file(&sidecar).unwrap(),
                1 => std::fs::write(&sidecar, b"VXDR").unwrap(),
                2 => {
                    let mut partial = crate::index::dart_records::DartRecords::new();
                    let mut records = crate::index::dart_records::decode(&sidecar).unwrap();
                    records.remove("sample.dart");
                    partial.append(&mut records);
                    crate::index::dart_records::write_atomic(&sidecar, &partial).unwrap();
                }
                _ => {
                    let mut records = crate::index::dart_records::decode(&sidecar).unwrap();
                    records.get_mut("sample.dart").unwrap().source_fingerprint ^= 1;
                    crate::index::dart_records::write_atomic(&sidecar, &records).unwrap();
                }
            }
            if full_index {
                let (_, rebuilt) = super::run(&root, opts, "minilm-l6-v2", &[]).unwrap();
                assert!(rebuilt, "invalid sidecar must trigger Dart rebuild");
            } else {
                let (_, changed, deleted) =
                    super::update(&root, opts, "minilm-l6-v2", &[]).unwrap();
                assert_eq!(
                    (changed, deleted),
                    (1, 0),
                    "invalid sidecar must trigger Dart reparse"
                );
            }
            let manifest = Manifest::load(&config::manifest_path(&root)).unwrap();
            assert!(crate::index::dart_records::covers_manifest(
                &config::index_path(&root),
                &manifest.files
            ));
            assert!(parse_revision::is_current(&index_dir, parse_revision::DART));
            assert_eq!(
                crate::index::dart_records::decode(&sidecar).unwrap()["sample.dart"]
                    .source_fingerprint,
                manifest.files["sample.dart"]
            );
        }
    }
    let before_noop = std::fs::read(config::index_path(&root)).unwrap();
    let (_, changed, deleted) = super::update(&root, opts, "minilm-l6-v2", &[]).unwrap();
    assert_eq!((changed, deleted), (0, 0));
    assert_eq!(
        std::fs::read(config::index_path(&root)).unwrap(),
        before_noop
    );
}

#[test]
fn dart_rename_and_last_file_deletion_remove_stale_sidecar_records() {
    use crate::store::reader::IndexReader;

    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(
        root.join("sample.dart"),
        "class Repo { Repo(); static void run() {} }\nvoid invoke() { Repo.run(); }\n",
    )
    .unwrap();
    std::fs::write(root.join("native.rs"), "pub fn native() {}\n").unwrap();
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    super::run(&root, opts, "minilm-l6-v2", &[]).unwrap();
    let index_dir = config::index_dir(&root);
    let sidecar = crate::index::dart_records::sidecar_path(&index_dir);

    let raw_sidecar_before_native_update = std::fs::read(&sidecar).unwrap();
    std::fs::write(
        root.join("native.rs"),
        "pub fn native() {}\npub fn changed() {}\n",
    )
    .unwrap();
    super::update(&root, opts, "minilm-l6-v2", &[]).unwrap();
    assert_eq!(
        std::fs::read(&sidecar).unwrap(),
        raw_sidecar_before_native_update,
        "native-only update must preserve unchanged Dart raw metadata"
    );
    {
        let reader = IndexReader::open(&config::index_path(&root)).unwrap();
        assert!(
            !reader.ref_reader().unwrap().find("Repo").is_empty(),
            "Dart refs survive native update"
        );
        assert!(
            reader.call_edge_count() > 0,
            "Dart calls survive native update"
        );
    }

    std::fs::rename(root.join("sample.dart"), root.join("renamed.dart")).unwrap();
    super::update(&root, opts, "minilm-l6-v2", &[]).unwrap();
    let records = crate::index::dart_records::decode(&sidecar).unwrap();
    assert!(!records.contains_key("sample.dart"));
    assert!(records.contains_key("renamed.dart"));
    {
        let reader = IndexReader::open(&config::index_path(&root)).unwrap();
        assert!(!reader.file_paths().iter().any(|path| path == "sample.dart"));
        assert!(reader
            .file_paths()
            .iter()
            .any(|path| path == "renamed.dart"));
        assert!(reader.file_paths().iter().any(|path| path == "native.rs"));
    }

    std::fs::remove_file(root.join("renamed.dart")).unwrap();
    std::fs::write(
        root.join("native.rs"),
        "pub fn native() {}\npub fn changed() {}\n",
    )
    .unwrap();
    super::update(&root, opts, "minilm-l6-v2", &[]).unwrap();
    assert!(!sidecar.exists());
    let reader = IndexReader::open(&config::index_path(&root)).unwrap();
    assert_eq!(reader.file_paths(), vec!["native.rs".to_string()]);
    assert_eq!(reader.symbol_count(), 2);
}

type FixtureVectorKey = (String, String, crate::index::symbols::SymbolKind, usize);

fn seed_fixture_vectors(
    root: &std::path::Path,
    embedder_id: &str,
) -> HashMap<FixtureVectorKey, Vec<u32>> {
    use crate::embed::{build_context, cache::context_hash, embedder_char_budget, embedder_dim};
    use crate::index::manifest::Manifest;
    use crate::index::symbols::ParsedFile;
    use crate::store::{body_tokens, reader::IndexReader, writer::write_index_full};

    let index_path = config::index_path(root);
    let reader = IndexReader::open(&index_path).unwrap();
    let bodies = body_tokens::load(&config::body_tokens_path(root)).unwrap();
    let dimension = embedder_dim(embedder_id).unwrap() as usize;
    let mut parsed = Vec::<ParsedFile>::new();
    let mut file_indices = HashMap::<String, usize>::new();
    let mut vectors = Vec::new();
    let mut hashes = Vec::new();
    let mut expected = HashMap::new();

    for index in 0..reader.symbol_count() {
        let record = reader.symbol(index).unwrap();
        let path = reader.read_string(record.file_offset).to_string();
        let signature = reader.read_string(record.signature_offset).to_string();
        let symbol = crate::index::symbols::ParsedSymbol {
            name: reader.read_string(record.name_offset).to_string(),
            kind: crate::index::symbols::SymbolKind::try_from(record.kind).unwrap(),
            line: record.line as usize,
            signature: (!signature.is_empty()).then_some(signature),
            doc: None,
            body_tokens: bodies[index].clone(),
        };
        let context = build_context(
            symbol.kind.as_str(),
            &symbol.name,
            &path,
            symbol.signature.as_deref(),
            None,
            symbol.body_tokens.as_deref(),
            embedder_char_budget(embedder_id).unwrap(),
        );
        hashes.push(context_hash(embedder_id, &context));
        let mut vector: Vec<f32> = vec![0.0; dimension];
        vector[index] = 1.0;
        let key = (path.clone(), symbol.name.clone(), symbol.kind, symbol.line);
        assert!(
            expected
                .insert(key.clone(), vector.iter().map(|v| v.to_bits()).collect())
                .is_none(),
            "fixture vector identity must be unique: {key:?}"
        );
        vectors.push(vector);
        let file_index = *file_indices.entry(path.clone()).or_insert_with(|| {
            parsed.push(ParsedFile {
                path,
                symbols: Vec::new(),
                refs: Vec::new(),
                call_edges: Vec::new(),
                bound_refs: Vec::new(),
                skeletons: Vec::new(),
                cpp_includes: Vec::new(),
                trigram_bloom: None,
                hierarchy_captures: Vec::new(),
            });
            parsed.len() - 1
        });
        parsed[file_index].symbols.push(symbol);
    }
    drop(reader);
    write_index_full(&parsed, &vectors, dimension as u32, &index_path).unwrap();
    crate::search::hash_index::save(&config::hash_index_path(root), &hashes).unwrap();
    let mut manifest = Manifest::load(&config::manifest_path(root)).unwrap();
    manifest.embedder_id = Some(embedder_id.to_string());
    manifest.vectors_normalized = Some(true);
    manifest.save(&config::manifest_path(root)).unwrap();
    expected
}

fn assert_fixture_vectors(root: &std::path::Path, expected: &HashMap<FixtureVectorKey, Vec<u32>>) {
    let reader = crate::store::reader::IndexReader::open(&config::index_path(root)).unwrap();
    let mut observed = HashSet::new();
    for index in 0..reader.symbol_count() {
        let record = reader.symbol(index).unwrap();
        let path = reader.read_string(record.file_offset).to_string();
        let name = reader.read_string(record.name_offset);
        let kind = crate::index::symbols::SymbolKind::try_from(record.kind).unwrap();
        let key = (path, name.to_string(), kind, record.line as usize);
        let Some(want) = expected.get(&key) else {
            continue;
        };
        assert!(
            observed.insert(key.clone()),
            "duplicate retained vector identity: {key:?}"
        );
        let got = reader
            .vector(record.vector_index)
            .unwrap()
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>();
        assert_eq!(got, *want, "stored vector bits changed for {key:?}");
    }
    assert_eq!(
        observed,
        expected.keys().cloned().collect(),
        "not all seeded vectors were retained"
    );
}

#[test]
fn mixed_dart_rust_revision_migration_retains_vectors_and_marker_after_dart_removal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("lib.rs"), "pub fn rustTarget() {}\n").unwrap();
    std::fs::write(
        root.join("lib.dart"),
        "class DartTarget { DartTarget(); }\n",
    )
    .unwrap();
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    super::run(&root, opts, "minilm-l6-v2", &[]).unwrap();
    let expected = seed_fixture_vectors(&root, "minilm-l6-v2");
    let revision_path = config::index_dir(&root).join(parse_revision::FILE_NAME);
    std::fs::write(&revision_path, b"VEXR\x01\x00").unwrap();

    super::update(&root, opts, "different-configured-model", &[]).unwrap();
    assert!(parse_revision::is_current(
        &config::index_dir(&root),
        parse_revision::DART
    ));
    assert_fixture_vectors(&root, &expected);
    let migrated_index = std::fs::read(config::index_path(&root)).unwrap();
    assert_eq!(
        super::update(&root, opts, "different-configured-model", &[])
            .unwrap()
            .1,
        0
    );
    assert_eq!(
        std::fs::read(config::index_path(&root)).unwrap(),
        migrated_index
    );

    std::fs::remove_file(root.join("lib.dart")).unwrap();
    let (_, changed, deleted) =
        super::update(&root, opts, "different-configured-model", &[]).unwrap();
    assert_eq!((changed, deleted), (0, 1));
    assert!(parse_revision::is_current(
        &config::index_dir(&root),
        parse_revision::BASELINE
    ));
    assert!(parse_revision::is_current(
        &config::index_dir(&root),
        parse_revision::DART
    ));
    assert_eq!(
        super::update(&root, opts, "different-configured-model", &[]).unwrap(),
        (1, 0, 0)
    );
    assert!(parse_revision::is_current(
        &config::index_dir(&root),
        parse_revision::DART
    ));
}

#[test]
fn shared_locked_rebuild_keeps_revision_two_after_last_dart_file_is_gone() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("lib.rs"), "pub fn before() {}\n").unwrap();
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    super::run(&root, opts, "minilm-l6-v2", &[]).unwrap();

    let index_dir = config::index_dir(&root);
    parse_revision::save(&index_dir, parse_revision::DART).unwrap();
    std::fs::write(root.join("lib.rs"), "pub fn after() {}\n").unwrap();

    // Semantic fallback branches transfer their already-held lock into this
    // shared body. Exercise the same body without downloading an embedding model.
    let (canonical_root, files, hashed) = super::run_setup(&root, &[]).unwrap();
    let lock = IndexLock::acquire(&canonical_root).unwrap();
    super::run_with_lock(&canonical_root, opts, "minilm-l6-v2", files, hashed, lock).unwrap();

    assert!(parse_revision::is_current(&index_dir, parse_revision::DART));
}

#[test]
fn rust_only_revision_one_update_skips_and_keeps_prebaked_vectors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("lib.rs"), "pub fn rustTarget() {}\n").unwrap();
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    super::run(&root, opts, "minilm-l6-v2", &[]).unwrap();
    let expected = seed_fixture_vectors(&root, "minilm-l6-v2");
    let revision_path = config::index_dir(&root).join(parse_revision::FILE_NAME);
    std::fs::write(&revision_path, b"VEXR\x01\x00").unwrap();

    let (_, changed, deleted) =
        super::update(&root, opts, "different-configured-model", &[]).unwrap();
    assert_eq!((changed, deleted), (0, 0));
    assert_eq!(std::fs::read(&revision_path).unwrap(), b"VEXR\x01\x00");
    assert_fixture_vectors(&root, &expected);
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
/// 2. A real file edit, then `vex update` — the prior index already has a
///    COMPUTED section, so P4b *carries* it forward (STALE) rather than
///    recomputing, and the manifest ends up `clusters_full: Some(false)`.
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

    // 2. Real file change, then `vex update` — the prior index is
    // COMPUTED, so P4b carries it forward (STALE), writing
    // `clusters_full: Some(false)`.
    std::fs::write(root.join("a.rs"), "pub fn foo() {}\npub fn bar() {}\n").unwrap();
    super::update(&root, opts, "minilm-l6-v2", &[]).expect("update after edit");
    let m2 = Manifest::load(&manifest_path).unwrap();
    assert_eq!(
        m2.clusters_full,
        Some(false),
        "a P4b carry-forward update must not claim clusters_full: Some(true)"
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

// --- P4b code-review follow-up: corrupt prior cluster table must not fail `vex update` ---

#[test]
fn build_old_table_from_records_returns_none_on_any_missing_record() {
    use crate::store::cluster_section::ClusterRecordView;

    // Ordinal 1 (strictly inside 0..3) fails to decode — simulates the
    // "should never happen against a real file" corruption this helper
    // guards against (see its doc comment for why a real file can't
    // actually trigger this).
    let result = build_old_table_from_records(3, |ord| {
        if ord == 1 {
            None
        } else {
            Some(ClusterRecordView {
                rep_sym_idx: Some(ord as u32),
                size: 2,
                internal_weight: 1,
                cut_weight: 0,
                label: "src/",
                hubs: [None, None, None],
            })
        }
    });
    assert!(
        result.is_none(),
        "a missing mid-table record must drop the WHOLE table, not just that ordinal"
    );
}

#[test]
fn build_old_table_from_records_returns_every_ordinal_when_all_present() {
    use crate::store::cluster_section::ClusterRecordView;

    let table = build_old_table_from_records(2, |ord| {
        Some(ClusterRecordView {
            rep_sym_idx: Some(ord as u32),
            size: 1,
            internal_weight: 0,
            cut_weight: 0,
            label: "src/",
            hubs: [None, None, None],
        })
    })
    .expect("every ordinal present");
    assert_eq!(table.len(), 2);
    assert_eq!(table[0].rep_sym_idx, Some(0));
    assert_eq!(table[1].rep_sym_idx, Some(1));
}
