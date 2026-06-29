# Multi-repo Phase 6 — Cross-repo strict-usages resolution

Status: **SHIPPED** (2026-06-29). Extends `docs/MULTIREPO.md` §7 option B
("gtags-style ordered fallback"). Index format bumped **v6 → v7**
(`UnresolvedRefsHeader`). All §9 review resolutions folded in before
scaffold. 3030/3030 nextest, clippy + stable-fmt clean.

## 1. Motivation & refined scope

A code audit (2026-06-29) found the cross-repo gap is much narrower than
`MULTIREPO.md` §7 implied. These already work cross-repo via name-fanout and
need **no** change:

| Command | Why it already crosses repos |
| --- | --- |
| `callers Foo --workspace` | callers-FST is keyed by callee **name** (`call_graph.rs:244`); each member records `bar → "Foo"` regardless of where `Foo` is defined. |
| `usages Foo --workspace` (non-strict) | `FstRefsChannel` is an FST identifier scan — finds the name in every member. |
| `impact` / `check` / `search` / `grep` | name / text fanout. |

The single genuine gap this phase closes:

- **`usages Foo --strict --workspace`** — a binder-confirmed reference to `Foo`
  living in a member that does **not** define `Foo`. Today that reference is
  dropped at index-write time: `writer.rs:544` only pushes a `RefEdge` when
  `to_sym_idx` resolves; an `Imported`/`Unresolved` ref whose name has no local
  definition yields `None` and is silently discarded. So at query time the
  member has no record it ever referenced `Foo`, and `StrictRefsChannel`
  (`channel/mod.rs:494`) — which iterates `find_ref_edges_by_symbol(sym_idx)` —
  finds nothing.

Out of scope (documented follow-ups): multi-hop `reachable` across a repo
boundary; `impact --strict` cross-repo (verdict already flips via call-graph +
text channels, so the binder miss is non-fatal); `--through` union override.

## 2. Design principle (gtags)

Defer resolution to **read time** over N already-built indexes. Never merge
corpora, never touch the Pass-2 `name_to_global` writer loop's resolution
semantics (locked constraint, `reference_pass2_resolver_placement`). We only
**additively persist** the refs that loop already drops, keyed by name, and
re-resolve them at query time against sibling members in declared order
(first-hit-wins).

## 3. Format change: v6 → v7

### New section, mirroring v5 `reference_edges`
A new `UnresolvedRefsHeader` (`#[repr(C)]`, fixed-offset) is appended **after**
`PatternSkeletonHeader`, exactly as v6 appended `PatternSkeletonHeader` after
`V5SectionHeader`. Three sub-sections:

```
unresolved_edges : [ UnresolvedRef { from_file_id: u32, line: u32, col_and_kind: u32 } ]
unresolved_fst   : FST  lowercased-name -> posting-list offset
unresolved_post  : posting lists -> edge indices into unresolved_edges
```

`UnresolvedRef` is `RefEdge` minus `to_sym_idx` (the name lives in the FST key,
not on the edge — leanest, identical access pattern to `ref_edges.rs`). 12-byte,
4-byte aligned, castable from mmap.

### Header & version
- `format::VERSION = 7`.
- `MIN_SUPPORTED_VERSION` unchanged → v3..v6 indexes still open; the new
  accessors return `None`/`false` (graceful, identical to `has_ref_edges()` on a
  pre-v5 index).
- `Header` gains `has_unresolved_refs_header()` (same idiom as
  `has_v5_section_header()` / `has_pattern_skeleton_header()`).
- Located at `Header::SIZE + CallGraphHeader::SIZE + V5SectionHeader::SIZE +
  PatternSkeletonHeader::SIZE`; `symbols_offset` (writer.rs:706) gains
  `+ UnresolvedRefsHeader::SIZE`.

### Forward/backward compat
- Sections are self-described via header offset fields; the reader reads
  `header.symbols_offset` directly (reader.rs:277), it does **not** recompute.
  So even a hypothetical old reader trusts the (correct, shifted) field.
- New reader + old v6 file: `has_unresolved_refs_header()` is false →
  cross-repo fallback no-ops → strict usages behaves exactly as today.
  Re-index picks up the section (standard "re-index after format bump").

## 4. Writer change

In the per-file `bound_refs` loop (`writer.rs:465-564`), when `to_sym_idx`
ends up `None`, additionally push to a new `unresolved_ref_builders`:

```rust
// after the `match &r.target { ... }` that produced `to_sym_idx`
if to_sym_idx.is_none() {
    let cross_file_candidate = matches!(
        r.target,
        BindTarget::Imported(_) | BindTarget::Unresolved
    );
    if cross_file_candidate
        && name_to_global.get(r.name.as_str()).is_none()   // zero local defs
        && is_meaningful_identifier(&r.name)
    {
        unresolved_ref_builders.push(UnresolvedRefBuilder {
            name: r.name.clone(), from_file_id: file_id,
            line: r.line as u32, col: r.col as u32, kind: u8::from(r.kind),
        });
    }
}
```

