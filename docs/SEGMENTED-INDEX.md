# Base + delta index — design sketch

> **Status: CLOSED 2026-08-16. Nothing here is implemented and nothing here
> will be.** The decision this document was written to inform has been taken:
> **do not build it.** Roadmap item #10 in the storage research is closed on two
> grounds — the premise was superseded by a much cheaper fix that shipped, and
> one acceptance criterion turned out to be false unfixably. Both are set out in
> §"Where this leaves the decision"; the short version is in the next paragraph
> but one. The rest of the document is kept because the measurements are sound
> and the shape is worth having if the question ever reopens.
>
> **Closure in one paragraph.** The write cost this design existed to remove
> reached a user through exactly one path — a *synchronous* rebuild inside
> `--auto-update`. Readers take no lock and a live mmap survives the atomic
> rename, so queries issued during a rebuild cost 8–15 ms. Making auto-update
> non-blocking (`--async-update`, shipped, no format change) took the
> user-visible number from **1891 ms to ~20 ms** — an order of magnitude better
> than this design's ~250 ms. What remained as its whole benefit was background
> CPU under `vex watch`, which cannot pay for a pointer protocol, a compaction
> policy, an approximate binder tier, and two changes to *today's* ranking.
> Independently: `writer.rs:658` binds a reference only when there is exactly one
> candidate, so a reference in a file **no tier re-extracts** flips its binding
> when the corpus-wide candidate count changes, with nothing recording it — only
> compaction restores parity, so **"compaction is never required for
> correctness" is false** and exact binder parity is unattainable at any cost.
>
> Everything below was written while the decision was still open, and is left in
> the present tense. Read it as the record of how the closure was reached.
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
> Review then found that `ref_edges` and `hierarchy_edges` address symbols by
> tier-local index, so a pair loses scope-accurate results across tiers. Step 3.5
> measured that loss (~18 % of strict reference edges at a 20 % delta), mapped
> the reader rules onto a recall/precision frontier, and established that the
> **writer**-side fix is the autonomous half. It also established the thing that
> caps the whole design: **exact binder parity with a rebuild is unattainable at
> any implementation cost**, because a reference in a file no tier re-extracts
> can change its binding when the corpus-wide candidate count changes — only
> compaction restores it. Acceptance criterion 4 is weakened accordingly
> (§"The finding that caps all of it"). Write-cost growth is measured and mild —
> ~95–550 ms at realistic delta sizes against ~1 850 ms today — but a compaction
> that lands on a `git pull` costs ~2.8 s, worse than today.
>
> **Both reviewers recommended descoping rather than proceeding**, and the two
> experiments they asked for have since run. The first one moves the ground: the
> 1.85 s only reaches a user through a **synchronous** auto-update, readers are
> never blocked by a rebuild, and making auto-update non-blocking — with
> primitives vex already has — takes the user-visible cost to **~20 ms**, an
> order of magnitude better than a segmented index could. The second says that
> if a pair is built anyway, a **tight compaction threshold beats any reader
> rule** (≥ 96 % of binder edges at a 5 % delta, ≥ 98.6 % at one file), which
> makes the writer refactor deferrable. See §"Both experiments ran".
>
> Pair identity — nothing tying a delta to the base it was written against — is
> now designed and needs no format change (§"Designed (B3)"). ~~**One blocker
> remains open: the cross-tier edges.**~~ **That blocker was not resolved; it was
> made moot by the closure above.** Requirements and what was still unmeasured
> when the decision was taken are in §"Where this leaves the decision".

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
  `to_sym_idx` the dead bitmap now kills, so callers of an edited symbol
  disappear until compaction.

Written first as "loses every caller elsewhere in the repo", which the
measurement below refutes — 82 % survive with no fix at all. The damage is still
proportional to the size of the base rather than to the size of the change,
which is the inversion this design exists to remove. It hits
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

#### Measured, and it splits cleanly into two directions

Step 3.5 measured the loss instead of arguing about it. CPython, 20 % of files
edited (whole-line comments stripped, no identifier renamed, so the reference
graph stays consistent), base = the corpus before the edit, delta = an index
over the edited files, control = a full rebuild after it. 343 symbol names
probed, 4 516 strict reference hits expected.

| what the pair can produce | recall (343 names) | recall (17 166 names) |
|---|---|---|
| edge sections only (drop edges whose target is dead) | 87.7 % | 81.7 % |
| + name resolution via the unresolved section — route (a) | 88.5 % | — |
| **+ shadow by referencing file — route (d), below** | **92.2 %** | **86.3 %** |
| + both (a) and (d) | 93.8 % | 92.9 % |

The second column exists because review swept the sample size and found the
recall **degrades monotonically** with it — 100 names gives 97.3 % for route
(d), 400 gives 92.2 %, a near-exhaustive 17 166 gives 86.3 %. The small sample
is optimistic, not noisy, so the headline is the right-hand column: route (d)
recovers about **half** of the loss, not three quarters, and a pair without any
fix loses ~18 % of strict hits rather than ~12 %.

Per symbol at the 343-name sample: 84 intact, 26 partly lost, **3 lost
entirely** — a user hitting one of those three sees a confident, silent, total
miss, which matters more for trust than the aggregate does.

The loss decomposes into two directions that need different answers, and the
proportions are the opposite of what an early draft of this section claimed. Of
the 557 hits lost at the 343-name sample, route (d) recovers **203 — 36 %**.
The remaining **354 (64 %)** is the other direction, and that is the half that
needs a writer change.

