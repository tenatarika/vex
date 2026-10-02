//! V9-FORMAT P3 bench (`docs/V9-FORMAT.md` §3.3, §8) — follows the
//! `benches/bundle.rs` shape (`cargo bench --bench graph_v9`).
//!
//! Four questions:
//!
//! 1. **CSR callees lookup vs the legacy v8 FST oracle** — one
//!    `CsrView::neighbors(s)` call vs one `FstOracleReader::find_decimal_key(s)`
//!    call, same underlying edge list. Gate: CSR lookup <= 10 ns (§3.1/§13 R16).
//! 2. **`ref_edges` offsets lookup vs the legacy v8 FST oracle** — same
//!    shape, the identity (`edge_idx`-elided) CSR variant.
//! 3. **`build_csr` vs the legacy `build_u32_keyed_fst`** — one-time
//!    build cost on the same random edge list.
//! 4. **Projection + Leiden on a synthetic 35k-node / 200k-site graph**,
//!    generated from a fixed LCG (deterministic, no external `rand`
//!    dependency needed for reproducibility across runs/machines).
//!    Gate: <= 250 ms total (§3.3's clustering budget).
//!
//! This bench supersedes the local `examples/*_measure.rs` experiments
//! (§8) — those stay maintainer-owned, gitignored, and untouched by this
//! phase.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use vex::cluster::projection::{
    ProjectionCallEdge, ProjectionHierarchyEdge, ProjectionInput, ProjectionRefEdge,
    ProjectionSymbol,
};
use vex::cluster::{self, leiden};
use vex::parse::language::Language;
use vex::store::csr::{self, CsrView};
use vex::store::legacy_v8::{self, FstOracleReader};

// ---------------------------------------------------------------------------
// Deterministic LCG — no external `rand` dependency, reproducible across
// machines and runs (same seed -> same sequence, forever).
// ---------------------------------------------------------------------------

struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed)
    }

    fn next_u64(&mut self) -> u64 {
        // Numerical Recipes LCG constants — fine for synthetic bench
        // data, not for anything security-sensitive.
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    fn next_u32_below(&mut self, bound: u32) -> u32 {
        (self.next_u64() % u64::from(bound)) as u32
    }
}

// ---------------------------------------------------------------------------
// 1 + 2: CSR lookup vs legacy v8 FST oracle (callees + ref_edges shapes)
// ---------------------------------------------------------------------------

/// Realistic callees density: ~5,000 symbols, ~36,000 call edges (the
/// `docs/V9-FORMAT.md` F3 "about 36k edges" figure).
const N_SYMBOLS: u32 = 5_000;
const N_CALL_EDGES: usize = 36_000;

fn build_callees_edge_list(seed: u64) -> Vec<(u32, u32)> {
    let mut lcg = Lcg::new(seed);
    (0..N_CALL_EDGES as u32)
        .map(|idx| (lcg.next_u32_below(N_SYMBOLS), idx))
        .collect()
}

fn bench_callees_lookup_csr_vs_legacy(c: &mut Criterion) {
    let entries = build_callees_edge_list(0x0C51_1EE5);
    let keys: Vec<u32> = entries.iter().map(|&(k, _)| k).collect();

    let (offsets, edge_idx) = csr::build_csr(&keys, N_SYMBOLS).expect("build_csr");
    let offsets_bytes = csr::encode_le_u32s(&offsets);
    let edge_bytes = csr::encode_le_u32s(&edge_idx);
    let m = edge_idx.len() as u32;
    let csr_view =
        CsrView::new(&offsets_bytes, Some(&edge_bytes), N_SYMBOLS, m).expect("CsrView::new");

    let (fst_bytes, posting_bytes) =
        legacy_v8::build_u32_keyed_fst(entries).expect("build_u32_keyed_fst");
    let oracle = FstOracleReader::new(&fst_bytes, &posting_bytes).expect("FstOracleReader::new");

    c.bench_function("graph_v9::callees_lookup_csr", |b| {
        let mut s: u32 = 0;
        b.iter(|| {
            s = (s + 1) % N_SYMBOLS;
            black_box(csr_view.neighbors(black_box(s)).count())
        });
    });

    c.bench_function("graph_v9::callees_lookup_legacy_fst_oracle", |b| {
        let mut s: u32 = 0;
        b.iter(|| {
            s = (s + 1) % N_SYMBOLS;
            black_box(oracle.find_decimal_key(black_box(s)).len())
        });
    });
}

