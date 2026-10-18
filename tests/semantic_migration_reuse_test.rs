use std::path::Path;

use vex::embed::{build_context, cache::context_hash, embedder_char_budget, embedder_dim};
use vex::index::{
    manifest::Manifest,
    pipeline::{self, IndexOptions},
};
use vex::store::{body_tokens, reader::IndexReader, writer::write_index_full};
use vex::util::config;

fn legacy_embedded_index(root: &Path, embedder_id: &str, dimension: u32) -> Vec<f32> {
    legacy_embedded_index_with(
        root,
        embedder_id,
        dimension,
        "pub fn reuse_target() { let value = 7; }\n",
        false,
        None,
        false,
    )
}

fn legacy_embedded_index_with(
    root: &Path,
    embedder_id: &str,
    dimension: u32,
    source: &str,
    duplicate_old_identity: bool,
    context_doc: Option<&str>,
    nonfinite_vector: bool,
) -> Vec<f32> {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join("sample.rs"), source).unwrap();
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    pipeline::run(root, opts, embedder_id, &[]).unwrap();

    let index_path = config::index_path(root);
    let reader = IndexReader::open(&index_path).unwrap();
    assert_eq!(
        reader.symbol_count(),
        if duplicate_old_identity { 2 } else { 1 }
    );
    let record = reader.symbol(0).unwrap();
    let symbol = vex::index::symbols::ParsedSymbol {
        name: reader.read_string(record.name_offset).to_string(),
        kind: vex::index::symbols::SymbolKind::try_from(record.kind).unwrap(),
        line: record.line as usize,
        signature: {
            let value = reader.read_string(record.signature_offset);
            (!value.is_empty()).then(|| value.to_string())
        },
        doc: None,
        body_tokens: body_tokens::load(&config::body_tokens_path(root)).unwrap()[0].clone(),
    };
    drop(reader);
    let context = build_context(
        symbol.kind.as_str(),
        &symbol.name,
        "sample.rs",
        symbol.signature.as_deref(),
        context_doc,
        symbol.body_tokens.as_deref(),
        embedder_char_budget(embedder_id).unwrap(),
    );
    let mut vector = (0..dimension)
        .map(|i| match i {
            0 => 1.0,
            1 => -0.0,
            _ => 0.0,
        })
        .collect::<Vec<_>>();
    if nonfinite_vector {
        vector[0] = f32::NAN;
    }
    let symbols = if duplicate_old_identity {
        vec![symbol.clone(), symbol]
    } else {
        vec![symbol]
    };
    let vectors = vec![vector.clone(); symbols.len()];
    write_index_full(
        &[vex::index::symbols::ParsedFile {
            path: "sample.rs".to_string(),
            symbols,
            refs: Vec::new(),
            call_edges: Vec::new(),
            bound_refs: Vec::new(),
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
            hierarchy_captures: Vec::new(),
        }],
        &vectors,
        dimension,
        &index_path,
    )
    .unwrap();
    vex::search::hash_index::save(
        &config::hash_index_path(root),
        &vec![context_hash(embedder_id, &context); vectors.len()],
    )
    .unwrap();
    let mut manifest = Manifest::load(&config::manifest_path(root)).unwrap();
    manifest.embedder_id = Some(embedder_id.to_string());
    manifest.vectors_normalized = Some(true);
    manifest.save(&config::manifest_path(root)).unwrap();
    std::fs::remove_file(config::index_dir(root).join("index.parse-revision")).unwrap();
    vector
}

