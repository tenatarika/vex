# vex index format v9: CSR adjacency and symbol clusters

Status: **IN PROGRESS** — design reviewed 2026-09-30 (architect + rust-reviewer); P0–P4b implemented and reviewed; P5 (`vex modules`) and P6 (MCP tool) pending. No release tag before P4b. **§13 supersedes any earlier section it conflicts with.** One v8 → v9 format bump with three changes:

1. **Callees CSR.** The callees FST keyed by 10-digit decimal strings becomes dense `offsets[n+1]` + `edge_idx[m]`.
2. **ref_edges CSR.** The same decimal-string FST in `reference_edges` becomes a dense offsets array.
3. **Symbol clusters.** Roadmap #11 Phase A: deterministic Leiden-CPM, a new cluster section, a `vex modules` command and a `modules` MCP tool.

Inputs: `docs/CALLGRAPH-STORAGE-RESEARCH.md` ("Measurement (2026-07-18)" and "Verdict") and `docs/STORAGE-RESEARCH.md` §9.

---

## 0. Findings that shape the design (verified in source)

| # | Fact | Where |
|---|---|---|
| F1 | `VERSION = 8`, `MIN_SUPPORTED_VERSION = 3`. The reader accepts any version in `[3..=8]` and rejects anything else with "index version mismatch … Re-run `vex index`". | `src/store/format.rs:49,53`; `src/store/reader.rs:51-61` |
| F2 | The header chain is fixed-size and version-gated: Header(168) + CallGraphHeader(128) + V5(48) + PatternSkeleton(168) + UnresolvedRefs(48) + Hierarchy(48) + UnresolvedHierarchy(48), so `symbols_offset = 656`. UnresolvedHierarchyHeader is documented as "the LAST header before `symbols_offset`". | `format.rs:352`; `writer.rs:891-898` |
| F3 | Callees keys come from `format!("{caller_sym_idx:010}")`. Each lookup allocates a String, walks the FST and allocates a posting Vec. | `call_graph.rs:70-72`, `:319-320` |
| F4 | Callees postings group edges in ascending `edge_idx` (sort by `(key, idx)`). The dedup step is a no-op because `idx` is unique. | `call_graph.rs:131-158` |
| F5 | **RefEdge records are already sorted by `to_sym_idx`**, and every posting list is the contiguous identity range of those records. This has held since the section was created (57e5193). So the `edge_idx` array for ref_edges would be the identity permutation. | `ref_edges.rs:44`, `:75-82` |
| F6 | `CallEdge` order is file order, then source order. The callers FST postings index into it. | `output.rs:137-167`; `writer.rs:484-493` |
| F7 | **`impact`, `reachable`, `paths` and `pr-impact` BFS go through the *callers* FST, not callees.** The CSR latency win therefore lands on `vex callees` (`cmd_callgraph.rs:204`), the `tests-for --include-fixtures` loop (`cmd_tests_for.rs:94`), `bundle` symbol mode (`cmd_bundle/symbol.rs:128`), and, for ref_edges, the `strict_refs` channel used by `usages --strict` and `impact` (`channel/mod.rs:495`). This corrects the "graph-walk hot paths" claim in the research doc. | `callgraph/mod.rs:34`; `channel/mod.rs:664,737` |
| F8 | `RefEdge` records no source symbol, only `from_file_id` and `line`. `SymbolRecord` has no end line. Source attribution for a ref therefore has to be derived. | `format.rs:418-423,643-651`; `parse/scope/mod.rs:229-235` |
| F9 | `CallEdgeBuilder` has an exact `caller_sym_idx` but the callee is a *name*, which may be external. | `call_graph.rs:22-26` |
| F10 | Pass-2 runs inside `write_index_to` and has everything clustering needs in memory: `name_to_global`, `sym_to_file_id`, `ref_edge_builders`, the hierarchy builders and `call_edges`. | `writer.rs:517-541,565-720,830,865-868` |
| F11 | `vex update` rebuilds every section wholesale. Symbols are renumbered as "unchanged files first, in old order, then re-parsed files". `reconstruct_unchanged` walks old `sym_idx` ascending and skips changed/deleted files and empty-name records. | `pipeline/mod.rs:812-813`; `parse_files.rs:88-154`; `writer.rs:406-444` |
| F12 | The writer does not know whether it is doing a full rebuild; `is_full_rebuild` stops at `write_output_locked`. | `pipeline/mod.rs:336,852`; `output.rs:220-279` |
| F13 | Ranking's `--context-path` boost is purely path-based, and **`vex eval` never sets `context_path`**, so it cannot measure a context-dependent boost today. | `search/rerank.rs:194-198,257`; `eval/harness.rs:289` |
| F14 | `--workspace` fans out only for index/search/impact/usages/update/grep/callers/callees/reachable/check/watch. It does *not* cover implementations or subtypes. | `cli/common.rs:51-65` |
| F15 | There is no petgraph dependency. proptest is a dev-dependency. | `Cargo.toml:192` |
| F16 | The binder-backed (strict) languages are rust, python, typescript, go, java, csharp, kotlin and cpp. Everything else relies only on the name-based call graph. | `src/parse/scope/` |

---

## 1. Compatibility: read both formats, with legacy scan fallbacks

**Decision:** keep `MIN_SUPPORTED_VERSION = 3`. A v9 binary opens v3–v8 files, but **deletes all decimal-FST decoding**:

- **v4–v8 callees:** linear scan over the `CallEdge` section, filtering `caller_sym_idx == s`. At about 36k edges this is roughly 30 µs per lookup.
- **v5–v8 ref_edges:** binary search (`partition_point`) over the `RefEdge` records. This works because of F5: every v5+ writer sorted them. It is O(log m) and does not need the FST.
- **Clusters on v3–v8:** absent. `vex modules` exits 1 with a hint to run `vex index`.

**Why not reject v8?**

- The repo has always read older formats (`reader.rs:45-50`). Hard-rejecting would break every agent's first query after an upgrade.
- v8 indexes heal themselves: the next `vex update`, including auto-update, runs `reconstruct_unchanged`. That path reads only the record sections (`CallEdge`, `RefEdge`, symbols), which are **byte-identical in v8 and v9**, so it writes v9. The FST code is still deleted, which is the clarity goal.

**Downgrade:** a v8 binary opening a v9 file hits the existing range gate at `reader.rs:55` ("found v9, this build supports v3..v8. Re-run `vex index`"). Add a pinned test mirroring `reader.rs:1898`.

**Update is not a full rebuild:** `vex update` on a v8 index produces a v9 index whose cluster header is **zeroed** (not computed). See §5 and open question Q2.

---

## 2. Byte layout

All new arrays are **little-endian `u32`**, written with `to_le_bytes`, matching the existing postings. Existing fixed headers stay native-endian `#[repr(C)]` dumps (`writer.rs:1076`), which is LE on every target vex ships. Every new variable section starts 4-aligned with zero pad bytes. Readers **never** cast to `&[u32]`: they decode with `u32::from_le_bytes` on bounds-checked 4-byte windows, so alignment is a courtesy, not a safety requirement (same discipline as `reader.rs:1279`).

### 2.1 Header chain (v9)

