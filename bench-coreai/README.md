# bench-coreai — throwaway research bench

**Status: research artifact, NOT for landing in main.**

## Why this exists

The architect review on "should vex add Apple Core AI as an alt embedding
backend" deferred the decision pending a measured win. Specifically:

> Until you can show `minilm-coreai` is materially faster than `gpu-coreml`
> ort on the *same* M-series chip for the *same* MiniLM weights, you're
> paying integration cost for a hypothesis.

This directory is that measurement. It runs the same corpus through two
(eventually three) backends and emits comparable JSON for diffing.

## What it measures

For each `(embedder, backend)` pair: cold model-load time, per-embedding
p50/p95 latency, and throughput at batch sizes 1 / 8 / 32. Also captures the
first 8 floats of the embedding for `corpus[0]` so a cross-backend cosine
drift check is a one-liner (see `compare.py`).

## Embedders compared

| ID | Dim | Size | Why included |
| --- | --- | --- | --- |
| `minilm-l6-v2` | 384 | ~22M params | vex's default; the model Apple already ships a recipe for |
| `jina-code` | 768 | ~117M params | Heavier model — exposes whether ANE/GPU win scales with model size, the question that decides if Core AI is worth pursuing for non-MiniLM embedders |

## Backends compared

| Backend | Status today | Notes |
| --- | --- | --- |
| ort + CPU | runs on macOS 26 | Floor for the comparison; vex's default when no GPU available |
| ort + CoreML EP | runs on macOS 26 | Already on the ANE — same hardware Core AI would target, just via Apple's older NN compiler. **This is the meaningful baseline.** |
| Apple Core AI (`.aimodel`) | skeleton only | Requires macOS 27 + Xcode 27 + Apple's export recipe. See `swift/README.md`. |

## Layout

```
bench-coreai/
├── Cargo.toml                          # standalone crate, OUTSIDE vex workspace
├── corpus.json                         # shared input (READ-ONLY: don't regenerate)
├── compare.py                          # sweeps results/*.json, prints comparison + cosine drift
├── src/main.rs                         # Rust bench (ort+CPU vs ort+CoreML EP)
├── swift/
│   ├── Package.swift                   # SwiftPM manifest, macOS 27+ platform
│   ├── README.md                       # how to fill the .aimodel + Core AI gaps
│   └── Sources/CoreAIBench/main.swift  # skeleton with TODO blocks
└── results/                            # JSON outputs land here
```

## Run the Rust side (today, on macOS)

```bash
cd bench-coreai
cargo run --release
```

First run downloads MiniLM (~86 MB) and jina-code (~470 MB) into vex's shared
fastembed cache (`~/Library/Caches/vex/embed/`). Subsequent runs are fast.

Output: `results/results-ort-<embedder>-<device>.json` (4 files: 2 embedders ×
2 devices).

## Run the Swift side (only on macOS 27)

See [`swift/README.md`](swift/README.md). Three steps: export `.aimodel`,
uncomment two lines in `Package.swift`, fill three `TODO:` blocks in
`main.swift`. Then `swift run -c release CoreAIBench`.

## Interpret the results

```bash
python3 compare.py results/
```

Prints something like:

```
=== minilm-l6-v2 (dim=384) ===
backend                   load_ms     b1 lat (ms)     b8 thru   b32 thru
ort+cpu                    1234.5            8.20        1250       4100
ort+coreml-ep              2100.3            4.50        3200       8400
coreai                     1800.0            2.80        6400      14000

  cosine drift on corpus[0] (full 384-dim):
    ort+coreml-ep vs coreai: 0.999823
```

`b1 lat (ms)` is `batch_wall_p50_ms` at `batch_size=1` — legitimate
per-embedding latency (one text per batch). For `b8` / `b32` the columns
report throughput, NOT batch_wall, because at `bs>1` a parallel batch
finishes as one event and dividing wall time by `bs` would conflate
throughput-per-slot with latency.

### Decision rules

- **`ort+coreml-ep` ≥ ~50% of Core AI throughput at batch=32**: Core AI not
  worth the integration cost (Swift bridge, second model asset, doubled CI
  matrix, macOS-27-only gate). Stay on ort + CoreML EP for ANE acceleration.
- **Core AI ≥ 2× ort+coreml-ep at batch=32**: ANE backend has a real signal,
  reconsider the proposal architect deferred. Pin the bench numbers + chip +
  macOS version, file a follow-up.
- **cosine drift < 0.999 vs ort+coreml-ep** (on the FULL 384/768-dim sample
  vector — partial-vector cosine on the first 8 dims is not statistically
  meaningful and earlier versions of this bench got this wrong): argues for
  separate embedder ID (`minilm-coreai`) rather than backend toggle —
  manifest mismatch is the honest path. (Two MiniLM impls running identical
  weights typically agree at cosine ≥ 0.9999; below 0.999 indicates real
  numerical disagreement that will change top-k ordering on noisy semantic
  queries.)
- **jina-code shows the same shape as MiniLM** (e.g. ort+coreml-ep already
  saturates ANE): Core AI integration buys nothing extra at larger models
  either — final nail in the defer decision.

## Cleanup

```bash
rm -rf bench-coreai/
```

The crate is OUTSIDE vex's workspace (`members = [".", "crates/vex-mcp"]` in
`../Cargo.toml`) — deletion is safe. No need to touch `Cargo.lock` either;
nothing in the main workspace depends on this.
