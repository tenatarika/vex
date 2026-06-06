//! v1.15.0 B1.2 — incremental HNSW update vs full-rebuild micro-bench.
//!
//! Compares `build_hnsw_at` (full rebuild) against the new
//! `build_hnsw_incremental_at` (load → diff → remove/add → save) at
//! representative churn rates. Both functions are `pub(super)` in
//! `src/index/pipeline/output.rs`; this bench inlines the usearch entry
//! points the same way `tests/cli_incremental_hnsw_test.rs` does to
//! avoid widening the public API for a bench-only fixture.
//!
//! ## Corpus
//!
//! `VEX_BENCH_CORPUS_SIZE` controls vector count (default `5000`).
//! Architect reference is 25k symbols; set the env var to `25000` for
//! headline numbers. Vectors are 384-dim random unit-ish floats from a
//! deterministic xorshift-style PRNG (no `rand` dep) so runs are
//! reproducible across machines.
//!
//! ## Scenarios
//!
//!   - `full_rebuild_baseline`              — `build_hnsw_at` over the whole corpus
//!   - `incremental_no_change`              — load + save round-trip floor
//!   - `incremental_1pct_churn`             — 1% remove + 1% add (editor-save shape)
//!   - `incremental_5pct_pure_add`          — 5% adds, 0 removes (new code, no deletions)
//!   - `incremental_5pct_pure_remove`       — 0 adds, 5% removes (HNSW relink cost)
//!   - `incremental_10pct_churn`            — PR / branch-switch shape
//!   - `incremental_25pct_churn`            — exactly at the tombstone threshold
//!   - `incremental_26pct_falls_back`       — strict-GT bail, measures Ok(false) path
//!   - `fallback_then_full_rebuild`         — orchestrator end-to-end on >25% churn
//!     (incremental bail + immediate full rebuild)
//!
//! ## Sample size
//!
//! Default Criterion sample size is 100; at 25k corpus the full-rebuild
//! scenarios go ~30s/iter so a 100-sample run is ~50 min. The bench
//! lowers expensive scenarios to `sample_size(10)` which keeps the full
//! 25k bench under ~10 min while still producing meaningful confidence
//! intervals. Override per scenario via `--sample-size` if needed.
//!
//! Run: `cargo bench --bench perf_b12`. Criterion HTML lands under
//! `target/criterion/`. `-- --quick` produces single-iteration numbers
//! quickly but Criterion 0.5 panics during post-bench stats export
//! (the iterations themselves complete and times print on the way; the
//! crash is in the final report). Skip `--quick` when you want the
//! HTML report, or use it to capture indicative numbers from stderr.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use tempfile::TempDir;

const DIM: usize = 384;
const DEFAULT_CORPUS_SIZE: usize = 5_000;
const TOMBSTONE_NUMERATOR: usize = 1;
const TOMBSTONE_DENOMINATOR: usize = 4;

fn corpus_size() -> usize {
    std::env::var("VEX_BENCH_CORPUS_SIZE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_CORPUS_SIZE)
}

/// Cached deterministic corpus shared across every bench iteration.
/// Built once per process; building 25k random 384-dim vectors costs
/// ~100ms which we don't want amortised into every iteration.
struct Corpus {
    hashes: Vec<u64>,
    vectors: Vec<Vec<f32>>,
}

static CORPUS: OnceLock<Corpus> = OnceLock::new();

/// Deterministic PRNG — same wrapping-mul pattern as `perf_v114`.
/// Stateless: `prng_at(i, j)` returns a deterministic f32 in [-1, 1]
/// for slot j of vector i. No allocations, no `rand` dep.
fn prng_at(i: usize, j: usize) -> f32 {
    let mut s = (i as u64).wrapping_mul(0x9E3779B97F4A7C15);
    s ^= (j as u64).wrapping_mul(0xBF58476D1CE4E5B9);
    s = s.wrapping_mul(0x94D049BB133111EB);
    // Map u64 → [-1, 1] via top-24 bits to keep f32 precision useful.
    let bits = (s >> 40) as u32;
    let unit = bits as f32 / ((1u32 << 24) - 1) as f32;
    unit * 2.0 - 1.0
}

fn make_unit_vec(seed: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    let mut norm_sq = 0.0_f32;
    for (j, slot) in v.iter_mut().enumerate() {
        let x = prng_at(seed, j);
        *slot = x;
        norm_sq += x * x;
    }
    let norm = norm_sq.sqrt().max(1e-12);
    for slot in v.iter_mut() {
        *slot /= norm;
    }
    v
}

fn corpus() -> &'static Corpus {
    CORPUS.get_or_init(|| {
        let n = corpus_size();
        let mut hashes = Vec::with_capacity(n);
        let mut vectors = Vec::with_capacity(n);
        for i in 0..n {
            // Hash mixing matches the v1.14 perf bench convention — keeps
            // the hash space dense and collision-free for the bench
            // corpus, separate from the `0xBEEF_*` namespace `churned()`
            // uses for replacements.
            hashes.push(0xDEAD_0000_u64.wrapping_add(i as u64));
            vectors.push(make_unit_vec(i));
        }
        Corpus { hashes, vectors }
    })
}

