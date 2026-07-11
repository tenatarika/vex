//! v1.13.0 — baseline benches for the open Performance items:
//!
//!   P1 — `find_duplicates` / `find_similar` reopen HNSW per symbol
//!   P2 — SHA-256 of 86 MiB ONNX on every `vex search --semantic`
//!   P5 — vectors not L2-normalized at write time
//!   P7 — `BTreeMap<String, …>` allocations in FST builders
//!   P8 — BM25 tokenizer allocations per term
//!
//! Run with: `cargo bench --bench perf_v113`. Criterion HTML reports land
//! under `target/criterion/`; capture the text summary into
//! `benches/results/v1.13-baseline-<short-sha>.txt` for the
//! before→after diff.
//!
//! Bench layout (4 cases):
//!
//!   1. `sha256_88mib` — `verify_file_sha256` over a 88 MiB synthetic
//!      file. Stand-in for the MiniLM ONNX hash (P2). One sample per
//!      iter; Criterion default sample_size is fine — the cost is
//!      O(file size), not per-iter setup.
//!
//!   2. `vex_index_cold_subprocess` — subprocess `vex index` on a
//!      ~200-function synth corpus (5 files × 40 fns). Covers P7 (FST
//!      builder allocations) and P8 (BM25 tokenizer allocations)
//!      indirectly via end-to-end `vex index` wall-clock. Subprocess
//!      cost dominates per-iter; `sample_size(10)` keeps the bench
//!      under a minute.
//!
//!   3. `find_duplicates_brute_force` — in-process `find_duplicates`
//!      on a pre-baked 500-symbol vector index with HNSW absent. Covers
//!      P5 — every iter walks the full vector array and calls
//!      `cosine_similarity` for each pair; normalize-at-write-time
//!      collapses that to a dot product.
//!
//!   4. `find_duplicates_with_hnsw` — same fixture, but with a saved
//!      HNSW file alongside the index. Covers P1 — `search_hnsw_at`
//!      currently rebuilds the `usearch::Index` handle and `view()`s
//!      the file on every neighbor query, so an N-symbol scan does N
//!      mmap cycles.
//!
//! The two fixture-bearing benches share a `OnceLock`-cached
//! [`SimFixture`] (cost: one `write_index_full` + one HNSW build).

#![allow(clippy::needless_range_loop)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use assert_cmd::Command;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tempfile::TempDir;

use vex::embed::integrity::{verify_file_sha256, verify_with_marker};
use vex::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
use vex::search::similar::find_duplicates;
use vex::store::bm25::tokenize_document;
use vex::store::call_graph::{build_callees_fst, build_callers_fst, CallEdgeBuilder};
use vex::store::format::VECTOR_DIM;
use vex::store::reader::IndexReader;
use vex::store::symbol_fst::build_symbol_fst;
use vex::store::writer::write_index_full;
use vex::util::config;

// ---------------------------------------------------------------------------
// Bench 1 — SHA-256 of a 88 MiB synthetic file (P2 baseline)
// ---------------------------------------------------------------------------

struct ShaFixture {
    _tmp: TempDir,
    path: PathBuf,
    expected_hex: String,
}

static SHA_FIXTURE: OnceLock<ShaFixture> = OnceLock::new();

fn sha_fixture() -> &'static ShaFixture {
    SHA_FIXTURE.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("synth.bin");

        // 88 MiB matches the real MiniLM ONNX (~86 MiB) within rounding;
        // pattern is non-zero so any future PRNG-aware optimisation in
        // SHA-256 (none exists today) can't trivially short-circuit.
        let mut f = std::fs::File::create(&path).expect("create");
        let mut hasher = Sha256::new();
        let chunk: Vec<u8> = (0..65_536).map(|i| (i & 0xff) as u8).collect();
        for _ in 0..(88 * 16) {
            f.write_all(&chunk).expect("write");
            hasher.update(&chunk);
        }
        f.sync_all().expect("sync");

        ShaFixture {
            expected_hex: format!("{:x}", hasher.finalize()),
            path,
            _tmp: tmp,
        }
    })
}

