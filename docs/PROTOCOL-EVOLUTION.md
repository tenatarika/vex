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
mis-attributes scope drops as diff drops. (Pre-v1.23.0 there was no
`scope_dropped` field; it shipped v1.23.0 per the status note above.)

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

**Shipped so far.** The **drift hint** (`_meta.vex.dev/search_hint`, v1.23.0)
and the **structured `def`/`neighbor` marker** (`result_kind`, gated by
`capabilities.structured_result_kind`, v1.24.0) — see the git log and
`docs/MCP-SCHEMA.md`. `result_kind` derives `"def"` from an *exact/prefix*
structural match (a query-level `structural_fuzzy` flag disqualifies
Levenshtein-fallback rows). Both cross-checked against the STORAGE-RESEARCH
appendix agent-output P0/P1 list; the two items below are the top *open* ones
from that reconciliation.

### 4.1 Concise agent-tuned `content` text channel [P0/P1]

**Problem.** `crates/vex-mcp/src/response.rs:88` sets
`content[0].text = serde_json::to_string_pretty(&content)` — it dumps the
**entire envelope JSON** into the LLM-facing text channel. Raw `bm25_score` /
`semantic_cosine` and verbose structure reach the model, violating the
two-audience model (STORAGE-RESEARCH appendix / MCP SEP-1624): `content` is the
concise LLM channel; `structuredContent` is the rich machine channel.

**Design (MCP-wrapper-only; no CLI/envelope change).** Replace the raw dump
with a concise renderer that *reads* the already-parsed envelope
(`structuredContent.results` + `_meta`) and builds a new text string. It **only
reads** — it must never mutate `envelope_results`/`envelope_meta` before they
are assigned into `structuredContent`/`_meta` (`response.rs:81-151`), so the
machine channels stay byte-identical (existing `mcp_response_places_signals_*`
+ tolerance tests lock this boundary).

**Rich render is `search`-shaped only; everything else is a *never-lossy*
fallback (review HIGH-3 / MEDIUM).** Only `search` emits the full row
`{name, kind, path, line, signals, result_kind}` that the sigil+`via:` line
needs. Other subcommands are genuinely heterogeneous: `usages` emits bare
`{path, line}` (no name/signals), `implementations` `{name, base, relation,
path, line}`, `duplicates`/`bundle` paired/object shapes, `history` differs
again. So the renderer dispatches on the `subcommand` string (already available
in `build_mcp_response`):
- **Row-shape family** (`search`, and only tools verified to share its row
  shape) → the `<def|neighbor sigil> name  path:line  via:<channel>` line, **no
  raw scores**, drift hint prepended, completeness line appended (§4.2).
- **Everything else** → **compact-but-complete JSON of `results`** — worst case
  is "less pretty than a bespoke summary," **never** "fields dropped / empty
  render." (A row-renderer that assumed an array would empty-render `bundle`'s
  object-shaped results — strictly worse than today's dump. The fallback must
  degrade to *current-fidelity*, only more compact.)

`structuredContent` is **unchanged** (full-fidelity, scores intact) everywhere.

**`via:` derivation** mirrors `result_kind`'s own precedence so `via:` and the
`def`/`neighbor` sigil can never disagree: `fst_hit` present → `via:name`;
else the contributing channel (`bm25_rank` → lexical, `semantic_rank` →
semantic). Signal fields are `skip_serializing_if` (§3.1), so **absence, not
`false`, means "not this channel."** Fixed precedence (structural > lexical >
semantic) resolves multi-channel rows. Display-quality only, derived from
`signals`.

**Break analysis + decisions (resolved in review).** `content[0].text` is
advisory per MCP spec (display/LLM channel, not a typed contract). **(1) Do NOT
gate `concise_text`** — gating an advisory text channel invites permanent
double-maintenance; straight-replace. **(2) Keep a `VEX_MCP_TEXT=raw` escape
hatch *indefinitely*** (one branch, cheap) — do **not** promise "one release" (cf.
the `mode_legacy` "slated v1.12, still here v1.22" honesty note). Clients
parsing `content` as JSON are already relying on undocumented behavior; the
hatch is their migration path.

