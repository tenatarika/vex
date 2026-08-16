# Base + delta index — design sketch

> **Status: design only. Nothing here is implemented, and the decision to build
> it has not been taken.** This document exists so the decision can be taken on
> evidence rather than intuition. Roadmap item #10 in the storage research.
>
> **Steps 1–3 of the sequencing below have run and passed; review then found
> that the step most likely to kill the design was never on the list.** The
> read-path gate passes at ~+0.1 ms p50 against a 1 ms budget, for the
> structural and BM25 channels — the semantic/HNSW channel is **not** covered
> (§"Criterion 1, measured"). BM25 across a pair is bit-identical to a full
> rebuild, including on a delta whose content genuinely diverges from the base's
> stale copy (§"Resolved — and it is not hard"). The `bodytokens` re-keying
> question dissolves once documents are never renumbered (§"Reconsidered").
>
> **But `ref_edges` and `hierarchy_edges` address symbols by tier-local index**,
> so a pair silently loses scope-accurate results across tiers, in proportion to
> the size of the base (§"The edges are the real problem"). That, the cost of
> the *second* update (§"The second update"), and the absence of any binding
> between a delta and its base (§"Nothing binds a delta") are the open blockers.
> Requirements and the recommended next step are in §"Where this leaves the
> decision" — which is **not** step 4.

## The problem, measured

`vex update` after editing **one file** in a large private Python codebase
(~6k indexed files, ~33k symbols) takes ~485 ms. Where it goes:

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
| **refs FST / ref_edges** | **`to_sym_idx` is a foreign key into one tier — see below** | **hard** |
| callgraph FSTs | union — edges are keyed by name, not by index position | easy |
| **hierarchy edges** | **same: `to_sym_idx` / `from_sym_idx` are tier-local** | **hard** |
| bloom sidecar | bitwise OR of the two filters | trivial |
| trigram sidecar | per-file records; delta shadows base by path | easy |
| pattern skeletons | union, keyed by file | easy |
| ~~**BM25**~~ | IDF is corpus-global — **measured: additive, bit-identical** | ~~hard~~ easy |
| ~~**`bodytokens`**~~ | positional — **moot: the pair never renumbers** | ~~hard~~ n/a |
| deletions | dead-document bitmap in the delta — same one BM25 needs | easy |
| HNSW / vectors | already incremental (B1.1/B1.2) — hash sidecar unchecked | open |

The two rows this table originally called hard — BM25 and `bodytokens` — were
the two the sequencing put first, and both moved: see §"Resolved — and it is not
hard" and §"Reconsidered". Review then found that the table had been wrong in
the other direction about two rows it called easy, and those are now the hard
ones: §"The edges are the real problem".

### The edges are the real problem

This was missed by generalising from the callgraph section, where the reasoning
holds, to two sections where it does not. A call edge names its callee with a
**string**:

```rust
pub struct CallEdge { caller_sym_idx, callee_name_offset, line, _pad }   // format.rs
```

A reference edge and a hierarchy edge do not. They carry **foreign keys into
their own tier's symbol and file tables**:

```rust
pub struct RefEdge       { to_sym_idx, from_file_id, line, col_and_kind }
pub struct HierarchyEdge { to_sym_idx, from_sym_idx, from_file_id, line_and_kind }
```

and the read path resolves a name to a `sym_idx` in one tier, then looks up
edges in that same tier — `channel/mod.rs` does
`for sym_idx in sym_fst.find(…) { for edge in ctx.reader.find_ref_edges_by_symbol(sym_idx) }`,
and `cmd_implementations.rs` does the same with `find_hierarchy_edges_by_symbol`
before resolving `edge.from_file_id` through *that* reader's file table.

A pair therefore loses edges in both directions, on the common case — edit one
file that refers to definitions in files that did not change:

- **delta → base.** The delta's Pass-2 resolver builds `name_to_global` from the
  symbols it is writing. A delta over one file holds only that file's
  definitions, so nearly every reference out of it resolves to nothing and
  spills into the name-keyed *unresolved* section. In single-repo mode nothing
  reads that section: `find_unresolved_refs_by_name` has exactly one consumer,
  `cross_repo_hits` in `cmd_usages.rs`, gated on a workspace member owning the
  name. The refs are written and never surfaced.
- **base → delta.** A base edge pointing into a file the delta re-indexed has a
  `to_sym_idx` the dead bitmap now kills. Edit one file and `vex usages
  --strict` on any symbol in it loses **every caller elsewhere in the repo**
  until compaction.