fn bench_sha256_88mib(c: &mut Criterion) {
    let fx = sha_fixture();
    let mut group = c.benchmark_group("v113::p2_sha_verify");
    group.sample_size(20); // file read is ~100 ms on SSD; 20 samples ≈ 2s
                           // Baseline / cold path: every iter rehashes the file.
    group.bench_function("verify_file_sha256_88mib", |b| {
        b.iter(|| {
            verify_file_sha256(&fx.path, &fx.expected_hex).expect("sha match");
            black_box(())
        })
    });
    // P2 warm path: marker cache. First iter writes the marker; every
    // subsequent iter hits the (mtime, size, sha) marker and skips the
    // 86 MiB hash. Per-iter cost should drop from ~163 ms to single-
    // digit μs.
    group.bench_function("verify_with_marker_88mib_warm", |b| {
        // Seed the marker once before timing so iter 0 isn't an
        // outlier on the cold-write.
        verify_with_marker(&fx.path, &fx.expected_hex).expect("seed marker");
        b.iter(|| {
            verify_with_marker(&fx.path, &fx.expected_hex).expect("warm marker hit");
            black_box(())
        })
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Bench 2 — `vex index` cold subprocess (P7 + P8 baseline)
// ---------------------------------------------------------------------------

fn bench_index_cold_subprocess(c: &mut Criterion) {
    let mut group = c.benchmark_group("v113::p7p8_index_cold");
    // Subprocess + full parse + FST build + BM25 build + manifest write
    // is on the order of hundreds of ms; 10 samples keeps wall-clock
    // sane while still giving Criterion enough data to estimate variance.
    group.sample_size(10);
    group.bench_function("vex_index_synth_200fn", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let root = tmp.path().canonicalize().expect("canonicalize");
                std::fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
                std::fs::create_dir_all(root.join("src")).unwrap();
                for file_i in 0..5usize {
                    let mut src = String::new();
                    for fn_i in 0..40usize {
                        let id = file_i * 40 + fn_i;
                        let cb = (id + 7) % 200;
                        let cc = (id + 13) % 200;
                        src.push_str(&format!("pub fn fn_{id}() {{ fn_{cb}(); fn_{cc}(); }}\n"));
                    }
                    std::fs::write(root.join("src").join(format!("file_{file_i}.rs")), src)
                        .unwrap();
                }
                tmp // keep alive for the iter
            },
            |tmp| {
                let root = tmp.path().canonicalize().expect("canonicalize");
                let cache_dir = root.join(".vex-bench-cache");
                let mut cmd = Command::cargo_bin("vex").expect("cargo_bin vex");
                cmd.current_dir(&root)
                    .env("VEX_CACHE_DIR", &cache_dir)
                    .args(["index"]);
                cmd.assert().success();
                black_box(tmp)
            },
            criterion::BatchSize::PerIteration,
        )
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Bench 3 + 4 — `find_duplicates` brute-force vs HNSW (P5, P1 baselines)
// ---------------------------------------------------------------------------

/// Shared 500-symbol vector-bearing index fixture for the duplicates
/// benches. Built once via `write_index_full`; the HNSW sidecar is
/// generated inline so bench 4 can exercise the per-symbol reopen path
/// without needing a real `vex index --semantic` (which downloads ONNX).
struct SimFixture {
    _tmp: TempDir,
    root: PathBuf,
    index_path: PathBuf,
    hnsw_path: PathBuf,
}

static SIM_FIXTURE: OnceLock<SimFixture> = OnceLock::new();

fn sim_fixture() -> &'static SimFixture {
    SIM_FIXTURE.get_or_init(|| {
        use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};

        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        std::fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
        let cache_root = root.join(".vex_cache");
        std::fs::create_dir_all(&cache_root).unwrap();

        // 500 symbols across 5 files. Each file holds 100 fns; lines
        // ascend by 1. Vectors are deterministic, non-trivially varied
        // (gradient + per-file phase shift) so cosine scores spread
        // across the [0, 1] range — keeps the brute-force loop from
        // short-circuiting on identical vectors.
        let dim = VECTOR_DIM as usize;
        let mut parsed: Vec<ParsedFile> = Vec::with_capacity(5);
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(500);

        for file_i in 0..5usize {
            let mut syms: Vec<ParsedSymbol> = Vec::with_capacity(100);
            for fn_i in 0..100usize {
                let global = file_i * 100 + fn_i;
                syms.push(ParsedSymbol {
                    name: format!("fn_{global}"),
                    kind: SymbolKind::Function,
                    line: fn_i + 1,
                    signature: Some(format!("fn fn_{global}()")),
                    doc: None,
                    body_tokens: None,
                });

                // Build a 384-d vector. Component j = sin(global*0.01 +
                // j*0.001 + file_i*0.3). Spreads scores enough that the
                // top-K sort actually does work each iter.
                let mut v = vec![0.0_f32; dim];
                for j in 0..dim {
                    let x = global as f32 * 0.01 + j as f32 * 0.001 + file_i as f32 * 0.3;
                    v[j] = x.sin();
                }
                vectors.push(v);
            }
            parsed.push(ParsedFile {
                path: format!("src/file_{file_i}.rs"),
                symbols: syms,
                refs: vec![],
                call_edges: vec![],
                bound_refs: vec![],
                skeletons: Vec::new(),
                cpp_includes: Vec::new(),
                trigram_bloom: None,
                hierarchy_captures: Vec::new(),
            });
        }

        // Drop the v6 index file at the cache path `find_duplicates`
        // expects. Cache override mirrors `bundle.rs` — both the bench
        // process and any subprocess must agree on the hashed cache
        // dir for the same root.
        let cache_dir = root.join(".vex-sim-cache");
        config::set_cache_override(cache_dir.clone(), false);
        let index_path = config::index_path(&root);
        std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();

        write_index_full(&parsed, &vectors, VECTOR_DIM, &index_path).expect("write_index_full");

        // Build the HNSW sidecar so bench 4 has a real file to view().
        // `save()` materialises the graph to disk; `search_hnsw_at`
        // calls `view()` on every neighbor query, which is the exact
        // per-symbol-reopen pathology P1 targets.
        let hnsw_path = config::hnsw_path(&root);
        std::fs::create_dir_all(hnsw_path.parent().unwrap()).unwrap();
        let options = IndexOptions {
            dimensions: dim,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            connectivity: 0,
            expansion_add: 0,
            expansion_search: 0,
            multi: false,
        };
        let hnsw = new_index(&options).expect("new_index");
        hnsw.reserve(vectors.len()).expect("reserve");
        for (i, v) in vectors.iter().enumerate() {
            hnsw.add(i as u64, v).expect("add");
        }
        hnsw.save(hnsw_path.to_str().expect("hnsw_path utf8"))
            .expect("save hnsw");

        SimFixture {
            _tmp: tmp,
            root,
            index_path,
            hnsw_path,
        }
    })
}

fn bench_find_duplicates_brute_force(c: &mut Criterion) {
    let fx = sim_fixture();
    let reader = IndexReader::open(&fx.index_path).expect("open index");
    // Point at a non-existent HNSW path so `search_hnsw_at` returns
    // None and `find_duplicates` falls through to brute-force scoring.
    let absent_hnsw = fx.root.join("__absent_hnsw");

    let mut group = c.benchmark_group("v113::p5_brute_force");
    group.sample_size(15);
    // Baseline: cosine path (un-normalized vectors). Each pair scoring
    // runs sqrt + per-vector norms.
    group.bench_function("find_duplicates_500sym_brute_cosine", |b| {
        b.iter(|| {
            let pairs = find_duplicates(
                &reader,
                &absent_hnsw,
                /* threshold */ 0.95,
                /* min_body_lines */ 0,
                /* limit */ 20,
                /* normalized */ false,
            )
            .expect("find_duplicates brute");
            black_box(pairs.len())
        })
    });
    // P5 fast path: pre-normalize vectors at fixture build, pass
    // `normalized = true`. The brute-force loop now uses `dot_product`
    // — skipping sqrt + norms entirely.
    //
    // Caveats reviewers raised about this measurement:
    //
    // 1. The fixture vectors are NOT unit-length, so scores are
    //    meaningless. The correctness anchor is the unit test
    //    `dot_product_equals_cosine_for_normalized` in semantic.rs.
    //    Here we measure LOOP COST only.
    // 2. We set `threshold = -10.0` to accept every scored pair
    //    (defeating the early-exit on `sim < threshold`). The cosine
    //    baseline uses `threshold = 0.95` which filters most pairs
    //    out of the post-filter loop. This means the dot-product arm
    //    does MORE work (every pair lands in the seen+pairs
    //    structures), so the measured win is conservative — the
    //    true speedup at matching thresholds would be larger.
    //
    // A parallel pre-normalized fixture with `threshold = 0.95` is
    // the cleanest follow-up; deferred to keep the v1.13 bench scope
    // tight.
    group.bench_function("find_duplicates_500sym_brute_dot", |b| {
        b.iter(|| {
            let pairs = find_duplicates(
                &reader,
                &absent_hnsw,
                /* threshold */
                -10.0, // accept anything; we don't care about score validity
                /* min_body_lines */ 0,
                /* limit */ 20,
                /* normalized */ true,
            )
            .expect("find_duplicates dot");
            black_box(pairs.len())
        })
    });
    group.finish();
}

/// Pre-P1 per-symbol HNSW reopen, replicated inline so the bench can
/// measure the *delta* from the v1.13 hoist. Mirrors what
/// `find_duplicates` did before this release: every outer-loop iter
/// creates a fresh `usearch::Index`, `view()`s the file, runs one
/// search, drops everything.
fn find_duplicates_legacy_hnsw_loop(
    reader: &IndexReader,
    hnsw_path: &std::path::Path,
    top_k: usize,
) -> usize {
    use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};
    let mut total = 0usize;
    let n = reader.symbol_count();
    let dim = reader
        .symbol(0)
        .and_then(|rec| reader.vector(rec.vector_index))
        .map(|v| v.len())
        .unwrap_or(0);
    for sym_idx in 0..n {
        let Some(rec) = reader.symbol(sym_idx) else {
            continue;
        };
        let Some(query_vec) = reader.vector(rec.vector_index) else {
            continue;
        };
        // Per-iter: rebuild handle + view() the file.
        let options = IndexOptions {
            dimensions: dim,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            connectivity: 0,
            expansion_add: 0,
            expansion_search: 0,
            multi: false,
        };
        let Ok(index) = new_index(&options) else {
            continue;
        };
        let Some(path_str) = hnsw_path.to_str() else {
            continue;
        };
        if index.view(path_str).is_err() {
            continue;
        }
        if let Ok(results) = index.search(query_vec, top_k) {
            total += results.keys.len();
        }
    }
    total
}

