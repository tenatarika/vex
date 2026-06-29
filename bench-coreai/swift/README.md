# Swift side — Apple Core AI

Measures the same `corpus.json` against Apple Core AI on macOS 27+. Mirrors
the Rust side's batch=1/8/32 latency + throughput schema so
`../compare.py` can diff the two backends.

## Prerequisites (all required)

- macOS 27.0+ (Core AI framework first ships here)
- Xcode 27.0+ (needs the matching Swift 6.x toolchain + SDK)
- `uv` installed (`brew install uv`) — for Apple's Python export recipes
- Local clone of <https://github.com/apple/coreai-models>

## Step 1 — Export MiniLM-L6-v2 to `.aimodel`

Apple ships export recipes that turn HuggingFace models into the `.aimodel`
resource folder format. The exact invocation is **illustrative** — the
apple/coreai-models repo is pre-1.0 at the time this skeleton was written,
so the CLI surface may have changed by the time you run this. Verify
against the repo's current README before running:

```bash
git clone https://github.com/apple/coreai-models.git
cd coreai-models
uv run coreai.model.registry --list-models    # confirm MiniLM is in the catalog
# Then follow the per-model README under models/ to export.
```

Drop the resulting `.aimodel` folder somewhere stable and update the
`MINILM_AIMODEL_PATH` constant in `Sources/CoreAIBench/main.swift`.

## Step 2 — Enable the Swift package dependency

Open `Package.swift` and uncomment:

- The `.package(url: "https://github.com/apple/coreai-models.git", ...)` line.
- The `.product(name: "CoreAIModels", package: "coreai-models")` line in the
  target dependencies.

(Both are pinned by branch right now because the package is pre-1.0; switch to
a tagged version once Apple cuts one.)

## Step 3 — Fill the `TODO:` block in main.swift

Search the file for `TODO:`. The one load-bearing block is `embedBatch` —
replace its `throw BenchError(...)` with Core AI's batched predict() call.
Inputs: tokenized texts (same WordPiece tokenizer the Python export
recipe used). Outputs: mean-pooled `last_hidden_state` → array of 384-dim
`[Float]`, L2-normalised to match the Rust side's stored vectors. The
`working-with-coreai` skill (in the apple/coreai-models repo) is the
canonical pattern.

`embedBatch` throws `BenchError` if unwired, which `main()` catches and
prints — no `fatalError` crash, so running before Step 3 produces a
readable "Core AI embedBatch() not wired" message and exits cleanly.

## Step 4 — Run

```bash
cd swift
swift run -c release CoreAIBench
```

Or set `BENCH_CORPUS` if you want to run from a different working directory:

```bash
BENCH_CORPUS=/abs/path/to/bench-coreai/corpus.json swift run -c release CoreAIBench
```

(main.swift reads `corpus.json` at runtime — NOT via SwiftPM resource bundle,
because SwiftPM forbids resources outside the package root, and duplicating
the file into `swift/` would break the "same input as the Rust side"
guarantee.)

Output: `../results/results-coreai-minilm-l6-v2.json` (schema identical to
the Rust side).

## Why no jina-code on the Core AI side (yet)?

`jina-code` is a separate model with its own ONNX weights — it has no
`.aimodel` export from Apple's catalog at launch. Two paths to fix this:

1. **Wait** — if jina-code becomes popular enough, Apple may add a recipe.
2. **Author one** — Apple ships `coreai-torch` (a Python primitives library
   for converting PyTorch models to `.aimodel`). The HuggingFace jina-code
   weights → PyTorch → `coreai-torch.export(...)` path is a separate
   research arc, NOT in scope for the throwaway bench.

For now the bench's primary signal is MiniLM-L6-v2 on Core AI vs ort+CoreML
EP — that's the question that gates whether building a Core AI backend in
vex is worth it at all.

## Why is this a skeleton and not a working binary?

The host running this bench-coreai/ scaffold today (June 2026) is on
macOS 26.3.1 — Core AI's framework symbols aren't linkable. The skeleton
is shaped so that the day macOS 27 GA's, the only diff vs working code is
the `embedBatch` TODO above + the commented Package.swift lines. The
skeleton compiles and runs on macOS 27 — `swift run` will print a readable
"Core AI embedBatch() not wired" error and exit (no crash), so you can
verify the harness end-to-end before filling the TODO.