fn bench_ref_edges_lookup_csr_vs_legacy(c: &mut Criterion) {
    let mut lcg = Lcg::new(0xBEEF_F00D);
    let mut keys: Vec<u32> = (0..N_CALL_EDGES as u32)
        .map(|_| lcg.next_u32_below(N_SYMBOLS))
        .collect();
    keys.sort_unstable(); // ref_edges' CSR shape requires sorted keys

    let offsets =
        csr::build_csr_offsets_sorted(&keys, N_SYMBOLS).expect("build_csr_offsets_sorted");
    let offsets_bytes = csr::encode_le_u32s(&offsets);
    let m = *offsets.last().unwrap();
    let csr_view = CsrView::new(&offsets_bytes, None, N_SYMBOLS, m).expect("CsrView::new");

    let entries: Vec<(u32, u32)> = keys
        .iter()
        .enumerate()
        .map(|(i, &k)| (k, i as u32))
        .collect();
    let (fst_bytes, posting_bytes) =
        legacy_v8::build_ref_edges_fst(&entries).expect("build_ref_edges_fst");
    let oracle = FstOracleReader::new(&fst_bytes, &posting_bytes).expect("FstOracleReader::new");

    c.bench_function("graph_v9::ref_edges_lookup_csr", |b| {
        let mut s: u32 = 0;
        b.iter(|| {
            s = (s + 1) % N_SYMBOLS;
            black_box(csr_view.neighbors(black_box(s)).count())
        });
    });

    c.bench_function("graph_v9::ref_edges_lookup_legacy_fst_oracle", |b| {
        let mut s: u32 = 0;
        b.iter(|| {
            s = (s + 1) % N_SYMBOLS;
            black_box(oracle.find_decimal_key(black_box(s)).len())
        });
    });
}

// ---------------------------------------------------------------------------
// 3: build_csr vs the legacy build_u32_keyed_fst
// ---------------------------------------------------------------------------

fn bench_build_csr_vs_legacy(c: &mut Criterion) {
    let entries = build_callees_edge_list(0x5EED_1234);
    let keys: Vec<u32> = entries.iter().map(|&(k, _)| k).collect();

    c.bench_function("graph_v9::build_csr", |b| {
        b.iter(|| black_box(csr::build_csr(black_box(&keys), N_SYMBOLS).unwrap()));
    });

    c.bench_function("graph_v9::build_legacy_u32_keyed_fst", |b| {
        b.iter(|| black_box(legacy_v8::build_u32_keyed_fst(black_box(entries.clone())).unwrap()));
    });
}

// ---------------------------------------------------------------------------
// 4: projection + Leiden on a synthetic 35k-node / 200k-site graph
// ---------------------------------------------------------------------------

const SYNTH_SYMBOLS: u32 = 35_000;
const SYNTH_SITES: usize = 200_000;

/// Build a synthetic `ProjectionInput`-shaped corpus: `SYNTH_SYMBOLS`
/// symbols spread over ~1,000 synthetic files (~35 symbols/file, a
/// realistic file size), and `SYNTH_SITES` call edges wired through
/// `ProjectionCallEdge` (same-file-smallest / project-wide-unique
/// resolution, exactly as a real corpus would resolve) from a fixed LCG.
///
/// Holds plain owned data (not `ProjectionSymbol<'a>`/`ProjectionCallEdge<'a>`
/// directly — those now borrow `path`/`name`/`callee_name` as `&str`, §13
/// perf review, which would make this struct self-referential). The
/// borrowed `Projection*` Vecs are rebuilt from this data once per corpus
/// in `report_and_bench_projection_leiden`, which outlives the bench loop.
struct SynthCorpus {
    sym_file_id: Vec<u32>,
    sym_line: Vec<u32>,
    sym_names: Vec<String>,
    call_caller: Vec<u32>,
    call_line: Vec<u32>,
    call_callee_names: Vec<String>,
    ref_edges: Vec<ProjectionRefEdge>,
    ambiguous: Vec<bool>,
    hierarchy_edges: Vec<ProjectionHierarchyEdge>,
    file_paths: Vec<String>,
}

