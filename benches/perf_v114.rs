//! v1.14.0 — Pass-2 C++ `#include` resolution micro-benches.
//!
//! The architect estimated **~2-10s wall on Pass-2** for a 50k-symbol
//! corpus when locking the BFS approach (vs the 80/20 same-dir-only
//! alternative). These benches validate the estimate by isolating the
//! two hot functions in `store::include_resolver`:
//!
//!   1. `build_include_graph` — runs **once per `vex index` build**.
//!      Walks every C++ ParsedFile and pushes each `#include "..."`
//!      string through the path resolver. For a 5k-file project at 10
//!      includes each (~50k include directives), expect single-digit
//!      ms — this is hash lookups, no I/O.
//!
//!   2. `resolve_via_include_bfs` — runs **once per `BindTarget::
//!      Unresolved` C++ ref**. For a 50k-symbol corpus, ~10-30% of
//!      refs from the binder are Unresolved (the rest are
//!      ModuleSymbol or Imported); the batch BFS cost is the
//!      architect's headline number.
//!
//! Run with: `cargo bench --bench perf_v114`. Criterion HTML reports
//! land under `target/criterion/`. The text summary is what we
//! compare against the architect's estimate.
//!
//! Corpus shape: 5k C++ files laid out as 50 dirs × 100 files. Each
//! file has 5 named symbols (25k total) and includes 8 random files
//! from the same dir + 2 from neighboring dirs (fan-out ~10). About
//! 10% of names are shared across 2-3 files (typical "namespace
//! collision" pattern).

#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;
use std::sync::OnceLock;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use vex::store::include_resolver::{
    build_basename_index, build_include_graph, resolve_via_include_bfs,
};

/// Deterministic synthetic corpus shared by every bench. Building it
/// is ~50 ms (paths + names + adjacency lists) so we cache via
/// `OnceLock` and reuse across benches. The corpus is intentionally
/// dense — 5k files, 50k include edges, 25k symbols — to mirror the
/// scale of the user's bug report (50k symbols, "deep-source" C++
/// repo on Windows).
struct Corpus {
    /// Path → file_id. The `HashMap<String, u32>` the writer maintains.
    file_ids: HashMap<String, u32>,
    /// `(path, includes-as-Strings)` tuples for `build_include_graph`.
    /// Owned so the bench can keep iterating without rebuilding inputs.
    cpp_files: Vec<(String, Vec<String>)>,
    /// Parallel `Vec<file_id>` indexed by sym_entries position — same
    /// shape the writer builds in v1.14 Pass-2 setup.
    sym_to_file_id: Vec<u32>,
    /// Owned `name_to_global` for the BFS bench. The resolver API
    /// takes `&HashMap<&str, Vec<u32>>`, so the bench wraps this into
    /// a view in the setup step (borrowing the owned `String` keys).
    name_to_global_owned: HashMap<String, Vec<u32>>,
    /// `(name, from_file_id)` pairs simulating the writer's Pass-2
    /// loop: each tuple is one Unresolved ref the BFS resolves. Mix
    /// of guaranteed-hit, guaranteed-miss, and ambiguous-name cases.
    bfs_calls: Vec<(String, u32)>,
}

/// Cheap deterministic "PRNG" — strict-mode hash mod N. Lets us pick
/// reproducible neighbors without pulling in a `rand` dep just for
/// the bench fixture.
fn det_pick(seed: u64, mod_n: usize) -> usize {
    (seed.wrapping_mul(2_654_435_761) as usize) % mod_n.max(1)
}

const N_DIRS: usize = 50;
const FILES_PER_DIR: usize = 100;
const N_FILES: usize = N_DIRS * FILES_PER_DIR; // 5000
const SYMS_PER_FILE: usize = 5;
const TOTAL_SYMS: usize = N_FILES * SYMS_PER_FILE; // 25_000
const INCLUDES_PER_FILE: usize = 10;
const N_BFS_CALLS: usize = 10_000;

fn corpus() -> &'static Corpus {
    static CORPUS: OnceLock<Corpus> = OnceLock::new();
    CORPUS.get_or_init(build_corpus)
}

