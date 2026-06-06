# Semantic Search Pipeline

How `vex` builds and updates the semantic-search index — the path from a
parsed symbol to a hit in `vex search --semantic`. Written after the
v1.15.0 B1.2 incremental update landed; consolidates pipeline knowledge
that was previously spread across CHANGELOG, inline comments, and
LIMITATIONS.md.

> **Quick model:** every symbol is hashed into a content-addressed key,
> embedded into a 384-dim vector, and the vectors land in an HNSW graph
> keyed by that hash. `vex update --semantic` mutates the existing
> graph in place (`load → diff → remove → add → save`) instead of
> rebuilding from scratch. The same hash is used by the embed cache
> and the HNSW, so changing a symbol's body changes both — re-embed
> only happens for actually-changed symbols, regardless of which file
> they live in.

---

## File layout

```text
<cache-dir>/<project-hash>/
├── index.vex             ← binary index (symbols, FST, BM25, call graph, refs)
├── index.hnsw            ← usearch HNSW graph, KEYED BY context_hash
├── index.hashes          ← v1.14.1 — sym_idx → context_hash sidecar (VEXH magic)
├── index.bodytokens      ← v1.15.0 — sym_idx → body_tokens sidecar (VEXT magic)
├── index.bloom           ← v1.12.0 — bloom prefilter for `vex check`
├── manifest.json         ← file hashes + version markers
└── <cache-dir>/embeddings/                              ← shared across projects
    └── …model files (ONNX, tokenizer)…
└── <cache-dir>/<project-hash>/embed_cache_<id>.bin      ← v1.13 E2b persistent embed cache
```

Three semantic-specific sidecars (`hnsw`, `hashes`, `bodytokens`) live
next to `index.vex`; the embed cache is per-(project, embedder); the
ONNX model files are shared across all projects on the machine. Every
sidecar is **independently optional** — absence falls back to a slower
but correct path (brute-force search, full rebuild, full re-embed).

### Versioning policy

- **`index.vex` format version** is `v6` (`MIN_SUPPORTED_VERSION = 3`).
  B1.2 did **not** bump the format — body_tokens persistence is a
  sidecar, not a section.
- **`index.hnsw`** is opaque usearch state; no version field of our own.
  Compatibility is whatever usearch promises (currently stable across
  the 2.25.x line we pin).
- **Sidecar versions** are per-file: `VEXH v1`, `VEXT v1`, `VEXB v1`.
  Each sidecar carries its own magic + version + count + guard against
  MAX_COUNT (≤ 10M entries) to prevent crafted-input OOM during load.
- **Manifest markers** (`vectors_normalized`, `cpp_includes_processed`,
  `body_tokens_persisted`) are `Option<bool>` — `None` means "pre-this
  version" with conservative fallback semantics, `Some(true)` is the
  current version's behaviour, `Some(false)` indicates a failed write
  that needs re-priming. Pre-v1.15 indexes carry `None` for the v1.15
  marker.

---

## Pipeline stages

```text
                 vex index --semantic
parsed source ──▶ ParsedFile ──▶ build_context ──▶ context_hash ──▶ embed_cache
                                                          │             │
                                                          ▼             ▼
                                                       VEXT          VEXH
                                                       sidecar       sidecar
                                                          │             │
                                                          ▼             ▼
                                                       reconstruct   build_hnsw
                                                       _unchanged    _at
                                                          │             │
                                                          └──────┬──────┘
                                                                 ▼
                                                          index.hnsw + sidecars
                                                                 │
                                                                 ▼
                                                  vex search --semantic
                                              ──▶ HnswHandle::open ──▶ search
```

### 1. Parse → ParsedSymbol

`parse::parse_file` walks the tree-sitter AST per language and emits
`ParsedSymbol { name, kind, line, signature, doc, body_tokens }`.
**`body_tokens`** is the deduped lower-cased identifier + literal stream
extracted from the def-node subtree by `extract_body_tokens` (max 400
bytes, max 2000 AST nodes visited). It feeds two consumers: BM25 (term
bag) and semantic embedding (context string).