fn legacy_embedded_multi_file_index(
    root: &Path,
    embedder_id: &str,
    files: &[(&str, &str, Option<&str>)],
    nonfinite_file: Option<&str>,
) -> std::collections::HashMap<String, Vec<f32>> {
    std::fs::create_dir_all(root).unwrap();
    for (path, source, _) in files {
        std::fs::write(root.join(path), source).unwrap();
    }
    pipeline::run(
        root,
        IndexOptions {
            with_embeddings: false,
            with_clusters: false,
            with_pattern_index: false,
            ..IndexOptions::default()
        },
        embedder_id,
        &[],
    )
    .unwrap();

    let index_path = config::index_path(root);
    let reader = IndexReader::open(&index_path).unwrap();
    let bodies = body_tokens::load(&config::body_tokens_path(root)).unwrap();
    let contexts = files
        .iter()
        .filter_map(|(path, _, doc)| doc.map(|doc| ((*path).to_string(), doc)))
        .collect::<std::collections::HashMap<_, _>>();
    let mut parsed_files = Vec::<vex::index::symbols::ParsedFile>::new();
    let mut file_indices = std::collections::HashMap::<String, usize>::new();
    let mut vectors = Vec::new();
    let mut hashes = Vec::new();
    let mut expected = std::collections::HashMap::new();
    let dim = embedder_dim(embedder_id).unwrap();
    for index in 0..reader.symbol_count() {
        let record = reader.symbol(index).unwrap();
        let path = reader.read_string(record.file_offset).to_string();
        let signature = reader.read_string(record.signature_offset).to_string();
        let symbol = vex::index::symbols::ParsedSymbol {
            name: reader.read_string(record.name_offset).to_string(),
            kind: vex::index::symbols::SymbolKind::try_from(record.kind).unwrap(),
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
            contexts.get(&path).copied(),
            symbol.body_tokens.as_deref(),
            embedder_char_budget(embedder_id).unwrap(),
        );
        hashes.push(context_hash(embedder_id, &context));
        let mut vector = vec![0.0; dim as usize];
        vector[index] = 1.0;
        if nonfinite_file == Some(path.as_str()) {
            vector[0] = f32::NAN;
        }
        expected.insert(symbol.name.clone(), vector.clone());
        vectors.push(vector);
        let file_index = *file_indices.entry(path.clone()).or_insert_with(|| {
            parsed_files.push(vex::index::symbols::ParsedFile {
                path: path.clone(),
                symbols: Vec::new(),
                refs: Vec::new(),
                call_edges: Vec::new(),
                bound_refs: Vec::new(),
                skeletons: Vec::new(),
                cpp_includes: Vec::new(),
                trigram_bloom: None,
                hierarchy_captures: Vec::new(),
            });
            parsed_files.len() - 1
        });
        parsed_files[file_index].symbols.push(symbol);
    }
    drop(reader);
    write_index_full(&parsed_files, &vectors, dim, &index_path).unwrap();
    vex::search::hash_index::save(&config::hash_index_path(root), &hashes).unwrap();
    let mut manifest = Manifest::load(&config::manifest_path(root)).unwrap();
    manifest.embedder_id = Some(embedder_id.to_string());
    manifest.vectors_normalized = Some(true);
    manifest.save(&config::manifest_path(root)).unwrap();
    std::fs::remove_file(config::index_dir(root).join("index.parse-revision")).unwrap();
    expected
}

fn assert_readable_without_reused_vectors(root: &Path, expected_symbols: usize) {
    let reader = IndexReader::open(&config::index_path(root)).unwrap();
    assert_eq!(reader.symbol_count(), expected_symbols);
    for index in 0..reader.symbol_count() {
        assert!(reader
            .vector(reader.symbol(index).unwrap().vector_index)
            .is_none());
    }
    assert_eq!(
        Manifest::load(&config::manifest_path(root))
            .unwrap()
            .embedder_id,
        None,
        "refusal must not label structural output with the old model"
    );
}

#[test]
fn migration_reuses_matching_prebaked_vectors() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let id = "minilm-l6-v2";
    let expected = legacy_embedded_index(&root, id, embedder_dim(id).unwrap());

    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    pipeline::update(&root, opts, "different-configured-model", &[]).unwrap();

    let reader = IndexReader::open(&config::index_path(&root)).unwrap();
    let actual = reader
        .vector(reader.symbol(0).unwrap().vector_index)
        .unwrap();
    assert_eq!(
        actual
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        "retained vector bits must survive migration"
    );
    assert_eq!(reader.header().vector_dim, expected.len() as u32);
    assert_eq!(
        Manifest::load(&config::manifest_path(&root))
            .unwrap()
            .embedder_id
            .as_deref(),
        Some(id),
        "migration must keep the original model identity"
    );
}