/// Materialise a fresh HNSW + hash-index sidecar at `cache_dir` from
/// the cached corpus. Used by every incremental bench's `iter_batched`
/// setup closure to provide a clean baseline per iteration.
fn seed_baseline(cache_dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let hnsw_path = cache_dir.join("index.hnsw");
    let hash_path = cache_dir.join("index.hashes");
    let c = corpus();
    write_hnsw(&hnsw_path, &c.vectors, &c.hashes);
    vex::search::hash_index::save(&hash_path, &c.hashes).expect("seed hash-index sidecar");
    (hnsw_path, hash_path)
}

/// Inline copy of `build_hnsw_at` (production `pub(super)`). Stays in
/// sync via the `usearch::IndexOptions` shape; a parameter drift would
/// surface as dim-mismatch panic during the load round-trip below.
fn write_hnsw(path: &Path, vectors: &[Vec<f32>], hashes: &[u64]) {
    use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};
    let options = IndexOptions {
        dimensions: DIM,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    };
    let index = new_index(&options).expect("new_index");
    index.reserve(vectors.len()).expect("reserve");
    for (vec, &h) in vectors.iter().zip(hashes.iter()) {
        index.add(h, vec).expect("add");
    }
    index
        .save(path.to_str().expect("hnsw path utf-8"))
        .expect("save HNSW");
}

/// Inline copy of `build_hnsw_incremental_at`'s load/diff/mutate/save
/// core. Returns the `Ok(true)` / `Ok(false)` contract of production.
fn incremental_apply(
    hnsw_path: &Path,
    hash_index_path: &Path,
    new_vectors: &[Vec<f32>],
    new_hashes: &[u64],
) -> bool {
    use std::collections::HashSet;
    use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};

    let old_hashes = vex::search::hash_index::load(hash_index_path).expect("load old sidecar");
    let old_set: HashSet<u64> = old_hashes.iter().copied().collect();
    let new_set: HashSet<u64> = new_hashes.iter().copied().collect();

    let to_remove: Vec<u64> = old_set.difference(&new_set).copied().collect();
    let to_add_indices: Vec<usize> = new_hashes
        .iter()
        .enumerate()
        .filter(|(_, h)| !old_set.contains(h))
        .map(|(i, _)| i)
        .collect();

    if to_remove.len() * TOMBSTONE_DENOMINATOR > old_hashes.len() * TOMBSTONE_NUMERATOR {
        return false;
    }

    let options = IndexOptions {
        dimensions: DIM,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    };
    let index = new_index(&options).expect("new_index");
    let path_str = hnsw_path.to_str().expect("hnsw path utf-8");
    index.load(path_str).expect("load HNSW");
    index
        .reserve(old_hashes.len() + to_add_indices.len())
        .expect("reserve");
    for h in &to_remove {
        let _ = index.remove(*h);
    }
    for &i in &to_add_indices {
        index.add(new_hashes[i], &new_vectors[i]).expect("add");
    }
    index.save(path_str).expect("save HNSW");
    vex::search::hash_index::save(hash_index_path, new_hashes).expect("save new sidecar");
    true
}

/// Replace the first `churn` entries of the cached corpus with fresh
/// hashes. Equal-count add/remove → matched-churn scenario.
fn churned_balanced(churn: usize) -> (Vec<Vec<f32>>, Vec<u64>) {
    let c = corpus();
    let mut vectors = c.vectors.clone();
    let mut hashes = c.hashes.clone();
    for i in 0..churn {
        hashes[i] = 0xBEEF_0000_u64.wrapping_add(i as u64);
        vectors[i] = make_unit_vec(0xBEEF_0000 + i);
    }
    (vectors, hashes)
}

/// Append `n_add` new entries to the cached corpus (no removes).
/// Captures the "new code, no deletions" branch of churn — what
/// happens when the user adds a function without removing anything.
fn appended(n_add: usize) -> (Vec<Vec<f32>>, Vec<u64>) {
    let c = corpus();
    let mut vectors = c.vectors.clone();
    let mut hashes = c.hashes.clone();
    let base = c.hashes.len();
    for i in 0..n_add {
        hashes.push(0xBEEF_0000_u64.wrapping_add((base + i) as u64));
        vectors.push(make_unit_vec(0xBEEF_0000 + base + i));
    }
    (vectors, hashes)
}

