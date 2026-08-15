# Base + delta index — design sketch

> **Status: design only. Nothing here is implemented, and the decision to build
> it has not been taken.** This document exists so the decision can be taken on
> evidence rather than intuition. Roadmap item #10 in the storage research.

## The problem, measured

`vex update` after editing **one file** in a 3.6 GB repository (6083 tracked
files, 33379 symbols) takes ~485 ms. Where it goes:

| Phase | ms | Scales with |
|---|---|---|
| `build_bm25_index` | 115 | all symbols |
| `write_index_to` (all FSTs) | 78 | all symbols |
| `bodytokens` sidecar | 75 | all symbols |
| `reconstruct_unchanged` | 38 | all symbols |
| `discover_files` | 38 | all files |
| `hash_files` (stat-cached since v1.25.6) | ~12 | all files |
| `trigram` sidecar | 12 | all files |
| `resolve_call_edges` | 10 | all edges |
| `bloom` | 6 | all symbols |
| **`parse_files` — the file that actually changed** | **6.5** | **the change** |

**~320 ms rebuilds artefacts for symbols that did not change. The real work is
6.5 ms — 1.3 %.** Change detection (`discover` + `hash`, ~50 ms) is irreducible:
you cannot know what moved without looking.

The ratio is stable across scales — the same phases were ~58 % of `vex update`
on a 6.8k-symbol repository and ~58 % on a 33k-symbol one — so this is a
property of the architecture, not of one corpus.

## Why not the full Lucene shape

The canonical answer is N immutable segments plus tombstones plus a merge
policy. It is proven, and it is the wrong first step here, because it taxes the
one thing vex is best at:

- **Search is currently one FST lookup over one mmap'd file.** That is where the
  constant ~5 ms comes from. N segments means N lookups plus a merge plus a
  liveness check per hit, on every query, forever.
- **A merge policy needs a scheduler.** vex is a CLI with no daemon. Merging
  would land on `vex index`, on a threshold inside `update`, or on `vex watch` —
  each of which is a policy decision with its own failure modes.
- **N is unbounded in the hot path.** A repository that goes a long time without
  compaction degrades search silently.

## The proposal: exactly two tiers

**base** — everything the current index already is, unchanged in format.
**delta** — one small index over only the files that changed since the base was
written.

A read opens both and merges. A write appends to (or rewrites) the delta only.
Compaction folds delta into base and is a *full rebuild* — precisely the code
path that exists today.

What this buys over N segments:

- The read path merges **two** readers, never more. The search cost is bounded
  and knowable, not a function of how long since compaction.
- **No merge policy.** Compact when the delta exceeds a threshold (file count or
  symbol count), or on explicit `vex index`. One number, one branch.
- **No new format.** The delta is an ordinary index. `IndexReader` opens it the
  way it opens any index. What is new is the *pair*, and the rules for reading a
  pair.

## What has to be resolved, per section

This is the real cost, and it is not uniform. Ordered by difficulty.

| Section | Merge rule | Difficulty |
|---|---|---|
| symbol FST | union; delta wins on path collision | easy |
| refs FST / ref_edges | union | easy |
| callgraph FSTs | union — edges are keyed by name, not by index position | easy |
| hierarchy edges | union | easy |
| bloom sidecar | bitwise OR of the two filters | trivial |
| trigram sidecar | per-file records; delta shadows base by path | easy |
| pattern skeletons | union, keyed by file | easy |
| **BM25** | **IDF is corpus-global** — see below | **hard** |
| **`bodytokens`** | **positional, keyed by symbol index** — see below | **hard** |
| deletions | a path in delta with no symbols must mask the base's | medium |
| HNSW / vectors | already incremental (B1.2); out of scope | n/a |

### BM25 is the first hard one

`Bm25IndexBuilder::new(doc_count)` takes a corpus-wide document count, and
scoring needs document lengths across the whole corpus. Two segments mean either

- **per-segment IDF**, which makes a term's score depend on which tier its symbol
  happens to live in — ranking drifts as a side effect of when a file was last
  edited, which is indefensible; or
- **a shared statistics block** updated on every write, which reintroduces the
  global rebuild this design exists to avoid — unless the statistics are
  maintained *incrementally* (add the delta's contribution to running totals),
  which is possible because doc-count and doc-length sums are additive, but
  needs its own correctness argument for deletions.

The additive-statistics route is the intended one. It must be designed
explicitly, not discovered during implementation.

### `bodytokens` is the second, and it is newly visible

At 75 ms it is the third-largest cost in the update, and it was not on anyone's
list until it was measured. It is a **positional** sidecar — entries are keyed by
symbol index, which is exactly the thing that changes when a base and a delta are
merged. Options, none free:

- re-key it by `(path, symbol name, line)` — stable across merges, larger on
  disk, and touches the B1.2 embedding-cache contract that depends on it;
- keep one `bodytokens` file per tier and resolve at read time, paying an
  indirection on the semantic path;
- or exclude it from the delta and accept that symbols in the delta have no
  body tokens until compaction — degrading semantic recall for recently edited
  files, which are the ones a user is most likely to search for. Rejected on
  those grounds, but noted because it is the cheapest option and someone will
  propose it.

### Deletions

A file deleted since the base was written must not surface from the base. The
delta therefore needs to record *negative* entries — "this path is gone" — and
every merge point must consult them. This is the tombstone concept, minus the
generality: with two tiers there is exactly one place to look.

## Crash safety comes along for free

Two tiers need a pointer to the current pair, which is roadmap item #4
(single-pointer atomic commit), re-rated to low urgency on its own but
*mandatory* here. That is the argument for folding them: #4 is cheap when it is
part of a design that needs it anyway, and disproportionate as a standalone
change.

## Budget, and the gate

The change is only worth making if it does not cost what it is meant to save.
Proposed acceptance criteria, to be fixed **before** implementation:

1. **Search latency: no worse than +1 ms** at p50 on the 33k-symbol corpus.
   This is the hard one; if two-tier reads cannot hold it, the design fails.
2. **`vex update` on a one-file edit: under 150 ms** on the same corpus, from
   485 ms.
3. **Ranking is unchanged** — the `vex eval` golden set must produce identical
   nDCG@10 / recall@10 / MRR before and after. This is what pins the BM25
   statistics work.
4. Compaction is never required for correctness, only for performance.

## Recommended sequencing

1. Prototype the **read path** first, against a hand-built base+delta pair.
   Measure search latency. If criterion 1 fails, stop — everything downstream is
   wasted.
2. Design the BM25 additive statistics, with deletions, on paper. Review it.
3. Decide `bodytokens`' re-keying, since it dictates whether the B1.2 contract
   moves.
4. Only then write the write path, the deletion records, and the pointer commit.

The order is deliberate: the two steps most likely to kill the design (search
latency, BM25 correctness) come first and cost the least.