**base → delta: references from unchanged files into edited ones.** The smaller
half (36 %), and route (a) cannot recover any of it: these references *resolved*
when the base was built, so they sit in its resolved edge section and appear in
nobody's unresolved section. But a base edge's target is still readable — the
symbol record is superseded, not deleted — so a reference to "the symbol that
was `name` in the base" is still a reference to `name`, at a line in a file that
did not change.

That gives a fourth route the design never listed: **shadow by the referencing
file, not by the target symbol.** Drop a base edge when the delta re-extracted
the file the reference lives in; keep it otherwise, whatever happened to its
target. Guard the one case that needs it — a symbol the edit deleted outright —
by checking the name still resolves somewhere in the pair.

Measured, route (d) leaves **zero** misses in untouched base files, and that
holds from 100 probed names to a near-exhaustive 17 166. It also *improves*
precision over the naive pair (934 → 607 false hits), because dropping base
edges from re-extracted files removes stale references at pre-edit line numbers.
It needs no format change, no new section and no tier demotion — it is a
different rule about which edges to ignore.

**But "zero misses" is not the completeness result it looks like.** The fixture
strips comments and nothing else: no definition is added, deleted, renamed or
moved. Under that edit every base edge out of an untouched file is a true hit
*by construction*, so the measurement has little power to falsify the rule it is
quoted for. Three edit shapes it cannot produce do break route (d):

- **A deleted symbol with a surviving namesake.** `Foo` is deleted from an
  edited file while another `Foo` exists elsewhere. The guard — "the name still
  resolves in the pair" — passes, so every base edge that pointed at the deleted
  `Foo` is reported as a usage of the survivor: confident false hits at real,
  unchanged `path:line`. A rename with a surviving namesake is the same
  mechanism.
- **An ambiguity flip, in both directions.** A reference in an *unchanged* file
  binds through the single-candidate fallback when a name has exactly one
  definition. An edit that adds a second definition makes the rebuild decline
  while route (d) still reports; an edit that removes one makes the rebuild
  resolve a reference the pair cannot produce at all, because no base edge
  exists to keep. No edge-shadowing rule can see either: the referencing file
  never changed.
- **A weaker guard than described.** `SymbolFstReader::find` keys on the
  lowercased name *and its CamelCase sub-tokens*, so `defines(delta, "Reader")`
  is true when the delta merely holds `BufferedReader`. The guard as measured
  asks "is this a sub-token of something in the pair", not "does this name still
  resolve".

Two corrections to the rule itself follow. Its shadow set must be requirement
3's dead-file set — `(base file table − currently discovered files) ∪ the
delta's paths` — not the delta's file table, which cannot see a deleted file and
would keep base edges pointing into paths that no longer exist. And its guard
must test that the *target record* still resolves, not that the name does.

**delta → base: references from edited files into unchanged definitions.** All
354 remaining misses are here. The delta's Pass-2 resolver builds its name map
from the symbols it is writing, so a reference out of an edited file into an
unchanged definition cannot bind. Route (a) recovers only 72 of the 354, because
a reference lands in the unresolved section only when the delta defines *no*
symbol of that name — and in an 883-file delta it usually defines one, so the
reference binds locally instead, to whatever the small tier happens to contain.

Which is also where the remaining false hits come from: in this sample **all 607
live in delta-owned files** — though at 50× the sample size four counterexamples
appear in untouched files, so that is an observation, not an invariant. A small
tier binds more aggressively than the corpus does: the single-candidate fallback
that makes `usages --strict` work sees one candidate for an ambiguous name where
the full index sees many and declines.

**The false-hit integers are one identifier, not a rate.** 595 of the 607 come
from the name `test`, and all 607 from just three names; every other probed name
contributes zero. Drop `test` and false hits fall 607 → 12 while recall barely
moves. CPython's test suite defines the bare identifier `test` in hundreds of
scopes, which is exactly the pathology the single-candidate fallback has on a
small tier. So the mechanism is real and worth fixing, but "607 false hits"
should not be read as a general precision figure for a pair.

#### One fix for both, and it is in the writer

Both residues have the same cause — the delta resolves references against
itself — and therefore the same direction of fix: **the delta's Pass-2 must see
the base's symbol table.** The base is already open, its symbol FST is already
mmap'd, and building a name map from its 78 535 symbols measures at 8–9 ms.

An early draft of this section called that "seed the name map" and concluded it
needed no format change. Review took the claim apart, and it does not survive
contact with `writer.rs`.

`name_to_global`'s values are **`SymbolRecord` positions in the tier being
written** — a comment at `writer.rs:504` says so, having been written to fix a
previous bug of exactly this kind. Five sites consume that space assuming the
local tier, and one of them, `writer.rs:664`, puts the value straight into
`RefEdgeBuilder { to_sym_idx }`, which is what lands on disk. A base-sourced
index written there is resolved by the reader against the *delta's* symbol
table: not a miss, an arbitrary wrong answer. The others —
`resolve_by_name_and_path`, `resolve_hierarchy_captures`, the include-BFS
resolver, and the `imported_by` pair builder — all index `sym_to_file_id`, a
`Vec` over the new tier.

So the honest scope of requirement 9 is: **a tier-tagged symbol reference
threaded through Pass-2**, with a writer branch that routes base-resolved hits
somewhere other than the local edge section. That is route (b) moved from the
format into memory, not route (b) eliminated. It is still the right call —
in-memory tagging is far cheaper than widening every on-disk edge record — but
it must be planned as a refactor, not a seeding.

Three further costs the early draft missed:

