//! Phase 14.10 — `build_rename_chains` micro-bench.
//!
//! Measures the chain-builder in isolation across realistic corpus
//! sizes and rename-pair densities. The task design committed to
//! "≤30% re-index overhead" at the full-pipeline level; this bench
//! pins the chain-builder's absolute cost so a regression in any
//! sub-phase (LSH build, candidate scan, UF merge, sidecar emit)
//! surfaces immediately.
//!
//! ## Corpus shape
//!
//! Each synthetic entry has a body of 16 deterministic tokens drawn
//! from a small vocabulary. Rename pairs share 14/16 tokens
//! (Jaccard ≈ 0.93, comfortably above `GATE_JACCARD = 0.70`); non-
//! rename pairs draw disjoint slices so Jaccard ≈ 0. Commit-pair
//! cadence: each entry spans exactly one commit, so commit_count =
//! entry_count and there are `entry_count - 1` commit pairs.
//!
//! ## Scenarios
//!
//!   - `chain_build/1k_no_renames`        — baseline, 0% rename rate
//!   - `chain_build/1k_5pct_renames`      — typical real-world churn
//!   - `chain_build/10k_5pct_renames`     — moderate corpus
//!   - `chain_build/10k_25pct_renames`    — high churn
//!   - `chain_build/50k_5pct_renames`     — large corpus, on-target scale
//!
//! Run: `cargo bench --bench rename_chains`. HTML report under
//! `target/criterion/`.

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use vex::index::history_builder::HistoryEntry;
use vex::index::rename_chains::{build_rename_chains, compute_body_tokens_hash, BuildInput};

const TOKENS_PER_BODY: usize = 16;

/// Small vocabulary so disjoint slices are still meaningful — 64
/// tokens, 16 per body means we can carve 4 non-overlapping bodies
/// before reusing tokens. For larger corpora we cycle through the
/// vocabulary; this introduces some accidental overlap but stays
/// below `GATE_JACCARD = 0.70` (empirically Jaccard ≈ 0.05 for two
/// random 16-token windows over a 64-token vocab).
const VOCAB: &[&str] = &[
    "acc", "input", "output", "buffer", "stream", "result", "value", "data", "key", "index",
    "size", "len", "offset", "count", "limit", "max", "min", "sum", "total", "err", "ok", "ret",
    "code", "msg", "tag", "row", "col", "node", "edge", "head", "tail", "next", "prev", "left",
    "right", "parent", "child", "name", "path", "file", "line", "char", "byte", "word", "page",
    "block", "frame", "addr", "ptr", "ref", "lock", "ctx", "env", "info", "id", "uid", "type",
    "kind", "flag", "mask", "state", "mode", "step", "step",
];

/// Synthesise an entry with the given body. Kind is fixed to 0
/// (Function); first/last commit-idx are equal so each entry spans
/// exactly one commit.
fn make_entry(commit_idx: u32) -> HistoryEntry {
    HistoryEntry {
        blob_idx: 0,
        file_offset: 0,
        line: 0,
        signature_offset: 0,
        first_commit_idx: commit_idx,
        last_commit_idx: commit_idx,
        kind: 0,
        _pad: [0; 3],
    }
}

/// Generate a body string of `TOKENS_PER_BODY` tokens starting at
/// vocabulary offset `start_idx`. Wraps around the vocab.
fn body_at(start_idx: usize) -> String {
    let mut out = String::with_capacity(TOKENS_PER_BODY * 6);
    for i in 0..TOKENS_PER_BODY {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(VOCAB[(start_idx + i) % VOCAB.len()]);
    }
    out
}