fn build_corpus() -> Corpus {
    // ---- 1. Paths + file_ids -----------------------------------------
    let mut paths: Vec<String> = Vec::with_capacity(N_FILES);
    let mut file_ids: HashMap<String, u32> = HashMap::with_capacity(N_FILES);
    for d in 0..N_DIRS {
        for f in 0..FILES_PER_DIR {
            // Mix of .cpp / .h so the basename index has variety.
            let ext = if f % 2 == 0 { "cpp" } else { "h" };
            let p = format!("src/d{d:02}/f{f:03}.{ext}");
            file_ids.insert(p.clone(), paths.len() as u32);
            paths.push(p);
        }
    }

    // ---- 2. Includes per file ----------------------------------------
    // Same-dir bias (8 of 10) + 2 cross-dir hops. Mirrors how real C++
    // projects look: a TU includes its own dir's headers heavily plus
    // a handful of "shared" headers from elsewhere.
    let mut cpp_files: Vec<(String, Vec<String>)> = Vec::with_capacity(N_FILES);
    for i in 0..N_FILES {
        let dir = i / FILES_PER_DIR;
        let mut incs: Vec<String> = Vec::with_capacity(INCLUDES_PER_FILE);
        for k in 0..INCLUDES_PER_FILE {
            let target_dir = if k < 8 {
                dir
            } else {
                (dir + det_pick(i as u64 * 31 + k as u64, N_DIRS - 1) + 1) % N_DIRS
            };
            let target_file = det_pick(i as u64 * 97 + k as u64, FILES_PER_DIR);
            if target_dir == dir && target_file == i % FILES_PER_DIR {
                // Skip self — `build_include_graph` would drop it
                // anyway but avoiding it here keeps the bench's per-file
                // include count consistent.
                continue;
            }
            // Quoted include path is what the parser hands the resolver.
            // Use the relative form from the current file's perspective
            // (just basename for same-dir, `../d??/...` for cross-dir).
            let target_ext = if target_file.is_multiple_of(2) {
                "cpp"
            } else {
                "h"
            };
            let inc = if target_dir == dir {
                format!("f{target_file:03}.{target_ext}")
            } else {
                format!("../d{target_dir:02}/f{target_file:03}.{target_ext}")
            };
            incs.push(inc);
        }
        cpp_files.push((paths[i].clone(), incs));
    }

    // ---- 3. Symbols + name_to_global ---------------------------------
    // 5 syms per file. Names look like `do_thing_<i>_<j>` with i = file
    // idx and j = sym idx — passes `is_meaningful_identifier` (has `_`).
    // Inject ~10% collisions by giving every 10th file's first symbol a
    // shared name `shared_helper_<dir>` so 100 of the 5000 files agree
    // on `shared_helper_<dir>`.
    let mut name_to_global_owned: HashMap<String, Vec<u32>> = HashMap::with_capacity(TOTAL_SYMS);
    let mut sym_to_file_id: Vec<u32> = Vec::with_capacity(TOTAL_SYMS);
    let mut sym_idx: u32 = 0;
    for i in 0..N_FILES {
        let dir = i / FILES_PER_DIR;
        for j in 0..SYMS_PER_FILE {
            let name = if j == 0 && i % 10 == 0 {
                // Collision name shared by every 10th file in the dir
                // (10 files per dir contribute, → ~10-way ambiguity).
                format!("shared_helper_{dir:02}")
            } else {
                format!("do_thing_{i:04}_{j}")
            };
            name_to_global_owned.entry(name).or_default().push(sym_idx);
            sym_to_file_id.push(i as u32);
            sym_idx += 1;
        }
    }

    // ---- 4. BFS call list --------------------------------------------
    // Half guaranteed-hit, half guaranteed-miss. Hit: name of a symbol
    // in a file the issuer includes (depth 1) or transitively reaches.
    // Miss: an unknown symbol name the resolver must walk the entire
    // reachable subgraph for before returning None — the WORST case
    // for the BFS.
    let mut bfs_calls: Vec<(String, u32)> = Vec::with_capacity(N_BFS_CALLS);
    for n in 0..N_BFS_CALLS {
        let from = det_pick(n as u64 * 2_147_483_647, N_FILES) as u32;
        if n % 2 == 0 {
            // Hit: a real symbol in some other file. Half of these
            // hit the collision name (10-way ambiguous), the other
            // half hit a unique name.
            let target_file = det_pick(n as u64 * 13, N_FILES);
            let name = if n % 4 == 0 {
                format!("shared_helper_{:02}", target_file / FILES_PER_DIR)
            } else {
                format!(
                    "do_thing_{target_file:04}_{}",
                    det_pick(n as u64 * 7, SYMS_PER_FILE)
                )
            };
            bfs_calls.push((name, from));
        } else {
            // Miss: name nothing defines. Forces BFS to exhaust the
            // reachable subgraph of `from`.
            bfs_calls.push((format!("nope_{n}"), from));
        }
    }

    Corpus {
        file_ids,
        cpp_files,
        sym_to_file_id,
        name_to_global_owned,
        bfs_calls,
    }
}

