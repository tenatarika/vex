# Typed hierarchy edge section (`extends` / `implements` / …) — design

Status: **DESIGN — reviewed, decisions LOCKED, ready for P1 scaffold.** Ranked #1
in `docs/STORAGE-RESEARCH.md` ("Adopt — Typed hierarchy edge section"). Reviewed by
architect + rust-reviewer + store-agent (all APPROVE-WITH-CHANGES); their must-fixes
and the Q1–Q5 lock-ins are folded in below (§10 records the decisions).

## 1. Goal

Persist type-hierarchy relationships (`class X extends Y`, `class X implements I`,
trait/mixin composition) as an indexed, mmap-backed edge section so that:

- **`vex implementations <T>`** becomes an index lookup instead of a full-tree
  tree-sitter walk (today: `src/hierarchy/mod.rs:29-59` re-parses every source
  file on **every** invocation).
- New verbs **`vex subtypes <T>`** / **`vex supertypes <X>`** are answerable
  without re-parsing.
- The edges compose with `--workspace` cross-repo resolution the same way
  `usages --strict` does (unresolved external supertypes spill by-name).

Non-goals (the Kythe/CodeQL boundary — need real type inference or a build):
`type-of`, field/member access, data/control flow. Explicitly deferred; see §9.

## 2. Current state (what we're replacing)

| Concern | Today | file:line |
|---|---|---|
| `vex implementations` | live parallel tree-sitter walk of the whole tree, **zero index** | `src/cli/cmd_implementations.rs:16`, `src/hierarchy/mod.rs:29` |
| supertype names | extracted per-language then **thrown away** | `src/hierarchy/queries.rs` (Rust/Py/Java/TS/C#/Swift/Kotlin/C++/PHP/Ruby) |
| Go hierarchy | not extracted (structural typing — no syntactic edge) | `queries.rs:306` |
| `usages --strict` (access pattern we mirror) | index-backed `reference_edges` (v5) lookup | `src/cli/cmd_usages.rs:57` |
| cross-file name→symbol resolution | `name_to_global` Pass-2 loop | `src/store/writer.rs:413-419`, consumed `475-559` |
| unresolved spill (multi-repo) | `UnresolvedRefsHeader` + by-name FST | `format.rs:240`, `unresolved_refs.rs:43`, `reader.rs:694` |
| current format version | `VERSION = 7` | `src/store/format.rs:41` |

**Key reuse insight:** the tree-sitter queries that find `extends`/`implements`
targets **already exist and are battle-tested** in `src/hierarchy/queries.rs`.
This work does not write new grammar queries — it moves the *existing* ones from
query-time to index-time and persists their output.

## 3. Data model

### 3.1 Direction: child → parent (universal convention)

SCIP, Kythe, and SemanticDB all store the edge on the **child** (subtype /
implementer / overrider) pointing **up** at the parent (supertype / interface).
We follow this: `from = child`, `to = parent`.

Consequence (confirmed by CSR literature + all three code-intel formats): the
headline query **"implementations of `T`"** is the *reverse* traversal — "all
`from` where `to = T`". So the section is **sorted/keyed by `to_sym_idx`** (the
parent), making implementations-of-T a single FST lookup + contiguous posting
scan — the *same* access pattern as `usages --strict` (`reader.rs:616`).

The forward query "supertypes of `X`" (given a child, list its parents) is the
opposite direction. See §5 for the decision on whether v8 ships the reverse
(from-keyed) index or defers it.

### 3.2 Edge kind byte (Kythe-style, not SCIP-style)

Kythe uses **distinct edge kinds**; SCIP unifies everything as
`is_implementation` (distinguished only by whether the linked symbols are types
or methods). We take Kythe's model because we already have a spare byte and an
explicit kind makes kind-filtered range scans free:

```
enum EdgeKind : u8 {
    Extends    = 0,   // nominal class inheritance (Rust supertrait, Python base,
                      //   Java/TS/C#/Kotlin/Swift/C++ extends, Ruby `<`)
    Implements = 1,   // nominal interface conformance (Java/C#/Kotlin/TS implements)
    Uses       = 2,   // trait/mixin composition (PHP `use`, Ruby include/extend/prepend)
    // reserved 3..=254 — Overrides, Satisfies (Go structural) added later, no format bump
}
```

`Overrides` and `Satisfies` are **reserved but not emitted in v1** — see §9.
Reserving the values means adding them later is data-additive, not a format bump.
The existing `queries.rs` already tags each match with a relation string
("inherits" / "implements" / "include" / "uses" / …); this maps 1:1 onto the
enum at extraction time.

