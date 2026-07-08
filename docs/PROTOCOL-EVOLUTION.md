# Response-Protocol Evolution — Plan

Status: **PLAN** (2026-07-08, revised after architect + code-reviewer
review). No code changes yet. This doc is the agreed methodology + backlog
for evolving the `ResponseEnvelope` (`protocol_version` currently `"v1"`)
without breaking existing consumers. It supersedes the earlier "defer
nested Signals to a v1→v2 bump" note from the v1.20.0 release audit (§7).

The core reversal: **a `protocol v2` version-gate is NOT the vehicle for
the pending contract changes.** Each item lands *additively* in the v1 line
via expand-and-contract + a tolerant reader, with opt-in driven by the
`capabilities` block — never by a version number. A real `v2` is reserved
for a single batched *contract cleanup* (dropping all deprecated fields at
once), cut only on the concrete trigger in §5.

Grounding: `docs/STORAGE-RESEARCH.md` §Appendix (protocol research thread),
the shipped `UsagesTrace.mode` / `mode_legacy` precedent
(`src/cli/trace.rs`), and the self-describing-`_meta` compat note in
`docs/MCP-SCHEMA.md`.

---

## 1. Guiding principle — additive-first

The envelope has two audiences with opposite tolerances:

- **MCP / code-mode consumers** parse `structuredContent` field-by-field
  (`crates/vex-mcp/src/response.rs`). Moving, renaming, retyping, or
  re-meaning a field breaks them.
- **LLM agents** read the `content` text channel and tolerate additive
  change trivially.

So the default is: **add, never move.** A new field with
`#[serde(skip_serializing_if = "Option::is_none")]` (or an `is_zero_*`
guard) is invisible to consumers that don't ask for it, and byte-identical
output is preserved for the un-opted-in path. This is exactly how D2/D4
already shipped `def_site_dropped`, `docs_dropped`, `bm25_score`,
`semantic_cosine` — additive slots on the locked 13.11 envelope.

### 1a. Hard compatibility invariants (do not violate)

1. **Never `#[serde(flatten)]` a struct of `Option` + `skip_serializing_if`
   fields.** `flatten` silently *drops* the skip guard on nested fields and
   emits `"field": null`, breaking byte-identity. This was the CRITICAL
   that killed the "Full" manifest refactor — see
   `reference_manifest_god_object_debt`. It is why nested Signals cannot be
   done as "internal struct, flat wire via flatten".
2. **Tolerant reader, both directions.** `ResponseEnvelope` / `Signals` /
   `MetaEnvelope` / `Capabilities` must never gain
   `#[serde(deny_unknown_fields)]`. Our readers ignore unknown keys, and we
   promise never to reject them.
3. **`skip_serializing_if` on every new `Option` / zero-valued field**, so
   the wire stays byte-identical for consumers that don't populate it.
4. **Never change the *type* of an existing field** (`u32`→`f64`, scalar→
   array, scalar→object). Field-by-field parsers key on JSON type. §3.1 is
   literally a restructure, so this must be explicit: the restructure ships
   under a *new key*, never by retyping `signals`.
5. **Never change the *semantics* of an existing field's value.** The
   subtlest break: a field keeps its name and type but the number now means
   something different. The additive rule protects field *presence and
   shape*, not *meaning*. This is the gap §3.2 must respect.
6. **`#[serde(rename = "…")]` strings are the wire contract**, not just the
   Rust field name. `MetaEnvelope` is full of `vex.dev/*` renamed keys
   (`vex.dev/stale`, `vex.dev/why_trace`, …); renaming the attribute string
   is a break even if the Rust field is renamed "cleanly".
7. **Protocol structs are serialize-only (no `Deserialize`) — keep it that
   way.** vex is the producer; consumers parse untyped JSON. Adding
   `Deserialize` would tempt `deny_unknown_fields` + round-trip coupling and
   structurally undermine invariant #2.

### 1b. Capabilities are a contract too

`capabilities` is itself part of the wire and evolves under its own rules:

- **Capability values are append-only.** A new `bundle_modes` entry is fine;
  reordering or removing one is a break. Order-stable arrays are a
  sub-contract — `cli_capabilities_test.rs` pins `bundle_modes` order and its
  comment notes downstream clients may rely on it.
- **Consumers MUST treat an absent capability key as `false`.** An old
  binary never emits a new flag; a new consumer reading an old server sees
  it missing. "Absent ⇒ unsupported" is the only safe rule, and it's the
  consumer-side half of invariant #2.