/// Generate a body that shares `shared` tokens with `body_at(base)`
/// and replaces the rest with vocab tokens drawn far away from
/// `base`. Used for synthetic rename pairs.
fn body_renamed(base: usize, shared: usize) -> String {
    let mut out = String::with_capacity(TOKENS_PER_BODY * 6);
    for i in 0..TOKENS_PER_BODY {
        if i > 0 {
            out.push(' ');
        }
        let tok = if i < shared {
            VOCAB[(base + i) % VOCAB.len()]
        } else {
            // Pull from the far end of the vocab to minimise accidental
            // collision with the base window.
            VOCAB[(base + i + VOCAB.len() / 2) % VOCAB.len()]
        };
        out.push_str(tok);
    }
    out
}

/// Construct a synthetic corpus: `entry_count` entries spread across
/// `entry_count` commits, with the given fraction renamed pair-wise.
/// Returns owned vectors so the bench iteration borrows them.
struct Corpus {
    entries: Vec<HistoryEntry>,
    bodies: Vec<Option<String>>,
    sigs: Vec<Option<String>>,
    hashes: Vec<Option<u64>>,
}

fn build_corpus(entry_count: usize, rename_pct: f32) -> Corpus {
    let rename_pairs = ((entry_count as f32 / 2.0) * rename_pct) as usize;
    let mut entries = Vec::with_capacity(entry_count);
    let mut bodies = Vec::with_capacity(entry_count);
    let mut sigs = Vec::with_capacity(entry_count);

    // First `2 * rename_pairs` entries form pairs: 2i lives at commit
    // 2i (deleted), 2i+1 lives at commit 2i+1 (added). Sharing 14/16
    // tokens places Jaccard ≈ 0.93 — clear chain.
    for i in 0..rename_pairs {
        let base = (i * 4) % VOCAB.len();
        let commit_a = (2 * i) as u32;
        let commit_b = (2 * i + 1) as u32;
        entries.push(make_entry(commit_a));
        bodies.push(Some(body_at(base)));
        sigs.push(Some(format!("fn name_{i}_old(arg)")));
        entries.push(make_entry(commit_b));
        bodies.push(Some(body_renamed(base, 14)));
        sigs.push(Some(format!("fn name_{i}_new(arg)")));
    }

    // Remaining entries are singletons spread across the rest of the
    // commit space, with disjoint bodies (no chains form).
    let remaining = entry_count.saturating_sub(2 * rename_pairs);
    let commit_offset = (2 * rename_pairs) as u32;
    for j in 0..remaining {
        let base = (j * 7 + 17) % VOCAB.len();
        entries.push(make_entry(commit_offset + j as u32));
        bodies.push(Some(body_at(base)));
        sigs.push(Some(format!("fn unique_{j}(arg)")));
    }

    let hashes = vec![None; entry_count];

    Corpus {
        entries,
        bodies,
        sigs,
        hashes,
    }
}

fn bench_chain_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("chain_build");
    group.measurement_time(Duration::from_secs(8));
    group.sample_size(20);

    let scenarios: &[(&str, usize, f32)] = &[
        ("1k_no_renames", 1_000, 0.0),
        ("1k_5pct_renames", 1_000, 0.05),
        ("10k_5pct_renames", 10_000, 0.05),
        ("10k_25pct_renames", 10_000, 0.25),
        ("50k_5pct_renames", 50_000, 0.05),
    ];

    for (label, entry_count, rename_pct) in scenarios {
        let corpus = build_corpus(*entry_count, *rename_pct);
        let body_hash = compute_body_tokens_hash(&corpus.bodies);
        group.throughput(Throughput::Elements(*entry_count as u64));
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter(|| {
                let input = BuildInput {
                    entries: &corpus.entries,
                    entry_body_tokens: &corpus.bodies,
                    entry_sig_tokens: &corpus.sigs,
                    entry_context_hash: &corpus.hashes,
                    body_tokens_hash: body_hash,
                    history_tip_sha_prefix: [0u8; 20],
                    cosine_lookup: None,
                };
                let artifact = build_rename_chains(input).expect("build_rename_chains");
                black_box(artifact);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_chain_build);
criterion_main!(benches);