The damage is proportional to the size of the base, not to the size of the
change — the exact inversion this design exists to remove. It hits
`usages --strict`, `impact` (the Binder tier), `implementations` and `subtypes`:
the commands whose whole value is that they are scope-accurate rather than
textual.

Three ways out, none free, and one of them has to be chosen before any code:

1. **Resolve cross-tier edges by name at query time**, through the existing
   unresolved-refs section, and wire that section into single-repo reads. Costs
   a documented tier demotion — a cross-tier hit is name-resolved, not
   binder-resolved — which `vex impact`'s verdict logic would have to reflect
   rather than silently treat as a binder hit.
2. **Widen the edge records to address symbols across tiers** — `(tier,
   tier-local index)` in the record. Exact, and it breaks "no new format": it is
   a format bump plus a rewrite of every edge reader.
3. **Scope the binder-backed commands out of the pair**: serve them from the
   base plus a live scan of the delta's files, or force compaction before they
   answer. Cheapest, and it means the design does not deliver its benefit for
   those commands — which should then be said out loud in `docs/LIMITATIONS.md`.

This is harder than BM25 turned out to be, and unlike BM25 it was never on the
sequencing list. That is the finding, not the answer.

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

#### Resolved — and it is not hard, because `df` is not stored

Step 2 ran, and the shape of the problem is different from the paragraph above.
Reading `store::bm25`: **`df` is never persisted.** It is `posting.len()`,
computed at query time. `tf` lives in the posting entry and `doc_len` in the
tier's own stats block, both per-document. So the only genuinely corpus-global
quantities are `N` and `avg_doc_len` — two scalars — and both are additive.

What makes them additive is knowing which base documents the delta has
superseded. Give the delta a **bitmap over base `sym_idx`** marking exactly
that, and:

- `N` = live base documents + delta documents;
- `avg_doc_len` = (base length sum − dead length sum + delta length sum) / `N`,
  every term of which is known when the delta is written;
- `df(t)` = live base postings for `t` + delta postings for `t`, counted at
  query time from posting lists the scorer already walks.

The bitmap also **subsumes the file-table shadow rule** from criterion 1: a
changed file, a file emptied of symbols, and a deleted file are all just base
documents marked dead. One mechanism, one place to look.

Measured against a full rebuild, with the delta superseding files the base
holds. The third column is the one that counts: there the delta's files were
genuinely **edited** before re-indexing (a frequent identifier renamed and
whole-line comments stripped, in 556 of 883 files), so the base keeps stale
copies whose term sets and lengths differ from the delta's, and the control is a
rebuild of the edited corpus.

| | 6.8k, unchanged content | 78.5k, unchanged content | **78.5k, edited content** |
|---|---|---|---|
| dead documents | 300 | 16070 | 16070 |
| dead vs delta length sums | equal | equal | **226303 vs 219615** |
| `N`, pair vs rebuild | 6818 = 6818 | 78535 = 78535 | **78514 = 78514** |
| `avg_doc_len` vs rebuild | 22.6468 = 22.6468 | 13.8291 = 13.8291 | **13.7476 = 13.7476** |
| BM25 scores vs rebuild | bit-identical | bit-identical | **bit-identical** (max Δ 0.0) |
| query cost vs one tier | ±0.000 ms p50 | −0.000 ms p50 | −0.000 ms p50 |
| dead bitmap build | 0.20 ms | 1.99 ms | 1.99 ms |

The first two columns were the original measurement and review was right to
distrust them: when the delta re-indexes content that has not changed, the dead
length sum and the delta length sum are equal, the subtraction cancels, and an
implementation that mixed the two up would look correct. The third column is
the fixture that breaks that symmetry — the sums differ by 6688 — and the pair
still reproduces the rebuild exactly, including a corpus that lost 21 symbols
to the edit.

The bitmap build is the only work that scales with the *base* rather than with
the change, and it is write-time: 2 ms against the 150 ms budget for a whole
`vex update`. It scans base symbol records to test each against the delta's
path set; a per-file symbol range in the index would remove even that, but
nothing forces it.

#### The one real requirement it uncovered: the tie-break

Scores match bit-for-bit, but result *order* does not always, and the reason is
worth stating precisely because criterion 3 depends on it. `Bm25Reader::search`
breaks score ties on `sym_idx` — a tier-local number with no meaning across a
pair. Of the queries whose ordering differed (45 of 187 on the small corpus;
41 of 185 on the large with a 5 % delta, 73 of 185 with a 20 % one, 73 of 189
with the edited-content delta), **every one is a tie**: same members in a different
order, or — where the tie straddles the top-k cut — a different member kept.
None involved differing scores.

