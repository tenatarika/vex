//! v1.15.0 B1.2 — incremental HNSW update vs full-rebuild micro-bench.
//!
//! Compares `build_hnsw_at` (full rebuild) against the new
//! `build_hnsw_incremental_at` (load → diff → remove/add → save) at
//! representative churn rates. Both functions are `pub(super)` in
//! `src/index/pipeline/output.rs`; this bench inlines the usearch entry
//! points the same way (`tests/cli_incremental_hnsw_test.rs` does too)
//! to avoid widening the public API for a bench-only fixture.
//!
//! Corpus: 5000 384-dim one-hot-ish vectors. Hashes are sym_idx-derived
//! (`0xDEAD_0000 + i`) so they're deterministic and unique across runs.
//!
//! Churn scenarios:
//!   * `incremental_no_change`      — 0 add / 0 remove (load + save round-trip floor)
//!   * `incremental_1pct_churn`     — 50 added / 50 removed
//!   * `incremental_10pct_churn`    — 500 added / 500 removed
//!   * `incremental_25pct_churn`    — exactly at the tombstone threshold (1250 removed)
//!   * `incremental_26pct_churn`    — just over → falls back to Ok(false), measures bail cost
//!   * `full_rebuild_baseline`      — `build_hnsw_at` over the same 5000 vectors
//!
//! Run: `cargo bench --bench perf_b12`. Criterion HTML lands under
//! `target/criterion/`; the headline number is `incremental_*` /
//! `full_rebuild_baseline` ratio.

use std::path::Path;
use std::sync::OnceLock;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use tempfile::TempDir;

const DIM: usize = 384;
const CORPUS_SIZE: usize = 5_000;
const TOMBSTONE_NUMERATOR: usize = 1;
const TOMBSTONE_DENOMINATOR: usize = 4;

/// Cached deterministic corpus shared across every bench iteration.
/// Building the 5000-vector seed is ~20 ms (orthogonal-ish one-hots)
/// which we don't want amortised into every iteration.
struct Corpus {
    hashes: Vec<u64>,
    vectors: Vec<Vec<f32>>,
}

static CORPUS: OnceLock<Corpus> = OnceLock::new();

fn corpus() -> &'static Corpus {
    CORPUS.get_or_init(|| {
        let mut hashes = Vec::with_capacity(CORPUS_SIZE);
        let mut vectors = Vec::with_capacity(CORPUS_SIZE);
        for i in 0..CORPUS_SIZE {
            hashes.push(0xDEAD_0000_u64.wrapping_add(i as u64));
            let mut v = vec![0.0_f32; DIM];
            // Spread the one-hot slot across the dim so HNSW's
            // neighbour search has actual fan-out — a single slot
            // would degenerate to identical vectors and skew timings.
            v[i % DIM] = 1.0;
            vectors.push(v);
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

/// Inline copy of `build_hnsw_at` from the production code path —
/// `pub(super)` there, replicated here so the bench can drive the
/// full-rebuild path without widening the API. Stays in sync via the
/// `usearch::IndexOptions` shape; a parameter drift would surface as
/// dim-mismatch panic during the load round-trip.
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

/// Inline copy of the incremental path's load/diff/mutate/save core.
/// Returns whether the function would have taken the incremental
/// branch — mirrors the `Ok(true)` / `Ok(false)` contract of the
/// production `build_hnsw_incremental_at`.
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
        // Threshold exceeded → production code returns Ok(false) and
        // caller falls back to full rebuild. Bench just reports the
        // early-bail cost.
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

/// Mutate the cached corpus into a new (vectors, hashes) pair where
/// the first `churn` entries are replaced with fresh hashes — produces
/// exactly `churn` removes + `churn` adds against the baseline.
fn churned(churn: usize) -> (Vec<Vec<f32>>, Vec<u64>) {
    let c = corpus();
    let mut vectors = c.vectors.clone();
    let mut hashes = c.hashes.clone();
    for i in 0..churn {
        // Distinct hash range so no collision with baseline keys —
        // 0xBEEF_* sits well away from 0xDEAD_*.
        hashes[i] = 0xBEEF_0000_u64.wrapping_add(i as u64);
        // Rotate the one-hot slot so the new vector is orthogonal to
        // the slot the baseline used at this index.
        let new_slot = (i % DIM + DIM / 2) % DIM;
        let mut v = vec![0.0_f32; DIM];
        v[new_slot] = 1.0;
        vectors[i] = v;
    }
    (vectors, hashes)
}

fn bench_b12(c: &mut Criterion) {
    let mut group = c.benchmark_group("hnsw_incremental_vs_full");

    // Baseline: full rebuild via `build_hnsw_at`. Every iteration
    // writes a fresh HNSW + sidecar from scratch over all 5000
    // vectors. This is the cost B1.2 is trying to shave.
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

    // 0% churn: pure load + save round trip. Floor cost of the
    // incremental path — what you pay even when nothing changed.
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
                assert!(applied, "no-change incremental must apply, not fall back");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // 1% churn — 50 removes + 50 adds. The realistic editor-save
    // scenario; this is what the user sees on a single-file save in
    // a 5000-symbol project.
    let churn_1pct = CORPUS_SIZE / 100;
    let (v_1pct, h_1pct) = churned(churn_1pct);
    group.bench_function("incremental_1pct_churn", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &v_1pct, &h_1pct);
                assert!(applied, "1% churn must apply incrementally");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // 10% churn — 500 removes + 500 adds. Larger PR / branch-switch
    // scenario. Still below the tombstone threshold.
    let churn_10pct = CORPUS_SIZE / 10;
    let (v_10pct, h_10pct) = churned(churn_10pct);
    group.bench_function("incremental_10pct_churn", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &v_10pct, &h_10pct);
                assert!(applied, "10% churn must apply incrementally");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // Exactly 25% churn — strict-GT threshold pins this as the last
    // case before fallback. Measures the worst-case-still-applied path.
    let churn_25pct = CORPUS_SIZE / 4;
    let (v_25pct, h_25pct) = churned(churn_25pct);
    group.bench_function("incremental_25pct_churn", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &v_25pct, &h_25pct);
                assert!(applied, "exactly 25% must apply (strict-GT semantics)");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    // Just over 25% — bails to Ok(false). Measures the bail cost:
    // sidecar load + set diff + threshold check, no HNSW load.
    // Compare against full_rebuild_baseline to verify the fallback
    // doesn't pay double (bail + full rebuild ≈ same as full rebuild
    // alone, which is what the orchestrator ends up doing).
    let churn_over_threshold = (CORPUS_SIZE / 4) + 1; // 1251 of 5000 = 25.02%
    let (v_over, h_over) = churned(churn_over_threshold);
    group.bench_function("incremental_26pct_churn_falls_back", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let (hnsw_path, hash_path) = seed_baseline(tmp.path());
                (tmp, hnsw_path, hash_path)
            },
            |(tmp, hnsw_path, hash_path)| {
                let applied = incremental_apply(&hnsw_path, &hash_path, &v_over, &h_over);
                assert!(!applied, "over-threshold must trigger fallback");
                black_box(tmp)
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_b12);
criterion_main!(benches);