- **The tier demotion comes back.** An `UnresolvedRef` record carries
  `(from_file_id, line, col, kind)` and no target identity, so a cross-tier hit
  is name-resolved at read time however carefully the writer bound it. That is
  the same demotion route (a) was charged for, and `vex impact`'s verdict logic
  would have to reflect it.
- **It breaks an invariant a shipped path documents.** `cmd_usages.rs:410`
  states that a member defining `name` cannot have `name` in its unresolved
  section, because the writer's capture gate excludes any name with a local
  definition — and the workspace fan-out skips owners on that basis. Reusing the
  section for "resolved against the base" makes both true at once, and the skip
  would drop exactly the hits this fix recovers.
- **The spill path is filtered.** That capture gate also runs
  `is_meaningful_identifier`, which rejects pure-lowercase names without an
  underscore — `main`, `read`, `close`, `parse` never reach the section. Any
  projection of "closes the 354" silently inherits that filter.

One thing the review confirmed rather than demolished: the **placement** is
right. The Pass-2 loop is sequential and runs after parsing, so seeding it does
not disturb the rayon parallelism that a per-language binder hook would break —
which is the constraint this project has already locked once.

So the routes collapse only partly. Route (c) (scope the binder-backed commands
out) stays unnecessary; route (a) alone is insufficient *and* imprecise; route
(b) is not eliminated but relocated into memory. What remains is route (d) in
the reader — cheap, and sound only for edits that do not change the definition
set — plus a tier-aware Pass-2 in the delta writer, whose recall is **projected
from where the misses are, not observed**.

**What this measurement does not cover:** one corpus, one edit shape, one delta
size, `usages --strict` only. `implementations` / `subtypes` read the hierarchy
section, which has the same tier-local shape and should be assumed to have the
same two directions until measured. And route (d) is measured as a *reader*
rule; the tier-tagged Pass-2 is designed, not built, so its recall is projected
from where the misses are, not observed.

#### Measured at target level: a recall/precision frontier, and a rule that dominates

The site-level projection was the review's main objection, so the comparison was
redone over **edges** — `(target path, target name, reference path, reference
line)` — so that reporting the right site for the wrong symbol scores as wrong.
Four candidate reader rules, written as drop-predicates over base edges:

- **naive** — drop ⟺ the target was superseded. (What a pair does with no work.)
- **route (d)** — drop ⟺ the referencing file was re-extracted.
- **route (d′)** — route (d) plus a per-target guard.
- **route (e)** — drop ⟺ **both**. Keep a base edge unless the delta is capable
  of producing its replacement. Its drop set is a subset of both others', so its
  recall cannot be lower than either — by construction, in any fixture.

Three edit shapes on CPython at a 20 % delta, 17 321 names, and — the control
this section was missing — one fixture with **no edit at all**, where the delta
re-indexes 402 byte-identical files:

| rule | comments stripped (lines shift) | definitions renamed away | no edit at all |
|---|---|---|---|
| naive | 81.2 % / 13 277 extra | 93.2 % / 2 741 | 93.2 % / 2 741 |
| route (d) | 86.0 % / **2 747** | 85.9 % / 2 741 | 85.9 % / 2 741 |
| route (d′) | 86.0 % / 2 747 | 85.9 % / 2 741 | 85.9 % / 2 741 |
| **route (e)** | **87.7 %** / 13 277 | **100.0 %** / 2 741 | **100.0 %** / 2 741 |

Read the second column of numbers in each cell: this is a frontier, not a
ranking. An earlier draft of this section reported recall only, which is the one
axis route (d) was designed to trade away — its whole original justification was
precision.

**The no-edit control settles what an earlier draft got wrong.** It claimed route
(d) "is 7 points worse than doing nothing" because of the renaming, and concluded
that B1's halves "do not separate". But the same 5-point gap appears with *no
edit whatsoever*, and review's decomposition shows **99.96 % of route (d)'s
losses are base edges from re-extracted files pointing at targets nothing
touched** — discarded because the reference sits in a re-extracted file, and
unrecoverable because the delta's Pass-2 sees only its own files. That is exactly
the delta→base defect §"One fix for both" already attributes to the **writer**
half. So the corrected conclusion is narrower and more useful:

> The **writer** half is the autonomous one: it fixes 64 % of the loss, it is
> the sole cause of the delta's over-binding, and it composes with the naive
> rule, which costs nothing. The **reader** rule cannot be evaluated before it,
> because every reader rule that drops base edges is trading them for delta
> edges the delta cannot yet produce.

**Route (e) is the rule to ship in the meantime.** It is perfect on both
line-stable shapes and best on the line-shifting one, and its precision is never
worse than naive's. The precision it does not fix — 13 277 extras on a
line-shifting edit — is *stale line numbers* from base edges whose file was
re-extracted, and route (d) is the only arm that removes them. Which says the
final rule is probably neither: keep a base edge from a re-extracted file only
where the delta produced no edge of its own for that target. That fallback shape
is unmeasured.

Two limits rather than results. The per-target guard (route (d′)) measures
**identical to route (d)** everywhere, so it is still untested — and for the same
reason the renaming fixture is weak overall: the names chosen (`setUp`,
`tearDown`, `close`, `write`, `read`, `run`) have hundreds of definitions each in
CPython, so the single-candidate fallback declines and few binder edges point at
them at all (766 candidates, 132 resolved edges for `setUp`). That one property
neuters the guard test *and* the recall comparison on that fixture. Testing
either needs an edit that deletes a **uniquely named** definition with real
incoming callers.