So the pair needs a tie-break on a key that means the same thing in both tiers.
`(path, name, line)` is the obvious one, and it is what RRF already uses
downstream. The clean sequencing is to adopt that key **in the single-tier path
first**, as a change of its own, and confirm `vex eval` is unmoved; the pair is
then identical to a rebuild by construction rather than by argument.

Three corrections to that from review, all of which make it bigger than it
looks:

- **It is not a no-op today.** RRF scores a result `1/(K + rank)` from its
  position in the per-channel list. That position is currently `sym_idx`-derived,
  so changing the tie-break changes ranks among ties, which changes RRF
  contributions, which can change final membership. `vex eval` is the gate, not
  a formality.
- **`(path, name, line)` is not total.** Nothing in the writer prevents two
  symbol records sharing it; `resolve_call_edges` documents the collision as "a
  parser bug, not a real case" and defensively keeps the first, but only for its
  own lookup table. Used as a sort key it re-admits `HashMap` iteration order at
  the residual ties, so the key needs a final deterministic component —
  `(path, name, line, kind, tier, tier-local index)`.
- **It is not only BM25.** The structural channel hard-codes `score: 1.0` for
  every hit, so *every* multi-hit structural query is one big tie whose order
  comes from FST and posting traversal — which is symbol-numbering-dependent and
  just as non-portable across a pair. Requirement 5 has to cover both channels.

#### Caveats on the exactness claim

- `avg_doc_len` is computed by the builder over **unclamped** document lengths,
  while the stats block stores them clamped to `u16::MAX`. A pair can only
  recover the clamped value for a dead document, so the two disagree whenever
  some symbol has more than 65,535 unique terms — rare enough that the builder
  logs a warning when it happens, but real. Fix by defining the average over
  clamped lengths (a one-line change that also matches what the scorer actually
  uses) or by storing an unclamped total. Note what review turned up while
  confirming this: **today's single index is already inconsistent here** — the
  average is unclamped while every `dl` the scorer divides by it is clamped — so
  this is a pre-existing bug that segmentation merely forces us to notice. The
  fix changes single-tier scores on any corpus that trips the clamp, so it needs
  the same `vex eval` gate as the tie-break.
- Bit-identity is a construction property, not a coincidence — per-symbol
  accumulation order is fixed by query-term order in both paths, and `N` /
  `avg_doc_len` come from exact `u64` sums with one final `f32` division on both
  sides — but it is conditional on the clamp point above and on the dead set
  being right.
- The numbers here are single runs on an unpinned laptop. An independent re-run
  during review reproduced every qualitative verdict and every equality, with
  different decimals (one configuration's worst case moved from +0.12 ms to
  +0.03 ms). Treat the conclusions as robust and the specific milliseconds as
  indicative.

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

#### Reconsidered — the premise is wrong

The list above assumes a merge renumbers symbols. It does not have to, and in
the read path measured for criterion 1 it does not: a result's identity is
`(tier, tier-local index)`, and each tier's positional sidecars are read with
that tier's own indices. Nothing is re-keyed because nothing is renumbered.
Renumbering happens at **compaction**, which is a full rebuild and writes the
sidecar the way it does today.

Two facts sharpen this. `body_tokens` has **no query-time reader at all** — its
only consumers are `pipeline::output` (write) and
`parse_files::reconstruct_unchanged` (read during `vex update`), so it is a
write-path concern, not a read-path one. And in a base+delta world the delta
re-indexes only changed files, so unchanged symbols are never reconstructed at
all: they stay in the base, with their body tokens, untouched. The work that
sidecar exists to avoid mostly stops happening.

What still needs checking, and is not claimed here: how the HNSW hash sidecar
(B1.1/B1.2) behaves across a pair. It is content-hash-keyed rather than
positional, which is why the table above scoped vectors out, but "already
incremental" is not the same as "already correct for two tiers".

### Deletions

A file deleted since the base was written must not surface from the base. The
delta therefore needs to record *negative* entries — "this path is gone" — and
every merge point must consult them. This is the tombstone concept, minus the
generality: with two tiers there is exactly one place to look.

Step 2 settled what those entries are: a **bitmap over base `sym_idx`**, which
the BM25 statistics need anyway. Deletion stops being its own mechanism — a
deleted file's documents are dead in the same bitmap as a changed file's, and a
merge point consults one bit instead of a path set. Note the ordering
consequence: the bitmap, not the delta's file table, is the authority. The file
table is a cheap approximation that covers changed and emptied files but cannot
see a deletion, since a deleted file appears in neither tier.