### 2. ParsedSymbol → context string

`embed::build_context(kind, name, path, signature, doc, body_tokens,
char_budget)` concatenates the symbol's structural fields into a
budget-truncated context string the embedder sees. Same function used
both during `vex index` (over freshly parsed symbols) and during
`vex update` (over reconstructed + parsed symbols). `body_tokens` is
included if `Some(_)`; pre-v1.15 reconstructed symbols had `None` here
and produced shorter, body-less contexts that drifted from fresh-parse
hashes — the bug B1.2 closes.

### 3. context_hash → embed cache lookup

`embed::cache::context_hash(embedder_id, ctx)` = `xxh3_64` over
`embedder_id || \0 || ctx`. Stable across runs given the same inputs.
Used as:

- **Embed cache key**: `embed_cache_<id>.bin` is a hashmap of
  `context_hash → vector` keyed by this hash. `generate_embeddings`
  probes the cache before touching ONNX; all-hit skips model load
  entirely (15280× warm in v1.13 P2's bench).
- **HNSW key**: the v1.14.1 B1.1 switch keys the HNSW graph by
  `context_hash`, not by `sym_idx`. Stable across `vex update` runs
  even when symbol order shifts.

### 4. Embed (only on cache miss)

`generate_embeddings` partitions symbols into cache hits + misses; misses
go through fastembed's `TextEmbedding::embed_batch` (ONNX + MiniLM-L6-v2
by default). New vectors are inserted into the cache, persisted
atomically.

### 5. Build HNSW

Two paths, picked by `pipeline::update`:

- **Full rebuild** (`build_hnsw_at`): fresh `usearch::Index`, reserve,
  `add(hash, vector)` for every (k, v), save. Used by `vex index`
  (always cold start) and by `vex update` when incremental can't apply.
- **Incremental** (`build_hnsw_incremental_at`): load existing HNSW
  into mutable index, diff old `index.hashes` vs new hashes, `remove(h)`
  orphans, `add(h, v)` additions, save, rewrite sidecar. Returns
  `Ok(true)` if applied; `Ok(false)` for clean fallback; `Err` only
  when HNSW saved but sidecar rewrite failed (loud — orchestrator
  surfaces it).

### 6. Write sidecars

`hash_index::save` writes `index.hashes` (sym_idx-ordered `Vec<u64>`),
`body_tokens::save` writes `index.bodytokens` (sym_idx-ordered
`Vec<Option<String>>`). Both via atomic `.tmp` + `rename`.
`Manifest::body_tokens_persisted` is set to `Some(save_ok)` so a write
failure shows up as `Body tokens: no` in `vex status` rather than
silently lying.

### 7. Query path: `HnswHandle::open`

`vex search --semantic` opens the HNSW handle:

1. `usearch::Index::view(path)` mmaps the HNSW file (read-only).
2. `hash_index::load(sidecar)` reads sym_idx-ordered hashes.
3. Size check: `index.size()` must equal `sidecar.len()`. Mismatch
   (e.g. half-written update) → bail to brute-force fallback.
4. Build `HashMap<u64, u32>` (hash → sym_idx). Collisions are first-wins
   with a `tracing::warn!`.
5. `search(query_vec, top_k)` returns `(keys, distances)`; we map each
   key back to sym_idx via the hashmap.

Any failure (missing file, view error, sidecar load error, size
mismatch) returns `None` from `HnswHandle::open` and the caller falls
back to a brute-force linear scan over the index's vectors section.
The user notices this only as slower search (~50ms vs ~4ms on 10k
symbols); results are identical.

---

## B1.2 — incremental update path

### Why hash-keyed

Pre-v1.14.1, the HNSW used `sym_idx` as key. `vex update` re-parses
changed files, re-emits symbols in order, and `sym_idx` of an unchanged
symbol can shift if an earlier file gains or loses symbols. The whole
HNSW was rebuilt to stay consistent. Switching to `context_hash`
decouples the key from position: the same `(name, kind, path,
signature, doc, body_tokens)` always hashes to the same key, regardless
of where it sits in the symbol stream.

