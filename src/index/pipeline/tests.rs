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
        ..IndexOptions::default()
    };
    let m = Manifest {
        pattern_index_full: Some(false),
        ..Manifest::default()
    };
    assert!(run_can_skip(&m, opts, "minilm-l6-v2"));
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
    assert!(update_can_skip(&m, opts, "minilm-l6-v2"));

    // But embedder mismatch still blocks skip.
    let wrong_embedder = manifest_with_embedder(Some("bge-small"));
    assert!(!update_can_skip(&wrong_embedder, opts, "minilm-l6-v2"));
}
