// Throwaway Swift bench: measure Apple Core AI embedding throughput against
// the same corpus the Rust side runs against ort+CPU and ort+CoreML EP.
//
// Status: SKELETON. Cannot run until:
//   1) Host is macOS 27 + Xcode 27 (Core AI framework available).
//   2) MiniLM-L6-v2 has been exported to .aimodel via Apple's Python recipe
//      from https://github.com/apple/coreai-models (see swift/README.md).
//   3) The .package(url:) and .product(...) lines in Package.swift are
//      uncommented (Apple's Swift package is currently pre-1.0).
//
// What it does once the gaps above are filled:
//   - Reads ../corpus.json (the byte-identical input the Rust side uses) —
//     by runtime path, NOT a SwiftPM resource. SwiftPM forbids resources
//     outside the package root, so duplicating the file into swift/ would
//     break the "same input" guarantee. Override via `BENCH_CORPUS` env.
//   - Cold-loads the .aimodel via Core AI; records model_load_ms.
//   - Throws away one warmup batch PER batch size (ANE graph compile is
//     deferred per input shape; a single bs=1 warmup doesn't cover bs>1).
//   - For batch sizes 1, 8, 32: runs the corpus chunked EXACTLY into bs,
//     ITERATIONS times. Reports batch_wall_p50/p95 (NOT per-embedding —
//     a parallel batch finishes as one event; dividing by bs is wrong)
//     and throughput_emb_per_sec.
//   - Emits results-coreai-minilm-l6-v2.json matching the Rust
//     DeviceResult schema so ../compare.py can diff cosine drift on the
//     FULL sample vector (partial-vector cosine isn't statistically
//     meaningful).

import Foundation
import Dispatch
// import CoreAIModels  // TODO: enable after apple/coreai-models ships a tagged release.

// MARK: - Errors

struct BenchError: Error, CustomStringConvertible {
    let message: String
    var description: String { message }
}

// MARK: - Schema (mirrors the Rust side's DeviceResult byte-for-byte)

struct BatchResult: Codable {
    let batchSize: Int
    let iterations: Int
    let totalEmbeddings: Int
    let wallSecs: Double
    let throughputEmbPerSec: Double
    let batchWallP50Ms: Double
    let batchWallP95Ms: Double

    enum CodingKeys: String, CodingKey {
        case batchSize = "batch_size"
        case iterations
        case totalEmbeddings = "total_embeddings"
        case wallSecs = "wall_secs"
        case throughputEmbPerSec = "throughput_emb_per_sec"
        case batchWallP50Ms = "batch_wall_p50_ms"
        case batchWallP95Ms = "batch_wall_p95_ms"
    }
}

struct DeviceResult: Codable {
    let embedderId: String
    let backend: String
    let modelLoadMs: Double
    let corpusSize: Int
    let vectorDim: Int
    let batches: [BatchResult]
    let sampleVecCorpus0: [Float]

    enum CodingKeys: String, CodingKey {
        case embedderId = "embedder_id"
        case backend
        case modelLoadMs = "model_load_ms"
        case corpusSize = "corpus_size"
        case vectorDim = "vector_dim"
        case batches
        case sampleVecCorpus0 = "sample_vec_corpus0"
    }
}

struct Corpus: Decodable {
    let description: String
    let charBudget: Int
    let contexts: [String]

    enum CodingKeys: String, CodingKey {
        case description
        case charBudget = "char_budget"
        case contexts
    }
}

// MARK: - Timing

// DispatchTime.uptimeNanoseconds is a monotonic ns-resolution clock — the
// Swift analogue of Rust's std::time::Instant. NSDate / Date() goes through
// gettimeofday and is wall-clock with ~1ms granularity, which would be
// systematically noisy at the batch latencies we measure (a fast ANE batch
// is 2-5ms). Cross-side comparison with the Rust Instant numbers requires
// matched resolution.
@inline(__always)
func elapsedMs(since t0: DispatchTime) -> Double {
    let ns = DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds
    return Double(ns) / 1_000_000.0
}

// MARK: - Statistics

func percentileSorted(_ sorted: [Double], _ p: Double) -> Double {
    guard !sorted.isEmpty else { return 0.0 }
    let idx = Int(((Double(sorted.count) - 1.0) * (p / 100.0)).rounded())
    return sorted[idx]
}

// MARK: - Chunking

// Swift's stdlib has no chunks_exact equivalent that drops the partial tail.
// Roll our own so the timed batches all have shape == bs, matching the Rust
// side's corpus.chunks_exact(bs) discipline.
func chunksExact<T>(_ arr: [T], size: Int) -> [[T]] {
    guard size > 0 else { return [] }
    var out: [[T]] = []
    var i = 0
    while i + size <= arr.count {
        out.append(Array(arr[i..<(i + size)]))
        i += size
    }
    return out
}

// MARK: - Corpus loading

func loadCorpus() throws -> [String] {
    // BENCH_CORPUS env override > "../corpus.json" relative to CWD (works
    // when `swift run` is invoked from swift/).
    let envPath = ProcessInfo.processInfo.environment["BENCH_CORPUS"]
    let path = envPath ?? "../corpus.json"
    let url = URL(fileURLWithPath: path)
    let data: Data
    do {
        data = try Data(contentsOf: url)
    } catch {
        throw BenchError(message: "corpus.json not found at '\(path)' — set BENCH_CORPUS or `cd swift && swift run` from the package root. Underlying error: \(error)")
    }
    let corpus = try JSONDecoder().decode(Corpus.self, from: data)
    return corpus.contexts
}