### Why body_tokens persistence

`compute_hashes_for` is invoked over the *full corpus* during update
(unchanged + freshly parsed). For freshly parsed symbols, `body_tokens`
comes straight from the parser. For unchanged ones, `reconstruct_unchanged`
rebuilds the `ParsedFile` from disk — and pre-v1.15 it left `body_tokens:
None`, which produced body-less hashes that didn't match fresh-parse.
The diff against the old `index.hashes` then saw every unchanged symbol
as "remove + add" — net effect: full HNSW rebuild every update.

B1.2 persists body_tokens to `index.bodytokens` so `reconstruct_unchanged`
can restore them. The hash for an unchanged symbol now matches what
`vex index` would produce, the diff stays small, incremental wins.

### Tombstone threshold

`build_hnsw_incremental_at` bails to full rebuild when
`|to_remove| > 0.25 × |old_hashes|` (strict-GT, integer arithmetic).
At higher churn the per-key `remove()` cost (HNSW relinks neighbours)
plus the on-disk tombstone overhead outweighs a clean rebuild. The
boundary case (exactly 25%) still applies the incremental path — pinned
by `incremental_at_exact_25_percent_threshold_does_not_fall_back`.

### Fallback contract

`build_hnsw_incremental_at` returns `Result<bool>`:

- `Ok(true)` — incremental applied; both HNSW and sidecar are in the
  post-update state. Orchestrator skips `build_hnsw`.
- `Ok(false)` — clean bail before any disk mutation:
  - missing HNSW or sidecar (cold start / pre-v1.14.1 index)
  - corrupt sidecar (hash_index::load failed)
  - dim mismatch on `index.load` (embedder changed)
  - tombstone threshold exceeded
  - usearch internal error during `new_index` / `reserve` / `add`
  - empty corpus (orchestrator handles cleanup separately)
- `Err` — only when HNSW was saved but the sidecar rewrite then
  failed. The two files are now inconsistent on disk; orchestrator
  surfaces the error loudly. `HnswHandle::open`'s size-check catches
  this on the next read and falls back to brute force; the next
  successful update self-heals.

### Performance

Criterion baseline at 5000 vectors, M1 Pro, random unit vectors
(`benches/perf_b12.rs`, results frozen in
`benches/results/v1.15.0-b12-baseline-*.txt`):

| Scenario | Time | vs full_rebuild |
|---|---|---|
| `full_rebuild_baseline` | 2.71 s | 1.0× |
| `incremental_no_change` | 15.4 ms | 176× |
| `incremental_1pct_churn` | 58.6 ms | 46× |
| `incremental_5pct_pure_add` | 234 ms | 11.6× |
| `incremental_5pct_pure_remove` | 15.2 ms | 178× |
| `incremental_10pct_churn` | 395 ms | 6.9× |
| `incremental_25pct_churn` (boundary) | 831 ms | 3.3× |
| `incremental_26pct_falls_back` | 1.77 ms | bail-only |
| `fallback_then_full_rebuild` | 2.63 s | ≈ baseline |

Notes:
- **Asymmetric add/remove**: usearch `remove()` only relinks immediate
  neighbours (~15ms at 5% churn) vs `add()` which walks the existing
  graph at O(log N × M) per insertion (~234ms at the same 5% churn).
  Pure-add scenarios are the slow case; pure-remove is near-floor.
- **No double-work bug**: when incremental bails, the orchestrator's
  full-rebuild path runs at full speed — bail cost (1.77ms) is
  negligible against the rebuild (2.63s ≈ 2.71s baseline).
- **Scales positively with corpus size**: at 25k vectors,
  `incremental_10pct_churn` widens to 7.8× from 6.9× because
  `full_rebuild` scales linearly with N while incremental scales with
  churn count. Bigger corpus = bigger relative win.

---

## Operational guidance

### Cold-start migration (pre-v1.15 → v1.15)