**Capture set (decision):** Imported-arm-unresolved + Unresolved-arm, both gated
by (a) zero local candidates — a name with ≥1 local def is not the cross-repo
case — and (b) `is_meaningful_identifier` (`extractor/mod.rs:132`) to drop
pure-lowercase noise (`get`, `total`). This bounds index bloat: only names
undefined locally AND meaningful are stored.

Section build + offset wiring mirrors the v5 ref_edges block at
writer.rs:688-689 / 742-749 / 928-934. New module
`src/store/unresolved_refs.rs` mirrors `ref_edges.rs`
(`build_unresolved_section`, `UnresolvedRefReader::find_by_name`).

## 5. Query-time cross-repo fallback

Lives in the `cmd_usages.rs` **workspace branch** (~345-459), `--strict` only.
`StrictRefsChannel` and the single-repo path stay byte-identical.

```
strict workspace usages of `Foo`:
  owners = members where symbol-FST defines `Foo`, in declared order
  for each member M:
      hits_M = existing strict ref_edges (unchanged; non-empty only if M ∈ owners)
      if owners is non-empty AND M ∉ owners:
          hits_M += reader_M.find_unresolved_refs_by_name("Foo")
                    tagged cross-repo "→ {owners.first().display_name}"
```

- Gate on `owners` non-empty: an unresolved ref whose name is defined **nowhere**
  in the workspace (typo, dynamic) is NOT surfaced — preserves strict precision.
- First-hit-wins owner only drives the display attribution; every non-owner
  member's unresolved refs to the name are surfaced (a symbol can be used from
  many repos).
- Output: reuse the existing group-by-repo layout; cross-repo hits get a
  `→ repoA` suffix (text) / `resolves_to: "repoA"` (json).

## 6. Incremental update

`bound_refs` are reconstructed for unchanged files on `vex update`
(`writer.rs:607` Q4-A path). Unresolved refs derive from the same
`bound_refs`, so the per-file loop re-emits them for re-parsed files
automatically. For **reconstructed** (unchanged) files we must also carry the
unresolved refs forward, OR accept that `vex update` degrades cross-repo strict
usages until a full `vex index`. **MVP proposal:** accept degradation, document
in LIMITATIONS §7 (consistent with the existing Q4-B "run `vex index` to fully
reconcile" seam). Full reconstruction parity is a follow-up.

## 7. Tests (TDD order)

1. `format.rs` — `UnresolvedRef::SIZE == 12`, header size/offset constants.
2. writer unit — an Imported ref with no local def produces one unresolved edge;
   a locally-resolved ref produces none; a `≥2 local candidates` name produces
   none.
3. reader roundtrip — `find_unresolved_refs_by_name` returns the edge with
   correct path/line.
4. backward compat — open a v6 fixture → `has_unresolved_refs()` false, no panic.
5. e2e — workspace {repoA defines `Foo`; repoB `use a::Foo; ...Foo()`};
   `usages Foo --strict --workspace` surfaces repoB's ref tagged `→ repoA`;
   `usages Bogus --strict --workspace` surfaces nothing (defined nowhere).

## 8. Risks

- **Index bloat** — unresolved-ref volume in import-heavy repos. Mitigated by
  the zero-local-def + meaningful-identifier gate; bench index size on a real
  repo before commit.
- **Capture precision** — Unresolved-arm includes duck-typed method calls;
  zero-local-def gate removes most, but a method name coincidentally matching a
  sibling's type/function could surface a false cross-repo usage. Strict mode
  promises binder-confirmed refs — a name-only cross-repo match is weaker.
  **RESOLVED (§9, shipped):** cross-repo hits render as a distinct
  name-resolved sub-tier (`(name-resolved)` / `confidence: "name"`), so
  single-repo `--strict` binder-confidence is never silently diluted.
- **Update degradation** — see §6 (resolved: carry-forward, §9).
- **Double-open — RESOLVED (Phase 6.1).** `usages_workspace` is now a
  two-phase orchestration: phase 1 runs `ensure_index_ready` + opens each
  member's reader ONCE (capturing stale + owner status), phase 2 reuses the
  held readers for both the per-repo outcome (`usages_from_reader`) and the
  cross-repo lookup (`cross_repo_hits`). The render path was split into
  `render_workspace_json` / `render_workspace_text`. Single-repo `usages`
  stays byte-identical (still via `usages_in_root`, which now wraps
  `usages_from_reader`). Bonus: cross-repo hits share the member's reader,
  so its `stale_reason` already covers them.

## 9. Review resolutions (architect + rust-reviewer, locked before scaffold)

**CRITICAL — incremental carry-forward is NOT deferred.** §6's "accept
degradation" is overturned. The Q4-A reconstruction (`parse_files.rs:205-291`)
rebuilds refs for unchanged files only from the **resolved** RefEdge section, so
unchanged-file unresolved refs vanish on the first `vex update` — killing the
headline feature after one routine update. Add a parallel
`ReconstructedUnresolvedRef { from_file_id, name, line, col, kind }` pass
mirroring `parse_files.rs:213-291` (read old index's unresolved section, skip
`changed`/`deleted` `from_path`, carry the rest). The writer appends them to
`unresolved_ref_builders` after the per-file loop, sibling to the
`reconstructed_refs` block at `writer.rs:607-686`. Simpler than resolved
carry-forward: the FST key IS the name, so NO `name_to_global` re-resolution and
NO path-tiebreak. Stays within the locked Pass-2 placement.

**HIGH — compat rationale corrected.** §3's "old reader trusts the shifted
field" is wrong. A v6 binary **rejects** a v7 file at `reader.rs:49-58`
(`version 7 ∉ MIN_SUPPORTED_VERSION..=VERSION`). Outcome (no corruption) holds,
mechanism is "old binary refuses v7." Forward-compat = refusal, not trust.

**HIGH — reader holds N members open (orchestration refactor).** `usages_in_root`
(`cmd_usages.rs:206-338`) opens, queries, and **drops** its reader before
returning. Cross-repo fallback needs every member's reader alive at once (owner
detection + non-owner unresolved lookup). Implement as a two-phase workspace
orchestration: phase 1 open all member readers + detect `owners` (symbol-FST
defines `Foo`); phase 2 per-member strict refs + (non-owner) unresolved lookup.
Refactor `usages_workspace` into sub-functions (already over the 50-line limit).