/// Build the borrow view `&HashMap<&str, Vec<u32>>` from the corpus's
/// owned name table. The resolver API takes a borrow; the bench keeps
/// the owned `String` keys in `Corpus` so the view points at them.
fn name_view(c: &Corpus) -> HashMap<&str, Vec<u32>> {
    c.name_to_global_owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect()
}

fn bench_build_include_graph(c: &mut Criterion) {
    let corpus = corpus();
    let basename_index = build_basename_index(&corpus.file_ids);
    let mut group = c.benchmark_group("v1.14_include_resolver");
    // Each iteration rebuilds the whole graph — matches what
    // `store::writer::write_index_with_call_graph_and_skeletons_and_fingerprints`
    // does once per `vex index` invocation.
    group.bench_function("build_include_graph_5k_files", |b| {
        b.iter(|| {
            let g = build_include_graph(
                corpus
                    .cpp_files
                    .iter()
                    .map(|(p, v)| (p.as_str(), v.as_slice())),
                black_box(&corpus.file_ids),
                black_box(&basename_index),
            );
            // black_box prevents the optimizer from concluding the
            // result is unused and inlining the graph build away.
            black_box(g.len())
        })
    });
    group.finish();
}

fn bench_bfs_resolve_batch(c: &mut Criterion) {
    let corpus = corpus();
    let basename_index = build_basename_index(&corpus.file_ids);
    let include_graph = build_include_graph(
        corpus
            .cpp_files
            .iter()
            .map(|(p, v)| (p.as_str(), v.as_slice())),
        &corpus.file_ids,
        &basename_index,
    );
    let ntg = name_view(corpus);

    let mut group = c.benchmark_group("v1.14_include_resolver");
    // The headline number: per-iter we run 10k BFS calls — the proxy
    // for "Pass-2 total cost at 50k-symbol scale". The architect's
    // 2-10s estimate maps to ~200-1000 μs per BFS at 10k calls; if we
    // come in well under that, the lock-in was right.
    group.bench_function("bfs_resolve_batch_10k", |b| {
        b.iter(|| {
            let mut hits = 0u64;
            for (name, from) in &corpus.bfs_calls {
                let r = resolve_via_include_bfs(
                    name,
                    *from,
                    black_box(&ntg),
                    black_box(&corpus.sym_to_file_id),
                    black_box(&include_graph),
                );
                if r.is_some() {
                    hits += 1;
                }
            }
            black_box(hits)
        })
    });

    // Single-call sanity bench — useful when iterating on the BFS
    // hot path itself (the batch number drowns out small wins).
    let (sample_name, sample_from) = corpus.bfs_calls[0].clone();
    group.bench_function("bfs_resolve_single_call", |b| {
        b.iter(|| {
            let r = resolve_via_include_bfs(
                black_box(&sample_name),
                black_box(sample_from),
                &ntg,
                &corpus.sym_to_file_id,
                &include_graph,
            );
            black_box(r)
        })
    });

    group.finish();
}

criterion_group!(benches, bench_build_include_graph, bench_bfs_resolve_batch);
criterion_main!(benches);
