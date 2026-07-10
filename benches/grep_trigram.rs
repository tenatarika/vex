//! grep trigram skip-index — P4 bench + selectivity (STORAGE-RESEARCH §2).
//!
//! Two questions:
//!
//! 1. **Does the skip-index actually save wall-clock?** `skip_active` vs
//!    `full_walk` grep the same corpus for a rare literal; the only
//!    difference is whether `index.trigram` is on disk. The delta is the
//!    I/O + regex the skip avoids.
//! 2. **How selective is the 2048-bit / k=1 bloom, by literal length?**
//!    The fixture prints a false-positive-rate table (fraction of files
//!    whose bloom *fails* to exclude a literal that is genuinely absent).
//!    Lower is better; this is the data behind the M/k tuning decision
//!    and the short-literal caveat in `docs/LIMITATIONS.md`.
//!
//! Run: `cargo bench --bench grep_trigram`. The FP table prints to stderr
//! during fixture init.

use std::path::PathBuf;
use std::sync::OnceLock;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tempfile::TempDir;

use vex::grep;
use vex::grep::trigram::{required_trigrams, TrigramBloom, BLOOM_BITS, BLOOM_HASHES};
use vex::index::pipeline;
use vex::store::trigram as store_trigram;
use vex::util::config;

/// Number of distinct code files in the synthetic corpus.
const N_FILES: usize = 500;

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    trigram_path: PathBuf,
    /// The persisted sidecar bytes, so each bench can deterministically
    /// restore (skip active) or remove (full walk) it regardless of the
    /// order Criterion runs the two bench functions.
    sidecar_bytes: Vec<u8>,
    /// A literal present in exactly one corpus file — the rare-hit query.
    rare_literal: String,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();

fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(|| {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");
        config::set_cache_override(root.join(".vex-bench-cache"), false);
        std::fs::create_dir_all(root.join("src")).unwrap();

        // N distinct small files. Only file 0 contains the rare literal;
        // the rest have unique-but-unrelated identifiers so a grep for the
        // rare literal legitimately matches exactly one file — the case
        // the skip-index is built to exploit.
        let rare_literal = "zqxraremarker".to_string();
        for i in 0..N_FILES {
            let body = if i == 0 {
                format!("pub fn f{i}() {{ let {rare_literal} = {i}; }}\n")
            } else {
                format!("pub fn f{i}() {{ let common_field_{i} = {i}; }}\n")
            };
            std::fs::write(root.join("src").join(format!("f{i}.rs")), body).unwrap();
        }

        pipeline::run(
            &root,
            pipeline::IndexOptions::default(),
            "minilm-l6-v2",
            &[],
        )
        .expect("pipeline::run");

        let trigram_path = config::trigram_path(&root);
        let sidecar_bytes = std::fs::read(&trigram_path).expect("read sidecar");

        print_selectivity_report(&trigram_path);

        Fixture {
            _tmp: tmp,
            root,
            trigram_path,
            sidecar_bytes,
            rare_literal,
        }
    })
}

/// Print a false-positive-rate table: over every file's bloom, the
/// fraction that FAIL to exclude a genuinely-absent random literal of a
/// given length. A random `[a-z]{L}` string is almost surely absent from
/// every file, so any `might_contain_all == true` is a false positive
/// (the file would be needlessly read). FP → 0 as L grows.
fn print_selectivity_report(trigram_path: &std::path::Path) {
    let records = store_trigram::load(trigram_path).expect("load sidecar");
    let blooms: Vec<TrigramBloom> = records
        .iter()
        .map(|r| TrigramBloom::from_raw(r.bloom))
        .collect();

    // Deterministic LCG so the table is reproducible across runs (no rand
    // dep, no Math.random). Numerical Recipes constants.
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u32
    };
    let rand_literal = |n: usize, next: &mut dyn FnMut() -> u32| -> String {
        (0..n)
            .map(|_| (b'a' + (next() % 26) as u8) as char)
            .collect()
    };

    const TRIALS: usize = 200;
    eprintln!(
        "\n[grep-trigram selectivity] BLOOM_BITS={BLOOM_BITS} k={BLOOM_HASHES} \
         files={} — FP = fraction of files a random absent literal FAILS to skip:",
        blooms.len()
    );
    for len in [3usize, 4, 5, 6, 8, 12] {
        let mut checked = 0u64;
        let mut fp = 0u64;
        for _ in 0..TRIALS {
            let lit = rand_literal(len, &mut next);
            let Some(tris) = required_trigrams(&lit) else {
                continue;
            };
            for bloom in &blooms {
                checked += 1;
                if bloom.might_contain_all(&tris) {
                    fp += 1;
                }
            }
        }
        let pct = if checked > 0 {
            100.0 * fp as f64 / checked as f64
        } else {
            0.0
        };
        eprintln!("  len {len:2}: FP {pct:6.2}%  ({fp}/{checked})");
    }
    eprintln!();
}

/// Restore the sidecar (skip active) or remove it (full walk) so each
/// bench is deterministic regardless of run order.
fn set_sidecar(fx: &Fixture, present: bool) {
    if present {
        std::fs::write(&fx.trigram_path, &fx.sidecar_bytes).unwrap();
    } else {
        let _ = std::fs::remove_file(&fx.trigram_path);
    }
}

fn bench_skip_active(c: &mut Criterion) {
    let fx = fixture();
    set_sidecar(fx, true);
    // Sanity: the rare literal must still match exactly once WITH the skip
    // active (proves the skip-index didn't drop the real hit).
    let hits = grep::search(&fx.root, &fx.rare_literal, None, 100, &[]).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "skip active must still find the one real hit"
    );

    let mut group = c.benchmark_group("grep_trigram");
    group.sample_size(20);
    group.bench_function("skip_active_rare_literal", |b| {
        b.iter(|| {
            let hits = grep::search(&fx.root, &fx.rare_literal, None, 100, &[]).unwrap();
            black_box(hits.len())
        })
    });
    group.finish();
}

fn bench_full_walk(c: &mut Criterion) {
    let fx = fixture();
    set_sidecar(fx, false);
    // Same query, same result — only the I/O differs (every file read).
    let hits = grep::search(&fx.root, &fx.rare_literal, None, 100, &[]).unwrap();
    assert_eq!(hits.len(), 1);

    let mut group = c.benchmark_group("grep_trigram");
    group.sample_size(20);
    group.bench_function("full_walk_rare_literal", |b| {
        b.iter(|| {
            let hits = grep::search(&fx.root, &fx.rare_literal, None, 100, &[]).unwrap();
            black_box(hits.len())
        })
    });
    group.finish();
    // Leave the sidecar restored so a re-run starts clean.
    set_sidecar(fx, true);
}

criterion_group!(benches, bench_skip_active, bench_full_walk);
criterion_main!(benches);