```text
off   size  struct
0     168   Header                     (unchanged; version = 9)
168   128   CallGraphHeader            (same size; callees fields re-meant, §2.2)
296   48    V5SectionHeader            (same size; ref index fields re-meant, §2.3)
344   168   PatternSkeletonHeader      (unchanged)
512   48    UnresolvedRefsHeader       (unchanged)
560   48    HierarchyHeader            (unchanged)
608   48    UnresolvedHierarchyHeader  (unchanged)
656   48    ClusterHeader              (NEW, v9+, always written, zeroed when absent)
704   ...   Symbols section            (symbols_offset = 704 on v9, 656 on v8)
```

- `Header::has_cluster_header()` returns `version >= 9`.
- `reader.open` gains the same fit check as `reader.rs:205-229` for the 704-byte chain, plus bounds checks for the cluster section's `(offset, len)` pairs.
- `format.rs` gains `ClusterHeader::SIZE == 48` and `ClusterRecord::SIZE == 32` pin tests next to `format.rs:661-693`.

### 2.2 CallGraphHeader: callees fields

The Rust fields are renamed `callees_fst_*` → `callees_index_*` and `callees_postings_*` → `callees_edge_idx_*`. Byte offsets are unchanged. The meaning depends on the version and is exposed through one accessor, `reader.callees_layout() -> Legacy | Csr`.

| byte | field | v4–v8 meaning | v9 meaning |
|---|---|---|---|
| 48 | `callees_index_offset` u64 | FST start | `offsets` start, 4-aligned (pad after callers postings) |
| 56 | `callees_index_len` u64 | FST len | `4 * (symbol_count + 1)`, or 0 when there are no call edges |
| 64 | `callees_edge_idx_offset` u64 | posting blob | `edge_idx` start (immediately after `offsets`) |
| 72 | `callees_edge_idx_len` u64 | posting len | `4 * call_edge_count`, or 0 |

```text
offsets : [u32 LE; symbol_count + 1]   offsets[0] = 0, non-decreasing, offsets[n] = m
edge_idx: [u32 LE; m]                  m = call_edges_len / 16; group s = edge_idx[offsets[s]..offsets[s+1]]
```

Within each group `edge_idx` ascends, so the contents are identical to the v8 posting lists (F4). The callers FST and postings, and the `CallEdge` records, are unchanged.

### 2.3 V5SectionHeader: ref_edges index

Fields are renamed `ref_edges_fst_*` → `ref_edges_index_*`, `ref_edges_postings_*` → `ref_edges_edge_idx_*`.

| byte | field | v9 meaning |
|---|---|---|
| 16 | `ref_edges_index_offset` | `offsets` start, 4-aligned |
| 24 | `ref_edges_index_len` | `4 * (symbol_count + 1)`, or 0 when `ref_edges_len == 0` |
| 32 | `ref_edges_edge_idx_offset` | = `index_offset + index_len` |
| 40 | `ref_edges_edge_idx_len` | **0 (identity-elided)** |

Records for symbol `s` are `RefEdge[offsets[s]..offsets[s+1]]`. Because of F5 the `edge_idx` array would be `0..m`, which wastes 4 bytes per edge. v9 stores none. The reader **requires** `edge_idx_len == 0` on v9 and treats any other value as corrupt, which leaves room to add a real `edge_idx` later without a bump (see Q1). This keeps the decided "offsets + edge_idx" shape with the identity case special-cased. It also turns the ref_edges side from space-neutral into space-positive: it drops the FST, the count words and 4 B per edge. Measure in P2.

### 2.4 ClusterHeader (48 B, `#[repr(C)]`, align 8)

| byte | field | type | notes |
|---|---|---|---|
| 0 | `assign_offset` | u64 | 4-aligned |
| 8 | `assign_len` | u64 | `4 * symbol_count`, or 0 (not computed) |
| 16 | `table_offset` | u64 | = `assign_offset + assign_len` |
| 24 | `table_len` | u64 | `32 * k` |
| 32 | `resolution_num` | u32 | γ = num/den (default 1/8, §3.4) |
| 36 | `resolution_den` | u32 | must be non-zero when COMPUTED |
| 40 | `flags` | u32 | bit0 COMPUTED, bit1 STALE, bit2 ITER_CAP_HIT, bits 3–31 reserved (write 0, ignore on read) |
| 44 | `algo_version` | u16 | 1 = "leiden-cpm/1" (weight table §3.2 + procedure §3.3) |
| 46 | `levels` | u16 | aggregation levels run (diagnostic) |

The section sits at the end of the file, after the unresolved-hierarchy postings: `[pad→4][assign][table]`.

**assign:** `[u32 LE; symbol_count]`, indexed by `sym_idx`.

| value | meaning |
|---|---|
| `0 .. k-1` | cluster ordinal |
| `0xFFFF_FFFF` | NOT_ELIGIBLE (excluded kind or language, §3.1) |
| `0xFFFF_FFFE` | UNCLUSTERED (eligible, but isolated or a final singleton) |
| `0xFFFF_FFFD` | NEW (symbol introduced by `vex update` after the last full build) |
| any other value `>= k` | corrupt; read as NOT_ELIGIBLE |

**table:** `ClusterRecord[k]`, 32 B, `#[repr(C)]`, eight u32 fields, align 4, no padding.

| byte | field | notes |
|---|---|---|
| 0 | `rep_sym_idx` | min member `sym_idx` at build time; `u32::MAX` if lost after update |
| 4 | `size` | member count at build time |
| 8 | `internal_weight` | Σ intra-cluster pair weights (saturating) |
| 12 | `cut_weight` | Σ weights to other clusters, UNCLUSTERED counted (saturating) |
| 16 | `label_offset` | Strings-pool offset of the dominant path prefix (§4.2) |
| 20 | `hubs[3]` | top-3 members by intra-cluster weighted degree; `u32::MAX` pads |

**Canonical ids:** ordinal `i` is the i-th cluster in ascending `rep_sym_idx` order. This is §9's "stable id = min sym_idx", made dense so that it survives renumbering on update (§5).

---

## 3. Build algorithms

### 3.1 CSR (callees and ref_edges): counting sort, O(n + m)

```text
build_csr(keys: &[u32] /* key of edge e */, n: u32) -> Result<(offsets, edge_idx)>
  deg = [0u32; n+1]
  for k in keys: if k >= n: bail!("csr key {k} >= symbol_count {n}")   // writer bug, not user input
                 deg[k+1] = deg[k+1].checked_add(1)?
  for i in 0..n: deg[i+1] = deg[i+1].checked_add(deg[i])?            // offsets
  cursor = deg[..n].to_vec()
  for (e, k) in keys.enumerate(): edge_idx[cursor[k]] = e; cursor[k] += 1   // stable, so ascending e
```

- For ref_edges, the keys are already sorted, so only `offsets` is emitted. A `debug_assert!` checks that `edge_idx` equals the identity.
- This lives in a new shared module, `src/store/csr.rs` (builder plus `CsrView` reader), replacing `build_u32_keyed_fst` (`call_graph.rs:131-171`), `encode_caller_key*` (`:64-83`), `encode_to_sym_key` (`ref_edges.rs:115-117`) and `CallGraphFstReader`'s callees use.
- `build_callers_fst` / `build_string_keyed_fst` stay unchanged.

### 3.2 Graph projection (inside `write_index_to`, after `writer.rs:868`)

This piggybacks on Pass-2 (F10). The cost is one extra sequential pass.

**Nodes.** Symbol `s` is eligible iff:
- its kind is not Module(13), Heading(12) or Package(11), and
- `Language::from_extension(path)` is not Markdown, Yaml, Toml, Css or Html.