### 1c. Snapshot / byte-identity is the enforcement mechanism

Every additive change ships with tests (design in §2a); the MCP snapshot
test pins byte-identity and already caught a description typo during the
vex-mcp split. Updating a golden must be a deliberate, reviewed act — never
the reflex fix for a failing byte-identity assertion.

---

## 2. Expand-and-contract playbook

The lifecycle every contract change follows. Precedent:
`UsagesTrace.mode` (new value `"fst_lookup"`) shipped alongside
`mode_legacy` (old `"text_scan"`).

1. **Expand** — add the new representation under a *new key*, next to the
   old one. Both are emitted; old readers keep the old field, new readers
   read the new one.
2. **Advertise** — flip a `capabilities.*` flag so consumers feature-detect
   and opt in. The flag flips `true` in the **same release** that first
   emits the field — never advertise ahead of emission.
3. **Deprecate** — document the old field as deprecated (doc comment +
   `MCP-SCHEMA.md`) and record it in the §5 removal manifest.
4. **Contract** — remove the old field. **Removals happen only at `v2`,
   never at a plain minor.** Removing inside v1.x would be a de-facto break
   in the line whose whole premise is "no breaks in v1.x".

> **Honesty about slip.** The `mode_legacy` doc comment says "slated for
> removal in v1.12"; the project is at v1.22 and it is still present. That
> is precisely why removals are batched at v2 rather than chased per-minor —
> per-minor removal windows are not honored in practice, and a missed one is
> a silent break. Treat the "v1.12" note as a cautionary precedent, not a
> model to copy.

### 2a. Testing each expand step

Per step, add (not just "update the snapshot"):

- **Un-opted-in byte-identity test** — assert a default-constructed / new
  field-absent envelope serializes *without* the new key. This guards the
  "invisible to non-opted-in consumers" promise and cannot be satisfied by
  editing a golden.
- **Capability↔emission coupling test** — assert `capability == true` iff
  the field can appear, `false`/absent iff it never does. Pins §2 step-2
  mechanically. Model: `bundle_mode_flag_all_matches_capabilities`.
- **Cross-crate tolerance test** — `crates/vex-mcp` reconstructs
  capabilities as untyped JSON; add a test that it tolerates an envelope
  carrying an unknown capability key AND an unknown `_meta.vex.dev/*` key.
  This proves invariant #2 for the real consumer, not just the producer.

---

## 3. Backlog — pending contract changes (Layer 1)

Historically tagged "needs v2". All land additively in v1.x.

### 3.1 `Signals` decomposition — internal now, wire nesting at v2

**Problem.** `src/protocol/mod.rs::Signals` is 8 flat `Option<...>` fields
mixing four concerns. Every new search channel forces another flat field
onto the lock-stable envelope.

Current flat shape (5 of 8 are `skip_serializing_if = Option::is_none`, so
`None` fields emit **no key** — not `null`):

```jsonc
"signals": {
  "fst_hit": true,        // structural
  "bm25_rank": 0,         // lexical
  "bm25_score": 4.2,      // lexical (f64)
  "semantic_rank": 1,     // semantic
  "semantic_cosine": 0.83,// semantic (f32)
  // fuzzy_distance    — lexical; omitted when None (always None today)
  // rerank_boost      — post-fusion; omitted when None
  "indegree": 12          // post-fusion; bundle-only
}
```

Concern grouping: **structural** `{fst_hit}` · **lexical**
`{bm25_rank, bm25_score, fuzzy_distance}` · **semantic**
`{semantic_rank, semantic_cosine}` · **post** `{rerank_boost, indegree}`.

**Decision (was open): split the work in two.**