## The second update, which this sketch never described

"A write appends to (or rewrites) the delta only" hides the cost model. Update 1
changes file A, so the delta is {A}. Update 2 changes file B. Then what?

- **Rewrite the delta over {A, B}** — you need A's symbols again, so either
  re-parse it (write cost becomes O(files changed since compaction): at a 20 %
  threshold, eventually a fifth of the repo per edit) or carry them forward with
  `reconstruct_unchanged`, which revives the 38 ms phase this design claims
  mostly stops happening, scaled to delta size.
- **Append in place** — then the delta is no longer "an ordinary index": it
  needs internal shadowing and its own liveness rules. That is N segments,
  rebuilt inside one file.
- **A third tier** — contradicts "exactly two tiers".

The 485 ms figure at the top of this document is a one-file edit against a
freshly built index: the *first* update after a compaction, the best case.
Criterion 2 ("under 150 ms on a one-file edit") inherits that framing and so
constrains nothing about the steady state, which is where users live.

What the design owes before step 4 is a write-cost function for update *N*
since compaction, a threshold chosen against it, and criterion 2 restated as
amortised or as worst-case-before-compaction. If the honest answer is "cost
grows until compaction, then spikes to a full rebuild", that is the design and
it should be judged as such.

Note this also weakens the case for *exactly two* tiers. The measurement says a
tier costs one FST lookup and one small mmap, flat in corpus and delta size —
linearly extrapolated, eight tiers still fit inside the 1 ms budget. The
objection to N segments that survives is "a merge policy needs a scheduler",
and this design has no compaction policy either. Two tiers is the option that
creates the rewrite-vs-append problem above; N segments is the option where
update *N* genuinely costs O(change). The choice should be re-argued against
the measurement rather than inherited from the sketch.

## Nothing binds a delta to the base it was written against

The dead bitmap is indexed by base `sym_idx`. Nothing in the format ties it to a
particular base. `IndexReader::open` validates magic and accepts any version in
`MIN_SUPPORTED_VERSION..=VERSION` — currently v3..v8 — with no build identity at
all. So a `vex index`, a compaction, or a different vex binary that rewrites the
base while a delta exists leaves every bit in that bitmap pointing at an
unrelated symbol: live symbols vanish, stale ones surface, no error, no stale
signal. A v3 base and a v8 delta likewise open cleanly and get merged
section-by-section.

The delta must therefore record `(base_build_id, base_symbol_count,
base_format_version)`, and the reader must validate the triple on open and, on
mismatch, refuse the pair — serve base-only and raise the existing stale signal.
`base_symbol_count` alone is a cheap guard that catches almost all of it. This
is two fields, and it is the difference between a bug and silently wrong
answers.

## Crash safety does not come along for free

Two tiers need a pointer to the current pair, which is roadmap item #4
(single-pointer atomic commit), re-rated to low urgency on its own but
*mandatory* here. Folding it in is right — it is disproportionate standalone.
Calling it *free*, as this section originally did, was wrong, and the two claims
sat in the same paragraph: a mandatory prerequisite is a cost.

What exists today is stronger than the sketch credited. `index.vex` is written
to a temp file, `sync_all`'d, renamed, and the parent directory fsync'd;
`manifest.json` is written last; every sidecar is guarded downstream by its own
check. Readers take **no lock**, and are safe purely because there is one file,
one atomic rename, and a live mmap that survives replacement.

A pointer buys atomicity of the pointer, not of the *pair*, and it introduces
failure modes this document has to answer before step 4:

- a reader resolves the pointer, is preempted, and compaction unlinks a member —
  the second open fails, or the reader keeps its base mapping and silently
  serves stale results;
- index files must become immutable and generation-named, which needs a
  reclamation protocol (refcount, grace period, or lease) or you either delete
  under readers or leak;
- on Windows, unlinking a mapped file fails outright, so a long-lived
  `vex watch` blocks reclamation and generations accumulate;
- two authorities on "what is indexed" — `manifest.json` and the pointer — that
  can disagree after a crash, where today the manifest is unambiguously last.

## Compaction needs a policy, and this design trades p50 for p99

"Compact when the delta exceeds a threshold… one number, one branch" is not a
policy. Unanswered: who runs it, whether a reader mid-compaction is safe, what a
crash mid-compaction leaves behind, and what stops a long-lived checkout from
never compacting.