**Tests.** Golden concise-text for `search`; assert no raw-score substring
(`bm25`, a cosine float) and that the sigil + `via:` + (when present) drift hint
+ completeness line appear; a `bundle` (object `results`) golden asserting the
fallback is complete (no dropped fields, non-empty); assert `structuredContent`
still carries full `signals` with scores (proves split, not deletion).

### 4.2 Result-completeness signal [delete-safety correctness]

**Problem.** A consumer cannot distinguish "these are *all* the hits" from
"top-N of many". For `vex usages` this is a **correctness** gap, not just UX:
"2 callers → safe to delete" is wrong if 2 is `limit` of 50.

**Design.** Additive `_meta` fields under the `vex.dev/*` namespace (like
`search_hint`), all `Option` + `skip_serializing_if`:
- `vex.dev/truncated: Option<bool>` — **presence encodes "the producer knows".**
  `Some(true)` = capped, `Some(false)` = the full set (a *positive*
  completeness statement), `None`/absent = **unknown** (see safety rule).
- `vex.dev/result_total: Option<usize>` — total matches when known.
- `vex.dev/result_total_exact: Option<bool>` — `false` marks a *lower bound*
  ("≥ N"), for the search branch below.

**Safety rule (review HIGH-1 — the load-bearing correction).** For a *safety*
signal, **absent must mean "unknown", never "complete".** So: a consumer treats
completeness as known **only** when `capabilities.result_completeness` is
advertised **and** a `truncated` key is present. An old CLI on `PATH` (version
skew is normal, §5.2a) emits no keys and advertises no capability → the §4.1
renderer must say nothing / "completeness unknown", **never** "all N". This is
the inverse of the "absent capability ⇒ false" rule (§1b): safe for a *feature*
flag, dangerous for a *safety* signal. The renderer feature-detects
`result_completeness` (already lifted) before any completeness claim, exactly as
§5.2a drops `--protocol` when unsupported.

**Scope — exact for `usages` ONLY in v1 (review HIGH, corrected).** Only
`usages` materializes the full post-filter set before `.take(limit)`: `total =
post_filter.len()` is *already* computed and carried in `UsagesOutcome`
(`src/cli/cmd_usages.rs:347-353`) — `result_total = Some(total)`,
`truncated = Some(total > limit)` is a zero-refactor one-liner. The other
"enumeration" commands do **not** have this shape:
- `callers`/`callees` early-**break** the posting-list loop at `out.len() >=
  limit` (`src/store/call_graph.rs:269-271`) — the full set is never built.
- `implementations`/callgraph set `fetch_limit = limit` unless a filter is
  active and discard the pre-`take` count inline.
Emitting exact completeness for those three needs removing the early-break /
double-scanning — a real perf/behavioral change. **v1 ships `usages` only;
callers/callees/implementations are DEFERRED** (tracked as a follow-up that
revisits the early-break). Shipping the flag as "all enumeration commands"
would make it *lie* for 3 of 4.

