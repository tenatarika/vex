//! Throwaway research bench: compare ort+CPU vs ort+CoreML EP for vex's
//! two registered embedders (MiniLM-L6-v2, jina-code).
//!
//! Runs on macOS today. The Swift sibling under `../swift/` will measure the
//! same corpus against Apple Core AI on macOS 27+ once `.aimodel` exports
//! exist — see `../swift/README.md`.
//!
//! Methodology:
//!   1. Cold-load each Embedder (records model_load_ms — includes any
//!      first-call EP warmup that ort defers until session creation).
//!   2. For each batch size: throw away ONE warmup batch at THAT bs so
//!      the ANE graph compile (deferred per input shape) isn't counted.
//!   3. For batch sizes 1, 8, 32: run the corpus chunked into batches of
//!      EXACTLY bs via `chunks_exact` (trailing partial chunks discarded
//!      so every timed batch has the same shape), ITERATIONS times.
//!      Record per-batch wall time, then aggregate:
//!        - `batch_wall_p50_ms` / `batch_wall_p95_ms` — wall time of ONE
//!          embed_batch() call. At bs=1 this IS per-embedding latency; at
//!          bs>1 it's batch-level. Don't divide by bs and call the result
//!          per-embedding latency — the model embeds the whole batch in
//!          parallel, so each text waits roughly the full batch time.
//!        - `throughput_emb_per_sec` — total embeddings ÷ total wall
//!          time. This IS per-embedding throughput regardless of how the
//!          model parallelises a batch.
//!   4. Dump results to `results/results-ort-<embedder>-<device>.json`.
//!
//! Why not Criterion: criterion's sampling assumes the cost-under-test is
//! cheap relative to setup. Here a model load is 500ms-10s and a batch of 32
//! is ~50ms — wrong shape for criterion's statistical model. Plain Instant
//! gives us what we actually want: cold-load and stable-state throughput.

use std::time::Instant;

use anyhow::{Context, Result};
use serde::Serialize;
use vex::embed::{make_embedder_with_device, Device, MINILM_ID};

const CORPUS_JSON: &str = include_str!("../corpus.json");
const BATCH_SIZES: &[usize] = &[1, 8, 32];
const ITERATIONS: usize = 10;
const EMBEDDERS: &[&str] = &[MINILM_ID, "jina-code"];

#[derive(Serialize)]
struct BatchResult {
    batch_size: usize,
    iterations: usize,
    total_embeddings: usize,
    wall_secs: f64,
    /// Embeddings per second across all batches × all iterations. The most
    /// honest single metric — `wall_secs` covers exactly the time spent
    /// producing `total_embeddings` embeddings, so the ratio is
    /// per-embedding throughput regardless of how the model parallelises a
    /// batch.
    throughput_emb_per_sec: f64,
    /// Wall time of a single `embed_batch()` call. At `batch_size = 1` this
    /// IS per-embedding latency. At `batch_size > 1` the model embeds the
    /// whole batch in parallel — each text waits ~the full batch time, not
    /// `batch_time / N` — so the field is BATCH-level. Don't divide by
    /// `batch_size` and call the result per-embedding latency.
    batch_wall_p50_ms: f64,
    batch_wall_p95_ms: f64,
}

#[derive(Serialize)]
struct DeviceResult {
    embedder_id: String,
    backend: String,
    model_load_ms: f64,
    corpus_size: usize,
    vector_dim: usize,
    batches: Vec<BatchResult>,
    /// Full embedding for `corpus[0]`. `compare.py` uses this to compute
    /// cosine drift between backends. Earlier versions stored only the
    /// first 8 floats, but a partial-vector cosine has no statistical
    /// relation to the full-vector cosine — the norm of the first 8 dims
    /// of a unit MiniLM vector is ≈ 0.14, so the 0.999-cosine decision
    /// threshold is uncalibrated against a partial-vector reading.
    sample_vec_corpus0: Vec<f32>,
}

fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * (p / 100.0)).round() as usize;
    sorted[idx]
}

