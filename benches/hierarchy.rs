//! Typed hierarchy edges — P4 bench (`docs/HIERARCHY-EDGES.md` §8 P4).
//!
//! Three questions:
//!
//! 1. **Is `vex implementations` actually faster off the v8 index than
//!    the pre-P3 live tree-sitter walk?** `implementations_index` (one
//!    FST lookup + one `find_hierarchy_edges_by_symbol` binary search)
//!    vs `implementations_live_walk` (`crate::hierarchy::find_implementations`,
//!    a full parallel re-parse of every corpus file), same corpus, same
//!    query.
//! 2. **What does the query-time transitive BFS (`vex subtypes`) cost**
//!    on a real multi-hop inheritance chain? `subtypes_bfs` walks a
//!    20-level chain end to end.
//! 3. Both benches share one corpus, built once via [`OnceLock`] (index
//!    build cost is amortized, matching `benches/grep_trigram.rs`).
//!
//! Run: `cargo bench --bench hierarchy`.
//!
//! Symbol-name -> index resolution: `cmd_implementations::resolve_name_to_indices`
//! and the BFS core `cmd_subtypes::transitive_subtypes` are both
//! `pub(crate)` (private to the `vex` binary/lib boundary as seen from an
//! external `benches/` crate), so this file does NOT call them. Instead
//! it uses the same public primitives they're built from:
//! `IndexReader::symbol_fst_reader()` / `SymbolFstReader::find` for name
//! resolution (mirrors `resolve_name_to_indices`'s exact lookup, minus
//! the case-insensitive re-check, which does not matter for these
//! benches' exact-case fixture names), and `IndexReader::find_hierarchy_edges_by_symbol`
//! plus a small local BFS for the transitive walk (mirrors
//! `transitive_subtypes`'s cycle-guard + depth-cap shape). No `src/`
//! visibility was widened for this bench — see the final QA report.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::OnceLock;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tempfile::TempDir;

use vex::index::pipeline;
use vex::store::format::EdgeKind;
use vex::store::reader::IndexReader;
use vex::util::config;

/// Number of direct implementers of the base trait `HubTrait`, for the
/// `implementations_*` benches. Large enough that the live walk has to
/// genuinely scan every file; small enough the whole bench suite stays
/// fast.
const N_IMPLEMENTERS: usize = 150;

/// Depth of the linear inheritance chain `Level0 <- Level1 <- ... <-
/// LevelN` used by `subtypes_bfs`, so the BFS actually performs multi-hop
/// work rather than resolving in one step.
const CHAIN_DEPTH: usize = 20;

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    index_path: PathBuf,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();

fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(|| {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");
        config::set_cache_override(root.join(".vex-bench-cache"), false);
        std::fs::create_dir_all(root.join("src")).unwrap();

        // Fixture 1: one base trait + N implementers, one `impl HubTrait
        // for ImplN {}` per file. `queries::inheritance_query(Rust)`
        // matches `(impl_item trait: (type_identifier) @base type:
        // (type_identifier) @child)` — exactly this shape — and tags it
        // `EdgeKind::Extends` (Rust's relation label is "impl", mapped to
        // Extends per `hierarchy::extract::relation_to_edge_kind`).
        std::fs::write(
            root.join("src").join("hub_trait.rs"),
            "pub trait HubTrait {\n    fn hub_method(&self);\n}\n",
        )
        .unwrap();
        for i in 0..N_IMPLEMENTERS {
            let body = format!(
                "pub struct Impl{i};\n\nimpl HubTrait for Impl{i} {{\n    fn hub_method(&self) {{}}\n}}\n"
            );
            std::fs::write(
                root.join("src").join(format!("impl_{i}.rs")),
                body,
            )
            .unwrap();
        }

        // Fixture 2: a linear 20-level chain (`Level0 <- Level1 <- ... <-
        // Level20`) so BFS actually recurses multiple hops. Rust's
        // `inheritance_query` only captures `impl_item` (trait-for-struct),
        // which cannot express a pure struct/trait chain of arbitrary
        // depth: each level's `to_sym_idx` (the extended parent) must be
        // *the same symbol* as the previous level's `from_sym_idx` (the
        // child), which only works if child and parent are the same kind
        // of thing across levels. Python's `class_definition` with
        // `superclasses` gives exactly that (`class LevelK(LevelK-1)`),
        // and its query (`queries.rs:40-58`) is captured and mapped to
        // `EdgeKind::Extends` the same way as every other language, so
        // this is a real, index-persisted multi-hop chain, not a
        // Rust-specific artifact of the fixture.
        let mut chain_src = String::from("class Level0:\n    pass\n\n");
        for lvl in 1..=CHAIN_DEPTH {
            chain_src.push_str(&format!(
                "class Level{lvl}(Level{prev}):\n    pass\n\n",
                prev = lvl - 1
            ));
        }
        std::fs::write(root.join("src").join("chain.py"), chain_src).unwrap();

        pipeline::run(
            &root,
            pipeline::IndexOptions::default(),
            "minilm-l6-v2",
            &[],
        )
        .expect("pipeline::run");

        let index_path = config::index_path(&root);
        assert!(
            index_path.exists(),
            "index not found at expected cache path: {}",
            index_path.display()
        );

        Fixture {
            _tmp: tmp,
            root,
            index_path,
        }
    })
}

