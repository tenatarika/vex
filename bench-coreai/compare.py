#!/usr/bin/env python3
"""Compare results-*.json files from the bench and print a side-by-side table.

Usage:
    python3 compare.py results/

Reads every `results-*.json` under the given directory, groups by
`embedder_id`, prints model_load + bs=1 latency + bs=8/32 throughput per
backend, and computes cosine drift between every pair of backends on the
FULL `sample_vec_corpus0` (384-dim for MiniLM, 768-dim for jina-code).

Earlier versions computed cosine on the first 8 floats only — a partial
cosine has no statistical relation to the full-vector cosine (norm of the
first 8 dims of a unit MiniLM vector is ≈ 0.14), so the 0.999 decision
threshold was uncalibrated. With full vectors the threshold is meaningful:
two MiniLM impls running identical weights typically agree at cosine
≥ 0.9999; a drift below 0.999 indicates a real numerical disagreement
between backends that will change ranking on noisy semantic queries.
"""
import json
import math
import sys
from pathlib import Path
from collections import defaultdict


def cosine(a, b):
    if not a or not b or len(a) != len(b):
        return float("nan")
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    return dot / (na * nb) if na and nb else float("nan")


def fmt(x, spec):
    if isinstance(x, float) and math.isnan(x):
        return "n/a"
    return format(x, spec)


def main():
    if len(sys.argv) != 2:
        print("usage: compare.py <results_dir>", file=sys.stderr)
        sys.exit(2)
    root = Path(sys.argv[1])
    runs = []
    for p in sorted(root.glob("results-*.json")):
        with p.open() as f:
            runs.append(json.load(f))
    if not runs:
        print(f"no results-*.json under {root}", file=sys.stderr)
        sys.exit(1)

    by_embedder = defaultdict(list)
    for r in runs:
        by_embedder[r["embedder_id"]].append(r)

    for embedder, items in by_embedder.items():
        print(f"\n=== {embedder} (dim={items[0]['vector_dim']}) ===")
        # bs=1 batch_wall IS per-embedding latency (single text per batch).
        # bs=8 / bs=32 use throughput because batch_wall at bs>1 is
        # batch-level — see Rust DeviceResult.batch_wall_p50_ms doc.
        print(f"{'backend':22}  {'load_ms':>9}  {'b1 lat (ms)':>12}  {'b8 thru':>10}  {'b32 thru':>10}")
        for r in sorted(items, key=lambda x: x["backend"]):
            batches_by_size = {x["batch_size"]: x for x in r["batches"]}
            b1 = batches_by_size.get(1, {})
            b8 = batches_by_size.get(8, {})
            b32 = batches_by_size.get(32, {})
            b1_lat = b1.get("batch_wall_p50_ms", float("nan"))
            b8_thru = b8.get("throughput_emb_per_sec", float("nan"))
            b32_thru = b32.get("throughput_emb_per_sec", float("nan"))
            print(
                f"{r['backend']:22}  {r['model_load_ms']:>9.1f}"
                f"  {fmt(b1_lat, '>12.2f')}"
                f"  {fmt(b8_thru, '>10.0f')}"
                f"  {fmt(b32_thru, '>10.0f')}"
            )

        # Cosine drift on FULL sample vector (384/768-dim — see module doc
        # for why the partial-vector cosine that earlier versions used was
        # not statistically meaningful).
        if len(items) >= 2:
            dim = len(items[0].get("sample_vec_corpus0") or [])
            print(f"\n  cosine drift on corpus[0] (full {dim}-dim):")
            for i in range(len(items)):
                for j in range(i + 1, len(items)):
                    a, b = items[i], items[j]
                    av = a.get("sample_vec_corpus0") or []
                    bv = b.get("sample_vec_corpus0") or []
                    c = cosine(av, bv)
                    if math.isnan(c):
                        flag = "  <-- MISSING SAMPLE"
                    elif c < 0.999:
                        flag = "  <-- DIVERGENT"
                    else:
                        flag = ""
                    print(f"    {a['backend']} vs {b['backend']}: {fmt(c, '.6f')}{flag}")


if __name__ == "__main__":
    main()