And `hierarchy_edges` behave the same way, with both shapes measured this time:
5.4 % loss naive / 4.3 % with route (d) on the comment-stripping edit, but
naive 97.5 % / route (d) 95.7 % on the renaming one — the same flip, from the
same cause. An earlier draft quoted only the first shape.

**The oracle's own blind spot,** for completeness: the compared tuple carries no
target line, so two same-named definitions in one file — Python's
platform-conditional `def` is the common case — are indistinguishable. A
reference bound to the wrong one of those scores as correct. Column collapsing
has the same shape and was already disclosed.

#### The finding that caps all of it: binder parity is unattainable

This document has been arguing about how large the cross-tier error can be made.
Review pointed out that it cannot be made zero, and the proof is already in the
code:

```rust
// writer.rs — the single-candidate fallback
name_to_global.get(r.name.as_str()).filter(|hits| hits.len() == 1)
```

A reference resolves only when its name has **exactly one** definition in the
corpus; with two or more the writer declines and records no edge. And an
unresolved reference is only spilled into the name-keyed section when the name is
defined *nowhere* locally and passes `is_meaningful_identifier`.

Now take a reference in a file **no tier re-extracts**. At base-build time its
name had two definitions, so there is no edge and no spill — nothing recorded at
all. An edit deletes one of those definitions: a full rebuild now resolves that
reference, and the pair cannot, because nothing in either tier knows the
reference exists. Symmetrically, an edit that adds a second definition makes an
edge the base recorded silently wrong, and no tier can tell.

No reader rule sees this — the referencing file did not change. The tier-tagged
Pass-2 of requirement 9 does not see it either — it re-resolves the *delta's*
files. **Only re-running Pass-2 over the whole corpus restores parity, which is
compaction.**

So **acceptance criterion 4 is false as written** for the binder sections.
"Compaction is never required for correctness" must become "compaction is
required for exact binder parity; between compactions the pair is approximate,
and says so." That is a `docs/LIMITATIONS.md` entry and an envelope signal — i.e.
partially route (c), which this document twice called unnecessary. The magnitude
is unmeasured and the mechanism is not exotic: the single-candidate fallback is
why `--strict` works at all in languages without an include graph, and creating or
destroying a namesake is an ordinary edit.

Everything else in B1 is haggling over how far above zero the error sits.

**The site-level measurement is also blind to the error route (d) most plausibly
introduces.** A hit
here is `(path, line)` — *where the reference occurs*, never *what it points
at*. Every case in the deleted-namesake and rename shapes above produces a
set-identical `(path, line)`: the reference site is unchanged and only its target
is wrong. Recall and false-hit counts are therefore **site-level**. That is
adequate for `vex usages <name>`, which unions all symbols of a name anyway, and
inadequate for anything that consumes the target — `impact`'s binder tier,
`subtypes`' traversal, `imported_by`. Column and duplicate references on one
line collapse in the same projection.

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

### Measured: what rewriting the delta actually costs

Step 3.5 measured the rewrite curve on CPython by building indexes over
uniformly-sampled subsets, which is what "rewrite the delta over the cumulative
changed set" costs. Two arms, because the answer turns on a cache that already
exists — the content-addressed blob-SHA parse cache only works in a git
checkout, so a fixture without one measures the pessimistic case:

| delta size | cold (no parse cache) | rewrite after a one-file edit (cache warm) |
|---|---|---|
| 400 files / 5.8k symbols | 401 ms | **119 ms** |
| 1 600 files / 25.8k symbols | 861 ms | **310 ms** |
| full corpus (compaction) | 2 240 ms | — |

**Which column applies is decided by git, not by cache warmth.** The blob cache
is keyed on committed blob SHAs and the git backend deliberately drops dirty
paths (`vcs/git.rs:288`) so a working-tree AST can never be filed under an index
blob. A delta is, by construction, the files that changed since the base — and
in the `vex watch` / `--auto-update` story that motivates this design, those are
uncommitted edits. They miss the cache in both directions. The warm column above
was produced by editing **one** file of 400, so it is the optimistic bound; a
delta of dirty files sits near the cold column.

The honest statement is therefore a **band**, and a line fitted to the two warm
points is 0.159 ms/file with a 55 ms intercept — not the 0.2/50 an earlier draft
published, which over-predicts its own upper point by 19 %. That intercept is
already the subset's own discover-and-write floor, so adding a separate
corpus-wide floor on top double-counts it. With that corrected, and adding the
8–9 ms the base-name-map seed costs (§"One fix for both" requires it, and the
subset builds never paid it):

| delta | all files committed | all files dirty |
|---|---|---|
| 5 % (220 files) | ~95 ms | ~250 ms |
| 20 % (884 files) | ~205 ms | ~550 ms |
| 36 % (1600 files) | ~320 ms | ~870 ms |

against **~1 850 ms** for today's full update. The growth C2 warned about is
real; even at its pessimistic end a pair is 2–7× cheaper, and 5–19× at the
optimistic end. That qualitative conclusion is the robust part; the milliseconds
are single runs on an unpinned laptop and should be read as indicative.

Two things the model does *not* cover: it is fitted over a 4× range and cannot
distinguish linear from `n log n`, which matters because BM25's dominant phase
is a sort plus an FST build; and extrapolated to the full corpus it predicts
~950 ms against a measured 2 240 ms, so it is wrong at its own compaction
threshold.