**HIGH — cross-repo hits are a DISTINCT confidence sub-tier (mandatory, not
optional).** Single-repo `--strict` promises binder-confirmed refs;
name-resolved cross-repo hits are weaker (esp. the Unresolved-arm duck-typed
case). Tag them: text `→ repoA (cross-repo, name-resolved)`, JSON `resolves_to:
"repoA"` + `confidence: "name"`. Locked into §5 output + §7 test 5 assertions.

**HIGH — deterministic edge sort.** `build_unresolved_section` sorts by
`(name_lc, from_file_id, line, col)` before FST insert (rayon makes `bound_refs`
order non-deterministic; the FST builder requires ascending keys). Mirrors
`ref_edges.rs:44`.

**HIGH — 24-bit column mask + `debug_assert`** replicated from `ref_edges.rs:57-62`
in the new builder (`col_and_kind` packing).

**HIGH — pin `UnresolvedRefsHeader` to exactly 6 × u64 (SIZE == 48)**, same shape
as `V5SectionHeader`. Extra fields would silently drift `symbols_offset`.

**CRITICAL/HIGH (rust) — reader accessor safety:** (a) `ptr.align_offset(align_of::
<UnresolvedRefsHeader>()) != 0 → None` guard (mirror `reader.rs:508-510`);
(b) wrap `find_unresolved_refs_by_name` FST traversal in
`catch_unwind(AssertUnwindSafe(..))` + `tracing::warn!` (mirror `reader.rs:625-635`);
(c) truncation/bounds guards mirroring `reader.rs:182-226`.

**FST key casing.** Capture stores `r.name` raw; the builder lowercases at
FST-insert time (consistent with `symbol_fst`). The capture gate
`name_to_global.get(r.name.as_str()).is_none()` is **raw-vs-raw** —
`name_to_global` is raw-keyed (`writer.rs:403-409`, from `sym_entries` raw
names), so the gate is consistent. Query-time `find_unresolved_refs_by_name`
lowercases its argument.

**Two-gate reader API.** `has_unresolved_refs_header()` (version gate) +
`has_unresolved_refs()` (data gate, `len > 0`) — mirrors
`has_v5_section_header()` / `has_ref_edges()`.

**MEDIUM — 4-byte alignment** for `unresolved_edges_offset` via `(off+3)&!3`
(mirror `writer.rs:745-746`). Capture is an explicit `else` of the
`Some(global)` branch. Update `format.rs:1-37` layout/version doc. Add an
update-degradation e2e test (build → `vex update` non-owner repo → cross-repo
ref still surfaces).

**Capture set (final):** `Imported`-arm + `Unresolved`-arm, gated by zero local
candidates AND `is_meaningful_identifier` (`crate::parse::extractor::
is_meaningful_identifier`). `Local`/`ModuleSymbol` excluded by the `matches!`
gate.