Excluded symbols get NOT_ELIGIBLE. Package is excluded because top-of-file imports would otherwise be attributed to it, turning it into a hub that glues together every file in the package. Eligible nodes are compacted to dense ids in ascending `sym_idx`, which preserves order.

**Edges.** All edges are symbol-to-symbol. Anything touching an ineligible or missing endpoint, and every self-loop, is dropped.

| source | from | to | kind weight |
|---|---|---|---|
| `CallEdgeBuilder` (F9) | `caller_sym_idx` (exact) | resolve `callee_name`: candidates in the caller's own file → smallest `sym_idx`; else exactly one candidate project-wide (the `writer.rs:656-661` rule); else drop (ambiguous or external) | CALL = 2 |
| `RefEdgeBuilder` | attributed (below) | `to_sym_idx` (exact) | Call → 2, Type → 1, Value → 1, Macro → 1 |
| `HierarchyEdgeBuilder` | `from_sym_idx` | `to_sym_idx` | 1 (Q4) |

- **Ref source attribution (F8).** For each file, build a list of eligible `(line, sym_idx)` sorted ascending. The source is the last entry with `line <= ref.line` (nearest-preceding definition). A ref before the first eligible symbol is dropped. This is an approximation: module-level code after a function gets attributed to that function. Real binder scopes are a non-goal.
- **Deduplication.** Sites are keyed by `(from, to, line)`, so a call seen both by the name-based call graph and as a binder `RefKind::Call` counts once.
- **"Import" edges.** These are the Imported-arm resolutions already present as `RefEdge`s (`writer.rs:605-613`). File-level `imported_by_pairs` (`writer.rs:577`) are **not** used, because file granularity would just duplicate the path signal.
- **Unresolved and external edges.** Unresolved refs, unresolved hierarchy and unresolved callees are dropped: they have no node.
- **Pair weight.** `w(u,v) = min(Σ kind weights over deduped sites, PAIR_CAP = 8)` for the undirected pair `(min, max)`. The cap stops one chatty pair from dominating.
- **Canonical form.** Sort the triples, merge them, and build a symmetric adjacency CSR with neighbours in ascending order. The result is independent of input order.

### 3.3 Leiden-CPM, deterministic (`src/cluster/leiden.rs`, hand-rolled, no crate)

**Objective (CPM).** `H(P) = Σ_c [ W_in(c) − γ · N_c(N_c−1)/2 ]`, where `N_c` is the sum of node sizes (all 1 at level 0).

**Integer arithmetic.** There are no floats anywhere. With γ = num/den, moving v from A to B scores:

```text
Δ'(v: A→B) = den·(w(v,B) − w(v,A∖v)) − num·n_v·(N_B − N_A + n_v)          // i64, exact
well_connected(v, S) ⇔ den·w(v, S∖v) ≥ num·n_v·(N_S − n_v)
well_connected(T, S) ⇔ den·w(T, S∖T) ≥ num·N_T·(N_S − N_T)
```

Bounds: Σw ≤ 8·m ≈ 1.6M and N ≤ 2³², so every product fits comfortably in i64. Use `checked_mul` in debug builds.

**Procedure.** Traag et al. 2019, with randomness removed:

1. **Local moving (queue-based).**
   - The queue starts as all nodes in ascending id, with an `in_queue` bitmap.
   - Pop the front node v. Accumulate `w(v, C)` into a scratch array indexed by community id, then reset only the touched entries. **No HashMap is iterated anywhere in this module.**
   - The candidates are the neighbouring communities plus the smallest empty community id. Empty ids are tracked in a `BTreeSet`.
   - Move only if the best `Δ' > 0`. Ties prefer staying put, then the smallest community id.
   - After a move, push each neighbour u (in ascending order) whose community differs from v's new one and that is not already queued.
2. **Refinement.**
   - For each community C (ascending id), start from singletons.
   - For each v in C (ascending), if v is still a singleton and `well_connected(v, C)`, consider the refined communities T ⊆ C adjacent to v with `well_connected(T, C)` and `Δ' ≥ 0`.
   - Take the argmax; ties go to the smallest T id.
   - This is the θ→0 greedy limit of Leiden's randomized choice. The γ-connectivity guarantee still holds, because merges only ever go into adjacent, well-connected T.
3. **Aggregation.**
   - Aggregate nodes are the refined communities, ordered by their min level-0 id.
   - Node weight = Σ member sizes. Edge weights are summed in integers. Edges internal to an aggregate are dropped, not kept as a self-weight: an aggregate's internal weight is the same constant for every candidate community it could join, so it cancels out of Δ' and both well-connectedness tests (P3 review, verified by derivation).
   - The aggregate's initial partition is the **unrefined** partition from step 1 (the Leiden key step).
4. **Levels.** Repeat 1–3 until refinement yields one aggregate node per community, or until `MAX_LEVELS = 32`.
5. **Outer iterations.** Re-run from the flattened partition until it no longer changes, capped at `MAX_ITERATIONS = 4`. Hitting either cap sets `ITER_CAP_HIT` and logs `tracing::info!`.
6. **Finalize.**
   - Flatten to level 0. Communities of size 1 become UNCLUSTERED.
   - Sort the remaining communities by min `sym_idx` to get ordinals.
   - Compute the records (§2.4) and labels (§4.2).

**SCC prepass: not in Phase A.** Leiden on the undirected projection already absorbs recursion cycles. Collapsing SCCs risks giant super-nodes in heavily mutually-recursive code, and would need a hand-rolled Tarjan (F15). Parked as a non-goal.

**Determinism guarantee.** Given the same eligible node set in `sym_idx` order, the same weighted edge multiset and the same γ, the section is byte-identical. The reasons:
- integer-only math;
- a single thread (clustering runs outside rayon);
- canonical sorting of inputs;
- tie-breaking by smallest id;
- no RNG.

On the same tree with the same binary, a full `vex index` run twice produces identical bytes (tested, §8).

**Complexity.**
- Projection: O(S log S) over S sites (sort).
- Local moving: O(m) per sweep, amortized by the queue.
- Refinement: O(m).
- Aggregation: O(m) using scratch arrays. Graph size shrinks geometrically per level.
- Expected on a repo with about 35k symbols and 200k raw sites (roughly 120k undirected pairs after dedup): projection ≤ 30 ms, Leiden ≤ 100 ms, peak memory ≤ 15 MB.
- **Budget gate:** ≤ 250 ms total and ≤ 3 % of full `vex index` wall time on the vex tree, enforced by the §8 bench.

### 3.4 Resolution γ — **confirmed in P3, measured**

The default is γ = **1/8**: in weight units, a node with one call edge (weight 2) joins a community of size ≤ 16.

Selection procedure (as run):
- Swept γ ∈ {1/32, 1/16, 1/8, 1/4, 1/2} via a local, gitignored harness
  (`examples/cluster_sweep_measure.rs`) that opens a real v9 index through
  `IndexReader`, rebuilds a `cluster::projection::ProjectionInput` from the
  reader's own call/ref/hierarchy-edge records, and runs `cluster::cluster`
  at each γ.
- **Known approximation**: the on-disk `RefEdge` section does not carry the
  writer's transient `ambiguous: Vec<bool>` (§13 R7) — that flag only exists
  inside `write_index_to` and is never persisted. The harness therefore
  treats every resolved ref edge as unambiguous, so its projected graph is a
  slightly denser upper bound on what P4's writer-side sweep will actually
  see. Purity/size numbers below are directionally correct, not
  byte-identical to a future from-the-writer sweep.