**Amortisation depends entirely on the edit pattern, and the flattering pattern
is the wrong one.** One file per edit gives ~880 updates between compactions at
a 20 % threshold, so the 2.24 s rebuild costs ~2.5 ms per update. But the events
that actually cross a 20 % threshold are `git pull`, a branch switch, a rebase,
a formatter run — each of which changes hundreds of files in **one** update.
Then a single user-facing operation pays the delta rewrite *and* the compaction:
≈ 550 ms + 2 240 ms ≈ **2.8 s, against 1 850 ms for the same operation today** —
a regression, at the moment a user is most likely to search next.

**Criterion 2 restated.** "Under 150 ms on a one-file edit" describes only the
first update after a compaction. The budget should be: *median update under
300 ms; update at the compaction threshold under 600 ms; and the
threshold-crossing update — rewrite plus compaction — under 2 s, i.e. no worse
than today's flat cost.* That last line is the one this design currently fails,
and it is a policy problem (when to compact, and whether compaction can be
deferred or done off the critical path), not a measurement problem.

**On two tiers versus N: B2 does not settle it, and an earlier draft claimed it
did.** What B2 shows is that write growth is mild. The choice between two tiers
and N turns on things B2 never measured — that both need a compaction policy
anyway (§"Compaction needs a policy"), that criterion 1 extrapolates to eight
tiers inside its budget, and that the real differential is bounded versus
unbounded readers per query. That argument is the sketch's, not the
measurement's, and it should be labelled as such.

## Nothing binds a delta to the base it was written against

The dead bitmap is indexed by base `sym_idx`. Nothing in the format ties it to a
particular base. `IndexReader::open` validates magic and accepts any version in
`MIN_SUPPORTED_VERSION..=VERSION` — currently v3..v8 — with no build identity at
all. So a `vex index`, a compaction, or a different vex binary that rewrites the
base while a delta exists leaves every bit in that bitmap pointing at an
unrelated symbol: live symbols vanish, stale ones surface, no error, no stale
signal. A v3 base and a v8 delta likewise open cleanly and get merged
section-by-section.

### Designed (B3)

**There is nothing in the current index to identify a build with.** `Header`
carries magic, version, symbol count, a vector dimension and section
offsets — no build id, no nonce, no content digest. The manifest is closer but
not sufficient: `indexed_at` has one-second granularity, so two rebuilds in the
same second are indistinguishable, and `git_head` does not move for a
working-tree edit, which is the case that matters most. So identity has to be
constructed.

**The triple.** A delta records, about the base it was written against:

| field | source | catches |
|---|---|---|
| `base_format_version` | the base's header | a v3 base merged with a v8 delta, which today both open cleanly and get merged section by section although their section sets differ |
| `base_symbol_count` | the base's header | any rebuild that adds or removes a symbol — the overwhelming majority of accidental replacements |
| `base_layout_digest` | xxh3 of the base's fixed-size header **and** its section sub-headers | any rebuild that changes a section's offset or length, i.e. essentially any content change that keeps the symbol count |

Validation is three comparisons over a few hundred bytes that the reader has
mapped anyway, plus one hash of those bytes — nanoseconds, against a read-path
budget of ~0.1 ms. It is affordable on every invocation, which matters because a
guard that is only checked sometimes is not a guard.

**Where it lives: the pointer, not the delta's header.** Roadmap #4 has to
introduce a pointer to the current pair regardless, and the pointer already has
to name both members. Recording the base's triple there means the pair is
validated in the *same* read that resolves it, and no index format changes — a
field in the delta's `Header` would be a format bump, and a field in a manifest
can be rewritten independently of the pair it describes.

**What the reader does on mismatch:** refuse the pair, serve the base alone, and
set the envelope's existing `vex.dev/stale` with a `vex.dev/stale_reason` naming
the mismatch. Silently ignoring the delta would mean answering from a stale index
with no signal at all, which is the failure this whole section exists to prevent.
A format-version mismatch is refused even when the other two fields agree,
because section presence differs across the accepted range.

**The residual, and why it is benign.** A rebuild that produces an identical
symbol count *and* an identical section layout is undetectable by this scheme.
That is precisely the case where the base's content is unchanged, and an
unchanged base yields the same `sym_idx` for every symbol — so a dead bitmap
indexed against the old build is still correct against the new one. The scheme
fails exactly where failure does not matter.

That argument rests on the writer being reproducible, so it was checked rather
than assumed: two independent builds of the same 883-file corpus, in different
directories and therefore different cache dirs, produced **byte-identical**
`index.vex` files (9 003 676 bytes, same SHA-256). That is stronger than the
symbol-order determinism the argument needs. It is one platform and one binary,
so the property should be pinned by a test before a correctness guard leans on
it — but it holds today.

**Upgrade path.** The next format bump — the callgraph CSR retrofit is already
waiting for one — should stamp an explicit random `build_id` into the header.
That removes the reliance on determinism and reduces the triple to one
comparison. Until then the derived digest is the honest substitute, not a
placeholder to be forgotten.

**What B3 does not solve:** the window between resolving the pointer and mapping
both members, which is #4's reclamation protocol; and the sidecars, each of which
keeps its own downstream guard.

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
4. ~~Compaction is never required for correctness, only for performance.~~
   **False as written, and not fixable** — see §"The finding that caps all of
   it". For `ref_edges` and `hierarchy_edges`, compaction is required for exact
   parity with a rebuild, because a reference in a file no tier re-extracted can
   change its binding when the corpus-wide candidate count changes. Restated:
   *compaction is never required for correctness of the sections that do not
   resolve names across files; for the binder sections the pair is approximate
   between compactions and must say so.*

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
   since compaction (B2). **B2 done.** **B1 measured and scoped, not closed** —
   one free reader rule that covers a third of the loss and holds only for edits
   that leave the definition set alone, plus a writer refactor that is still
   unbuilt, plus four sections nobody has looked at. B3 (pair identity) is two
   fields and still open.
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