/// Uniform-random edges over the whole node space — kept as an explicit
/// **worst-case stress** case (P3 follow-up #3): almost no two sites land
/// on the same `(file, line, to)` key and almost no pair repeats, so
/// `PAIR_CAP`/dedup barely reduce the graph (measured: 199,737 distinct
/// pairs survive out of 200,000 raw sites — see the report). A real
/// codebase's call graph is nowhere near this adversarial; this case is
/// report-only, not what the <= 250ms budget is measured against (see
/// [`build_synthetic_corpus_planted`] for that).
fn build_synthetic_corpus_random(seed: u64) -> SynthCorpus {
    let mut lcg = Lcg::new(seed);
    const SYMS_PER_FILE: u32 = 35;
    let n_files = SYNTH_SYMBOLS.div_ceil(SYMS_PER_FILE);

    let mut sym_file_id = Vec::with_capacity(SYNTH_SYMBOLS as usize);
    let mut sym_line = Vec::with_capacity(SYNTH_SYMBOLS as usize);
    let mut sym_names = Vec::with_capacity(SYNTH_SYMBOLS as usize);
    let mut file_paths = Vec::with_capacity(n_files as usize);
    for f in 0..n_files {
        file_paths.push(format!("src/synth_{f:05}.rs"));
    }
    for sym_idx in 0..SYNTH_SYMBOLS {
        sym_file_id.push(sym_idx / SYMS_PER_FILE);
        sym_line.push((sym_idx % SYMS_PER_FILE) + 1);
        sym_names.push(format!("fn_{sym_idx}"));
    }

    // Call edges: resolved by caller sym_idx calling a per-file-unique
    // name (name is globally unique here, so "exactly one eligible
    // candidate project-wide" always resolves — this bench measures
    // projection+Leiden throughput, not call-name-resolution edge cases,
    // which `src/cluster/projection.rs`'s unit tests already cover).
    let mut call_caller = Vec::with_capacity(SYNTH_SITES);
    let mut call_line = Vec::with_capacity(SYNTH_SITES);
    let mut call_callee_names = Vec::with_capacity(SYNTH_SITES);
    for i in 0..SYNTH_SITES as u32 {
        let caller = lcg.next_u32_below(SYNTH_SYMBOLS);
        let callee = lcg.next_u32_below(SYNTH_SYMBOLS);
        call_caller.push(caller);
        call_line.push(i + 1);
        call_callee_names.push(format!("fn_{callee}"));
    }

    SynthCorpus {
        sym_file_id,
        sym_line,
        sym_names,
        call_caller,
        call_line,
        call_callee_names,
        ref_edges: Vec::new(),
        ambiguous: Vec::new(),
        hierarchy_edges: Vec::new(),
        file_paths,
    }
}

/// Number of global "hub" utility functions in the planted-community
/// generator — a small fixed set every community's edges can reach into,
/// simulating common utilities (logging, string helpers, etc.) that real
/// codebases call from everywhere.
const NUM_HUBS: u32 = 80;