#[test]
fn migration_with_new_file_keeps_partial_vectors_and_skips_unaligned_hnsw() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let id = "minilm-l6-v2";
    let expected = legacy_embedded_index(&root, id, embedder_dim(id).unwrap());
    let existing_hashes = vex::search::hash_index::load(&config::hash_index_path(&root)).unwrap();
    pipeline::build_hnsw_at(
        &config::hnsw_path(&root),
        &config::hash_index_path(&root),
        std::slice::from_ref(&expected),
        &existing_hashes,
    )
    .unwrap();
    assert!(config::hnsw_path(&root).exists());
    assert!(config::hash_index_path(&root).exists());
    std::fs::write(root.join("new.rs"), "pub fn new_unembedded() {}\n").unwrap();

    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    pipeline::update(&root, opts, "different-configured-model", &[]).unwrap();

    let reader = IndexReader::open(&config::index_path(&root)).unwrap();
    assert_eq!(reader.symbol_count(), 2);
    assert_eq!(reader.header().vector_dim, expected.len() as u32);
    let symbols = (0..reader.symbol_count())
        .map(|i| {
            let record = reader.symbol(i).unwrap();
            let name = reader.read_string(record.name_offset).to_string();
            let vector = reader.vector(record.vector_index).map(<[f32]>::to_vec);
            (name, vector)
        })
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        symbols["reuse_target"]
            .as_ref()
            .unwrap()
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>()
    );
    assert!(symbols["new_unembedded"].is_none());
    assert_eq!(
        Manifest::load(&config::manifest_path(&root))
            .unwrap()
            .embedder_id
            .as_deref(),
        Some(id)
    );
    assert!(
        !config::hnsw_path(&root).exists(),
        "partial vectors must not leave an HNSW index with misaligned symbol hashes"
    );
    assert!(!config::hash_index_path(&root).exists());

    pipeline::update(&root, opts, "different-configured-model", &[]).unwrap();
    let reader = IndexReader::open(&config::index_path(&root)).unwrap();
    assert_eq!(reader.symbol_count(), 2);
    assert_eq!(
        Manifest::load(&config::manifest_path(&root))
            .unwrap()
            .embedder_id
            .as_deref(),
        Some(id)
    );
}

#[test]
fn migration_does_not_reuse_changed_context() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let id = "minilm-l6-v2";
    legacy_embedded_index(&root, id, embedder_dim(id).unwrap());
    std::fs::write(
        root.join("sample.rs"),
        "pub fn reuse_target() { let value = 8; }\n",
    )
    .unwrap();

    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    pipeline::update(&root, opts, id, &[]).unwrap();

    let reader = IndexReader::open(&config::index_path(&root)).unwrap();
    assert_eq!(
        reader.symbol_count(),
        1,
        "updated structural index remains readable"
    );
    assert!(reader
        .vector(reader.symbol(0).unwrap().vector_index)
        .is_none());
    assert_eq!(
        Manifest::load(&config::manifest_path(&root))
            .unwrap()
            .embedder_id,
        None
    );
}

#[test]
fn migration_rejects_foreign_model_or_wrong_dimension_safely() {
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    for (foreign_model, dimension, corrupt_hashes) in [
        (true, embedder_dim("minilm-l6-v2").unwrap(), false),
        (false, 8, false),
        (false, embedder_dim("minilm-l6-v2").unwrap(), true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let id = "minilm-l6-v2";
        legacy_embedded_index(&root, id, dimension);
        if foreign_model {
            let mut manifest = Manifest::load(&config::manifest_path(&root)).unwrap();
            manifest.embedder_id = Some("unknown-foreign-model".to_string());
            manifest.save(&config::manifest_path(&root)).unwrap();
        }
        if corrupt_hashes {
            std::fs::write(config::hash_index_path(&root), b"corrupt-sidecar").unwrap();
        }

        pipeline::update(&root, opts, "configured-model", &[]).unwrap();

        let reader = IndexReader::open(&config::index_path(&root)).unwrap();
        assert_eq!(
            reader.symbol_count(),
            1,
            "refusal must leave a readable structural index"
        );
        assert!(reader
            .vector(reader.symbol(0).unwrap().vector_index)
            .is_none());
        assert_eq!(
            Manifest::load(&config::manifest_path(&root))
                .unwrap()
                .embedder_id,
            None
        );
    }
}

#[test]
fn migration_rejects_unmarked_vector_normalization() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let id = "minilm-l6-v2";
    legacy_embedded_index(&root, id, embedder_dim(id).unwrap());
    let mut manifest = Manifest::load(&config::manifest_path(&root)).unwrap();
    manifest.vectors_normalized = Some(false);
    manifest.save(&config::manifest_path(&root)).unwrap();

    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    pipeline::update(&root, opts, id, &[]).unwrap();

    let reader = IndexReader::open(&config::index_path(&root)).unwrap();
    assert_eq!(reader.symbol_count(), 1);
    assert!(reader
        .vector(reader.symbol(0).unwrap().vector_index)
        .is_none());
    assert_eq!(
        Manifest::load(&config::manifest_path(&root))
            .unwrap()
            .embedder_id,
        None
    );
}