- **B1. Cross-tier `ref_edges` / `hierarchy_edges` — blocking, and now bounded
  from below as well as above.** A pair loses ~18 % of strict reference edges at
  a 20 % delta; the best reader rule measured (route (e)) reaches 100 % on
  line-stable edits and 87.7 % when the edit shifts lines, at a precision cost
  only route (d) removes — a frontier, not a fix. The autonomous half is the
  **writer** (requirement 9): a tier-tagged Pass-2, which fixes 64 % of the loss
  and composes with the zero-work naive rule. And **exact parity is
  unattainable at any cost** (§"The finding that caps all of it"), so criterion 4
  is weakened and this design ships an approximate binder tier between
  compactions whatever else is built. Unpriced still: the call graph's caller
  side (probably free — both endpoints of any lookup are same-tier), the
  `subtypes` traversal across tiers (error compounds per hop), `imported_by`
  (a delta's manifest under-populates the cascade and recovery costs O(corpus)),
  and `pattern_index_full` (no worse than today's post-update state).
- ~~**B2. A write-cost function for update *N* since compaction.**~~
  **Measured, with one policy problem left.** ≈ 55 ms + 0.16 ms per file
  changed, as a band from ~95 ms (committed files, 5 % delta) to ~550 ms (dirty
  files, 20 %), against ~1 850 ms today. Criterion 2 restated. What is not
  settled is compaction scheduling: a threshold-crossing update pays rewrite +
  compaction ≈ 2.8 s, worse than today. See §"Measured: what rewriting the delta
  actually costs".
- ~~**B3. Pair identity.**~~ **Designed.** The pointer that roadmap #4 must
  introduce anyway carries `(base_format_version, base_symbol_count,
  base_layout_digest)`; the reader validates all three in the same read that
  resolves the pointer — a few hundred already-mapped bytes and one hash — and on
  mismatch refuses the pair, serves the base alone and sets the envelope's
  existing `vex.dev/stale` + `stale_reason`. No index format change. The one
  undetectable case is a rebuild with identical layout and symbol count, which is
  exactly the case where the old bitmap stays valid, because builds are
  reproducible — verified byte-for-byte, not assumed. See §"Designed (B3)".

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
8. **Every merge point declares which liveness rule it is under.** Requirement 3
   shadows a *symbol* by its defining file; edges are shadowed by their
   *referencing* file and deliberately ignore target liveness. One query applies
   both rules to the same posting list — dead-filter the `sym_idx` when listing
   definitions, do not dead-filter it when following its edges. That is
   implementable but it is a trap to leave implicit, so the design owes this
   table rather than an inference:

   | section | liveness key | status |
   |---|---|---|
   | symbol records / symbol FST | defining file (dead bitmap) | requirement 3 |
   | `ref_edges` | referencing file; target liveness ignored | measured |
   | `hierarchy_edges` | referencing file, **and** `from_sym_idx` must still resolve | unmeasured |
   | call graph | **`caller_sym_idx` is tier-local too** — the difficulty table's "keyed by name" is true only of the callee side | unaddressed |
   | `unresolved_hierarchy` | as hierarchy | unaddressed |
   | trigram | path | table above |
   | pattern skeletons | file id — **and** `pattern_index_full`, which a delta can never set, so a pair permanently degrades `vex pattern` to live scan | unaddressed |
   | `imported_by` | project-level, lives in the manifest — a delta writer sees only its own edges, so writing the delta's manifest **erases the base's map** and breaks the cascade | unaddressed |
   | corpus aggregates (`top_n_by_indegree`) | dead bitmap | requirement 7 |

   Transitive traversals are a separate problem no shadow rule solves:
   `subtypes` recurses `find_hierarchy_edges_by_symbol` in one reader at every
   hop, so a chain that crosses the tier boundary simply stops there. That needs
   name-keyed hopping or the tier-tagged identity of requirement 9.
9. **The delta's Pass-2 carries a tier tag.** Resolving against base ∪ delta is
   the goal — otherwise references out of edited files bind to whatever namesake
   the small tier holds, costing recall one way and precision the other — but
   `name_to_global`'s values are `SymbolRecord` positions in the tier being
   written, and one consumer writes them straight into the on-disk edge section.
   So this is a threaded `(tier, index)` through five Pass-2 consumers plus a
   writer branch for base-resolved hits, plus a decision on the
   `is_meaningful_identifier` gate that filters the spill path, plus a
   re-statement of the owner-skip invariant `cross_repo_hits` documents, plus
   the tier demotion it forces on `impact`'s verdict logic.

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

### The descope both reviewers recommend

Not "shelve the idea" and not "proceed to step 4":

> **The delta carries no reference or hierarchy edges.** `usages --strict`,
> `impact`'s binder tier, `implementations` and `subtypes` read the base alone and
> set the envelope's existing `vex.dev/stale` + `stale_reason`. `search`,
> `grep`/trigram, `callers`/`callees`, `impact`'s call-graph tier and BM25 read
> the pair.

This is route (c) with a signal, and after step 3.5 it is stronger than it looked:

- **B1 disappears.** No tier tagging, no reader-rule selection, no demotion, no
  `is_meaningful_identifier` decision, no collision with the owner-skip invariant.
- **The call graph is portable for free** — a call edge names its callee by
  string and its caller lives in the same tier as the edge, so both endpoints of
  any lookup are same-tier by construction. `impact` keeps its cheapest channel
  fresh and loses only its binder channel.
- **It is epistemically better than any B1 outcome.** Stale-but-internally-
  consistent plus a `stale` flag beats fresh-but-7-to-19 %-wrong-silently, which
  is this document's own stated error preference.
- It is honest about the parity cap instead of discovering it in production.
- It costs nothing to build.

What remains to justify is then the *rest* of the design — requirements 1, 3–7
plus #4's pointer and reclamation protocol plus a compaction policy that
currently regresses the `git pull` case — for a background command with **no
named user-facing symptom**. That question is still open, and it is the one worth
answering next.

### Both experiments ran. The first one undercuts the design's premise.

**Is the 1.85 s visible where it is paid?** Measured on the 78.5k-symbol corpus:

| | wall clock |
|---|---|
| `vex search` on a fresh index | 21–25 ms |
| `vex search` on a **stale** index, no auto-update | 20 ms (stale results, `stale` flag) |
| `vex search --auto-update` on a stale index | **1 891 ms** |
| the same command once the index is fresh | 35 ms |
| `vex search` × 12, **issued while a rebuild runs** | **8–15 ms each, all succeeding** |

The last row is the important one. Readers take no lock and a live mmap survives
the atomic rename, so a rebuild is completely invisible to concurrent queries. The
1.85 s reaches a user through exactly one path: `handle_staleness` rebuilding
**synchronously** before answering. And vex already owns the primitive to not do
that — `pipeline::run_or_busy` / `update_or_busy`, exposed as `--no-wait` on
`index` and `update`, plus the `vex.dev/stale` + `stale_reason` envelope fields
that exist to say "these results are stale and here is why".

So the scheduling fix — serve the current index, kick the update behind it — takes
the user-visible cost from **1 891 ms to ~20 ms**. A segmented index, at its
measured best, takes the same path to ~250 ms. **The cheap fix is an order of
magnitude better than the expensive one at the only place a user feels this.**

That does not make a segmented index worthless — `vex watch` still burns 1.85 s of
CPU per edit, and a delta would cut that to ~100–250 ms — but it moves the
justification from "latency users feel" to "background CPU", which is a much
weaker case and one this document has never argued.

### If it is built anyway: the matrix says use a tight threshold

The full experiment review asked for — recall **and** false edges, five reader
rules, three delta sizes, three edit shapes including one that **adds and deletes
files**, which nothing had modelled:

| fixture | edges | naive | route (d) | route (d′) | route (e) | fallback |
|---|---|---|---|---|---|---|
| shift / 1 file | 85 790 | 98.6 % / 654 | 99.1 % / 630 | 99.1 % / 630 | 99.1 % / 654 | 99.1 % / 654 |
| shift / 5 % | 85 974 | 94.4 % / 2 812 | 96.2 % / **809** | 96.2 % / 809 | **96.7 %** / 2 951 | 96.7 % / 2 942 |
| shift / 20 % | 85 130 | 80.6 % / 13 619 | 85.3 % / **3 297** | 85.3 % / 3 297 | **87.0 %** / 13 820 | 86.9 % / 13 531 |
| stable / 1 file | 84 682 | 99.1 % / 625 | 99.1 % / 866 | 99.1 % / 625 | 99.1 % / 866 | 99.1 % / 866 |
| stable / 5 % | 84 802 | 97.3 % / **525** | 95.4 % / 1 268 | 95.4 % / 648 | **98.4 %** / 1 283 | 98.4 % / 1 283 |
| stable / 20 % | 80 447 | 92.5 % / **2 909** | 81.6 % / 4 898 | 81.6 % / 2 992 | **96.0 %** / 4 958 | 95.8 % / 4 943 |
| adddel / 1 file | 83 502 | 99.2 % / 915 | 99.2 % / **802** | 99.2 % / 802 | 99.2 % / 915 | 99.2 % / 915 |
| adddel / 5 % | 72 243 | 98.0 % / 8 755 | 98.0 % / **7 560** | 98.0 % / 7 560 | 98.0 % / 8 755 | 98.0 % / 8 755 |
| adddel / 20 % | 61 673 | 92.8 % / 17 701 | 92.8 % / **12 706** | 92.8 % / 12 706 | 92.8 % / 17 701 | 92.8 % / 17 701 |

Four conclusions, in order of how much they matter:

1. **Delta size dominates the rule choice.** At a one-file delta every rule is
   98.6–99.2 %; at 5 % the best is 96.7–98.4 %; only at 20 % does the spread open
   up (81.6–96.0 %). So a **tight compaction threshold is worth more than any
   reader rule**, and it makes requirement 9 — the tier-tagged Pass-2 — deferrable
   rather than blocking. That is the single most useful thing this matrix says.
2. **Route (e) is best or tied on recall in all nine cells**, as set inclusion
   predicts, and it is the only rule that is never *worse* than doing nothing.
   Route (d) is worse than naive on both line-stable rows — by 11 points at 20 % —
   which is the finding an earlier draft mis-attributed to renaming.
3. **Precision and recall pull in opposite directions and no rule wins both.**
   Route (d) is the precision winner on line-shifting edits (809 vs 2 951 at 5 %)
   because only it removes base edges at pre-edit line numbers; naive is the
   precision winner on line-stable ones. The `fallback` refinement — keep a base
   edge from a re-extracted file only where the delta produced none — tracks
   route (e) almost exactly, so it is not worth its complexity.
4. **The add/delete shape exposes a rule none of the five implements.** Its false
   edges are high for every arm (7 560–17 701) because a base edge whose *target*
   lives in a **deleted** file is dropped by nothing: naive tests "target's file is
   in the delta", which a deleted file is not. The correct predicate has three
   parts — drop if the target's file is dead and not re-added; keep a superseded
   target only if the delta redefines it; drop a base edge from a re-extracted file
   only where the delta produced its own. That composite is what should be
   implemented, and it is unmeasured.

### What is left to decide — ✅ **decided 2026-08-16: do not build it**

Answering the three questions below in order: **(1) no**, background CPU under
`vex watch` does not pay for the protocol, the policy, the approximate binder
tier and two changes to today's ranking; so (2) and (3) do not arise. The cheap
fix at the bottom of this section shipped as `--async-update`. Roadmap #10 is
closed in `docs/STORAGE-RESEARCH.md` §"#10 closed".

Three things outlived the design and are worth taking on their own: the
**sidecar I/O batching** found while hunting the cheap alternative (one
`Vec<u8>` + `fs::write` per sidecar; bodytokens 195.55 → 7.18 ms, embed-cache
save 7225 → 8.06 ms), the **stat cache** on `hash_files`, and the observation
that **a no-match query is ~92 % `Levenshtein::new`** — 1.6 ms on a large repo,
3.2 ms on a *small* one, index-independent, and the current tail of vex search
latency. That last one is the open cheap win and never needed a segmented index.

The questions as they stood, kept for the record:

1. **Is background CPU worth this?** The scheduling fix removes the latency case
   entirely. What remains is 1.85 s of CPU per edit under `vex watch`, which a
   delta would cut to ~100–250 ms. That is the whole remaining benefit, and it has
   to pay for #4's pointer and reclamation protocol, a compaction policy, the
   approximate binder tier, and requirements 4 and 5 (which change *today's*
   single-tier rankings and need a `vex eval` gate).
2. **If yes, the descope plus a tight threshold is the shape.** Binder sections
   out of the pair with a `stale` signal, or in the pair with a one-file-to-5 %
   threshold where every reader rule holds ≥ 96 %. Requirement 9 becomes an
   optimisation.
3. **The composite reader rule** in point 4 above is the thing to implement if the
   binder sections stay in — not route (d), and not route (e) alone.

The cheap fix should ship regardless of all of it: **make auto-update
non-blocking**. It is the one change here with a measured, order-of-magnitude,
user-visible win, and the primitives already exist. — **It did: `--async-update`
/ `async_update`, plus an MCP argument and a `vex capabilities` flag.**

### Verdicts

Reviewed 2026-08-15 by architect (**REJECT** for proceeding to step 4 — not a
rejection of the idea, of the sequencing claim) and rust-reviewer
(**APPROVE-WITH-CHANGES**; its one critical finding, that the BM25 exactness
fixture never exercised the dead-length subtraction, was correct in substance
and has been answered by the edited-content column in §"Resolved"). The
recommended next step is a **step 3.5** — B1 and B2 on paper, both cheaper than
a write path and either capable of reshaping the design — with B3 folded in
regardless.

Step 3.5 reviewed 2026-08-16, architect (**REJECT B1 as answered**,
APPROVE-WITH-CHANGES on B2) and rust-reviewer (**APPROVE-WITH-CHANGES**; it
swept the sample size, which this document had not). Both were substantially
right and this section is the corrected version. What they caught, recorded
because the pattern repeats:

- **An arithmetic claim contradicted by the table three lines above it.** Route
  (d) recovers 36 % of the loss; the text called it "the bulk". A reader who
  trusted the prose would have mis-scoped the remaining work by a factor of two.
- **A completeness result that was a property of the fixture.** "Zero misses in
  untouched base files" is trivially true for an edit that changes no
  definitions. It survived a 50× larger probe, and it still is not evidence for
  the general rule it was quoted for.
- **Point estimates that were a sample-size artefact.** Recall degrades
  monotonically from 97 % at 100 names to 86 % at 17 166. The small sample was
  optimistic, not noisy.
- **Aggregates that were one identifier.** 595 of 607 false hits came from
  `test`.
- **A fix described as smaller than it is.** "Seed the name map" is a
  tier-tagging refactor through five consumers, one of which writes to disk.
- **A cost band reported as a point**, because the blob-SHA parse cache
  deliberately refuses dirty files — which is exactly what a delta contains.

One correction in the other direction: rust-reviewer stated `vex index` has no
short-circuit. It does — `pipeline::run_with_lock` skips the rebuild when the
manifest and file fingerprints match — which is what produced a suspicious
6.9 ms "rewrite" earlier in this work. Its own B2 numbers are unaffected; it
made real edits.

B1's target-level work reviewed 2026-08-16 as well: architect **descope**,
rust-reviewer **APPROVE-WITH-CHANGES**. Both corrections are folded in above, and
the pattern of error is worth recording because it is the third instance:

- **A conclusion reported on one axis of a two-axis trade.** The table had two
  recall columns and no precision column, for a rule whose justification was
  precision.
- **A missing control.** The "flip" was attributed to renaming; a no-edit fixture
  reproduces it, and 99.96 % of the losses have nothing to do with the edit.
  One control would have caught it, and it took a reviewer to build one.
- **A rule nobody enumerated.** Writing the candidates as drop-predicates makes
  route (e) obvious and shows it dominates on recall by set inclusion. Two ad-hoc
  single-condition rules were measured against each other for a week instead.
- **A conclusion overshooting its evidence** — "the halves do not separate" from
  data that only shows one half cannot be validated before the other.
- **One fixture's numbers quoted where two shapes existed.** The hierarchy
  paragraph cited only the shape that flattered the rule; the other reverses it.
