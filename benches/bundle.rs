//! Phase 13.2 Inc 7 — `vex bundle` latency baseline.
//!
//! First Criterion `.rs` benchmark in the repo. Three benches matching
//! the architect-review plan:
//!
//! 1. `bench_pr_impact_bfs_sequential` — pure synthetic BFS (no
//!    IndexReader). Decides whether `assemble_pr_impact` needs
//!    rayon-ization (A10: revisit if > 100 ms on 50 changed symbols ×
//!    depth=2).
//! 2. `bench_project_indegree_scan` — `top_n_by_indegree` on a real
//!    synthetic-project IndexReader (~500 functions, ~1000 call
//!    edges). Validates the A5 indegree-only descope is fast.
//! 3. `bench_symbol_assembly` — `assemble_symbol` end-to-end on the
//!    same fixture. Validates the symbol-mode pipeline (FST resolve +
//!    body extract + callers + callees + similar guard) without
//!    process-spawn overhead.
//!
//! Run with: `cargo bench --bench bundle`. Output also gets HTML
//! reports under `target/criterion/`.

use std::path::PathBuf;
use std::sync::OnceLock;

use assert_cmd::Command;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tempfile::TempDir;

use vex::callgraph::bfs::find_reachable;
use vex::callgraph::indegree::top_n_by_indegree;
use vex::callgraph::CallMatch;
use vex::cli::args::ScopeArgs;
use vex::cli::cmd_bundle::{self, BundleArgs, BundleCtx, BundleModeFlag};
use vex::store::reader::IndexReader;
use vex::util::config;

// ---------------------------------------------------------------------------
// Bench 1 — pr-impact BFS (synthetic graph, no IndexReader)
// ---------------------------------------------------------------------------

/// Build a synthetic adjacency that mimics the BFS load `pr-impact`
/// produces. Each changed symbol has 3 direct callers; each caller has
/// 3 of their own; depth=2 walk fans out to ~9 reachable per change.
/// 50 changes × 9 = 450 visits per bench iteration — a realistic
/// "medium PR" load.
fn build_synthetic_callers_of() -> impl Fn(&str) -> Vec<CallMatch> + Clone {
    use std::collections::HashMap;
    let mut adj: HashMap<String, Vec<CallMatch>> = HashMap::new();
    for change in 0..50 {
        let target = format!("changed_{change}");
        let direct: Vec<CallMatch> = (0..3)
            .map(|i| CallMatch {
                name: format!("caller_{change}_{i}"),
                path: format!("src/file_{change}.rs"),
                line: 10 * (i + 1),
            })
            .collect();
        // Each direct caller in turn has 3 callers of its own (depth-2
        // frontier). Insert those edges too so BFS at depth=2 walks
        // them.
        for d in &direct {
            let deeper: Vec<CallMatch> = (0..3)
                .map(|i| CallMatch {
                    name: format!("{}_caller_{i}", d.name),
                    path: "src/deeper.rs".to_string(),
                    line: 100 * (i + 1),
                })
                .collect();
            adj.insert(d.name.clone(), deeper);
        }
        adj.insert(target, direct);
    }
    move |name: &str| adj.get(name).cloned().unwrap_or_default()
}

fn bench_pr_impact_bfs_sequential(c: &mut Criterion) {
    let callers_of = build_synthetic_callers_of();
    // 50 changed symbols, depth=2, BFS cap 1024 — mirrors
    // PR_IMPACT_CALLERS_FETCH_CAP in cmd_bundle.rs and the depth=2
    // default surfaced by clap.
    c.bench_function("bundle::pr_impact::bfs_sequential_50x_depth2", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for change in 0..50 {
                let name = format!("changed_{change}");
                let r = find_reachable(&callers_of, &name, 2, 1024);
                total += r.len();
            }
            black_box(total)
        })
    });
}

// ---------------------------------------------------------------------------
// Bench 2 + 3 — indegree scan + symbol assembly on a real IndexReader
// ---------------------------------------------------------------------------

/// One-time fixture shared between the indegree and symbol benches.
/// Building a real index via `vex index` subprocess costs ~1-2s; we
/// pay it once at first access via [`OnceLock`].
struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    index_path: PathBuf,
    hnsw_path: PathBuf,
    /// A symbol name we know exists in the fixture — used as the
    /// `--symbol` target for `bench_symbol_assembly`.
    target_name: String,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();

fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(|| {
        let tmp = TempDir::new().expect("tempdir");
        // Canonicalize so `config::index_path` agrees with the
        // subprocess's `root.canonicalize()` before hashing — on macOS
        // `/var/folders/...` is a symlink under `/private/var/...`
        // and the hash differs by canonicalization.
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");

        std::fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();

        // Synthetic project: 200 functions in 5 files, each function
        // calls 2-3 others. Yields ~500 call edges and gives the
        // indegree distribution enough spread to exercise the sort
        // (some functions called 5+ times, some only once).
        for file_i in 0..5 {
            let mut src = String::new();
            for fn_i in 0..40 {
                let id = file_i * 40 + fn_i;
                // Each function calls 2 deterministic neighbors —
                // gives every callee 1-4 callers depending on the
                // permutation. Self-call avoided.
                let cb = (id + 7) % 200;
                let cc = (id + 13) % 200;
                src.push_str(&format!("pub fn fn_{id}() {{ fn_{cb}(); fn_{cc}(); }}\n"));
            }
            std::fs::write(root.join("src").join(format!("file_{file_i}.rs")), src).unwrap();
        }

        // Build the index via the dev binary. Disabling semantic keeps
        // the bench fixture cheap; symbol mode soft-degrades to empty
        // similar[] when has_vectors == false.
        //
        // Cache layout: the bench process needs `config::index_path`
        // to resolve to the same hashed location the subprocess writes
        // to. The subprocess reads `VEX_CACHE_DIR`; the bench process
        // honours `set_cache_override`. We install the same root via
        // both mechanisms so the lookup post-build agrees.
        let cache_dir = root.join(".vex-bench-cache");
        config::set_cache_override(cache_dir.clone(), false);
        let mut cmd = Command::cargo_bin("vex").expect("cargo_bin vex");
        cmd.current_dir(&root)
            .env("VEX_CACHE_DIR", &cache_dir)
            .args(["index"]);
        cmd.assert().success();

        let index_path = config::index_path(&root);
        let hnsw_path = config::hnsw_path(&root);
        assert!(
            index_path.exists(),
            "index not found at expected cache path: {}",
            index_path.display()
        );

        // (No chdir needed — `extract_body` now uses `ctx.root.join(...)`
        // so symbol-body resolution is cwd-independent. Review fix C1.)

        Fixture {
            target_name: "fn_42".to_string(),
            _tmp: tmp,
            root,
            index_path,
            hnsw_path,
        }
    })
}

fn bench_project_indegree_scan(c: &mut Criterion) {
    let fx = fixture();
    let reader = IndexReader::open(&fx.index_path).expect("open index");

    c.bench_function("bundle::project::indegree_scan_top30", |b| {
        b.iter(|| {
            let report = top_n_by_indegree(&reader, 30, None);
            black_box(report.rows.len() + report.total_ranked)
        })
    });
}

fn bench_symbol_assembly(c: &mut Criterion) {
    let fx = fixture();
    let reader = IndexReader::open(&fx.index_path).expect("open index");
    let scope = ScopeArgs::default();
    let excludes: Vec<String> = Vec::new();
    let ctx = BundleCtx {
        root: fx.root.clone(),
        scope: &scope,
        reader: &reader,
        hnsw_path: fx.hnsw_path.clone(),
        excludes: &excludes,
        // No semantic / find_similar path exercised by this bench fixture
        // (no vectors written); cosine fallback is the safe default.
        vectors_normalized: false,
    };
    let target: &str = &fx.target_name;
    let args = BundleArgs {
        mode: BundleModeFlag::Symbol,
        symbol: Some(target),
        base: None,
        depth: 2,
        path_glob: None,
        top_n: 30,
        callers_max: 10,
        callees_max: 10,
        similar_max: 5,
        tests_max: 20,
        directory_tree_only: false,
        directory_tree_top: 30,
    };

    c.bench_function("bundle::symbol::assemble_full_pipeline", |b| {
        b.iter(|| {
            let (resp, _) = cmd_bundle::assemble_symbol(&args, &ctx).expect("assemble_symbol");
            black_box(resp.items.len())
        })
    });
}

criterion_group!(
    benches,
    bench_pr_impact_bfs_sequential,
    bench_project_indegree_scan,
    bench_symbol_assembly,
);
criterion_main!(benches);