Compaction is a full rebuild, so it costs more than the 485 ms this design is
trying to reduce — and the only places it can fire are inside `pipeline::update`,
which both `vex search --auto-update` and `vex watch` call. The spike therefore
lands in a user-facing latency path at an unpredictable moment. Writers
serialise on `IndexLock`; readers do not lock at all, so a reader concurrent
with compaction is governed by whatever the pointer protocol above resolves.

Two consequences worth stating plainly. Compaction re-runs Pass-2 over the whole
corpus, so the resolved edge set **changes discontinuously at compaction** —
`--strict` results improve out of nowhere, which is user-visible
nondeterminism. And corpus-aggregate commands are not merely "lookups with a
shadow rule": `top_n_by_indegree` counts distinct caller indices corpus-wide and
must consult the dead bitmap or double-count across tiers.

Both acceptance criteria below are p50. This design specifically buys median
latency with tail latency, so it needs a tail criterion too.

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

## Criterion 1, measured

Step 1 of the sequencing below, run against hand-built base+delta pairs on two
corpora: this repository (6.8k symbols, 618 files) and CPython (78.5k symbols,
4419 files), each split so that base ∪ delta = the control index. Two delta
sizes on the large corpus — 5 % of files, and 20 %, which stands in for a pair
sitting at a plausible compaction threshold. Queries are 186 real symbol names
drawn from the control index, plus 50 perturbed names that match nothing and so
fall through to the Levenshtein rung. Both index-backed channels the CLI runs
per search are timed (symbol-FST structural, BM25), warm page cache, per-query
median over 15 repetitions.

**The pair costs +0.12 ms at p50 in the worst configuration measured — against
a 1 ms budget.**

| Corpus / delta | hit queries | miss queries | + extra open | worst total |
|---|---|---|---|---|
| 6.8k symbols, 5 % | +0.001 ms | +0.042 ms | +0.025 ms | **+0.07 ms** |
| 78.5k symbols, 5 % | +0.000 ms | +0.089 ms | +0.028 ms | **+0.12 ms** |
| 78.5k symbols, 20 % | +0.001 ms | +0.045 ms | +0.068 ms | **+0.11 ms** |

The penalty does not grow with corpus size or with delta size, which is what
the two-tier shape predicts: the extra work is one more FST lookup and one more
small mmap, neither a function of how much is in the base.

### It passes only if the read path is built this way

The first attempt failed the gate, by a lot — **+1.7 ms at p50 on queries that
hit**, which is worse than the budget by itself. The cause is not the second
reader. It is that `structural::search_with_fuzzy` resolves
exact → prefix → Levenshtein *inside one index*. Run per tier, the tier that
simply does not hold the symbol falls through its whole ladder and builds a
Levenshtein automaton, while the other tier is answering from an exact hit.

Two rules follow, and they are requirements on the implementation, not
suggestions:

1. **The fallback ladder is evaluated across the pair, one rung at a time.**
   Both tiers answer "exact?"; only if neither does do both answer "prefix?";
   and so on. The expensive rung is reached exactly when a single-tier index
   would have reached it.
2. **The Levenshtein automaton is built once and streamed against both tiers.**
   Building it is the whole cost of a miss query — measured on the large
   corpus: `Levenshtein::new` 1.61 ms, the same query streamed against the
   78.5k-symbol FST 1.76 ms, so traversal is ~0.15 ms and construction is
   ~92 % of it. Building it per tier doubles a fixed cost that has nothing to
   do with how many symbols each tier holds. `SymbolFstReader::find_fuzzy`
   constructs the automaton internally, so a tiered read needs a variant that
   accepts one.

With both rules, the fuzzy rung across two tiers costs 0.11 ms of traversal on
top of the one construction — against 3.41 ms for the naive version.

### The delta's shadow set comes from the file table

A read must drop base hits for any file the delta has re-indexed. Deriving that
path set by scanning the delta's symbols is both slower and **wrong**: on the
large corpus the delta's file table lists 118 paths while its symbols mention
only 111, because seven re-indexed files hold no symbols at all. Shadowing from
the scan leaves the base's stale symbols for those seven files visible. The
file table is the correct source and the cheap one — 0.079 ms versus 0.494 ms
to build at a 20 % delta, and flat in symbol count.

Note this covers *changed and emptied* files only. A file **deleted** since the
base was written appears in neither table, so it still needs the negative
entries described in §Deletions. The file table is not a substitute for
tombstones.

Step 2 then made the point moot in the right direction: the dead-document
bitmap it needs for BM25 covers all three cases and is checked with one bit
rather than a path lookup, so the shadow rule should read the bitmap and the
file table drops out of the read path entirely.