#[test]
fn migration_refuses_ambiguous_name_kind_identity() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let id = "minilm-l6-v2";
    let source =
        "pub fn reuse_target() { let value = 7; }\npub fn reuse_target() { let value = 7; }\n";
    legacy_embedded_index_with(
        &root,
        id,
        embedder_dim(id).unwrap(),
        source,
        true,
        None,
        false,
    );

    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    pipeline::update(&root, opts, "different-configured-model", &[]).unwrap();

    assert_readable_without_reused_vectors(&root, 2);
}

#[test]
fn migration_refuses_context_that_depended_on_unpersisted_docs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let id = "minilm-l6-v2";
    let source = "/// persisted doc\npub fn reuse_target() { let value = 7; }\n";
    legacy_embedded_index_with(
        &root,
        id,
        embedder_dim(id).unwrap(),
        source,
        false,
        Some("persisted doc"),
        false,
    );

    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    pipeline::update(&root, opts, "different-configured-model", &[]).unwrap();

    assert_readable_without_reused_vectors(&root, 1);
}

#[test]
fn migration_skips_only_file_with_unpersisted_context() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let id = "minilm-l6-v2";
    let expected = legacy_embedded_multi_file_index(
        &root,
        id,
        &[
            (
                "doc.rs",
                "/// persisted doc\npub fn doc_target() { let value = 7; }\n",
                Some("persisted doc"),
            ),
            ("keep.rs", "pub fn keep_target() { let value = 9; }\n", None),
        ],
        None,
    );

    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    pipeline::update(&root, opts, "different-configured-model", &[]).unwrap();

    let reader = IndexReader::open(&config::index_path(&root)).unwrap();
    let mut vectors = std::collections::HashMap::new();
    for index in 0..reader.symbol_count() {
        let record = reader.symbol(index).unwrap();
        let name = reader.read_string(record.name_offset).to_string();
        vectors.insert(
            name,
            reader.vector(record.vector_index).map(<[f32]>::to_vec),
        );
    }
    assert!(vectors["doc_target"].is_none());
    assert_eq!(
        vectors["keep_target"]
            .as_ref()
            .unwrap()
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        expected["keep_target"]
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        "one file's missing persisted doc context must not discard another file's vector"
    );
    assert!(!config::hnsw_path(&root).exists());
    assert!(!config::hash_index_path(&root).exists());
}

#[test]
fn migration_refuses_nonfinite_file_even_when_another_file_context_mismatches() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let id = "minilm-l6-v2";
    legacy_embedded_multi_file_index(
        &root,
        id,
        &[
            (
                "doc.rs",
                "/// persisted doc\npub fn doc_target() { let value = 7; }\n",
                Some("persisted doc"),
            ),
            ("keep.rs", "pub fn keep_target() { let value = 9; }\n", None),
            (
                "finite.rs",
                "pub fn finite_target() { let value = 11; }\n",
                None,
            ),
        ],
        Some("keep.rs"),
    );
    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };

    pipeline::update(&root, opts, "different-configured-model", &[]).unwrap();

    assert_readable_without_reused_vectors(&root, 3);
}

#[test]
fn migration_refuses_nonfinite_persisted_vector() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let id = "minilm-l6-v2";
    legacy_embedded_index_with(
        &root,
        id,
        embedder_dim(id).unwrap(),
        "pub fn reuse_target() { let value = 7; }\n",
        false,
        None,
        true,
    );

    let opts = IndexOptions {
        with_embeddings: false,
        with_clusters: false,
        with_pattern_index: false,
        ..IndexOptions::default()
    };
    pipeline::update(&root, opts, "different-configured-model", &[]).unwrap();

    assert_readable_without_reused_vectors(&root, 1);
}