/// Planted-community synthetic generator (P3 follow-up #3): partitions
/// `SYNTH_SYMBOLS` nodes into communities of size 5..=60 (uniform-random
/// per community via the fixed LCG), then draws `SYNTH_SITES` call
/// edges with 80% landing *inside* the caller's own community and 20%
/// landing on one of [`NUM_HUBS`] global hub functions, picked with a
/// power-law-ish bias toward a few popular hubs (the min of two uniform
/// draws over the hub rank — skews density toward rank 0 without
/// floating point). This is what `docs/V9-FORMAT.md` §3.3's own
/// complexity estimate ("~120k undirected pairs after dedup" from 200k
/// raw sites) was modeled on: real call graphs cluster around files and
/// a handful of hot utility functions, which both the `(file, line, to)`
/// site dedup and `PAIR_CAP` were designed to compress. **This is the
/// case the <= 250ms clustering budget is measured against** — not the
/// uniform-random stress case above.
fn build_synthetic_corpus_planted(seed: u64) -> SynthCorpus {
    let mut lcg = Lcg::new(seed);
    const SYMS_PER_FILE: u32 = 35;
    let n_files = SYNTH_SYMBOLS.div_ceil(SYMS_PER_FILE);
    let mut file_paths = Vec::with_capacity(n_files as usize);
    for f in 0..n_files {
        file_paths.push(format!("src/synth_{f:05}.rs"));
    }

    // Partition node ids 0..SYNTH_SYMBOLS into communities of size 5..=60.
    let mut community_of: Vec<u32> = Vec::with_capacity(SYNTH_SYMBOLS as usize);
    let mut community_bounds: Vec<(u32, u32)> = Vec::new();
    let mut next_node = 0u32;
    while next_node < SYNTH_SYMBOLS {
        let size = 5 + lcg.next_u32_below(56); // 5..=60
        let end = (next_node + size).min(SYNTH_SYMBOLS);
        community_bounds.push((next_node, end));
        for _ in next_node..end {
            community_of.push(community_bounds.len() as u32 - 1);
        }
        next_node = end;
    }

    let mut sym_file_id = Vec::with_capacity(SYNTH_SYMBOLS as usize);
    let mut sym_line = Vec::with_capacity(SYNTH_SYMBOLS as usize);
    let mut sym_names = Vec::with_capacity(SYNTH_SYMBOLS as usize);
    for sym_idx in 0..SYNTH_SYMBOLS {
        sym_file_id.push(sym_idx / SYMS_PER_FILE);
        sym_line.push((sym_idx % SYMS_PER_FILE) + 1);
        sym_names.push(format!("fn_{sym_idx}"));
    }

    let hubs: Vec<u32> = (0..NUM_HUBS)
        .map(|_| lcg.next_u32_below(SYNTH_SYMBOLS))
        .collect();

    let mut call_caller = Vec::with_capacity(SYNTH_SITES);
    let mut call_line = Vec::with_capacity(SYNTH_SITES);
    let mut call_callee_names = Vec::with_capacity(SYNTH_SITES);
    for i in 0..SYNTH_SITES as u32 {
        let caller = lcg.next_u32_below(SYNTH_SYMBOLS);
        let roll = lcg.next_u32_below(100);
        let callee = if roll < 80 {
            // 80%: intra-community edge.
            let cid = community_of[caller as usize] as usize;
            let (start, end) = community_bounds[cid];
            start + lcg.next_u32_below(end - start)
        } else {
            // 20%: a global hub, rank biased toward 0 (min of two
            // uniform draws over 0..NUM_HUBS skews density low).
            let a = lcg.next_u32_below(NUM_HUBS);
            let b = lcg.next_u32_below(NUM_HUBS);
            hubs[a.min(b) as usize]
        };
        call_caller.push(caller);
        call_line.push(i + 1);
        call_callee_names.push(format!("fn_{callee}"));
    }

    SynthCorpus {
        sym_file_id,
        sym_line,
        sym_names,
        call_caller,
        call_line,
        call_callee_names,
        ref_edges: Vec::new(),
        ambiguous: Vec::new(),
        hierarchy_edges: Vec::new(),
        file_paths,
    }
}

/// Rebuilds the borrowed `Projection*` Vecs from a `SynthCorpus` — shared
/// by every bench function below so each one borrows straight from the
/// corpus's owned strings (no further clone) instead of duplicating this
/// mapping per bench.
fn build_projection_vecs(
    corpus: &SynthCorpus,
) -> (Vec<ProjectionSymbol<'_>>, Vec<ProjectionCallEdge<'_>>) {
    let symbols: Vec<ProjectionSymbol<'_>> = (0..SYNTH_SYMBOLS as usize)
        .map(|i| ProjectionSymbol {
            sym_idx: i as u32,
            path: &corpus.file_paths[corpus.sym_file_id[i] as usize],
            line: corpus.sym_line[i],
            kind: 0, // Function — always eligible
            name: &corpus.sym_names[i],
            language: Some(Language::Rust),
        })
        .collect();
    let call_edges: Vec<ProjectionCallEdge<'_>> = (0..corpus.call_caller.len())
        .map(|i| ProjectionCallEdge {
            caller_sym_idx: corpus.call_caller[i],
            callee_name: &corpus.call_callee_names[i],
            line: corpus.call_line[i],
        })
        .collect();
    (symbols, call_edges)
}