### What this does not measure

- Only the structural and BM25 channels of `search`. The **semantic/HNSW
  channel is not measured at all**, and it is the one with a known cross-tier
  problem: `semantic.rs` refuses its index unless its size equals
  `reader.symbol_count()`, which for a pair is ambiguous, and the failure mode
  is a silent fall-through to brute-force cosine over the whole corpus.
- Only `search`. The other index-backed commands (`usages`, `callers`,
  `implementations`, …) read their own FST sections with exact lookups and no
  fuzzy fallback, so the per-tier *latency* there is a second lookup, not a
  second automaton — but latency was never their problem. See §"The edges are
  the real problem" for what actually happens to them.
- Which arm each number came from. The "+ extra open" column builds the shadow
  set from the delta's **file table**, the mechanism §Deletions later replaces
  with the bitmap; the bitmap is only comparably cheap at open if it is stored
  in the delta and mmap'd rather than rebuilt. And the headline table's BM25 arm
  is `bm25::search` run per tier and merged by score — a latency proxy for a
  computation this design rejects, since the two tiers' scores are not
  comparable. The exact-statistics arm is timed separately, in §"Resolved".
- Warm page cache, in one process. A real CLI invocation opens a second file
  and faults its pages in; the measured open penalty (0.03–0.07 ms warm) is a
  floor, not the cold number. The base index dominates either way.
- Nothing about ranking. Which `LIMIT` results survive a merge is criterion 3,
  and the BM25 statistics question below is untouched by any of this.

### A separate finding, outside this design

On the current single-tier index, a query that matches nothing costs 1.6 ms
(large corpus) to 3.2 ms (small) — and essentially all of it is
`Levenshtein::new`, which is a function of the query and the edit distance, not
of the index. That is the tail of vex's search latency today, it gets *worse*
on smaller repositories, and it is independent of segmentation. Worth its own
look; not part of #10.

## Recommended sequencing

1. ~~Prototype the **read path** first, against a hand-built base+delta pair.
   Measure search latency. If criterion 1 fails, stop — everything downstream is
   wasted.~~ **Done — passes; see §"Criterion 1, measured".**
2. ~~Design the BM25 additive statistics, with deletions, on paper. Review it.~~
   **Done, and demonstrated rather than argued — bit-identical scores against a
   full rebuild on both corpora; see §"Resolved — and it is not hard".** Still
   wants review, and it hands one prerequisite to step 4: the global tie-break.
3. ~~Decide `bodytokens`' re-keying, since it dictates whether the B1.2 contract
   moves.~~ **Answered by the same principle — the pair never renumbers, so the
   contract does not move; see §"Reconsidered".** Open residue: the HNSW hash
   sidecar across a pair.
3.5. **New, and it should have been first.** Cross-tier `ref_edges` /
   `hierarchy_edges` resolution (B1), and the write-cost function for update *N*
   since compaction (B2). Both are paper exercises, both are cheaper than a
   write path, and either can reshape the design.
4. Only then write the write path, the deletion records, and the pointer commit.

The order was deliberate — the steps most likely to kill the design first,
because they cost the least — and the ordering was wrong. Search latency and
BM25 correctness both passed; the thing that can actually kill it, cross-tier
edge resolution, was misclassified as "easy — union" in the difficulty table
and so never got a step. That is worth remembering the next time this document's
sequencing is trusted: a table filled in by intuition decided what got measured.

## Where this leaves the decision

Nothing here says the design should be built. It says that two cheap steps that
could have stopped it did not, that a third problem nobody had listed probably
can, and that a pile of open questions are now requirements.

### Blocking, before any implementation

- **B1. Cross-tier `ref_edges` / `hierarchy_edges`.** Pick one of the three
  routes in §"The edges are the real problem" and design it. Until then the pair
  silently degrades exactly the commands that justify vex over grep.
- **B2. A write-cost function for update *N* since compaction**, with criterion
  2 restated as amortised or worst-case-before-compaction, and the two-vs-N
  tier choice re-argued against the measurement (§"The second update").
- **B3. Pair identity.** The delta records `(base_build_id, base_symbol_count,
  base_format_version)`; the reader refuses a mismatched pair and serves
  base-only with the stale signal (§"Nothing binds a delta").

### Requirements the measurements established

1. The exact→prefix→fuzzy ladder is evaluated across the pair, one rung at a
   time, with **one** Levenshtein automaton shared between tiers. (Verified
   possible: `fst` 0.4.7 implements `Automaton` for `&A`, and `Levenshtein`'s
   methods take `&self`.)