// MARK: - Embedding (TODO blocks)

// TODO: set this to the absolute path of the MiniLM .aimodel folder produced
// by the Apple export recipe. Use the same MiniLM-L6-v2 weights the Rust
// side embeds — otherwise the cross-side cosine-drift check is meaningless.
let MINILM_AIMODEL_PATH = "/PATH/TO/minilm-l6-v2.aimodel"

func embedBatch(_ texts: [String]) throws -> [[Float]] {
    // TODO: replace with Core AI's batched predict() once apple/coreai-models
    // ships. Apple's runtime exposes batching at the AIModel level — see the
    // working-with-coreai skill in apple/coreai-models for the canonical
    // pattern. Outputs MUST be L2-normalised to match the Rust side's stored
    // vectors (src/store/writer.rs::write_section_vectors normalises before
    // write); otherwise the cosine-drift comparison still works (cosine is
    // scale-invariant) but the raw float values in sample_vec_corpus0 will
    // look different at first glance.
    throw BenchError(message: "Core AI embedBatch() not wired — see swift/README.md for export and integration steps.")
}

// MARK: - Bench

let BATCH_SIZES = [1, 8, 32]
let ITERATIONS = 10

func benchCoreAI(corpus: [String]) throws -> DeviceResult {
    print("--- minilm-l6-v2 on Core AI (.aimodel) ---")

    let loadStart = DispatchTime.now()
    // TODO: load the .aimodel here. Until then the bench will fail at the
    // first embedBatch call with a BenchError caught by main() — NOT a
    // fatalError crash, so the message is readable.
    let modelLoadMs = elapsedMs(since: loadStart)
    print(String(format: "  model_load_ms: %.1f", modelLoadMs))

    // First-symbol sample for cross-run drift comparison — full vector, not
    // first-8 (partial-vector cosine is not meaningful — see Rust's
    // DeviceResult.sample_vec_corpus0 doc).
    let sample = try embedBatch([corpus[0]])
    let sampleVecCorpus0 = sample[0]
    let vectorDim = sample[0].count

    var batchesOut: [BatchResult] = []
    for bs in BATCH_SIZES {
        // chunks_exact equivalent — drop the partial tail. Matches the Rust
        // side so cross-side timings cover the same input shapes.
        let chunks = chunksExact(corpus, size: bs)
        if chunks.isEmpty {
            print("  !! bs=\(bs): corpus too small for an exact chunk; skipping")
            continue
        }

        // Per-batch-size warmup. ANE graph compile is per input shape, so a
        // bs=1 warmup does NOT cover bs=8 or bs=32.
        _ = try embedBatch(chunks[0])

        var perBatchMs: [Double] = []
        perBatchMs.reserveCapacity(chunks.count * ITERATIONS)
        var totalEmbs = 0
        let wallStart = DispatchTime.now()
        for _ in 0..<ITERATIONS {
            for batch in chunks {
                let t = DispatchTime.now()
                _ = try embedBatch(batch)
                perBatchMs.append(elapsedMs(since: t))
                totalEmbs += batch.count
            }
        }
        let wallSecs = elapsedMs(since: wallStart) / 1000.0
        let throughput = Double(totalEmbs) / wallSecs

        let sortedMs = perBatchMs.sorted()
        let p50 = percentileSorted(sortedMs, 50.0)
        let p95 = percentileSorted(sortedMs, 95.0)
        print(String(format: "  batch=%d: batch_p50=%.2fms  batch_p95=%.2fms  throughput=%.0f emb/s  (%d embs in %.2fs, %d timed chunks)",
                     bs, p50, p95, throughput, totalEmbs, wallSecs, sortedMs.count))
        batchesOut.append(BatchResult(
            batchSize: bs, iterations: ITERATIONS, totalEmbeddings: totalEmbs,
            wallSecs: wallSecs, throughputEmbPerSec: throughput,
            batchWallP50Ms: p50, batchWallP95Ms: p95))
    }

    return DeviceResult(
        embedderId: "minilm-l6-v2",
        backend: "coreai",
        modelLoadMs: modelLoadMs,
        corpusSize: corpus.count,
        vectorDim: vectorDim,
        batches: batchesOut,
        sampleVecCorpus0: sampleVecCorpus0
    )
}

// MARK: - Entry

do {
    print("== bench-coreai (Swift side: Apple Core AI) ==")
    let corpus = try loadCorpus()
    print("Corpus: \(corpus.count) samples\n")

    let result = try benchCoreAI(corpus: corpus)
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
    let data = try encoder.encode(result)
    let outPath = "../results/results-coreai-minilm-l6-v2.json"
    try data.write(to: URL(fileURLWithPath: outPath))
    print("\n-> \(outPath)")
    print("\nTODO: add jina-code once Apple ships an export recipe (or roll one")
    print("       via coreai-torch — see swift/README.md §Future work).")
} catch {
    print("bench failed: \(error)")
    exit(1)
}