/// Runs projection+Leiden once (unmeasured, for the eprintln report)
/// plus the criterion-measured loop, for a given corpus/label. Shared by
/// both the stress and planted-community benches so the two report
/// lines are directly comparable.
fn report_and_bench_projection_leiden(
    c: &mut Criterion,
    bench_name: &str,
    label: &str,
    corpus: &SynthCorpus,
) {
    let (symbols, call_edges) = build_projection_vecs(corpus);
    let input = ProjectionInput {
        symbol_count: SYNTH_SYMBOLS,
        symbols: &symbols,
        call_edges: &call_edges,
        ref_edges: &corpus.ref_edges,
        ambiguous: &corpus.ambiguous,
        hierarchy_edges: &corpus.hierarchy_edges,
        file_paths: &corpus.file_paths,
    };

    // Report the numbers once outside the measured loop too, so `cargo
    // bench -- --nocapture` output states the gate comparison plainly.
    // Report-only: no assert here, matching this repo's bench convention
    // (see e.g. benches/bundle.rs) of reporting numbers against a gate
    // in text rather than failing the bench run.
    let projected = vex::cluster::projection::project(&input);
    let t0 = std::time::Instant::now();
    let _ = leiden::run(&projected.graph, leiden::Resolution::DEFAULT);
    let leiden_only = t0.elapsed();
    let t1 = std::time::Instant::now();
    let out = cluster::cluster(&input, (1, 8));
    let total = t1.elapsed();
    eprintln!(
        "graph_v9 [{label}]: {SYNTH_SYMBOLS} nodes / {SYNTH_SITES} sites -> \
         leiden-only {leiden_only:?}, projection+leiden+finalize {total:?} (gate: <= 250ms), \
         clusters={} levels={} iter_cap_hit={} graph_edges={}",
        out.clusters.len(),
        out.levels,
        out.iter_cap_hit,
        projected.graph.edge_count(),
    );

    c.bench_function(bench_name, |b| {
        b.iter(|| black_box(cluster::cluster(black_box(&input), (1, 8))));
    });
}

fn bench_projection_and_leiden_synthetic_random_stress(c: &mut Criterion) {
    let corpus = build_synthetic_corpus_random(0x51A5_5EED);
    report_and_bench_projection_leiden(
        c,
        "graph_v9::projection_plus_leiden_synthetic_35k_200k_random_stress",
        "uniform-random, worst-case stress",
        &corpus,
    );
}

fn bench_projection_and_leiden_synthetic_planted(c: &mut Criterion) {
    let corpus = build_synthetic_corpus_planted(0xC0FF_EE11);
    report_and_bench_projection_leiden(
        c,
        "graph_v9::projection_plus_leiden_synthetic_35k_200k_planted",
        "planted-community, gate target",
        &corpus,
    );
}

/// `project()` alone (no Leiden, no finalize) on the planted-community
/// corpus — isolates the projection cost the rest of this bench file
/// only measures bundled with Leiden, so a projection-only regression
/// (or improvement) is visible without the clustering noise on top.
fn bench_project_only_synthetic_planted(c: &mut Criterion) {
    let corpus = build_synthetic_corpus_planted(0xC0FF_EE11);
    let (symbols, call_edges) = build_projection_vecs(&corpus);
    let input = ProjectionInput {
        symbol_count: SYNTH_SYMBOLS,
        symbols: &symbols,
        call_edges: &call_edges,
        ref_edges: &corpus.ref_edges,
        ambiguous: &corpus.ambiguous,
        hierarchy_edges: &corpus.hierarchy_edges,
        file_paths: &corpus.file_paths,
    };

    c.bench_function("graph_v9::project_only_synthetic_35k_200k_planted", |b| {
        b.iter(|| black_box(vex::cluster::projection::project(black_box(&input))));
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = bench_callees_lookup_csr_vs_legacy,
        bench_ref_edges_lookup_csr_vs_legacy,
        bench_build_csr_vs_legacy,
        bench_projection_and_leiden_synthetic_random_stress,
        bench_projection_and_leiden_synthetic_planted,
        bench_project_only_synthetic_planted,
}
criterion_main!(benches);