2. Documents are never renumbered. Identity is `(tier, tier-local index)`;
   positional sidecars stay per tier; renumbering is compaction's job. This
   holds for the JSON/MCP envelope and the cascade cache — no `sym_idx` reaches
   user-visible output — and fails only for on-disk foreign keys, which is B1.
3. The delta carries a **persisted, mmap'd** bitmap of superseded base
   documents, and the dead test is applied **per candidate, before any per-tier
   truncation** — otherwise a name whose first postings are all dead returns
   nothing where a single index returned ten. Its derivation is
   `(base file table − currently discovered files) ∪ delta's own paths`, which
   is what makes it cover renames and newly-excluded paths as well as deletions;
   the delta's own path set alone does not.
4. `N` and `avg_doc_len` for the pair are stored, not recomputed, and the
   builder's average is redefined over clamped document lengths — a change that
   moves single-tier scores where the clamp fires, so it takes the same
   `vex eval` gate as requirement 5.
5. Score ties break on a key that is stable across tiers and total —
   `(path, name, line, kind, tier, tier-local index)` — in **both** the BM25 and
   the structural channel, adopted in the single-tier path first so `vex eval`
   can measure what it moves.
6. The delta inherits the base's build configuration. A delta built without
   BM25 silently excludes its files from `N` and `avg_doc_len`; one built
   without `--semantic` drops recently edited files out of the semantic channel
   — the same degradation this document rejects for `bodytokens`. Any change to
   `embedder_id`, `vectors_normalized` or the sticky opt-outs forces compaction
   instead of producing a mixed pair.
7. Aggregate-over-corpus reads consult the bitmap too. `top_n_by_indegree`
   counts distinct callers corpus-wide and would double-count across tiers.

### Still unmeasured

The write path itself; the pointer commit and its reclamation protocol; the
HNSW size invariant across a pair (§"What this does not measure"); compaction's
policy, cost and mid-compaction reader contract. A per-artefact table — which of
the ten-odd files under `index_dir` are per-tier, which are project-level, which
are only rebuilt at compaction — does not exist yet and should.

### The cheaper thing, measured — one half of it lands, the other does not

Both halves were measured on CPython (78 535 symbols, 4 419 files), where a
one-file-edit `vex update` costs **2 020 ms**.

**`bodytokens`: 195 ms → 7 ms, and not for the reason anyone assumed.** The
round trip an update pays was `load` 47 ms + `save` 148 ms. That is not the
layout: `save` issued *two unbuffered `write_all` syscalls per record* — some
157 000 of them for 12 MiB of payload — and `load` read per record the same way.
Serialising into one buffer and writing once, with the format and every
validation unchanged, takes the round trip to **7.25 ms (27×)** and the whole
update to ~1 855 ms. Byte-for-byte the same sidecar; 3 672 tests still pass.

The per-file-block layout the review proposed was also measured — copy the
unchanged byte ranges, re-encode only the changed span, write and rename — and
it lands at 1.4–1.9 ms for 1 to 200 changed records. That is another ~6 ms
beyond buffering, for a layout change, an API change and a new invariant.
**Not worth it.** Buffering captures 96 % of the available win with none of it.

**The same pattern was in two more sidecars, and one of them was far worse.**

| sidecar | before | after | |
|---|---|---|---|
| `bodytokens` round trip, 78.5k records | 195.55 ms | 5.97 ms | 33× |
| `trigram` save, 2 233 file records | 13.30 ms | 0.20 ms | 67× |
| `trigram` save, 435 file records | 2.48 ms | 0.06 ms | 39× |
| **`embed_cache` save, 20 000 × 384-dim vectors** | **7 018 ms** | **8.96 ms** | **784×** |

`trigram::save` issued six `write_all` calls per file record; the new output is
asserted byte-identical to the old. `EmbedCache::save` issued **one `write_all`
per `f32`** — 385 syscalls per cached vector, 7.7 million for a 20 000-entry
cache — so it cost about 0.36 ms per cached symbol. That is ~7 s at the
synthetic size measured here and would be ~28 s on a 78k-symbol corpus, paid on
every `vex index --semantic` and every semantic `vex update`. The cache is
written for one symbol's worth of change just as readily as for a full build.

That number is synthetic — building a real cache needs a semantic index, and the
codec cost depends only on entry count and dimension — but it is the production
`save` and `load` on both sides of the comparison.