- **Now, in v1.x — internal refactor only, wire unchanged.** Introduce
  `StructuralSignals` / `LexicalSignals` / `SemanticSignals` / `PostSignals`
  sub-structs used at every construction site
  (`src/protocol/signals.rs::build_signals`, and the `Signals { .. }`
  literals / helpers in `cmd_bundle/{symbol,project,mod}.rs`; note
  `cmd_bundle/pr_impact.rs` builds `Signals` indirectly via
  `signals_fst_hit()`). Assemble the flat wire `Signals` from them with an
  **explicit map** (never `flatten` — invariant #1). This directly relieves
  the "every new channel adds a flat field" pain internally and is
  zero-risk on the wire.
- **At v2 — the wire nesting.** Emit `signals` in nested form and drop the
  flat fields. `capabilities.signals_nested` flips at that point.

**Why not emit both flat + `signals_grouped` during a v1.x expand phase:**
two serialized copies of the same data is not just wire bloat — it is a
**divergence hazard**. Once flat and nested are populated from code (even
"same internals"), a channel added to one and forgotten in the other ships
silently inconsistent signals. That is the `FilterSnapshot`
"update-both-in-lock-step" bug class, escalated to serialized output. And
no code-mode consumer needs nested grouping today. If a concrete consumer
materializes before v2, revisit — until then, single-copy is correct.

### 3.2 `diff_dropped` residual — add an overlapping `scope_dropped` sub-count

**Status: SHIPPED (v1.23.0).** `UsagesTrace.scope_dropped` (`skip_if
is_zero_usize`) now carries `DropCounts.scope` as an overlapping sub-count in
the `--why` trace; `diff_dropped` is unchanged. No capability flag — it rides
under the existing `why` capability like its `def_site_dropped` /
`docs_dropped` siblings (see §6). The `narrowing_dropped` rename remains a v2
item.

**Problem.** In `cmd_usages`, the drop residual is computed as
`diff_dropped = pre_filter_count − total − def_site_dropped − docs_dropped`
(where `total` is post-scope + filter_path + diff). The channel *does*
compute `DropCounts.scope`, but `cmd_usages` never reads it — so path_scope
glob misses vanish into the `diff_dropped` residual. The balance
`pre_filter_count − def_site_dropped − docs_dropped − diff_dropped ==
hits_after_filter` holds *by construction*, but `diff_dropped` is really
`scope + filter_path + diff` combined, and an agent reading the field name
mis-attributes scope drops as diff drops. There is no `scope_dropped` field
today.

**Additive plan (truly additive — no semantic change).** Surface the
already-computed `DropCounts.scope` as a new `scope_dropped: usize`
(`skip_serializing_if = is_zero_usize`), documented as an **overlapping
sub-breakdown**: "of the `diff_dropped` residual, this many were
path_scope glob misses." **`diff_dropped` keeps its exact current meaning
and value**, so the old balance equation still holds for consumers that
reverse-engineered it (invariant #5). New readers get the finer
attribution.

A true rename (`diff_dropped` → `narrowing_dropped`, with `diff_dropped`
carrying *only* diff/filter_path) *does* change an existing field's meaning
and is therefore a break — defer that rename to the v2 contract batch (§5).
Do **not** narrow `diff_dropped` in the expand phase.

### 3.3 Argument aliases

**Problem.** Some flag/param renames are desirable but a hard rename breaks
scripted callers and MCP arg schemas.

**Additive plan.** Tolerant reader accepts both old and new names (old as a
hidden alias), canonicalizes internally. Precedent:
`UsagesTrace.mode_legacy`. Alias removal goes in the v2 contract batch.

---

## 4. Agent-output improvements (Layer 2 — highest value)

Per the protocol research these outrank nested Signals in agent value.
Split by break-risk:

- **Text `content` channel — ship now, no gate.** Free-form text for the
  LLM: a `via:` tag + `def`/`neighbor` marker per result, **no raw scores**,
  and the "no exact hit → these are neighbors" **drift hint inline in the
  payload** (agents don't see stderr — directly fixes the known
  `vex search` misuse footgun, `reference_search_ranking_drift`). This is
  the highest-value, zero-risk change and needs no capability flag to add.
  **Status: the drift hint is SHIPPED (v1.23.0).** It is carried in the
  envelope as `_meta.vex.dev/search_hint` `{ reason: "no_local_definition",
  query, message }`, set on the single-repo JSON path when an
  identifier-shaped query has zero structural hits, and on the workspace
  envelope's top-level `_meta` when *every* member drifts (query-scoped). The
  MCP builder needs no change — it already dumps the full envelope to
  `content[0].text` and propagates `_meta`. The `via:` / `def`/`neighbor`
  text markers remain future work.
- **`structuredContent` `def`/`neighbor` marker — gated.** Adding a
  structured result-kind field for code-mode consumers is an additive
  envelope change and gets its own flag (`capabilities.structured_result_kind`)
  under the §2 playbook. Keep it separate from the ungated text hint so the
  text fix ships immediately without waiting on the structured-marker
  design.

**Sequencing.** Land the §3.1 *internal* sub-struct refactor before the
§4 structured additions — §4 adds fields to the per-result shape
(`SearchResultWithSignals`, `src/cli/output.rs`) that §3.1 also touches;
stabilize the construction sites first, then build §4 on top. §3.2 and §3.3
are independent and can land in any order.

---

## 5. What `protocol v2` actually removes (the contract batch)

`v2` performs the *contract* step for everything deprecated during the
expand phases. Prospective removal manifest (kept current as items
deprecate):

- flat `Signals` fields — superseded by nested wire form (§3.1)
- `UsagesTrace.mode_legacy` — superseded by `mode`
- argument aliases from §3.3
- `diff_dropped` → `narrowing_dropped` rename, if chosen (§3.2)

**Cut trigger (concrete, to avoid indefinite deferral):** cut v2 when
(a) ≥ 2 fields have completed the full expand→advertise→deprecate cycle,
**and** (b) ≥ 1 minor has elapsed since the most recent deprecation was
announced (a real migration window). Not before.

**Coexistence model (decide at v2, recommended now):** a dual-audience
protocol should support **request-time selection** — the consumer names the
protocol version it wants (a request `_meta`/param or MCP negotiation), and
the server emits v1 or v2 accordingly — rather than a flag-day bump that
strands every un-updated consumer. `v2` therefore also means: teach the
producer to emit either shape on request, keep v1 emission until a
deprecation window closes. Write the full mechanics up here when v2 is
scheduled.

---

## 6. Capabilities — the negotiation surface

`capabilities` (`src/protocol/capabilities.rs::current()`) is how consumers
feature-detect additive changes instead of gating on `protocol_version`.
Evolution rules live in §1b; per-flag emission rule in §2 step-2.

Current flags: `signals`, `empty_reason`, `bundle_modes`, `why`,
`scope_filters`, `metadata_filters`, `auto_update`, `history_diff`.

Proposed additions (flip as each expand step lands):

- `structured_result_kind` — §4 `def`/`neighbor` marker in
  `structuredContent` (v1.x). The text-channel drift hint is **not** gated.
- `signals_nested` — §3.1 nested wire form. Flips at **v2**, not during
  v1.x (the v1.x work is internal-only).

No flag for §3.2 `scope_dropped` (revised from an earlier draft): it is a
sub-field of the `--why` trace, which is already gated wholesale by the `why`
capability. Its siblings `def_site_dropped` / `docs_dropped` (v1.20.0) ship
ungated the same way; a per-field flag for one of three peers would be
inconsistent. Trace sub-fields ride under `why`; only top-level envelope
shape changes earn their own flag.

---

## 7. Supersedes — the v1.20.0 "defer to v2" note

The v1.20.0 three-reviewer audit recorded: *"nested Signals is a wire break
→ defer to next `protocol_version` bump (v1 → v2); do not land in v1.x."*
That framing assumed the only clean shape was a breaking reshape.

The later protocol research (STORAGE-RESEARCH appendix) showed the useful
part (internal concern-grouping) is achievable additively, and only the
*wire* reshape needs v2 — so the version-gate is no longer the blocker for
progress. This doc is the current source of truth; the deferred-debt note
(`reference_v1_20_deferred_debt`) is historical context.

---

## 8. References

- `docs/STORAGE-RESEARCH.md` — §Appendix "MCP/JSON output protocol
  research" (additive-first verdict + agent-output P0/P1). **Local-only**;
  its load-bearing conclusions are reproduced above.
- `docs/MCP-SCHEMA.md` — envelope schema + self-describing-`_meta` compat
  note (lines ~205-209)
- `src/protocol/mod.rs` — `ResponseEnvelope`, `Signals`, `MetaEnvelope`,
  `Capabilities` (serialize-only; `MetaEnvelope` `vex.dev/*` renamed keys)
- `src/protocol/signals.rs` — `build_signals` (the §3.1 sub-struct target)
- `src/protocol/capabilities.rs` — `current()`; where new flags flip
- `src/cli/trace.rs` — `UsagesTrace.mode` / `mode_legacy` expand-and-contract
  precedent; `def_site_dropped` / `docs_dropped` / `is_zero_usize` additive
  slots; the §3.2 drop-accounting
- `crates/vex-mcp/src/response.rs` — field-by-field MCP consumer;
  capabilities reconstructed as untyped JSON (the §2a tolerance target)
- `tests/cli_capabilities_test.rs` — capability-pinning tests; model for the
  §2a coupling test and the `bundle_modes` order sub-contract
- `docs/LIMITATIONS.md` — §7 mixed-`--semantic` workspace note