/// Mirrors `cmd_implementations::resolve_name_to_indices` using only
/// public `IndexReader` API (see module doc for why the private helper
/// itself isn't reachable from `benches/`).
fn resolve_name_to_indices(reader: &IndexReader, name: &str) -> Vec<u32> {
    let Some(sym_fst) = reader.symbol_fst_reader() else {
        return Vec::new();
    };
    sym_fst.find(name)
}

/// Mirrors `cmd_subtypes::transitive_subtypes`'s cycle-guard + depth-cap
/// BFS shape using only public `IndexReader` API.
fn transitive_subtypes_bfs(
    reader: &IndexReader,
    starts: &[u32],
    depth_cap: usize,
) -> Vec<(u32, usize)> {
    let mut visited: HashSet<u32> = starts.iter().copied().collect();
    let mut queue: VecDeque<(u32, usize)> = starts.iter().map(|&s| (s, 0)).collect();
    let mut out = Vec::new();

    while let Some((parent, parent_depth)) = queue.pop_front() {
        if parent_depth >= depth_cap {
            continue;
        }
        let child_depth = parent_depth + 1;
        for edge in reader.find_hierarchy_edges_by_symbol(parent) {
            let Ok(kind) = EdgeKind::try_from(edge.edge_kind_bits()) else {
                continue;
            };
            if !matches!(kind, EdgeKind::Extends | EdgeKind::Implements) {
                continue;
            }
            let child = edge.from_sym_idx;
            if !visited.insert(child) {
                continue;
            }
            out.push((child, child_depth));
            queue.push_back((child, child_depth));
        }
    }
    out
}

fn bench_implementations_index(c: &mut Criterion) {
    let fx = fixture();
    let reader = IndexReader::open(&fx.index_path).expect("open index");
    assert!(
        reader.has_hierarchy_edges(),
        "fixture must produce a non-empty hierarchy_edges section"
    );

    // Sanity: the index path must find all N_IMPLEMENTERS direct
    // children of HubTrait, proving the fixture's `impl HubTrait for
    // ImplN` shape actually persisted as Extends edges.
    let starts = resolve_name_to_indices(&reader, "HubTrait");
    assert_eq!(
        starts.len(),
        1,
        "HubTrait must resolve to exactly one symbol"
    );
    let direct: usize = starts
        .iter()
        .map(|&s| reader.find_hierarchy_edges_by_symbol(s).len())
        .sum();
    assert_eq!(
        direct, N_IMPLEMENTERS,
        "expected {N_IMPLEMENTERS} direct implementers via the index"
    );

    let mut group = c.benchmark_group("hierarchy");
    group.sample_size(20);
    group.bench_function("implementations_index", |b| {
        b.iter(|| {
            let starts = resolve_name_to_indices(&reader, "HubTrait");
            let total: usize = starts
                .iter()
                .map(|&s| reader.find_hierarchy_edges_by_symbol(s).len())
                .sum();
            black_box(total)
        })
    });
    group.finish();
}

fn bench_implementations_live_walk(c: &mut Criterion) {
    let fx = fixture();

    // Sanity: the live walk must find the same N_IMPLEMENTERS direct
    // implementers via the tree-sitter re-parse path, confirming both
    // benches answer the identical query against the identical corpus.
    let hits = vex::hierarchy::find_implementations(&fx.root, "HubTrait", usize::MAX, &[]).unwrap();
    assert_eq!(
        hits.len(),
        N_IMPLEMENTERS,
        "live walk must also find {N_IMPLEMENTERS} implementers"
    );

    let mut group = c.benchmark_group("hierarchy");
    group.sample_size(20);
    group.bench_function("implementations_live_walk", |b| {
        b.iter(|| {
            let hits = vex::hierarchy::find_implementations(&fx.root, "HubTrait", usize::MAX, &[])
                .unwrap();
            black_box(hits.len())
        })
    });
    group.finish();
}

fn bench_subtypes_bfs(c: &mut Criterion) {
    let fx = fixture();
    let reader = IndexReader::open(&fx.index_path).expect("open index");

    let starts = resolve_name_to_indices(&reader, "Level0");
    assert_eq!(starts.len(), 1, "Level0 must resolve to exactly one symbol");

    // Sanity: the chain must actually walk CHAIN_DEPTH hops — one
    // Extends edge per level (Level{K} extends Level{K-1}), so the BFS
    // from Level0 must discover exactly CHAIN_DEPTH descendants, one per
    // depth 1..=CHAIN_DEPTH.
    let hits = transitive_subtypes_bfs(&reader, &starts, 64);
    let max_depth = hits.iter().map(|(_, d)| *d).max().unwrap_or(0);
    assert_eq!(
        hits.len(),
        CHAIN_DEPTH,
        "expected exactly {CHAIN_DEPTH} descendants, got {} ({:?})",
        hits.len(),
        hits
    );
    assert_eq!(
        max_depth, CHAIN_DEPTH,
        "expected the BFS to reach exactly depth {CHAIN_DEPTH}, got {max_depth}"
    );

    let mut group = c.benchmark_group("hierarchy");
    group.sample_size(20);
    group.bench_function("subtypes_bfs", |b| {
        b.iter(|| {
            let hits = transitive_subtypes_bfs(&reader, &starts, 64);
            black_box(hits.len())
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_implementations_index,
    bench_implementations_live_walk,
    bench_subtypes_bfs,
);
criterion_main!(benches);