fn bench_one(
    embedder_id: &str,
    device: Device,
    backend_label: &str,
    corpus: &[String],
) -> Result<DeviceResult> {
    println!("--- {embedder_id} on {backend_label} ---");

    let load_t0 = Instant::now();
    let mut embedder = make_embedder_with_device(embedder_id, device, false)
        .with_context(|| format!("construct {embedder_id} on {backend_label}"))?;
    let model_load_ms = load_t0.elapsed().as_secs_f64() * 1000.0;
    println!("  model_load_ms: {model_load_ms:.1}");

    // First-symbol sample for cross-run drift comparison — full vector, not
    // first-8 (partial-vector cosine is not meaningful — see field doc).
    let sample_in = vec![corpus[0].clone()];
    let sample = embedder.embed_batch(&sample_in)?;
    let sample_vec_corpus0 = sample[0].clone();
    let vector_dim = sample[0].len();

    let mut batches_out = Vec::with_capacity(BATCH_SIZES.len());
    for &bs in BATCH_SIZES {
        // chunks_exact: drop the trailing partial chunk so every timed batch
        // has the same shape. A mixed full-and-partial `chunks()` loop
        // would mix a fast-small batch with a slow-full batch in the p50
        // sample, corrupting the comparison. The discarded tail is wasted
        // work but the bench is throwaway and corpus size is small.
        let chunks: Vec<Vec<String>> = corpus.chunks_exact(bs).map(|c| c.to_vec()).collect();
        if chunks.is_empty() {
            eprintln!("  !! bs={bs}: corpus too small for an exact chunk; skipping");
            continue;
        }

        // Per-batch-size warmup. ANE graph compile is per input shape, so a
        // bs=1 warmup does NOT cover bs=8 or bs=32. Throw away one batch at
        // this bs before recording.
        embedder.embed_batch(&chunks[0])?;

        let mut per_batch_ms: Vec<f64> = Vec::with_capacity(chunks.len() * ITERATIONS);
        let mut total_embs = 0usize;
        let wall_t0 = Instant::now();
        for _ in 0..ITERATIONS {
            for batch in &chunks {
                let t = Instant::now();
                embedder.embed_batch(batch)?;
                per_batch_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                total_embs += batch.len();
            }
        }
        let wall_secs = wall_t0.elapsed().as_secs_f64();
        let throughput = total_embs as f64 / wall_secs;

        // Sort once, index twice. total_cmp is defensive against NaN
        // (Instant → f64 can't produce NaN in practice, but
        // partial_cmp().unwrap() is a latent footgun).
        let mut sorted_ms = per_batch_ms;
        sorted_ms.sort_by(|a, b| a.total_cmp(b));
        let p50 = percentile_sorted(&sorted_ms, 50.0);
        let p95 = percentile_sorted(&sorted_ms, 95.0);
        println!(
            "  batch={bs}: batch_p50={p50:.2}ms  batch_p95={p95:.2}ms  \
             throughput={throughput:.0} emb/s  ({total_embs} embs in {wall_secs:.2}s, \
             {} timed chunks)",
            sorted_ms.len()
        );
        batches_out.push(BatchResult {
            batch_size: bs,
            iterations: ITERATIONS,
            total_embeddings: total_embs,
            wall_secs,
            throughput_emb_per_sec: throughput,
            batch_wall_p50_ms: p50,
            batch_wall_p95_ms: p95,
        });
    }
    println!();

    Ok(DeviceResult {
        embedder_id: embedder_id.to_string(),
        backend: backend_label.to_string(),
        model_load_ms,
        corpus_size: corpus.len(),
        vector_dim,
        batches: batches_out,
        sample_vec_corpus0,
    })
}

fn main() -> Result<()> {
    let corpus_json: serde_json::Value =
        serde_json::from_str(CORPUS_JSON).context("parse corpus.json")?;
    let corpus: Vec<String> = corpus_json["contexts"]
        .as_array()
        .context("corpus.json missing 'contexts' array")?
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .collect();
    println!("== bench-coreai (Rust side: ort+CPU vs ort+CoreML EP) ==");
    println!("Corpus: {} samples", corpus.len());
    println!();

    // CoreMl is gated by the `gpu-coreml` Cargo feature on the vex dep (see
    // Cargo.toml); on a CPU-only vex build the CoreMl Device variant still
    // EXISTS in the enum, but execution_providers() returns empty and we'd
    // accidentally measure ort+CPU twice. The compile-time check below
    // refuses to build off macOS, where the whole comparison is meaningless
    // (no ANE, no Core AI).
    #[cfg(not(target_os = "macos"))]
    compile_error!("This bench is macOS-only — Core AI side has no equivalent elsewhere.");

    let pairs: &[(Device, &str)] = &[(Device::Cpu, "ort+CPU"), (Device::CoreMl, "ort+CoreML EP")];

    std::fs::create_dir_all("results").context("create results dir")?;

    for embedder_id in EMBEDDERS {
        println!("### Embedder: {embedder_id}");
        for (device, label) in pairs {
            match bench_one(embedder_id, *device, label, &corpus) {
                Ok(result) => {
                    let safe_label = label.replace(['+', ' '], "-").to_ascii_lowercase();
                    let out_path = format!("results/results-ort-{embedder_id}-{safe_label}.json");
                    let json = serde_json::to_string_pretty(&result)?;
                    std::fs::write(&out_path, json).with_context(|| format!("write {out_path}"))?;
                    println!("  -> {out_path}");
                }
                Err(e) => {
                    eprintln!("  !! skipped {embedder_id} on {label}: {e:#}");
                }
            }
            println!();
        }
    }

    println!("Done. Compare across runs with: python3 compare.py results/");
    Ok(())
}