/// Drop the last `n_remove` entries from the cached corpus (no adds).
/// HNSW `remove()` relinks neighbours; this isolates that cost from
/// the `add()` cost the balanced scenarios mix in.
fn truncated(n_remove: usize) -> (Vec<Vec<f32>>, Vec<u64>) {
    let c = corpus();
    let keep = c.hashes.len() - n_remove;
    (c.vectors[..keep].to_vec(), c.hashes[..keep].to_vec())
}

fn bench_b12(c: &mut Criterion) {
    let n = corpus_size();
    eprintln!("perf_b12: corpus_size = {n} (set VEX_BENCH_CORPUS_SIZE to override)");

    // Pre-build the corpus on the main thread so OnceLock init doesn't
    // accidentally land inside a `measurement` block.
    let _ = corpus();

    // Pre-compute churn cohorts before defining benches so the cost
    // doesn't land inside any iter_batched closure.
    let churn_1pct = n / 100;
    let churn_5pct = n / 20;
    let churn_10pct = n / 10;
    let churn_25pct = n / 4;
    // Just over 25% — strict-GT semantics put 26% in the fallback.
    let churn_over_threshold = (n / 4) + 1;

    let (v_1pct, h_1pct) = churned_balanced(churn_1pct);
    let (v_add_5pct, h_add_5pct) = appended(churn_5pct);
    let (v_rm_5pct, h_rm_5pct) = truncated(churn_5pct);
    let (v_10pct, h_10pct) = churned_balanced(churn_10pct);
    let (v_25pct, h_25pct) = churned_balanced(churn_25pct);
    let (v_over, h_over) = churned_balanced(churn_over_threshold);

    let mut group = c.benchmark_group("hnsw_incremental_vs_full");
    // At 25k corpus full-rebuild crosses 30s/iter — the Criterion
    // default 100-sample run would take ~50 min. Cap expensive
    // scenarios at 10 samples (Criterion minimum) while keeping
    // cheap fallback scenarios at the default for tight CIs.
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    // ─── Baseline: full rebuild via `build_hnsw_at` ───────────────────
    group.bench_function("full_rebuild_baseline", |b| {
        let cor = corpus();
        b.iter_batched(
            || TempDir::new().expect("tempdir"),
            |tmp| {
                let hnsw_path = tmp.path().join("index.hnsw");
                let hash_path = tmp.path().join("index.hashes");
                write_hnsw(&hnsw_path, &cor.vectors, &cor.hashes);
                vex::search::hash_index::save(&hash_path, &cor.hashes).expect("save sidecar");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // ─── 0% churn: load + save round trip floor ───────────────────────
    group.bench_function("incremental_no_change", |b| {
        let cor = corpus();
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &cor.vectors, &cor.hashes);
                assert!(applied, "no-change incremental must apply");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // ─── 1% balanced churn ────────────────────────────────────────────
    group.bench_function("incremental_1pct_churn", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &v_1pct, &h_1pct);
                assert!(applied, "1% churn must apply");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // ─── 5% pure add (no removes) ─────────────────────────────────────
    group.bench_function("incremental_5pct_pure_add", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &v_add_5pct, &h_add_5pct);
                assert!(applied, "pure-add must apply (zero removes)");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // ─── 5% pure remove (no adds) — isolates HNSW relink cost ─────────
    group.bench_function("incremental_5pct_pure_remove", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &v_rm_5pct, &h_rm_5pct);
                assert!(applied, "pure-remove 5% below threshold must apply");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // ─── 10% balanced churn ───────────────────────────────────────────
    group.bench_function("incremental_10pct_churn", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &v_10pct, &h_10pct);
                assert!(applied, "10% churn must apply");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // ─── Exactly 25% (strict-GT boundary) ─────────────────────────────
    group.bench_function("incremental_25pct_churn", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &v_25pct, &h_25pct);
                assert!(applied, "exactly 25% must apply (strict-GT)");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // ─── 26% — bails to Ok(false); measures bail cost in isolation ────
    group.bench_function("incremental_26pct_falls_back", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &v_over, &h_over);
                assert!(!applied, "over-threshold must fall back");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // ─── End-to-end orchestrator on >25% churn ────────────────────────
    // What the user actually pays when churn busts the threshold: the
    // incremental bail plus the unavoidable full rebuild that follows.
    // Should be ~= bail + full_rebuild_baseline; if it's materially
    // larger we have a double-work bug.
    group.bench_function("fallback_then_full_rebuild", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &v_over, &h_over);
                assert!(!applied);
                // Mirror what `pipeline::update` does on Ok(false): call
                // build_hnsw with the same `new_vectors` / `new_hashes`.
                write_hnsw(&hnsw_path, &v_over, &h_over);
                vex::search::hash_index::save(&hash_path, &h_over).expect("save sidecar");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_b12);
criterion_main!(benches);