Pre-v1.15 indexes have no `index.bodytokens`. The first
`vex update --semantic` after upgrading reads `body_tokens: None` for
unchanged symbols, computes body-less hashes, and the diff against the
v1.14.1 `index.hashes` treats every unchanged symbol as `remove + add`
→ full rebuild. Correctness is unaffected; only the speedup is gated.

To enable incremental immediately, run `vex index --semantic` once.
That writes the sidecar; subsequent updates are incremental.

Confirm via `vex status`:

```
$ vex status
…
Body tokens: yes (incremental HNSW update enabled)
```

`Body tokens: no` means the next semantic update will be a full rebuild.
JSON envelope: `body_tokens_persisted: bool`.

Cold-start applies **per-index**, not globally — each project needs
its own priming run.

### When does incremental fire?

All four conditions must hold:

1. `vex update` (not `vex index`). `vex index` always builds from scratch.
2. `--semantic` flag (or `.vex.toml` `semantic = true`).
3. Some change to the file set. With zero changes, the orchestrator
   short-circuits before touching HNSW (`"nothing to update"`).
4. `|to_remove| / |old_hashes|` ≤ 25%.

If any condition fails, the orchestrator does a full rebuild (or
nothing at all, for the no-change case). Tracing at `info` level shows
which path was taken:

```
$ RUST_LOG=info vex update --semantic
…
HNSW incremental update applied  added=3 removed=1 new_size=5012 old_size=5010
```

vs:

```
HNSW incremental: tombstone threshold exceeded (1500/5000 > 1/4) → full rebuild
HNSW index built  vectors=4500 …
```

### Disk-state recovery

If a process is killed mid-update, the on-disk state can land in one
of a few configurations. All are recoverable on the next successful
update or `vex index --semantic`:

| State after kill | What `HnswHandle::open` does | Recovery |
|---|---|---|
| HNSW only (no sidecar) | Bails to brute force | Next update writes sidecar |
| Sidecar only (no HNSW) | Bails to brute force | Next update writes HNSW |
| Both present, sizes mismatch | Bails to brute force | Next update aligns |
| Both present, sizes match | Uses HNSW (may have wrong vectors at some slots) | Next update overwrites |

The fourth case is the only one that could in theory produce wrong
results in the gap between failure and next update — but the size
match implies the HNSW saved successfully AND the sidecar saved
successfully; the only path that lands here is a process kill
*between* `index.save()` and `hash_index::save()` after the HNSW
already reflects the new state but the sidecar reflects the old.
Resolution: the orchestrator returns `Err` rather than `Ok` in this
case, so the user sees the error and re-runs.

---

## Cross-references

- **CHANGELOG.md** — per-release changes, search for `B1.1` /
  `B1.2` / `body_tokens` / `incremental HNSW`.
- **docs/LIMITATIONS.md §4b** — first-update-after-upgrade cold-start
  contract.
- **docs/CONCURRENCY.md** — index lock and herd-fix details that apply
  to the semantic write path too.
- **`src/index/pipeline/output.rs`** — `build_hnsw_at`,
  `build_hnsw_incremental_at`, `generate_embeddings`,
  `compute_hashes_for`, `prune_embed_cache`. All `pub fn` under
  `#[doc(hidden)] pub use` re-export at `vex::index::pipeline` so the
  bench and integration test reach them.
- **`src/store/body_tokens.rs`** — `VEXT` sidecar I/O.
- **`src/search/hash_index.rs`** — `VEXH` sidecar I/O.
- **`src/search/semantic.rs`** — `HnswHandle::open` / `search`.
- **`src/embed/cache.rs`** — `EmbedCache`, `context_hash`,
  `sweep_to`.
- **`benches/perf_b12.rs`** — incremental vs full-rebuild
  micro-benchmark. Configure corpus size via `VEX_BENCH_CORPUS_SIZE`.
- **`tests/incremental_hnsw_property_test.rs`** — proptest
  equivalence guarantee between incremental and full-rebuild paths.
- **`fuzz/fuzz_targets/fuzz_incremental_hnsw.rs`** — libFuzzer
  harness for `build_hnsw_incremental_at`. Run via
  `cargo +nightly fuzz run fuzz_incremental_hnsw`.