**Safe decode (rust-reviewer CRITICAL — locked).** `EdgeKind` is **never the
on-disk field type**. The disk byte stays inside the packed `u32` `line_and_kind`
(§3.3); the reader decodes it via `TryFrom<u8>` (or a raw-byte `match` with an
explicit `_ =>` catch-all), **never `mem::transmute`**. A reserved/unknown byte
(3..=254 — which *will* appear once Overrides/Satisfies land, and *can* appear from
a corrupt/adversarial file) must decode to "unknown kind → skip", not UB. The
catch-all here is required robustness, not a wildcard-hiding-variants smell (that
lint applies to exhaustive in-memory enums, not to decoding an untrusted external
byte).

### 3.3 Record layout (16 bytes, mirrors `RefEdge`)

```rust
#[repr(C)]
pub struct HierarchyEdge {
    pub to_sym_idx:   u32,  // resolved PARENT symbol index (the CSR grouping key)
    pub from_sym_idx: u32,  // resolved CHILD symbol index
    pub from_file_id: u32,  // file where the `extends`/`implements` clause lives
    pub line_and_kind: u32, // 8-bit EdgeKind in the top byte, 24-bit line in the low 3 bytes
}
```

16 bytes, `#[repr(C)]`, four `u32` (align 4, no padding). We store `from_sym_idx`
(not just `from_file_id`, which is all `RefEdge` keeps) because a hierarchy edge's
useful output is a **symbol** (the child's *name*, printed by `vex
implementations`), whereas a ref-edge's output is a *site* (file:line is the whole
answer). Storing the child index makes the name a single `reader.symbol(idx)`
lookup and makes both the §8 carry-forward and the future supertypes-of-X reverse
index cheap. **Q5 locked: keep `from_sym_idx`** (all three reviewers).

**Line-cap guard (rust-reviewer — locked).** `line_and_kind` packs the *line* in
24 bits (max 16,777,215). Unlike `RefEdge`'s 24-bit *column* (unreachable in real
source), a 24-bit line ceiling is closer to reach for generated/adversarial files,
and this value is what the tool prints and jumps to. So the builder guards it with
a **real `Result` check** (`bail!` or skip-and-`warn!`), **not** the compiled-out
`debug_assert!` that `ref_edges.rs:57` uses — silent release-build truncation to a
wrong line is unacceptable here.

**Zero-copy read (rust-reviewer — locked).** The record array is walked by
byte-offset + `ptr::copy_nonoverlapping` into a stack `MaybeUninit<HierarchyEdge>`
(the `RefEdgeReader` pattern), **never** a per-element aligned `&HierarchyEdge`
cast — mmap posting offsets are not guaranteed 4-byte aligned. Every field is a
plain integer, so any 16-byte pattern is a *valid* `HierarchyEdge` (bit-pattern
safety); *semantic* validity (indices in range) is enforced by the §8 bounds
checks. Format is little-endian-only by construction, same as the existing three
record types.

### 3.4 On-disk section (v8, additive)

New `HierarchyHeader` after `UnresolvedRefsHeader`, gated by `VERSION >= 8`, three
offset/len pairs:

```rust
#[repr(C)]
pub struct HierarchyHeader {
    pub edges_offset:    u64, pub edges_len:    u64,  // sorted HierarchyEdge[] (by to_sym_idx)
    pub index_offset:    u64, pub index_len:    u64,  // sorted HierarchyPostingEntry[] (by to_sym_idx)
    pub postings_offset: u64, pub postings_len: u64,  // per-parent posting lists (child edge indices)
}
```

**Index structure — sorted array + binary search, NOT an FST (store-agent
CRITICAL — locked).** `ref_edges.rs`/`call_graph.rs` key their FSTs on a
zero-padded-decimal *string* encoding of a `u32` symbol index — a copy-for-
consistency choice that is *debt*, not a considered decision: an FST buys prefix
compression and fuzzy/range queries, none of which apply to a **dense integer
key**. `to_sym_idx` is already a dense array index into the Symbols section, so a
plain sorted array binary-searched with `partition_point` is strictly better
(O(log n) direct index arithmetic, no ascending-insertion-order fragility, ~8
bytes/entry vs FST node + 10-byte encoded key). Propagating the stringified-u32
FST a third time would compound the debt and lock the wrong field names into the
format (changing it post-ship is a v9 bump). So:

```rust
#[repr(C)]
pub struct HierarchyPostingEntry {
    pub to_sym_idx:     u32,  // parent symbol index (the CSR grouping key)
    pub posting_offset: u32,  // byte offset into the postings blob
}
```

CSR-by-target: sort edges by `to_sym_idx`; the index array (also sorted by
`to_sym_idx`) maps parent → posting-offset; postings enumerate the child edge
indices. `implementations-of-T` = `binary_search_by_key(&T, |e| e.to_sym_idx)` →
posting scan. Retire-the-FST refactor for the older two sections is a separate
follow-up ticket, out of scope here. Older indexes (v7) lack the header → readers
fall back to the live walk (§7).

**Header-chain mechanics (store-agent — locked).** `HierarchyHeader` is a
fixed-size header **always written** (zeroed when empty), inserted in the chain
*after* `UnresolvedRefsHeader` and *before* `symbols_offset`; the
`writer.rs:768` `symbols_offset = … + UnresolvedRefsHeader::SIZE` accumulator gains
`+ HierarchyHeader::SIZE`, and the matching `reader.rs:645` offset chain gains a
`.checked_add(HierarchyHeader::SIZE)`. A `hierarchy_header_is_N_bytes` size-pin
test (mirroring `unresolved_refs_header_is_forty_eight_bytes`) guards against a
silent field-add drifting `symbols_offset`. Update the `format.rs` top-of-file
layout doc-comment and add the "DO NOT add fields without updating the chain"
warning to both the new header and `UnresolvedRefsHeader` (which now has a section
downstream of it). The variable-length section (edges + index + postings) is
written **last**, after the v7 unresolved sub-sections, 4-byte-aligned via the
`(x + 3) & !3` idiom.

### 3.5 Unresolved / external supertypes (must NOT drop)

`class Foo extends SomeStdlibClass` where the parent is outside the corpus is the
sharp edge: an integer `to_sym_idx` can't dangle. **Every mature format keeps the
edge** (SCIP: dangling string symbol; Kythe: target VName with no facts). Dropping
it silently makes `Foo` look like a root type — the classic bug.

**Q3 locked: a PARALLEL `unresolved_hierarchy` section, not reuse of
`UnresolvedRefsHeader`** (architect HIGH-1 + store-agent + rust-reviewer):

1. `UnresolvedRef` (`format.rs:333`) has no field to distinguish a hierarchy edge
   from a normal reference — reuse would force either polluting `RefKind`'s
   discriminant space with `EdgeKind` values (couples two orthogonal enums) or
   inventing a tag byte with no home in the current layout.
2. The ref-edge spill gate (`writer.rs:589`) runs `is_meaningful_identifier`, which
   **rejects pure-lowercase identifiers without `_`** (per the
   `meaningful_identifier_filter` memory). Type names are usually PascalCase, but
   Ruby/Python/PHP lowercase mixin/base names would be **silently dropped**,
   recreating the exact "looks like a root type" bug this section prevents.

The parallel section **spills every unresolved supertype unconditionally** (a name
from an `extends`/`implements`/`use` clause is meaningful by construction — the
noise-filter rationale does not apply). It reuses the `unresolved_refs.rs` builder
*recipe* (~115 lines, trivially cloned) minus the semantic baggage, keyed by the
verbatim supertype name so `--workspace` cross-repo resolution and future re-index
pick them up. Because §3.4 already introduces the sorted-array primitive, the
parallel section is cheap to build with the same code.

## 4. Extraction (index-time, reuses existing queries)

Wire the `src/hierarchy/queries.rs` SCM captures into the parse pipeline so each
parsed file emits `(child_name, parent_name, EdgeKind, file_id, line)` tuples —
the same captures `find_in_source` (`hierarchy/mod.rs:62`) computes today, but
persisted instead of filtered against a query string. The child is a local
definition (already in the symbol table); the parent is a syntactic name to be
resolved in Pass-2.

## 5. Resolution — reuse `name_to_global` Pass-2 (consistency over novelty)

Parent-name → global-symbol resolution reuses the `name_to_global` maps built by
the `writer.rs:413-419` loop (per `pass2-resolver-placement` memory: cross-file
resolution MUST live in this region, not a per-language hook — architect-locked).

**Placement (architect HIGH-2 — locked): a SEPARATE pass AFTER the per-file
`bound_refs` loop closes (`writer.rs:616`), NOT interleaved into it.** A `class X
extends Y` can name a parent defined in *any* file, including one parsed later, so
resolution needs the *complete* `name_to_global` — exactly like the reconstructed-
ref second pass (`writer.rs:618-724`), and unlike the inline ref-edge arm whose
`ModuleSymbol(base_idx+local)` targets are backward-only. Placed after the loop it
adds zero rayon risk (that region is already sequential) and only *reads* the shared
maps, so it cannot corrupt existing ref-edge resolution.

**Q1 locked: mirror the ref-edge single-candidate behavior** (architect;
consistency over novelty):

- **Unique** (exactly one global symbol with the name) → real edge, `to_sym_idx` set.
- **External / zero candidates** → spill by-name to `unresolved_hierarchy` (§3.5),
  edge preserved.
- **Ambiguous** (>1 candidate, e.g. two `Bar` in different modules) → **bail, do not
  guess** (matches `writer.rs:552-557`). Research (SCIP/Kythe/rust-analyzer) is
  explicit that silent-pick is the *worst* failure mode; with no import resolution
  and no type info there's nothing to make a pick sound, and adding speculative
  locality-ranking would create a *second* silent-mispick surface (compounding the
  aliased-import hazard, §9). A locality-ranked / `--strict`=Unique upgrade is a
  fast-follow that improves *both* ref-edges and hierarchy together, out of scope
  for v8.

## 6. Reverse index (supertypes-of-X) — DECISION NEEDED

The target-keyed CSR (§3.4) answers implementations/subtypes-of-T in one lookup
but **not** supertypes-of-X. Options:

- **(A) v8 ships target-keyed only.** `vex supertypes <X>` is deferred or answered
  by a linear scan of the (small) edge list. Simplest; smallest format.
- **(B) v8 ships both directions.** Add a second FST keyed on `from_sym_idx` +
  second posting block (~2× FST/postings; edges stored once). Pure derived data,
  built in the same pass (Ligra/Gemini "store both" pattern).
- **(C) supertypes-of-X answered without an index** — a child's parents are few
  and the child is a known local symbol; its own parents can be captured directly
  at parse time into the symbol record. No reverse CSR needed.

**Q2 locked: Option (A) — target-keyed only for v8** (architect). Ship the headline
query (implementations/subtypes-of-T). `vex supertypes <X>` is deferred to a **v9
additive reverse section** (option B: a second sorted-array index keyed on
`from_sym_idx`, pure derived data built in the same pass). Option (C) (parse-time
parent capture on the `SymbolRecord`) is **rejected** — it fights the fixed-width
`SymbolRecord` layout (`format.rs:376`) and splits hierarchy data across two
structures. Deferral is format-safe *because* the record keeps `from_sym_idx`
(Q5), so the v9 reverse index is a normal additive header, not a rewrite.

## 7. Query surface

- **`vex implementations <T>`** — index lookup when the v8 section exists; **fall
  back to the current live tree-sitter walk when it doesn't** (old index / not
  yet re-indexed). Same output shape; no behavior regression.
- **`vex subtypes <T>`** — new; transitive-down closure over `Extends`/`Implements`
  from the target-keyed section (BFS, like `bundle`'s BFS). **Cycle guard (architect
  CRITICAL-3 — locked): the BFS MUST carry a `visited: HashSet<u32>` on `sym_idx`
  plus a hard depth/node-count cap.** Syntactic extraction gives no acyclicity
  guarantee — TS declaration merging, mutual generic bounds, and adversarial/broken
  source can produce `A extends B, B extends A` or self-edges `class A extends A`;
  a naive queue loops forever. Unit-test an explicit `A→B→A` cycle. `Q4 locked:
  compute the transitive closure at query time` (this BFS), **not** build-time
  materialization — vex is a warm local mmap where a bounded BFS is microseconds,
  and materialized transitive edges would bloat the section and complicate the §8
  carry-forward (distinguishing direct from derived edges on update).
- **`vex supertypes <X>`** — **deferred to v9** per §6 (Q2=A).
- **MCP:** add `implementations` (already exists — swap to index path) + new tools
  behind the same additive-envelope rules as the multirepo Phase-8 work.

## 8. Phasing

- **P1** — format only: `HierarchyEdge`, `HierarchyPostingEntry`, `HierarchyHeader`
  structs; `EdgeKind` + safe `TryFrom<u8>` decode; `VERSION 7→8`; `has_hierarchy_
  header()`; `symbols_offset` chain edit + size-pin test; reader accessor
  `find_hierarchy_edges_by_symbol` (sorted-array binary search, mirrors
  `reader.rs:616` minus the FST). **No extraction.** store-agent + aqa gate.
  **Load-time validation is P1 acceptance criteria (rust-reviewer HIGH), not
  implicit:** (a) `slice_or_empty` + `checked_add` on all three offset/len pairs;
  (b) header alignment + `end > mmap.len()` guard; (c) per-posting `edge_idx`
  bounds-check against `edges_len / HierarchyEdge::SIZE` before every copy;
  (d) posting-list length guards on the count `u32` and each entry; (e) v7-reads-
  clean + v8-rejected-by-pre-v8 SemVer test. Two fuzz-style tests:
  OOB-posting-index and corrupt/truncated-header, both must return empty, never
  panic (NodeTextExt / bloom::load lesson).
- **P2** — extraction + resolution: emit builders from `queries.rs` captures at
  parse time; resolve parent names in a **post-loop pass** (§5); build the sorted-
  array section; unconditional spill to `unresolved_hierarchy` (§3.5).
- **P2a** — **`vex update` carry-forward (architect CRITICAL-1 + store-agent HIGH —
  mandatory, its own sub-task).** Unchanged files are NOT re-parsed
  (`reconstruct_unchanged` rebuilds `ParsedFile` with `bound_refs: Vec::new()`), so
  parse-time hierarchy captures vanish for them — the first `vex update` after ship
  would silently drop every unchanged file's edges. Mirror the ref-edge machinery:
  a `ReconstructedHierarchyEdge` type in `src/index/types.rs`, a `reader.rs`
  `hierarchy_edges_all()` accessor to read the *old* index's edges, and a remap/re-
  resolve pass positioned like `writer.rs:645-724`. aqa must test `vex index` then
  `vex update` on a single-file change and assert unchanged-file implementers
  survive.
- **P3** — wire `vex implementations` to the index (**live-walk fallback preserved**
  when the section is absent) + `vex subtypes` (with the §7 cycle guard); `vex
  status` line ("Hierarchy edges: N"); MCP `implementations` swap.
- **P4** — bench + LIMITATIONS.md honesty section (§9) + docs. Watch item: per-BFS-
  hop `Vec` allocation in the accessor — iterator-based accessor if measurable.
  (`vex supertypes` is a separate v9 effort, not P4.)

## 9. Limitations (honest, → docs/LIMITATIONS.md)

Purely-syntactic tree-sitter extraction is **incomplete and sometimes wrong** —
document, don't hide:

- **Go: no `implements` edge at all** — structural interface satisfaction is never
  written in source. Categorical blind spot (`satisfies` reserved, needs method-set
  inference we don't do).
- **TypeScript structural conformance & declaration merging** — a class can satisfy
  an interface with no `implements` clause; parent set can span merged/partial
  declarations across files. Under-reported.
- **Rust `#[derive(...)]` / macro-generated impls** — exist only post-expansion;
  tree-sitter sees an attribute, not an `impl_item`. Invisible.
- **C++ macro'd base lists** — preprocessor-blind; `class X : BASE_MACRO` unresolved.
- **Aliased imports** (`use/import … as`) — the captured token is the alias, not the
  real type → possible wrong/unresolved target. **Java exempt** (no aliased type
  imports).
- **Name ambiguity** — two same-named parents in different modules; see §5 / Q1.
- **`Overrides` deferred** — "method X overrides Y" fundamentally needs resolved
  parameter types (overload disambiguation); soundly doable only marker-gated
  (`override`/`@Override`) AND hierarchy-resolved. Reserved kind, not emitted in v1.

## 10. Decisions (LOCKED after review)

Resolved by architect + rust-reviewer + store-agent (all APPROVE-WITH-CHANGES):

- **Q1 (resolution/ambiguity):** mirror ref-edge single-candidate — Unique→edge,
  zero→spill, **ambiguous→bail (do not guess)**. No locality-ranking in v8. (§5)
- **Q2 (reverse index):** **Option (A) target-keyed only**; `supertypes-of-X`
  deferred to a v9 additive reverse section. Reject option (C). (§6)
- **Q3 (unresolved spill):** **parallel `unresolved_hierarchy` section**, spilling
  unconditionally (no `is_meaningful_identifier` gate). (§3.5)
- **Q4 (transitivity):** **query-time BFS** with `visited` set + depth cap, not
  build-time materialization. (§7)
- **Q5 (`from_sym_idx`):** **keep it** — child output needs a name, not just a site;
  also unlocks carry-forward + the v9 reverse index cheaply. (§3.3)

Cross-cutting must-fixes folded in: `EdgeKind` safe `TryFrom` decode never
`transmute` (§3.2); sorted-array index not FST (§3.4); `Result`-guarded 24-bit line
cap (§3.3); resolution as a post-loop pass (§5); `vex update` carry-forward as
mandatory P2a (§8); load-time bounds-check checklist as P1 acceptance (§8);
`symbols_offset` chain edit + size-pin test (§3.4).