**Search — honest lower bound, not silence (review HIGH-2 + simpler
alternative).** Ranked search caps *inside* fusion
(`src/search/fusion.rs:54`, fed with `limit` not an over-fetch ceiling), so no
full total exists. But pure-defer is the *dangerous* option: the §4.1 search
render would look identical to a complete enumeration and be over-trusted.
Instead search emits `truncated = Some(returned == limit)` **plus**
`result_total = Some(pool_size)` with `result_total_exact = Some(false)` — an
honest "≥ N, more may exist". The renderer must actively disclaim ("ranked —
showing N, total unknown"), never silently omit.

**Capability — gated (review resolved).** `result_completeness` **is gated**,
decisively: absence must be distinguishable from `false` for a safety signal,
which the ungated `_meta` precedent (`search_hint`: missing hint = no hint)
cannot express. The flag advertises "this build emits completeness where it
supports it"; a supported command with the capability present and no `truncated`
key is a producer bug, not "complete".

**Bundle (review LOW-1).** `bundle` already computes a `was_truncated` bit
(`cmd_bundle/symbol.rs`); when bundle later joins §4.2 it should feed the *same*
`vex.dev/truncated` slot, not a parallel truncation vocabulary.

**Tests.** §2a byte-identity (a command that doesn't know → no keys); `usages`
with `limit < total` → `truncated:true` + exact `result_total`; `usages` with
`limit ≥ total` → `truncated:false` (positive completeness); search → `truncated`
+ `result_total_exact:false`; capability↔emission coupling; skew: an
advertised-capability-absent envelope → §4.1 renders "unknown", never "all N".

**Sequencing.** §4.2 first (producer-side `_meta`, `usages`-scoped, low risk),
then §4.1 (consumes §4.2's `_meta` + feature-detects the capability). §4.1 is
`vex-mcp`-only; §4.2 touches the CLI `usages`/search producers + `MetaEnvelope`
(two construction sites: `build_search_meta`, `default_meta_for` — exhaustive,
compile-checked).

---

## 5. `protocol v2` — the contract batch (mechanics)

Status: **DESIGN** (2026-07-08). v2 is *scheduled for design*, not for a cut.
This section is the "write the full mechanics when v2 is scheduled"
deliverable the earlier draft deferred. No code lands from this section until
it passes architect review and the per-item readiness gates in §5.4 are met.

### 5.1 Two break classes — do not conflate them

The earlier removal manifest lumped four items, but they are **two different
kinds of break** with different vehicles:

- **Output-envelope reshape** — changes the *shape of what vex emits*
  (`ResponseEnvelope` / `Signals` / trace). A consumer parsing the response
  breaks. This is what `protocol_version` selects, and it is the *only* thing
  request-time version selection (§5.2) governs.
- **Input-argument removal** — vex stops *accepting* a deprecated request arg
  name (`name`, `names`, singular `symbol`, `filter`, `target`←`symbol`).
  This is orthogonal to the response version: an input alias is read *before*
  any envelope is produced. Version-gating inputs ("accept `name` only when
  the caller asked for v1 output") is incoherent — the caller hasn't been
  parsed yet. So input aliases are **not** part of the `protocol_version`
  contract.

Consequence: input aliases are *cheap to keep forever* (one `read_canonical_*`
branch each) and removing them strands scripted callers for no wire-shape
benefit. **Recommendation: keep input aliases indefinitely**; drop them only
on a genuine flag-day major (a hypothetical `vex 2.0` *product* release), not
as part of the response-`protocol_version` v2. Track them here but do not
gate them on `protocol_version`. The *sole* legitimate vehicle for input-alias
removal is a startup/compile-time product-major gate (a hypothetical
`VEX_PRODUCT_MAJOR` / `vex 2.0`), categorically **not** the per-request response
`protocol_version` — named here so a future reader doesn't re-propose gating
inputs on the wire version.

### 5.2 Coexistence model — request-time output-version selection

A dual-audience protocol must let the consumer **name the response version it
wants**; the producer emits that shape. Never a flag-day bump that strands
un-updated consumers.

- **Default stays `v1`.** Absent an explicit request, vex emits v1 forever
  (until a deprecation window formally closes — §5.5). "Absent ⇒ v1" is the
  input-side mirror of the "absent capability ⇒ false" rule (§1b).
- **Discovery via `capabilities`.** Add `capabilities.protocol_versions:
  ["v1","v2"]` (append-only array, §1b sub-contract) so a consumer learns v2
  exists *before* requesting it. This is the feature-detect surface; the
  `protocol_version` *string* remains a report of what was actually emitted.
- **Selection mechanism (two front doors, one core):**
  - **MCP:** read `params._meta["vex.dev/protocol_version"]` on the tool call
    — the same `_meta` channel vex already reads `traceparent` from
    (`crates/vex-mcp/src/response.rs`). Mirrors MCP's own `initialize`
    `protocolVersion` handshake (client proposes, server emits a supported
    version), so it is idiomatic for MCP consumers.
  - **CLI:** a global `--protocol <v1|v2>` flag, plus `VEX_PROTOCOL` env for
    scripts.
  - Both resolve to one `ProtocolVersion` enum handed to the producer.

- **Version parsing is an ordered enum, never a string compare.**
  `ProtocolVersion` is an ordered enum with an explicit `FromStr`: a parseable
  `vN` above the newest supported clamps *down* to the newest supported; a
  non-`vN` / garbage value falls back to `V1`; **never errors** (forward-compat).
  Do not lexically compare version strings — `"v10" < "v9"` lexically is the
  same `v1.12`-sorts-before-`v1.9` bug class the project has already hit.
- **Clamp is a downgrade — pair it with "read the emitted version."** Clamping
  a `v3` request down to `v2` only round-trips safely if older shapes are
  strict shape-ancestors of newer ones (true here: §5.1 keeps inputs orthogonal
  and v2 only *reshapes outputs*). The consumer contract is therefore: **the
  requested version and the emitted `protocol_version` may differ; always read
  the emitted value.** State this wherever v2 is advertised.

#### 5.2a Cross-process negotiation (MCP wrapper ⇄ CLI subprocess)

The MCP server does **not** emit the envelope — it spawns `vex <sub> …` as a
subprocess and lifts the CLI's stdout envelope (`crates/vex-mcp/src/response.rs`).
So the wrapper and the CLI binary are **two independently-versioned processes**
(Homebrew / self-update updates each separately), and version skew is the
*normal* case, not an edge case. Rules:

- The wrapper translates `_meta["vex.dev/protocol_version"]` into a `--protocol`
  CLI flag — but **only after feature-detecting** the callee CLI's
  `capabilities.protocol_versions` (which it already lifts). If the requested
  version is not listed, the wrapper **drops the flag** (→ CLI emits its
  default) rather than forwarding it. Forwarding `--protocol v2` to an old CLI
  that never learned the flag would clap-error → `-32000`; the "never error /
  clamp" promise lives in the *CLL* and an old CLI cannot honor it, so the
  clamp must happen in the *wrapper* for the cross-process path.
- Silent downgrade is the correct failure: wrapper strips the flag, CLI emits
  v1, `protocol_version: "v1"` reports it truthfully. The agent asked for v2
  and got v1 with no error — fine, because the consumer contract is "read the
  emitted version" (above).
- **Default-drift guard (MED-1):** when the CLI's *default* eventually flips to
  v2 (§5.5), the wrapper must pin `--protocol v1` **explicitly** for un-opted
  requests during the deprecation window — never rely on the CLI default —
  so a CLI-side default flip cannot leak v2 to agents that never opted in.
  "Absent ⇒ v1" is a promise the wrapper must actively enforce, not inherit.

### 5.3 Producer mechanics

The shape fork is a **serialization-boundary** decision, not a flag threaded
into constructors. Both the architect and rust-reviewer design passes converged
on this (do not re-litigate):

- **Two distinct types, one wrapper, manual dispatch.** v1's shape is the
  *literal, unmodified* `Signals` struct (`#[derive(Serialize)]`, untouched).
  v2's shape is a new `SignalsNested` struct **built from the same four §3.1
  sub-structs** (`StructuralSignals` / `LexicalSignals` / `SemanticSignals` /
  `PostSignals`). A wrapper enum `SignalsWire<'a> { V1(&'a Signals),
  V2(SignalsNested) }` gets a **two-line manual `Serialize`** that just
  delegates to the active arm's derived impl. No `#[serde(untagged)]` (a
  deserialize-oriented attribute, misleading on serialize-only types; buys
  nothing), no `flatten` (invariant #1), no single struct with version-reading
  `skip_serializing_if` (that *is* the drift hazard).
- **One source of truth ⇒ divergence-proof.** Because both shapes project from
  the *same* sub-structs, the §3.1 flat+nested divergence hazard is impossible
  by construction — a new channel added to the sub-structs feeds both wire
  shapes. This is why the §3.1 groundwork made v2 "a mechanical swap."
- **`ProtocolVersion` lives in `src/protocol/`** (beside `PROTOCOL_VERSION`) —
  it is a wire concept both the CLI and the `vex-mcp` crate reason about, not a
  `cli/` detail.
- **Single branch point.** Version is *resolved* once at the request boundary
  (`cmd_*` / `build_command`) and *applied* once, inside the envelope printer
  (`print_envelope` / `print_search_envelope`, plus the bespoke `capabilities`
  envelope in `cmd_trivial.rs`) — the ~3 sites that actually call
  `serde_json::to_string`. `build_signals` and the per-channel construction
  logic stay **version-agnostic**. Honest sizing: reaching those ~3 printer
  sites from `main.rs` still needs the version plumbed through the existing
  per-command context/args rather than a new bare parameter on every `cmd_*`
  signature — confirm the shared context type before implementation; it is not
  literally "3 sites, zero fan-out."
- **`protocol_version` string and shape are ONE decision, set together.** A bug
  where the shape forks to v2 but the reported string still says `"v1"` (or
  vice-versa) is the classic desync; select both from the same resolved
  `ProtocolVersion` in the same function.
- **v1 output must stay byte-identical.** The V1 arm wraps the literal existing
  types — never a re-declared "V1Signals" copy that a future edit could reorder
  (`serde_json` emits in declaration order). The §1c snapshot suite guards it;
  v2 gets its own parallel golden.
- **The `--why` trace is a *second, independent* branch point.** It rides in
  `MetaEnvelope.why_trace: Option<serde_json::Value>` — an untyped blob built in
  `src/cli/trace.rs`, so the compiler will *not* catch a v2 trace reshape
  (§5.4's `narrowing_dropped`). It needs its own version branch + dual-golden;
  do not assume "the serializer branches" covers it.
- Serialize-only structs stay serialize-only (§1a invariant #7); the wrapper's
  manual `Serialize` adds no `Deserialize` and no round-tripping.

### 5.4 v2 output contents, by readiness tier

Only Tier-B (output-reshape) items belong to `protocol_version` v2. Each needs
its expand step **before** v2 can contract it:

- **Nested `Signals` wire** — v2 emits `signals` grouped by channel
  (structural / lexical / semantic / post; the §3.1 sub-structs already model
  this internally) and drops the flat fields. `capabilities.signals_nested`
  flips when v2 can emit it. *Readiness: expand NOT started* — and §3.1
  deliberately rejected emitting flat+nested together in v1.x (divergence
  hazard), so nested is v2-native (expand+contract happen together under the
  version gate). This is the headline v2 reshape.
- **`diff_dropped` → `narrowing_dropped`** — v2 renames the residual and lets
  `diff_dropped` (if kept at all) carry *only* diff/filter_path, with
  `narrowing_dropped` carrying the combined narrowing count. *Readiness:
  expand NOT started* (`narrowing_dropped` exists only in this doc). Lower
  value than nested Signals; may be dropped from v2 scope.

`mode_legacy` (an *output* field) is the one ready-now output removal: `mode`
has been emitted alongside it since v1.8/1.9 (many minors). It can be dropped
in v2's flat→cleanup pass with no expand work outstanding.

### 5.5 Cut trigger + deprecation window (refined)

Cut v2 only when, for **the output items it will reshape**:
(a) each has completed expand→advertise→deprecate, **and**
(b) ≥ 1 minor has elapsed since that item's deprecation was announced.
Freshly-deprecated items (e.g. `filter`, deprecated v1.24.0) are simply *not
in the first v2* — they wait for their window, or ride a later reshape. v2 is
not all-or-nothing; it reshapes whatever is ready and leaves the rest on v1
semantics.

**Gate (b) is exempted for v2-native reshapes.** Nested `Signals` has *no
pre-v2 deprecation event* — flat fields are only "deprecated" at the instant v2
ships nested (§3.1 forbade a v1.x flat+nested coexistence phase). So gate (b) is
circular/vacuous for it. Resolution: v2-native reshapes satisfy gate (b) via the
*post-cut* deprecation window below (v1 stays default while consumers migrate),
not a pre-cut one. Gate (b) applies only to items that had a real v1.x expand
phase (aliases, `mode_legacy`).

After cutting: keep emitting v1 by default through a published deprecation
window (≥ N minors, N TBD at cut) before flipping the default to v2. Announce
the window in CHANGELOG + `MCP-SCHEMA.md`.

### 5.6 Testing the version fork

- **Dual goldens** — every snapshot that today pins the v1 envelope gains a
  v2 sibling. v1 golden must not move (§1c).
- **Version-matrix test** — request each of `{unset, v1, v2}` and assert the
  emitted `protocol_version` + shape; assert unset ≡ v1 byte-for-byte.
- **Capability↔version coupling** — `protocol_versions` lists exactly the
  versions the producer can emit; `signals_nested` true iff v2 emits nested.
- **Cross-process skew matrix (§5.2a)** — test the cross-product of
  `{wrapper knows v2} × {CLI supports only v1}` and `{old wrapper} × {CLI
  supports v2}`. Assert: a v2 request to a v1-only CLI → wrapper strips
  `--protocol`, CLI returns v1 **success** (never `-32000`); an old wrapper
  never sends a version and the v2-capable CLI stays on its default. Pin the
  benign reverse case so a future wrapper change can't regress it.
- **Cross-crate tolerance** — the MCP wrapper tolerates an unknown requested
  version and never errors: it forwards `--protocol` only when the callee's
  `protocol_versions` lists it, else drops it (§5.2a).

### 5.7 Prospective removal manifest (kept current)

- flat `Signals` fields — Tier B, expand not started (§5.4)
- `diff_dropped`→`narrowing_dropped` — Tier B, expand not started, maybe cut
- `UsagesTrace.mode_legacy` — output, ready now
- input arg aliases (`name`/`names`/singular `symbol`/`filter`/`target`) —
  **§5.1: keep indefinitely**, not a `protocol_version` concern

---

## 6. Capabilities — the negotiation surface

`capabilities` (`src/protocol/capabilities.rs::current()`) is how consumers
feature-detect additive changes instead of gating on `protocol_version`.
Evolution rules live in §1b; per-flag emission rule in §2 step-2.

Current flags: `signals`, `empty_reason`, `bundle_modes`, `why`,
`scope_filters`, `metadata_filters`, `auto_update`, `history_diff`,
`structured_result_kind` (v1.24.0).

Proposed additions (flip as each expand step lands):

- `structured_result_kind` — §4 `def`/`neighbor` marker in
  `structuredContent`. **SHIPPED (v1.24.0), flipped `true`.** The text-channel
  drift hint is **not** gated.
- `signals_nested` — §3.1 nested wire form. Flips at **v2**, not during
  v1.x (the v1.x work is internal-only). True iff a requested v2 envelope
  emits nested `signals` (§5.4 Tier B).
- `result_completeness` — §4.2 completeness signal
  (`_meta.vex.dev/{truncated,result_total,result_total_exact}`). **Gated
  (resolved):** absence must be distinguishable from `false` for a safety
  signal, which an ungated `_meta` field can't express. Flips in the release
  that first emits it; v1 scope is `usages` (exact) + search (lower bound);
  callers/callees/implementations deferred (§4.2).
- `concise_text` — **NOT a capability (resolved).** §4.1 straight-replaces the
  advisory `content` text; gating a text channel invites double-maintenance.
  Migration via the indefinite `VEX_MCP_TEXT=raw` env hatch, not a flag.
- `protocol_versions` — §5.2 discovery array (e.g. `["v1","v2"]`) listing the
  response versions the producer can emit on request. Append-only (§1b). Lets
  a consumer feature-detect v2 before selecting it via
  `_meta["vex.dev/protocol_version"]` / `--protocol`. Elements are the `Display`
  of `ProtocolVersion::ALL` (not free strings), and the array is order-pinned by
  a test mirroring `bundle_mode_flag_all_matches_capabilities` so the discovery
  set and the `--protocol`-accepted set cannot drift.

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