- Metrics collected: clustered fraction, median and max cluster size, and
  path purity (share of each cluster's members whose path falls under that
  cluster's own computed label prefix, size-weighted across clusters).
  Leiden wall time is reported but is not a selection criterion (see §3.3's
  bench gate instead). **Stability** (Jaccard overlap under a synthetic
  one-file edit) was **not** measured in this sweep — that metric needs the
  P4b update-carry machinery to produce a second, incrementally-updated
  partition to compare against, so it is deferred to the P4b incremental
  consistency tests (§8).
- Corpora: this repository itself, a second Rust repository (~2,300
  symbols, a smaller personal-tooling codebase), and a small Python/Django
  repository (13 symbols, 6 eligible) that turned out too small to produce
  any multi-member cluster at any γ — included anyway as a sanity check
  that the empty/near-empty case degrades to zero clusters without
  crashing, not as a data point for the γ choice. Per this document's
  anonymity convention, non-vex repos are identified only by shape
  (language, symbol count), never by name or path.

**Results — this repository** (5,524 eligible symbols; 10 % of eligible = 552):

| γ | clusters | clustered | clustered % | median size | max size | purity | Leiden wall time |
|---|---|---|---|---|---|---|---|
| 1/32 | 307 | 4,483 | 81.2 % | 10 | 79 | 0.893 | 23.2 ms |
| 1/16 | 400 | 4,365 | 79.0 % | 8 | 59 | 0.911 | 17.4 ms |
| **1/8** | **508** | **4,142** | **75.0 %** | **7** | **41** | **0.932** | **15.7 ms** |
| 1/4 | 657 | 3,823 | 69.2 % | 5 | 32 | 0.942 | 15.1 ms |
| 1/2 | 806 | 3,236 | 58.6 % | 3 | 17 | 0.951 | 15.3 ms |

**Results — second Rust repository** (1,523 eligible symbols; 10 % of eligible = 152):

| γ | clusters | clustered | clustered % | median size | max size | purity | Leiden wall time |
|---|---|---|---|---|---|---|---|
| 1/32 | 72 | 1,101 | 72.3 % | 7 | 134 | 0.874 | 4.6 ms |
| 1/16 | 96 | 1,095 | 71.9 % | 7 | 97 | 0.894 | 4.5 ms |
| **1/8** | **117** | **951** | **62.4 %** | **5** | **64** | **0.909** | **4.5 ms** |
| 1/4 | 138 | 742 | 48.7 % | 4 | 43 | 0.902 | 4.3 ms |
| 1/2 | 161 | 621 | 40.8 % | 3 | 36 | 0.908 | 4.7 ms |

Applying the §3.4 rule (clustered ≥ 60 %, max cluster ≤ 10 % of eligible):

- **The size ceiling never binds.** On this repository the ceiling is 552 and
  the largest cluster at any γ is 79 (1.4 % of eligible). On the second
  repository the ceiling is 152 and the largest cluster is 134 at γ = 1/32
  (8.8 %), falling to 64 at γ = 1/8 (4.2 %).
- **The 60 % floor is the binding constraint.** It excludes γ = 1/2 on this
  repository (58.6 %), and γ = 1/4 and 1/2 on the second (48.7 %, 40.8 %).
  That leaves {1/32, 1/16, 1/8} valid on both.
- **Purity picks 1/8.** Among those three, 1/8 has the highest purity on
  each corpus (0.932 here, 0.909 there) and averaged across them: 0.884
  (1/32), 0.903 (1/16), **0.921 (1/8)**.

**Decision: keep γ = 1/8** as the shipped default. Two caveats:
- **The floor margin is thin on smaller corpora.** At γ = 1/8 the second
  repository sits only 2.4 points above the 60 % floor (62.4 %). Smaller or
  less-connected corpora may land under it, so the floor is a quality signal
  to report, not a hard gate.
- **Leiden hits its iteration cap on this repository.** At γ = 1/8 and 1/4 it
  stops at `MAX_ITERATIONS = 4` (`ITER_CAP_HIT`) instead of converging. The
  output is still deterministic, but this is worth watching in the P4 stability
  measurements.

Re-measured 2026-10-01 after the P3 `refine()` singleton-gate fix (a
representative that had already absorbed followers could defect and strand
them). The earlier table predated the fix; the decision did not change.

- γ is persisted in the header, so output is self-describing.
- `VEX_CLUSTER_RESOLUTION=a/b` is an undocumented experiment knob. A `.vex.toml` setting is Q3.

---

## 4. `vex modules`

### 4.1 CLI (`src/cli/cmd_modules.rs`, `Commands::Modules` in `args.rs`)

```text
vex modules [SYMBOL] [-p PATH] [-l/--limit 50] [--min-size 3] [--members N]
            [--sort size|cohesion] [--include G]... [--exclude G]...   (ScopeArgs)
            [--auto-update] [--no-stale-check] [--workspace]  (+ global --format json)
```

**List mode** (no SYMBOL):
- Show clusters with `size >= --min-size`, sorted by size descending (or cohesion), ties by ordinal.
- `--members` defaults to 0.
- Scope filters apply to members. A cluster is shown iff ≥ 1 member is in scope, and the displayed size is the in-scope count.

**Symbol mode:**
- Resolve SYMBOL exactly via the symbol FST, as in `call_graph.rs:301-314`.
- For each match, print its status and cluster, with `--members` defaulting to 25.
- Members are ordered by path, then line. They are found by an O(n) scan of `assign`.

Text output:

```text
Modules — leiden-cpm/1 γ=1/8 · 412 clusters (≥3) · 1,203 unclustered · 2,210 not eligible
  #0   src/store/          148 symbols  cohesion 0.81  hubs: IndexReader, write_index_to, build_ref_edges_section
  #7   src/cli/cmd_bundle/  41 symbols  cohesion 0.64  hubs: assemble_symbol, ...
! clusters are frozen at the last `vex index`; 37 symbols changed/added since are unclustered — run `vex index` to recompute
```

- The `!` line appears only when STALE is set. The NEW count is live, from scanning `assign`.
- Cohesion = `internal / (internal + cut)`; 0 when both are 0.

JSON: a standard envelope via `print_envelope` (`cli/output.rs:23`). All keys are additive (PROTOCOL-EVOLUTION §1a).

```json
{"algorithm":"leiden-cpm/1","resolution":"1/8","stale":false,"new_since_build":0,
 "total_clusters":412,"unclustered":1203,"not_eligible":2210,
 "clusters":[{"id":0,"label":"src/store/","size":148,"size_at_build":150,"cohesion":0.81,
   "internal_weight":913,"cut_weight":214,
   "hubs":[{"name":"IndexReader","path":"src/store/reader.rs","line":13}],
   "members":[{"name":"...","path":"...","line":1,"kind":"function"}]}],
 "symbol":[{"name":"IndexReader","path":"src/store/reader.rs","line":13,
   "status":"clustered","cluster_id":0}],
 "empty_reason":null}
```

- `status` is one of `clustered | unclustered | not_eligible | new_since_build`.
- `empty_reason` is one of `clusters_not_built | symbol_not_found | symbol_unclustered | filtered_all`, and is omitted when there are results.
- Cluster staleness is **not** carried in `vex.dev/stale`, which already means "index older than the working tree".

**Exit codes** (add `modules` to the "Distinguishes 0 / 1" list in `docs/EXIT-CODES.md`):

| code | when |
|---|---|
| 0 | ≥ 1 cluster printed, or in symbol mode ≥ 1 match is clustered |
| 1 | no cluster section (v3–v8 index, `--no-clusters`, or an index created by `update` from v8), everything filtered out, symbol not found, or every match unclustered / not eligible / new. The hint is printed on stderr and `empty_reason` is set. This follows the `subtypes` precedent. |
| 2 | corrupt cluster section (bail at open, §7) or any handler error |

**`--workspace`:** per member only, grouped by repo like `reachable_workspace` (`cmd_callgraph.rs:508`). Clusters never span members, because cross-member edges live in no single index. `--limit` applies per member (MULTIREPO convention). Add the variant to `extract_workspace_flag` (`common.rs:51`).

**Index flag:** `vex index --no-clusters` sets `IndexOptions.with_clusters = false` (default true). This adds a `clusters_computed: Option<bool>` manifest marker, and `manifest_options_cover` (`pipeline/mod.rs:137`) treats "requested but not computed" as not covered, so a later `vex index` does not skip.

**`vex status`** gets additive JSON keys next to `hierarchy_edges` (`cmd_status.rs:129`): `"clusters": k`, `"clusters_stale": bool`, `"clusters_new_since_build": n`.

### 4.2 Labels and hubs (computed at build time, frozen)

- **Label:** the deepest directory prefix that contains ≥ 60 % of the members' files. If no prefix of depth ≥ 1 reaches that share, the label is `"(mixed) <most common top dir>/"`. It is interned into the Strings pool (the StringPool is still open inside `write_index_to`).
- **Hubs:** the top 3 members by intra-cluster weighted degree, descending, ties by smallest `sym_idx`.

### 4.3 MCP tool `modules`

- `build_modules` in `crates/vex-mcp/src/tools/graph.rs`, modelled on `build_implementations` (`graph.rs:96`).
- A route in `tools/mod.rs:49`.
- A descriptor in `descriptors.rs` (near line 331) and an updated insta snapshot `vex_mcp__tests__tool_descriptors.snap`.

```json
{"name":"modules",
 "description":"De-facto modules: clusters of symbols that call/reference each other (deterministic Leiden-CPM over call+ref+hierarchy edges, computed on full `vex index`). Without `symbol`: list clusters with label (dominant path prefix), size, cohesion, hub symbols. With `symbol`: that symbol's cluster and members. Requires a v9 index built by `vex index`; after `vex update` clusters are frozen and flagged `stale`. Empty result + hint on older indexes.",
 "inputSchema":{"type":"object","properties":{
   "symbol":{"type":"string"},"limit":{"type":"integer","default":50},
   "min_size":{"type":"integer","default":3},"members":{"type":"integer"},
   "sort":{"type":"string","enum":["size","cohesion"],"default":"size"},
   "include":{"type":"array","items":{"type":"string"}},"exclude":{"type":"array","items":{"type":"string"}},
   "project_root":{"type":"string"},"auto_update":{"type":"boolean","default":true},
   "async_update":{"type":"boolean","default":false},"no_stale_check":{"type":"boolean","default":false},
   "workspace":{"type":"boolean","default":false}}}}
```

- `min_size` is validated to `[1, 1_000_000]` and `members` to `[0, 10_000]`, returning −32602 on violation, as `build_subtypes` does.
- **Capability:** add `symbol_clusters: bool` to `Capabilities` (`protocol/mod.rs:29`), set to `true` in `capabilities.rs:3`, and pin it in `tests/cli_capabilities_test.rs`. Consumers treat an absent flag as false (§1b).

---

## 5. `vex update`: precise "freeze and mark stale" semantics

Update **never runs Leiden**. The writer receives a `ClusterInput` (new parameter threaded from `write_output_locked`, closing F12):

```rust
enum ClusterInput { Compute { resolution: (u32, u32) }, Carry(ClusterCarry), None }
```

`vex index` passes `Compute` (or `None` with `--no-clusters`). `vex update` passes `Carry` if the old index has COMPUTED set, otherwise `None`.

Carry rules, applied to the new `sym_idx` i:

1. **Unchanged-file symbols** (i < unchanged_count) take the old assignment verbatim, including sentinels. This is exact and positional. `reconstruct_unchanged` pushes `old_assign[i_old]` into a parallel `Vec<u32>` right next to `current_symbols.push` (`parse_files.rs:154`), so dropped empty-name records stay aligned. The writer's sequential numbering (F11) makes new idx = position in that vector. It also builds `old_to_new: Vec<u32>`.
2. **Re-parsed (changed-file) symbols** look up `(path, name, kind)` among the old symbols of the *same path*. If the key matches exactly one old symbol, they inherit its assignment; otherwise they get NEW. Line numbers are excluded because edits shift them.
3. **New-file symbols** get NEW.
4. **Deleted-file symbols** disappear.
5. **Table:** carried record by record. `rep_sym_idx` and `hubs` are remapped through `old_to_new` (key matches included); a symbol that did not survive becomes `u32::MAX`. `size`, weights and `label_offset` (re-interned) are frozen build-time values. Readers compute live member counts.
6. **Flags:** keep COMPUTED, set STALE, keep γ, `algo_version` and `levels`. STALE is only cleared by a full `vex index`. The skip path (`pipeline/mod.rs` "nothing to update") writes nothing, so it never marks the section stale.

**Why this definition:**
- "Clear on first edit" makes `modules` useless under `vex watch`.
- "Recompute on update" violates Phase A and churns ids on every save because of CPM degeneracy.
- Keying *everything* by `(path, name, line)` breaks as soon as lines shift.
- Positional carry is exact for unchanged files and free; the one key match keeps an edited file's symbols from all falling out.

**Contract** (documented in `vex modules --help` and the MCP description): *cluster ids are stable within one full-index generation only. Do not persist them across `vex index` runs.*

---

## 6. Ranking boost: **not in Phase A**

Reasons:
- **It cannot be measured:** `vex eval` never passes `context_path` (F13), and the golden `queries.toml` has no context field.
- **It degrades silently:** stale or NEW assignments would skew ranking after an update.
- **The design space is still open:** is the context a path or a symbol?

Follow-up (Phase A+1, a separate PR, no format change):
1. Add an optional `context_path` field per query to the golden-set schema (`eval/harness.rs:65`), plus about 15 context-bearing queries.
2. Add `cluster_proximity_boost` next to `module_proximity_boost` (`rerank.rs:257`): same cluster as any symbol defined in the context file → ×`CLUSTER_SAME` (start at 1.15). It is multiplicative with the path boosts and **disabled when STALE**.
3. Gate it behind `VEX_CLUSTER_BOOST=1`. Run `vex eval --json` with it off and on across the 3 §3.4 corpora.
4. Make it default-on only if nDCG@10 and MRR do not drop on the existing context-free set **and** improve on the context set. Record the numbers in `docs/RANKING-EVAL.md`.

---

## 7. Validating untrusted counts (every new reader)

General rules:
- Header `u64` values go through `usize::try_from`, then `checked_add` / `checked_mul`.
- Any failure at open → `bail!("… corrupted (…). Re-run `vex index` to rebuild.")`, matching the existing messages.
- Any failure inside a lookup → empty result, never a panic.
- No allocation is sized from an untrusted count unless it is capped by what the blob can physically hold (the `call_graph.rs:207-211` idiom).

At `open()` (structural checks, O(1)):

| check | failure |
|---|---|
| v9 chain fits (704 B) | bail |
| every `(offset, len)` of the callees index, ref index and cluster section is `≤ mmap_len`, with saturating add | bail |
| callees: `index_len ∈ {0, 4·(symbol_count+1)}`; `edge_idx_len == 4·call_edge_count` iff `index_len > 0` | bail |
| ref: `index_len ∈ {0, 4·(symbol_count+1)}`; v9 `edge_idx_len == 0` | bail |
| cluster: COMPUTED ⇒ `assign_len == 4·symbol_count`, `table_len % 32 == 0`, `k = table_len/32 ≤ symbol_count/2`, `resolution_den ≠ 0`; not COMPUTED ⇒ all lens 0 | bail |

Per lookup (`CsrView::neighbors(s)`):
- `s < n`, otherwise empty.
- Read `start = off[s]` and `end = off[s+1]` with bounds-checked decode. Require `start ≤ end ≤ m`, otherwise empty.
- Iterate with no allocation. For callees, each `edge_idx < call_edge_count`, otherwise skip (as `ref_edges.rs:151-156` does).

Cluster reader:
- Any assignment `≥ k` that is not a sentinel reads as NOT_ELIGIBLE.
- `rep` / `hubs` values `≥ symbol_count` read as absent.
- Labels go through `read_string`, which is already bounded.
- Per-cluster aggregation vectors are sized `k`, which is bounded by `symbol_count`, which is bounded by the file size (`reader.rs:87-97`).

Monotonicity of `offsets` is **not** checked at open (that would be O(n) on every command). Per-lookup checks make a non-monotone array degrade to empty results. `vex status --verify` may add a full scan (Q6).

**Fuzz** (new `[[bin]]` entries in `fuzz/Cargo.toml`, seeds via `generate_seeds.sh`):
- `fuzz_csr`: arbitrary `offsets` and `edge_idx` bytes plus `n`, `m` and a list of queries, against the public `vex::store::csr::CsrView::new(..)` (both callees mode and identity mode).
- `fuzz_cluster_section`: arbitrary assign and table bytes plus header fields, against `ClusterSectionReader::new(..)`, exercising `summary()`, `members(k)` and `status(sym)`.
- `fuzz_leiden`: a `pub fn __fuzz_leiden_bytes(data)` shim in `src/cluster/leiden.rs` (the `__fuzz_*` pattern, e.g. `store/rename_chains.rs:714`). It decodes a graph of ≤ 256 nodes, runs Leiden twice and asserts: identical output, every cluster induces a connected subgraph, and no Δ' overflow.
- `fuzz_index_reader`: regenerate its corpus with v9 seeds, including a COMPUTED + STALE index.

---

## 8. Test plan

**Unit tests:**
- **Layout:**
  - pinned sizes: `ClusterHeader == 48`, `ClusterRecord == 32` (align 4), CallGraphHeader still 128, V5 still 48;
  - v9 `symbols_offset == 704`;
  - write → open → read roundtrip for every new field.
- **CSR equals v8.** The old `build_callees_fst` moves to a `#[doc(hidden)] pub mod legacy_v8` used as an oracle by tests and the bench only. A proptest over random `(caller, idx)` lists asserts that for every s the CSR group equals the v8 FST posting list, element for element. Same for ref_edges: `records[off[s]..off[s+1]]` equals the v8 posting list. Also covered: out-of-range key → `Err`, empty input, `n = 0`.
- **Legacy fallbacks:** callees linear scan and ref-edge binary search on a v8 fixture both match the recorded v8 FST answers.
- **Leiden on hand-built graphs with known partitions:**
  - two K5 joined by one bridge → 2 clusters;
  - a ring of 8 K4 (the modularity resolution-limit counterexample) → 8 clusters under CPM;
  - a star → hub plus leaves in 1 cluster at γ=1/8 (document);
  - disconnected components never merged;
  - isolated node and final singleton → UNCLUSTERED;
  - empty graph;
  - single edge.
- **Leiden properties** (proptest):
  - the same graph with edges fed in shuffled order gives an identical result;
  - running under `rayon::ThreadPoolBuilder` with 1 and with 8 threads gives an identical result (inputs come from `par_iter`);
  - every cluster is connected;
  - `H(final) ≥ H(singletons)`;
  - ordinals ascend by min member.
- **Projection:** weight table; dedup of a call site seen by both the call graph and the binder; PAIR_CAP; nearest-preceding attribution, including the "before first symbol → dropped" case; ineligible kinds and languages excluded.

**Integration tests** (assert_cmd; new `tests/cli_modules_test.rs`, fixtures under `tests/fixtures/`):
- a multi-directory Rust fixture: JSON envelope shape, `--min-size`, `--members`, `--sort`, scope filters, symbol mode, exit codes 0 / 1 / 2 (the 2 case uses a corrupt `table_len` via the `adversarial_format_test.rs` style);
- `--no-clusters` → exit 1 with `clusters_not_built`;
- `--workspace` grouping;
- `vex status` keys;
- `cli_capabilities_test` checks `symbol_clusters`;
- MCP descriptor snapshot plus a `build_modules` argv test.

**v8 compatibility:**
- Check in a small v8 index fixture `tests/fixtures/format/v8_small/`, generated in P0 by the pre-bump binary, with golden `callees` / `usages --strict` / `callers` outputs.
- Assert that the v9 binary reproduces them, that `modules` exits 1, and that `vex update` rewrites the file as v9 with a zeroed ClusterHeader.

**Incremental consistency** (new `tests/incremental_consistency_clusters.rs`, following `incremental_consistency_ref_edges.rs`):
- index → edit one file → update:
  - STALE is set;
  - every unchanged symbol keeps its ordinal (compared by `(path, name, line)`);
  - an edited file's surviving symbols keep theirs;
  - an added symbol → NEW;
  - reps and hubs remapped or `u32::MAX`.
- Then `vex index`: STALE cleared, and the cluster section is byte-identical to a fresh index in a clean clone.
- Full index twice gives byte-identical cluster sections (determinism).
- After update, callees and ref-edge answers match a fresh full index.

**Bench** (`benches/graph_v9.rs`, following `benches/bundle.rs`; `[[bench]] harness=false`):
- callees lookup: CSR vs the legacy_v8 FST oracle;
- ref lookup: offsets vs the v8 FST;
- `build_csr` vs `build_callees_fst`;
- projection + Leiden on a synthetic 35k-node / 200k-site graph from a fixed LCG.
- Gates: CSR lookup ≤ 10 ns, and the §3.3 clustering budget.
- The bench supersedes the local `examples/*_measure.rs` experiments. Those are
  maintainer-owned, gitignored and never edited or deleted by any phase.

---

## 9. Implementation phases (each separately committable, each green)

| # | Commit | Content |
|---|---|---|
| P0 | `test(format): pin v8 fixture + golden graph answers` | Check in the v8 fixture and golden outputs (§8 compatibility). No production change. |
| P1 | `feat(store): csr module + legacy_v8 oracle` | `src/store/csr.rs` builder plus `CsrView`, proptest equivalence against the oracle, `fuzz_csr`. Not wired in. |
| P2 | `feat(format)!: v9 — CSR callees/ref_edges, ClusterHeader slot` | `VERSION = 9`; header field renames; ClusterHeader written zeroed; writer emits CSR (`writer.rs:494,831,918-941,1194-1219`); reader version dispatch plus v4–v8 fallbacks; delete `encode_caller_key*`, `encode_to_sym_key` and the callees FST path; update `tests/call_graph_test.rs`; open-time validation and adversarial tests; downgrade-gate test; P0 goldens green. **CSR fully lands here, and the one format bump already reserves the cluster slot, so no v10 is needed.** |
| P3 | `feat(cluster): deterministic Leiden-CPM + projection` | `src/cluster/{mod,projection,leiden}.rs` as pure functions over builders; unit tests and proptests, `fuzz_leiden`, bench, γ sweep recorded in this document. Not wired in. |
| P4 | `feat(index): compute clusters on vex index, carry on update` | `ClusterInput` threaded through `write_output_locked` → writer; compute after `writer.rs:868`; labels and hubs; `--no-clusters`; manifest marker; `reconstruct_unchanged` carry (§5); cluster reader, `fuzz_cluster_section`; incremental tests; `vex status` keys. |
| P5 | `feat(cli): vex modules` | Command, text and JSON output, workspace support, exit codes; `EXIT-CODES.md`, README, `vex` skill doc. |
| P6 | `feat(mcp): modules tool + symbol_clusters capability` | Descriptor, snapshot, capabilities test. |
| P7 (not Phase A) | `exp(ranking): cluster proximity boost` | §6. |

---

## 10. Risks

- **Degeneracy and id churn between full indexes.** Small edits followed by `vex index` can relabel many clusters. Mitigated by the "stable within a generation" contract and by labels and hubs, which are more stable than ordinals. Real mitigation is Phase B.
- **Clusters drift stale under `vex watch`,** which never runs a full index. Mitigated by the visible stale line and NEW counts. Q2 might add a `vex index`-in-background suggestion.
- **Quality depends on binder coverage** (F16). Text-tier languages get only name-resolved call edges, so expect many UNCLUSTERED symbols there. `vex modules` reports counts honestly.
- **Giant-cluster risk** from utility hubs. Mitigated by PAIR_CAP, CPM (no resolution limit) and the max-size check in the γ sweep. Degree damping is Q5.
- **Nearest-preceding attribution** mis-assigns module-level code (§3.2).
- **Build-time regression.** Guarded by the bench budget. Clustering is skipped with `--no-clusters`.
- **Header field re-meaning** (same bytes, version-dependent semantics). Mitigated by the single `callees_layout()` accessor and renamed fields.
- **Older binaries cannot read v9;** downgrading needs a re-index (existing gate message).

## 11. Non-goals

- Phase B incremental Leiden (dynamic frontier, seeding from the prior partition, Jaccard id matching).
- SCC prepass.
- The ranking boost (§6).
- Partition-aware `impact`.
- Cluster ids in `search` / `show` output.
- Cross-member (workspace) clusters.
- Neighbour-list compression (StreamVByte / Elias-Fano).
- Converting the callers FST (it is legitimately name-keyed).
- Converting `hierarchy_edges`, whose sparse layout is correct for its density.
- Binder-scope source attribution for refs.

## 12. Open questions for the maintainer

- **Q1:** Elide the ref_edges `edge_idx` array because it is the identity (§2.3)? This deviates from the literal "offsets + edge_idx" scope decision. The recommendation is yes: it is reversible without a bump, since the reader requires `len == 0` today.
- **Q2:** Should `vex update` compute clusters from scratch when the prior index has none (the v8 → v9 upgrade path), or stay strictly "full index only"? The recommendation is strict Phase A.
- **Q3:** Should γ be user-configurable (`.vex.toml [clusters] resolution = "1/8"`), or stay a constant pinned by `algo_version`?
- **Q4:** Should hierarchy edges be included at weight 1 (the recommendation), weighted differently, or excluded? Interface-in-core with implementations in plugins argues for a low weight.
- **Q5:** Is hub-degree damping (e.g. weight / log(deg)) acceptable in `algo_version` 1, or should it be left for tuning after the P3 sweep?
- **Q6:** Add a `vex status --verify` full-scan integrity check (CSR monotonicity, cluster table consistency), or leave validation to per-lookup checks?
- **Q7:** Should the command be named `modules` only, or also get a `clusters` alias? §9 of STORAGE-RESEARCH offers both.

---

## 13. Review resolutions (2026-09-30) — supersede §1–§12 where they conflict

Two design reviews ran in parallel before any code: architect (2 CRITICAL, 5 HIGH)
and rust-reviewer (1 CRITICAL, 1 HIGH). Every finding is resolved below.
Findings were checked against source; the file-walk one (R6) was re-verified
by hand (`src/util/walk.rs:10-30` never sorts).

### Correctness of the ref_edges elision

- **R1 (rust C1).** Eliding `edge_idx` makes "RefEdge records sorted by
  `to_sym_idx`" a load-bearing invariant. It is enforced in **release**, twice:
  - **Writer:** `ensure!` that the records are sorted before writing a v9 file
    with `edge_idx_len == 0`, not a `debug_assert!`. This is O(m) once per
    index. A failure fails the write loudly; it is a writer bug.
  - **Reader:** `ref_edges_for(s)` yields only records in
    `records[off[s]..off[s+1]]` whose `to_sym_idx == s`. It drops the rest and
    logs `warn!` once per reader. The cost is O(k) per lookup, the same k it
    already iterates. A mis-sorted file therefore degrades to *missing* refs,
    never *another symbol's* refs.
- **R2 (arch M4).** Drop the claim that a real `edge_idx` "can be added later
  without a bump". A v9 reader requiring `len == 0` would call such a file
  corrupt. Sortedness is pinned as a **v9 format invariant**, with a test.

### Open-time validation must never brick the index

- **R3 (arch C1).** Remove `k ≤ symbol_count/2` and every other *semantic*
  cluster check from `open()`. A `vex update` that deletes files shrinks
  `symbol_count` while the frozen table keeps k, so this check would brick every
  command.
  - **At `open()`:** only bounds checks, `table_len % RECORD == 0` and
    `assign_len == 4·symbol_count`.
  - **In a lazy `ClusterSectionReader::new`:** semantic checks. A bad cluster
    section fails only `vex modules` (exit 2), never `search`, `callers`, etc.
  - The header gains `build_symbol_count: u32` (see R9).
- **R4 (arch M10).** Add O(1) CSR checks at open: `offsets[0] == 0` and
  `offsets[n] == m`, for both callees and refs.

### Skip gates

- **R5 (arch C2, M3).** The cluster marker goes in **`run_can_skip` only**, as
  `manifest.clusters_full: Option<bool>`, mirroring `pattern_index_full`
  (`pipeline/mod.rs:171-178`). It must not go into the shared
  `manifest_options_cover`, or every no-change `vex update` stops skipping,
  forever.
  - **`vex index`:** a no-change run does not skip when `clusters_full != Some(true)`
    and clusters are wanted. This clears STALE.
  - **`vex update`:** `update_can_skip` returns false when
    `header.version < VERSION`, so an untouched v8 index converges to v9 on the
    next update instead of staying v8 forever.

### Determinism and quality of the projection

- **R6 (arch H1).** The §3.3 guarantee holds for *node order*, but `sym_idx`
  itself follows readdir order, which differs across APFS, ext4 and NTFS.
  - Leiden's node order, tie-breaks and the cluster-ordinal order use the
    canonical key **`(path, line, kind, name, sym_idx)`**.
  - Results map back to `sym_idx` afterwards.
  - The global walk order stays unsorted. That is out of scope and would change
    `sym_idx` for everything.
  - §3.3's "min `sym_idx`" and §2.4's "ascending `rep_sym_idx`" read as "min
    canonical key". `rep_sym_idx` still stores the member's `sym_idx`.
- **R7 (arch H2).** Ambiguous resolutions are not edges.
  - The writer keeps a transient `ambiguous: Vec<bool>` parallel to
    `ref_edge_builders`. It is set when the Imported arm's
    `resolve_by_name_and_path(.., None, ..)` had more than one candidate
    (`writer.rs:44-45,605-613`).
  - The projection drops those edges.
  - The "exactly one project-wide candidate" call rule counts **eligible**
    candidates only.
- **R8 (arch L).** The call/ref dedup key is `(from_file, line, to)`, not
  `(from, to, line)`, so a mismatch between nearest-preceding attribution and
  the exact caller does not double-count. Known approximations to document:
  - the same-file callee rule can attribute `Bar::new()` to a local `Foo::new`;
  - the P3 sweep reports how many refs are attributed to Constant/Property sources.

### ClusterHeader and phasing

- **R9 (arch H3, rust L1).** ClusterHeader grows to **64 B**:
  - the 48 B of §2.4, then `build_symbol_count: u32` at 48 and 12 reserved
    bytes (write 0, ignore on read);
  - `symbols_offset = 720` on v9;
  - **field order is load-bearing** (all u64 first), pinned by `SIZE == 64`
    and per-field `offset_of!` tests.
  - The P2 reader **ignores the cluster section's content entirely**.
  - **No release tag is cut between P2 and P4b**, so the v9 layout ships once,
    complete. (This is a release-process rule for the maintainer.)
- **R10 (arch L).** P4 splits into **P4a** (compute on `vex index`, reader,
  `vex status` keys) and **P4b** (carry on update).

### Update carry

- **R11 (rust H1).** Do not use the in-scope `unchanged_count`
  (`pipeline/mod.rs:816`). It counts *vectors* and is 0 without embeddings.
  - `reconstruct_unchanged` builds the per-symbol carry vector itself. It pushes
    next to `current_symbols.push` (`parse_files.rs:154`) and is gated only by
    the same `continue`s.
  - That `Vec<u32>` and `old_to_new` are threaded to the writer, so no boundary
    arithmetic is left at the call site.
  - Assert the carry is built from the reader opened after the lock
    (`pipeline/mod.rs:715`), the same one `reconstruct_unchanged` uses.
- **R12 (arch H4).** The `(path, name, kind)` match requires **1:1
  uniqueness on both sides**; otherwise the symbol is NEW. Cascade files (in
  `changed_set` but with an unchanged content hash) carry positionally within
  the file when the old and new symbol counts match.
- **R13 (arch M8).** Carry applies the eligibility predicate. New ineligible
  symbols are NOT_ELIGIBLE, not NEW.
- **R14 (arch H5 → Q2).** `vex update` computes clusters **once** when the
  prior index is not COMPUTED and the manifest does not record `--no-clusters`.
  Every later update carries. `update` already holds the full edge set
  (`parse_files.rs:192-220`, `writer.rs:749-807`), so v8 upgraders on
  auto-update and MCP get clusters without a manual `vex index`.

### Arithmetic, API and algorithm

- **R15 (rust M1, arch M6).** Δ' and connectivity use **i128** intermediates,
  always, not debug-only checked math. `VEX_CLUSTER_RESOLUTION` is validated at
  parse: `0 < num ≤ 1024`, `0 < den ≤ 1024`, otherwise warn and use the default.
- **R16 (rust M2; as built in P1).** `CsrView::neighbors(s)` returns
  `CsrNeighbors<'a>`. This is a zero-allocation `Iterator<Item = u32>` enum
  with the variants `Edges` (decodes the borrowed LE `edge_idx` window),
  `Identity` (the implicit range of the elided ref_edges shape, which has no
  bytes to slice) and `Empty`. A literal `&'a [u8]` cannot represent the identity
  shape. `find_callees_fast` keeps its `Vec<CallMatch>` signature; the ≤ 10 ns
  bench gate measures the neighbour lookup.
- **R17 (arch M7).** Level termination follows Traag et al.: stop when local
  moving leaves every aggregate node a singleton community (`|P| = |V(G)|`).
  - If `MAX_LEVELS` is hit, run a final refinement before exit, so "every
    cluster connected" holds unconditionally.
  - The 4 outer iterations stay as a convergence cap.
- **R18 (arch M5).** `build_csr` filters a key `≥ n` with `warn!` in the writer
  and a `debug_assert!`, instead of bailing (`ModuleSymbol` uses an unchecked
  `wrapping_add`, `writer.rs:595`).
  **This applies to `build_csr` only.** `build_csr_offsets_sorted` (the elided
  ref_edges shape) **bails** on a key `≥ n`. It has no `edge_idx` to drop a
  record from, so filtering would misalign offsets against the physical `RefEdge`
  array: keys `[0, 5, 1]` with `n = 3` would make group 1 point at the dropped
  record. The P2 writer therefore filters bad `to_sym_idx` records *before*
  sorting and writing, so records and offsets stay in lockstep. Both builders
  return `Result` and use checked prefix sums.

### Legacy read path

- **R19 (arch M1, M2).** Legacy (v4–v8) callees and refs build an
  **in-memory CSR once per `IndexReader`** (`OnceCell`) from the record
  sections. This gives one query path for all versions, O(m) once instead of
  O(m) per `tests-for` call, and no unverified binary-search assumption on
  v5–v7 record order (the counting sort doesn't need sortedness).

### Tests

- **R20 (rust M3, arch M9).** No checked-in binary v8 fixture.
  - `legacy_v8` (the oracle module of §8) gains a programmatic v8-file builder
    used by the tests.
  - v8 compatibility is tested at **library level** (`IndexReader::open` +
    lookups), not through the CLI, where auto-update would rewrite it to v9
    before the query runs.
  - One CLI test covers the upgrade itself: v8 file → `vex update` → v9.

### Small corrections

- **R21 (arch L, rust L2).** Cluster work and label interning run **before**
  the layout math at `writer.rs:884`. Update the `format.rs:352` "LAST header"
  comment in P2. The label rule's 60 % share counts **members**, and paths split
  on `/` (already POSIX via `to_rel_posix`).

### Verdicts on §12

| Q | Decision |
|---|---|
| Q1 | Elide ref `edge_idx` (with R1 and R2). |
| Q2 | Compute once on update when absent (R14). |
| Q3 | No config in Phase A. γ is a constant stored in the header, plus the validated env knob. If γ ever becomes configurable, it must join `run_can_skip`. |
| Q4 | Include hierarchy edges at weight 1. Resolution is single-candidate only (`writer.rs:134-141`), so they are low-noise. |
| Q5 | Defer damping. `log` breaks the integer-only rule; if needed, integer damping plus an `algo_version` bump. |
| Q6 | No `--verify` now (R3, R4 instead). |
| Q7 | `modules` primary, hidden clap `alias = "clusters"`, one MCP tool. |

### Revised phase table

P0 legacy_v8 oracle + programmatic v8 builder + library-level goldens →
P1 `csr.rs` → P2 v9 bump (CSR + 64 B zeroed ClusterHeader, R1/R4/R5-update/R19; the same
commit deletes `legacy_v8`'s production-parity tests, which import the removed
FST functions, and keeps the CSR-vs-oracle proptests) →
P3 Leiden + projection (R6–R8, R15, R17) → P4a compute on index →
P4b carry + compute-once on update (R11–R14) → P5 `vex modules` → P6 MCP.
**No tag between P2 and P4b.**