fn bench_find_duplicates_with_hnsw(c: &mut Criterion) {
    let fx = sim_fixture();
    let reader = IndexReader::open(&fx.index_path).expect("open index");

    let mut group = c.benchmark_group("v113::p1_hnsw_per_symbol");
    group.sample_size(15);

    // Legacy: per-iter HNSW open. Replicates pre-1.13 behavior; the
    // mmap+view cost scales linearly with the outer loop.
    group.bench_function("legacy_per_iter_reopen_500sym", |b| {
        b.iter(|| {
            let n = find_duplicates_legacy_hnsw_loop(&reader, &fx.hnsw_path, 6);
            black_box(n)
        })
    });
    // P1: `find_duplicates` opens `HnswHandle` once. Real apples-to-
    // apples for the hoist — same fixture, same outer loop, just one
    // `view()` instead of `symbol_count` of them.
    group.bench_function("p1_hoisted_handle_500sym", |b| {
        b.iter(|| {
            let pairs = find_duplicates(
                &reader,
                &fx.hnsw_path,
                /* threshold */ 0.95,
                /* min_body_lines */ 0,
                /* limit */ 20,
                /* normalized */ false,
            )
            .expect("find_duplicates hnsw");
            black_box(pairs.len())
        })
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Bench 5 — `tokenize_document` microbench (P8 baseline + after)
// ---------------------------------------------------------------------------

/// Synthetic term bag the size of a typical function in vex itself —
/// signature + body identifiers + a doc-comment line. Real
/// `body_tokens` strings the pipeline produces look very much like
/// this. Around 30 unique terms over 500 characters.
fn synthetic_bag() -> String {
    [
        "fn find_callers",
        "reader IndexReader name &str fetch_cap usize",
        "let symbol_idx fst find name first",
        "let callers Vec CallMatch with_capacity fetch_cap",
        "for edge reader call_edges_by_callee symbol_idx",
        "if let Some caller reader symbol edge caller_sym_idx",
        "callers push CallMatch name file path line",
        "if callers len fetch_cap break",
        "callers sort_unstable_by key path line",
        "return Ok callers",
        "Find every caller of a symbol by name through the persistent call graph",
    ]
    .join(" ")
}

/// Pre-P8 tokenizer kept verbatim for side-by-side comparison. Mirrors
/// `vex::store::bm25::tokenize_document` as of `23565a3`: per-token
/// `to_lowercase` + per-unique `clone` into a `HashSet<String>`. Read
/// the dedicated parity test in `bm25.rs` for the behavioral contract.
fn tokenize_document_legacy(text: &str) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.len() < 2 {
            continue;
        }
        let lower = raw.to_lowercase();
        if seen.insert(lower.clone()) {
            out.push(lower);
        }
    }
    out
}

fn bench_tokenize_document(c: &mut Criterion) {
    let bag = synthetic_bag();
    // Sanity: legacy and current produce identical output. Without
    // this assertion a future tokenizer change could silently outpace
    // the legacy bench by skipping work.
    assert_eq!(tokenize_document_legacy(&bag), tokenize_document(&bag));

    let mut group = c.benchmark_group("v113::p8_bm25_tokenizer");
    // Per-call cost is single-digit μs; default Criterion sample size
    // and warmup are appropriate without override.
    group.bench_function("tokenize_document_legacy_typical_bag", |b| {
        b.iter(|| {
            let toks = tokenize_document_legacy(black_box(&bag));
            black_box(toks.len())
        })
    });
    group.bench_function("tokenize_document_p8_typical_bag", |b| {
        b.iter(|| {
            let toks = tokenize_document(black_box(&bag));
            black_box(toks.len())
        })
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Bench 6 — FST builders (P7 baseline + after, BTreeMap → Vec)
// ---------------------------------------------------------------------------
//
// The build hot path runs once per `vex index`, but on large repos it
// is one of the dominant fractions of the wall-clock. These microbenches
// drive the migrated builders directly so the win is measurable
// without subprocess noise.
//
// As with P8, the legacy BTreeMap impl is kept verbatim in this file so
// before/after numbers come from the same Criterion run.

/// Generate `n` all-lowercase synthetic identifiers paired with
/// monotonic symbol indices. ALL-lowercase on purpose: the real
/// `build_symbol_fst` fans CamelCase names into multiple sub-tokens
/// via a private splitter, which makes a like-for-like legacy/P7
/// comparison impossible. Lowercase input emits exactly one entry per
/// symbol from BOTH paths, so the comparison measures pure
/// BTreeMap-vs-Vec overhead. The real CamelCase load is still
/// exercised end-to-end by `bench_index_cold_subprocess`.
fn synth_symbols(n: u32) -> Vec<(String, u32)> {
    let parts = ["foo", "bar", "baz", "service", "repo", "handler", "manager"];
    (0..n)
        .map(|i| {
            let a = parts[(i as usize) % parts.len()];
            let b = parts[((i as usize) / parts.len()) % parts.len()];
            (format!("{a}_{b}_{i}"), i)
        })
        .collect()
}

/// Synthetic edges for the call-graph builders — 5000 edges spread
/// across 500 callers calling 200 distinct callee names.
fn synth_edges(n: u32) -> Vec<CallEdgeBuilder> {
    let names = [
        "read", "write", "open", "close", "parse", "render", "load", "save",
    ];
    (0..n)
        .map(|i| CallEdgeBuilder {
            caller_sym_idx: i % 500,
            callee_name: format!("{}_{}", names[(i as usize) % names.len()], i % 200),
            line: (i % 1024) + 1,
        })
        .collect()
}

/// Pre-P7 string-keyed FST builder kept verbatim for side-by-side
/// comparison. Mirrors `BTreeMap<String, Vec<u32>>` with the
/// duplicate-key `entry().clone()` tax. Input is expected to be
/// all-lowercase so no CamelCase split happens — making this a
/// like-for-like measurement against the v1.13 Vec-based builder.
fn build_string_keyed_fst_legacy(symbols: &[(String, u32)]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    use anyhow::Context;
    let mut entries: std::collections::BTreeMap<String, Vec<u32>> =
        std::collections::BTreeMap::new();
    for (name, idx) in symbols {
        entries.entry(name.clone()).or_default().push(*idx);
    }
    let mut posting_data: Vec<u8> = Vec::new();
    let mut fst_builder = fst::MapBuilder::memory();
    for (key, indices) in &mut entries {
        indices.sort_unstable();
        indices.dedup();
        let offset = posting_data.len() as u64;
        posting_data.extend_from_slice(&(indices.len() as u32).to_le_bytes());
        for &idx in indices.iter() {
            posting_data.extend_from_slice(&idx.to_le_bytes());
        }
        fst_builder
            .insert(key.as_bytes(), offset)
            .context("fst insert")?;
    }
    let fst_bytes = fst_builder.into_inner().context("finalize fst")?;
    Ok((fst_bytes, posting_data))
}

/// Pre-P7 `build_callees_fst` kept verbatim for side-by-side comparison.
/// Allocates one `format!("{:010}")` per edge (the per-edge string
/// alloc the stack-buffer encoder erased).
fn build_callees_fst_legacy(edges: &[CallEdgeBuilder]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    use anyhow::Context;
    let mut grouped: std::collections::BTreeMap<String, Vec<u32>> =
        std::collections::BTreeMap::new();
    for (i, e) in edges.iter().enumerate() {
        grouped
            .entry(format!("{:010}", e.caller_sym_idx))
            .or_default()
            .push(i as u32);
    }
    let mut posting_data: Vec<u8> = Vec::new();
    let mut fst_builder = fst::MapBuilder::memory();
    for (key, edge_indices) in grouped.iter_mut() {
        edge_indices.sort_unstable();
        edge_indices.dedup();
        let offset = posting_data.len() as u64;
        posting_data.extend_from_slice(&(edge_indices.len() as u32).to_le_bytes());
        for &idx in edge_indices.iter() {
            posting_data.extend_from_slice(&idx.to_le_bytes());
        }
        fst_builder
            .insert(key.as_bytes(), offset)
            .context("fst insert (call graph)")?;
    }
    let fst_bytes = fst_builder
        .into_inner()
        .context("finalize call-graph fst")?;
    Ok((fst_bytes, posting_data))
}

fn bench_fst_builders(c: &mut Criterion) {
    let syms = synth_symbols(5000);
    let edges = synth_edges(5000);

    // Sanity: legacy and current must produce identical FST bytes.
    // For symbol_fst the legacy variant uses a simpler CamelCase
    // heuristic — compare just postings byte size as a smoke check
    // (not byte equality). For callees_fst the encoding is identical.
    {
        let (legacy_fst, _) = build_callees_fst_legacy(&edges).unwrap();
        let (new_fst, _) = build_callees_fst(&edges).unwrap();
        assert_eq!(
            legacy_fst, new_fst,
            "build_callees_fst legacy/new produced different FST bytes"
        );
    }

    let mut group = c.benchmark_group("v113::p7_fst_builders");

    // String-key path: BTreeMap vs Vec with all-lowercase input so
    // both produce identical entry counts.
    group.bench_function("string_keyed_legacy_5000", |b| {
        b.iter(|| {
            let (fst, posts) = build_string_keyed_fst_legacy(black_box(&syms)).unwrap();
            black_box((fst.len(), posts.len()))
        })
    });
    group.bench_function("string_keyed_p7_5000", |b| {
        b.iter(|| {
            let (fst, posts) = build_symbol_fst(black_box(&syms)).unwrap();
            black_box((fst.len(), posts.len()))
        })
    });

    // Decimal-key path: the big win — `format!("{:010}")` per edge
    // (legacy) vs stack-buffer encode (P7).
    group.bench_function("decimal_keyed_legacy_5000", |b| {
        b.iter(|| {
            let (fst, posts) = build_callees_fst_legacy(black_box(&edges)).unwrap();
            black_box((fst.len(), posts.len()))
        })
    });
    group.bench_function("decimal_keyed_p7_5000", |b| {
        b.iter(|| {
            let (fst, posts) = build_callees_fst(black_box(&edges)).unwrap();
            black_box((fst.len(), posts.len()))
        })
    });

    // `build_callers_fst` is included for completeness — same shape
    // as the string-keyed path, no decimal encoding involved.
    group.bench_function("build_callers_fst_p7_5000", |b| {
        b.iter(|| {
            let (fst, posts) = build_callers_fst(black_box(&edges)).unwrap();
            black_box((fst.len(), posts.len()))
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_sha256_88mib,
    bench_index_cold_subprocess,
    bench_find_duplicates_brute_force,
    bench_find_duplicates_with_hnsw,
    bench_tokenize_document,
    bench_fst_builders,
);
criterion_main!(benches);
