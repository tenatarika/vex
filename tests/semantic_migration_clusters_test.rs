use std::collections::HashSet;

use vex::embed::cache::{context_hash, EmbedCache};
use vex::embed::{build_context, MINILM_CHAR_BUDGET, MINILM_DIM, MINILM_ID};
use vex::index::pipeline::{self, IndexOptions};
use vex::parse::{language::Language, parse_file};
use vex::store::cluster_section::{ClusterStatus, ClusterSummary};
use vex::store::reader::IndexReader;
use vex::util::config;

#[test]
fn migration_recomputes_default_clusters_once_after_vector_prefix_reorder() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let cache_dir = root.join(".vex_cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    vex::util::config::set_cache_override(cache_dir.clone(), true);
    std::fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();

    let sources = [
        (
            "src/mod_a.rs",
            "/// persisted doc\npub fn f0() -> i32 { f1() + f2() }\n\
             pub fn f1() -> i32 { f2() }\n\
             pub fn f2() -> i32 { f3() }\n\
             pub fn f3() -> i32 { f0() }\n",
        ),
        (
            "src/mod_b.rs",
            "pub fn g0() -> i32 { g1() + g2() }\n\
             pub fn g1() -> i32 { g2() }\n\
             pub fn g2() -> i32 { g3() }\n\
             pub fn g3() -> i32 { g0() }\n",
        ),
        ("src/lib.rs", "pub mod mod_a;\npub mod mod_b;\n"),
    ];
    for (path, source) in sources {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
    }

    let mut cache = EmbedCache::empty(MINILM_ID, MINILM_DIM);
    let mut expected_vectors = std::collections::HashMap::new();
    let mut axis = 0;
    for (path, source) in sources {
        let parsed = parse_file(path, source, Language::Rust).unwrap();
        for symbol in parsed.symbols {
            let context = build_context(
                symbol.kind.as_str(),
                &symbol.name,
                path,
                symbol.signature.as_deref(),
                symbol.doc.as_deref(),
                symbol.body_tokens.as_deref(),
                MINILM_CHAR_BUDGET,
            );
            let mut vector = vec![0.0; MINILM_DIM as usize];
            vector[axis] = 1.0;
            axis += 1;
            assert!(cache.insert(context_hash(MINILM_ID, &context), vector.clone()));
            expected_vectors.insert(symbol.name, vector);
        }
    }
    assert_eq!(axis, 8);
    cache
        .save(&config::embed_cache_path(&root, MINILM_ID))
        .unwrap();
    pipeline::run(
        &root,
        IndexOptions {
            with_embeddings: true,
            ..IndexOptions::default()
        },
        MINILM_ID,
        &[],
    )
    .unwrap();

    let index_path = config::index_path(&root);
    let before = IndexReader::open(&index_path).unwrap();
    assert_eq!(before.symbol_count(), 8);
    assert_eq!(before.cluster_section_reader().unwrap().summary().k, 2);
    drop(before);
    std::fs::remove_file(config::index_dir(&root).join("index.parse-revision")).unwrap();

    let opts = IndexOptions::default();
    pipeline::update(&root, opts, "different-configured-model", &[]).unwrap();

    let migrated_bytes = std::fs::read(&index_path).unwrap();
    let migrated = IndexReader::open(&index_path).unwrap();
    assert_eq!(migrated.file_paths()[0], "src/mod_b.rs");
    assert_eq!(migrated.header().vector_dim, MINILM_DIM);
    assert_eq!(migrated.symbol_count(), 8);
    let vectors = (0..migrated.symbol_count())
        .map(|index| {
            let symbol = migrated.symbol(index).unwrap();
            (
                migrated.read_string(symbol.name_offset).to_string(),
                migrated.vector(symbol.vector_index).map(<[f32]>::to_vec),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    let retained = ["g0", "g1", "g2", "g3"];
    let dropped = ["f0", "f1", "f2", "f3"];
    assert!(retained.iter().all(|name| {
        vectors[*name]
            .as_ref()
            .is_some_and(|actual| actual == &expected_vectors[*name])
    }));
    assert!(dropped.iter().all(|name| vectors[*name].is_none()));
    assert!(!config::hnsw_path(&root).exists());
    assert!(!config::hash_index_path(&root).exists());

    let cluster = migrated.cluster_section_reader().unwrap();
    let summary = cluster.summary();
    assert_eq!(summary.k, 2);
    assert!(
        !summary.stale,
        "migration recomputes clusters for the reordered symbols"
    );
    let status = |name: &str| {
        let index = (0..migrated.symbol_count())
            .find(|&index| {
                let symbol = migrated.symbol(index).unwrap();
                migrated.read_string(symbol.name_offset) == name
            })
            .unwrap();
        cluster.status(index.try_into().unwrap())
    };
    let group_a = [status("f0"), status("f1"), status("f2"), status("f3")];
    let group_b = [status("g0"), status("g1"), status("g2"), status("g3")];
    let ClusterStatus::Clustered(group_a_id) = group_a[0] else {
        panic!("mod_a clique must be clustered: {group_a:?}");
    };
    let ClusterStatus::Clustered(group_b_id) = group_b[0] else {
        panic!("mod_b clique must be clustered: {group_b:?}");
    };
    assert!(group_a
        .iter()
        .all(|status| *status == ClusterStatus::Clustered(group_a_id)));
    assert!(group_b
        .iter()
        .all(|status| *status == ClusterStatus::Clustered(group_b_id)));
    assert_ne!(group_a_id, group_b_id);

    drop(migrated);
    pipeline::update(&root, opts, "different-configured-model", &[]).unwrap();
    assert_eq!(
        std::fs::read(&index_path).unwrap(),
        migrated_bytes,
        "the next default no-op update must not recompute or rewrite clusters"
    );
    let after_noop = IndexReader::open(&index_path).unwrap();
    let after_summary: ClusterSummary = after_noop.cluster_section_reader().unwrap().summary();
    assert_eq!(after_summary.k, 2);
    assert!(!after_summary.stale);

    let mut actual = HashSet::new();
    for index in 0..after_noop.symbol_count() {
        let symbol = after_noop.symbol(index).unwrap();
        if let Some(vector) = after_noop.vector(symbol.vector_index) {
            actual.insert(after_noop.read_string(symbol.name_offset).to_string());
            assert_eq!(vector.len(), MINILM_DIM as usize);
        }
    }
    assert_eq!(actual, HashSet::from(retained.map(str::to_string)));
}