Review caught one thing worth recording, because it is a trap any "just buffer
it" change walks into: reading the whole file up front moves the point at which
a corrupt file is refused. The old readers rejected bad magic after four bytes;
a naive `fs::read` pays for the file in full first, and in `EmbedCache`'s case
that silently invalidated a comment claiming the entry-count cap prevented an
OOM on read.

The three codecs now share `util::sidecar::SidecarReader`, which reads in the
order the format is written: magic, then the header, then a body **bounded by
what the header just claimed**. A file whose header says "three records" cannot
cost more than three records' worth of memory however large it is on disk, and
an absurd `count` is refused from the header alone. That is a tighter bound than
a size ceiling derived from the format's theoretical maximum, which for the
embedding cache would have been 15 GB and therefore no bound at all.

Both reviewers also asked for golden-byte tests, since "identical bytes, no
version bump" is the entire justification for skipping a format change and a
round-trip test cannot prove it — a symmetric change to `save` and `load` passes
every round trip while misparsing every sidecar already on disk. Those now exist
for all three formats, alongside header-truncation and bounded-body tests.

**Term-bag caching for BM25: rejected, 8 %.** Decomposing `build_bm25_index`:

| phase | ms | share |
|---|---|---|
| T — assemble the bag + `tokenize_document` | 105 | 33 % |
| A — `add_document` | 65 | 20 % |
| B — sort + FST + postings | 152 | **47 %** |
| total | 322 | |

Caching the term bags removes T, but the cache has to be read back and split:
48 ms to load plus 30 ms to split, so the net saving is **27 ms — 8 % of the
BM25 phase and 1.3 % of the update**, in exchange for another 9 MiB sidecar to
keep coherent. Not worth building.

What the decomposition does say is that BM25's cost is dominated by phase B, a
sort and an FST build **over every symbol in the corpus** — which is exactly the
global-rebuild problem this design exists to remove, and which no amount of
caching upstream of it touches. So the measurement cuts both ways: it takes
`bodytokens` off the table as an argument for segmentation, and it strengthens
the argument for BM25.

**Net effect on the comparison.** On the non-semantic path the cheap alternative
removes ~165 ms of a 2 020 ms update — 8 %. It does not make the segmented index
unnecessary: it removes the weakest of the three phases from the case for it and
leaves the strongest (`build_bm25_index`, and `write_index_to` with it)
untouched.

On the **semantic** path the picture is different in kind. A multi-second
sidecar write, scaling with corpus size and paid on every update, was not on any
roadmap and is not something segmentation would have fixed — a delta still has
to write the cache. It was found only because the review asked for the cheap
thing to be measured before the expensive one.

### The original framing of that challenge, kept for the record

From this document's own opening table, `bodytokens` costs 75 ms per update and
has **no query-time reader at all**: it is a `Vec<Option<String>>` fully
deserialised and fully re-serialised every update, moving overwhelmingly
identical strings. A per-file-block or copy-through layout would cut most of
that inside one module, with no format change, no read path, no atomicity, no
GC, no compaction. `build_bm25_index` at 115 ms re-tokenises documents whose
term bags were just read back from that same sidecar; caching the tokenised bag
is likewise one module.

That is roughly 150 ms of the addressable 320 ms, with none of B1–B3 attached.
The design should either do that first and re-measure whether 485 → ~330 ms
still leaves a problem worth a two-file index, or state why not.

That challenge has now been answered by measurement — see the section above.
Half of it was right and is done; half of it does not pay. What stands
unanswered is its last point: this document never names a user-facing symptom.
`vex update` runs from `watch` or `--auto-update`, and 485 ms (or 2 s on a
78k-symbol corpus) in either is not self-evidently a defect. A change of this
blast radius should say whose experience it improves.

### Two bugs found while reviewing this, unrelated to the design

- `search_with_fallback`'s fuzzy rung breaks only its inner loop on reaching the
  limit, so it can return slightly more than `limit` results.
- The `avg_doc_len` clamp inconsistency described in §"Caveats" is present in
  today's single index, not introduced by segmentation.

### Verdicts

Reviewed 2026-08-15 by architect (**REJECT** for proceeding to step 4 — not a
rejection of the idea, of the sequencing claim) and rust-reviewer
(**APPROVE-WITH-CHANGES**; its one critical finding, that the BM25 exactness
fixture never exercised the dead-length subtraction, was correct in substance
and has been answered by the edited-content column in §"Resolved"). The
recommended next step is a **step 3.5** — B1 and B2 on paper, both cheaper than
a write path and either capable of reshaping the design — with B3 folded in
regardless.
