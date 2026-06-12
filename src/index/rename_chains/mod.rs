//! v1.17+ Phase 14.10 — symbol-rename chain tracking.
//!
//! Collapses rename + move + signature-change-resilient transitions
//! across commits into a single chain so `vex history bar` returns the
//! full pre-rename + post-rename timeline.
//!
//! See `.claude/Task/PHASE14.10-symbol-rename-tracking.md` for the
//! design. Algorithm: per-commit-pair LSH candidate pruning →
//! interned-token Jaccard verification (+ optional MiniLM cosine
//! tiebreaker) → greedy 1:1 assignment per pair → serial union-find
//! merge → content-derived chain_id.
//!
//! ## Module status
//!
//! Wired: `crate::index::pipeline::output::write_rename_chains_sidecar`
//! calls [`build_rename_chains`] after a successful git_history sidecar
//! write. Several scoring + lookup helpers (`minhash::estimate_jaccard`,
//! `score::CosineLookup::{from_hashed_vectors, cosine, len}`,
//! `score::TokenInterner::unique_token_count`) are not yet consumed —
//! they land with the MiniLM-tiebreaker integration in a follow-up. The
//! module-wide `dead_code` allow keeps these scaffolded without lint
//! spam; the orchestrator drives the load-bearing surface.

#![allow(dead_code)]

pub mod lsh;
pub mod minhash;
pub mod score;
pub mod weights;

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use rayon::prelude::*;
use xxhash_rust::xxh3::xxh3_64;

use crate::index::history_builder::HistoryEntry;
use crate::store::rename_chains::{ChainTableEntry, ForwardEntry, RenameChainsArtifact};

use self::lsh::BandTable;
use self::minhash::{signature, Signature};
use self::score::{jaccard_sorted, CosineLookup, TokenInterner};
use self::weights::{
    GATE_JACCARD, GATE_LEN_RATIO, GATE_SCORE, W_BODY_NO_COS, W_BODY_WITH_COS, W_COS, W_SIG_NO_COS,
    W_SIG_WITH_COS,
};

// =====================================================================
// Public API
// =====================================================================

/// Inputs to [`build_rename_chains`]. All slices are indexed by
/// `entry_idx` (i.e. parallel to `entries`).
///
/// The caller owns the body / signature token strings. They typically
/// come from a future history-keyed body_tokens sidecar (TBD) or from
/// re-parsing blobs through the Phase 14.7 blob cache; the chain
/// builder is agnostic to that source so the algorithm can be unit-
/// tested with synthetic strings.
// `#[doc(hidden)] pub` (instead of `pub(crate)`) so `benches/rename_chains.rs`
// can drive the orchestrator from outside the crate-private layer. The
// `doc(hidden)` keeps the symbol off the crate's documented surface;
// downstream consumers should treat `RenameChainsReader` (in
// `crate::store::rename_chains`) as the supported API.
#[doc(hidden)]
pub struct BuildInput<'a> {
    /// One per HistoryEntry. The entry's `kind`, `first_commit_idx`,
    /// `last_commit_idx` drive the per-commit-pair link discovery.
    pub entries: &'a [HistoryEntry],
    /// Body-token strings keyed by entry_idx. `None` = body not
    /// available (e.g. parser couldn't extract one). Whitespace-
    /// separated, already lowercased by the extractor.
    pub entry_body_tokens: &'a [Option<String>],
    /// Signature-token strings keyed by entry_idx. `None` = no
    /// signature. Whitespace-separated.
    pub entry_sig_tokens: &'a [Option<String>],
    /// `context_hash` for each entry, when known. Keyed by entry_idx.
    /// Used to look up MiniLM vectors via [`CosineLookup`]; `None`
    /// means the entry has no embedding (e.g. blob was not in the
    /// current-tip embedding set).
    pub entry_context_hash: &'a [Option<u64>],
    /// Pre-computed body_tokens hash for the header staleness guard.
    /// Caller is free to pick the encoding so long as it is stable
    /// across rebuilds — see [`compute_body_tokens_hash`] for the
    /// default.
    pub body_tokens_hash: u64,
    /// Raw 20-byte tip SHA of the history sidecar this artifact is
    /// paired with. Used in the staleness guard.
    pub history_tip_sha_prefix: [u8; 20],
    /// MiniLM cosine lookup, when semantic embeddings are available.
    /// `None` engages the no-cosine renormalised weights.
    pub cosine_lookup: Option<&'a CosineLookup<'a>>,
}

/// Default encoding used by [`compute_body_tokens_hash`] / header
/// staleness guard. Matches the `index.bodytokens` sidecar's record
/// shape so a caller can hash the source bytes directly if they
/// already have them in this format.
///
/// Bytes per record: `[u32_le byte_len][utf-8 bytes; byte_len]`, or
/// a single `u32::MAX` sentinel for `None`.
#[doc(hidden)] pub fn compute_body_tokens_hash(records: &[Option<String>]) -> u64 {
    // Single-pass hasher would be marginally cheaper, but xxh3_64 over
    // a Vec<u8> is plenty fast for the once-per-build call site.
    let mut buf: Vec<u8> = Vec::with_capacity(records.len() * 8);
    for r in records {
        match r {
            None => buf.extend_from_slice(&u32::MAX.to_le_bytes()),
            Some(s) => {
                // `extract_body_tokens` caps at 400 bytes per symbol; a
                // value above u32::MAX would imply that cap was lifted
                // (or a malformed caller). Surface in dev rather than
                // silently truncating the recorded length and producing
                // a hash that disagrees with a re-read.
                debug_assert!(
                    s.len() <= u32::MAX as usize,
                    "body_tokens record exceeds u32::MAX bytes",
                );
                let len = u32::try_from(s.len()).unwrap_or(u32::MAX);
                buf.extend_from_slice(&len.to_le_bytes());
                buf.extend_from_slice(s.as_bytes());
            }
        }
    }
    xxh3_64(&buf)
}

/// Build a rename-chains artifact from a freshly-built `HistorySection`
/// plus per-entry token strings.
///
/// Phases:
/// 1. **Phase 0 (serial)** — intern tokens, compute MinHash signatures,
///    populate the LSH band table.
/// 2. **Phase A (rayon par_iter over commit pairs)** — for each
///    `(C, C+1)` boundary: collect `dels` (entries with
///    `last_commit_idx == C`), `adds` (entries with
///    `first_commit_idx == C+1`); query LSH for candidates; apply gates
///    (kind, length-ratio, body Jaccard, composite score); greedy 1:1
///    assignment.
/// 3. **Phase B (serial)** — union all per-pair links into a single
///    union-find.
/// 4. **Phase C (serial)** — derive `chain_id` per UF root from sorted
///    body_tokens.
/// 5. **Phase D (serial)** — emit `ForwardEntry` (only chains ≥ 2
///    members), `ChainTableEntry`, flat member list.
#[doc(hidden)] pub fn build_rename_chains(input: BuildInput<'_>) -> Result<RenameChainsArtifact> {
    if input.entries.len() != input.entry_body_tokens.len()
        || input.entries.len() != input.entry_sig_tokens.len()
        || input.entries.len() != input.entry_context_hash.len()
    {
        bail!(
            "BuildInput slice lengths disagree: entries={}, body={}, sig={}, hash={}",
            input.entries.len(),
            input.entry_body_tokens.len(),
            input.entry_sig_tokens.len(),
            input.entry_context_hash.len(),
        );
    }

    // -----------------------------------------------------------------
    // Phase 0 — serial pre-computation.
    // -----------------------------------------------------------------
    let phase0 = Phase0::build(&input);

    // -----------------------------------------------------------------
    // Phase A — rayon par_iter over commit pairs.
    // -----------------------------------------------------------------
    let commit_count = compute_commit_count(input.entries);
    let pair_count = commit_count.saturating_sub(1);

    let per_pair_links: Vec<Vec<Link>> = (0..pair_count)
        .into_par_iter()
        .map(|c| discover_links_for_pair(c as u32, (c + 1) as u32, &phase0, &input))
        .collect();

    // -----------------------------------------------------------------
    // Phase B — serial UF merge over all links (deterministic order:
    // iterate pairs ascending, links inside each pair already sorted
    // by `discover_links_for_pair`'s 1:1-assignment pass).
    // -----------------------------------------------------------------
    let mut uf = UnionFind::new(input.entries.len());
    for pair_links in &per_pair_links {
        for link in pair_links {
            uf.union(link.del as usize, link.add as usize);
        }
    }

    // -----------------------------------------------------------------
    // Phase C — chain_id derivation (content-stable).
    // -----------------------------------------------------------------
    let chain_membership = derive_chain_membership(&mut uf, input.entry_body_tokens);

    // -----------------------------------------------------------------
    // Phase D — emit the sidecar artifact.
    // -----------------------------------------------------------------
    let scores_by_entry = collect_scores(&per_pair_links);
    let artifact = emit_artifact(
        chain_membership,
        scores_by_entry,
        input.body_tokens_hash,
        input.history_tip_sha_prefix,
    );

    Ok(artifact)
}

// =====================================================================
// Phase 0 — serial pre-computation
// =====================================================================

struct Phase0 {
    /// `body_tokens[entry_idx]`: interned-and-sorted ids for the body.
    body_tokens: Vec<Vec<u32>>,
    /// `sig_tokens[entry_idx]`: interned-and-sorted ids for the
    /// signature.
    sig_tokens: Vec<Vec<u32>>,
    /// `body_len[entry_idx]`: byte-length of the original body string
    /// before tokenisation. Used by the `GATE_LEN_RATIO` filter.
    body_len: Vec<u32>,
    /// `minhash_sig[entry_idx]`: MinHash signature over body_tokens.
    /// Empty entries (no body) still have a sentinel all-MAX
    /// signature.
    minhash_sig: Vec<Signature>,
    /// LSH band table, populated with signatures from entries that
    /// have non-empty body tokens. Entries with no body don't
    /// participate in LSH candidate discovery.
    lsh: BandTable,
}

impl Phase0 {
    fn build(input: &BuildInput<'_>) -> Self {
        let n = input.entries.len();
        let mut interner = TokenInterner::new();

        let mut body_tokens: Vec<Vec<u32>> = Vec::with_capacity(n);
        let mut sig_tokens: Vec<Vec<u32>> = Vec::with_capacity(n);
        let mut body_len: Vec<u32> = Vec::with_capacity(n);
        let mut minhash_sig: Vec<Signature> = Vec::with_capacity(n);
        let mut lsh = BandTable::new();

        for i in 0..n {
            let body_str = input.entry_body_tokens[i].as_deref();
            let sig_str = input.entry_sig_tokens[i].as_deref();

            body_tokens.push(interner.tokenise(body_str));
            sig_tokens.push(interner.tokenise(sig_str));
            body_len.push(body_str.map(|s| s.len() as u32).unwrap_or(0));

            // Build MinHash sig over the raw body tokens (not the
            // interned ids — the LSH layer is content-addressed and
            // must be reproducible across runs without depending on
            // interning order).
            let tokens: Vec<&str> = body_str
                .map(|s| s.split_whitespace().collect())
                .unwrap_or_default();
            let sig = signature(&tokens);

            // Only insert non-empty bodies into the LSH table.
            // Empty-body entries would all share the sentinel
            // all-MAX signature and cause O(N²) blow-up in candidate
            // discovery.
            if !tokens.is_empty() {
                lsh.insert(i as u32, &sig);
            }
            minhash_sig.push(sig);
        }

        Self {
            body_tokens,
            sig_tokens,
            body_len,
            minhash_sig,
            lsh,
        }
    }
}

// =====================================================================
// Phase A — per-commit-pair link discovery
// =====================================================================

#[derive(Debug, Clone, Copy)]
struct Link {
    /// `entry_idx` of the deletion side (last_commit_idx == c).
    del: u32,
    /// `entry_idx` of the addition side (first_commit_idx == c + 1).
    add: u32,
    /// Composite score; `score >= GATE_SCORE` by construction.
    score: f32,
}

fn discover_links_for_pair(
    c: u32,
    c_next: u32,
    phase0: &Phase0,
    input: &BuildInput<'_>,
) -> Vec<Link> {
    // Collect deletion and addition entry indices for this commit pair.
    let dels: Vec<u32> = input
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.last_commit_idx == c)
        .map(|(i, _)| i as u32)
        .collect();
    let adds_set: HashSet<u32> = input
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.first_commit_idx == c_next)
        .map(|(i, _)| i as u32)
        .collect();

    if dels.is_empty() || adds_set.is_empty() {
        return Vec::new();
    }

    let mut links: Vec<Link> = Vec::new();
    for del_idx in &dels {
        // LSH candidates for the deletion-side signature, restricted
        // to the addition-side set at this commit boundary.
        let cands = phase0
            .lsh
            .candidates(&phase0.minhash_sig[*del_idx as usize]);
        for cand_idx in cands {
            if !adds_set.contains(&cand_idx) {
                continue;
            }
            if let Some(link) = score_pair(*del_idx, cand_idx, phase0, input) {
                links.push(link);
            }
        }
    }

    // Greedy 1:1 assignment per commit pair, deterministic tie-break.
    // Order: score desc, del.entry_idx asc, add.entry_idx asc.
    links.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.del.cmp(&b.del))
            .then(a.add.cmp(&b.add))
    });

    let mut used_del: HashSet<u32> = HashSet::new();
    let mut used_add: HashSet<u32> = HashSet::new();
    links.retain(|l| {
        if used_del.contains(&l.del) || used_add.contains(&l.add) {
            false
        } else {
            used_del.insert(l.del);
            used_add.insert(l.add);
            true
        }
    });

    links
}

fn score_pair(del_idx: u32, add_idx: u32, phase0: &Phase0, input: &BuildInput<'_>) -> Option<Link> {
    let del = &input.entries[del_idx as usize];
    let add = &input.entries[add_idx as usize];

    // Gate 0: defence-in-depth — empty body on either side. The LSH
    // layer in Phase 0 already skips inserts for entries with no
    // body tokens (so this branch is unreachable from the LSH-driven
    // candidate stream), and the length-ratio gate would also bail
    // on `hi == 0`. But `jaccard_sorted(&[], &[])` returns 1.0 by
    // contract (two empty sets are vacuously identical), which
    // would pass GATE_JACCARD. Guard the pair-scoring path directly
    // so a future LSH-bypass for any reason can't sneak empty-body
    // entries through as a "perfect match".
    if phase0.body_tokens[del_idx as usize].is_empty()
        || phase0.body_tokens[add_idx as usize].is_empty()
    {
        return None;
    }

    // Gate 1: kind must match (function ↔ function, class ↔ class).
    // Cheapest filter, applied first.
    if del.kind != add.kind {
        return None;
    }

    // Gate 2: length ratio. RefactoringMiner 3.0's primary fix against
    // extract-method false positives.
    let ld = phase0.body_len[del_idx as usize];
    let lc = phase0.body_len[add_idx as usize];
    let (lo, hi) = if ld < lc { (ld, lc) } else { (lc, ld) };
    if hi == 0 {
        return None;
    }
    let ratio = lo as f32 / hi as f32;
    if ratio < GATE_LEN_RATIO {
        return None;
    }

    // Gate 3: body Jaccard, exact (not estimated). SourcererCC's
    // empirical Type-2/3 optimum.
    let j_body = jaccard_sorted(
        &phase0.body_tokens[del_idx as usize],
        &phase0.body_tokens[add_idx as usize],
    );
    if j_body < GATE_JACCARD {
        return None;
    }

    // Signature Jaccard — used in composite score only.
    let j_sig = jaccard_sorted(
        &phase0.sig_tokens[del_idx as usize],
        &phase0.sig_tokens[add_idx as usize],
    );

    // Composite score — branch on cosine availability.
    let score = match input.cosine_lookup {
        Some(cos) => {
            let cos_value = match (
                input.entry_context_hash[del_idx as usize],
                input.entry_context_hash[add_idx as usize],
            ) {
                (Some(h_a), Some(h_b)) => cos.cosine(h_a, h_b).max(0.0),
                _ => 0.0,
            };
            W_BODY_WITH_COS * j_body + W_SIG_WITH_COS * j_sig + W_COS * cos_value
        }
        None => W_BODY_NO_COS * j_body + W_SIG_NO_COS * j_sig,
    };

    if score < GATE_SCORE {
        return None;
    }

    Some(Link {
        del: del_idx,
        add: add_idx,
        score,
    })
}

fn compute_commit_count(entries: &[HistoryEntry]) -> usize {
    // The maximum commit_idx referenced by any entry, plus 1. If the
    // section is empty there are no commit pairs to walk.
    let max = entries
        .iter()
        .map(|e| e.last_commit_idx.max(e.first_commit_idx))
        .max()
        .map(|m| m as usize + 1)
        .unwrap_or(0);
    max
}

// =====================================================================
// Phase B — union-find
// =====================================================================

/// Path-compressed union-find. Allocated once per build, dropped at
/// the end of Phase C. Size ≈ N × 4 B which is trivial vs the
/// MinHash sigs.
struct UnionFind {
    parent: Vec<u32>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n as u32).collect(),
        }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] as usize != root {
            root = self.parent[root] as usize;
        }
        // Path compression.
        let mut cur = x;
        while self.parent[cur] as usize != root {
            let next = self.parent[cur] as usize;
            self.parent[cur] = root as u32;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        // Stable tie-break: lower-index root wins. Keeps `chain_id`
        // anchor deterministic across runs.
        if ra < rb {
            self.parent[rb] = ra as u32;
        } else {
            self.parent[ra] = rb as u32;
        }
    }
}

// =====================================================================
// Phase C — chain_id derivation
// =====================================================================

struct ChainMembership {
    /// `chain_id -> sorted Vec<entry_idx>`. Only chains with ≥ 2
    /// members are present.
    by_chain: Vec<(u64, Vec<u32>)>,
}

fn derive_chain_membership(
    uf: &mut UnionFind,
    entry_body_tokens: &[Option<String>],
) -> ChainMembership {
    // Group entries by UF root.
    let n = entry_body_tokens.len();
    let mut root_to_entries: HashMap<u32, Vec<u32>> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i) as u32;
        root_to_entries.entry(root).or_default().push(i as u32);
    }

    // For each root with ≥ 2 members, derive chain_id from sorted
    // body_tokens of the root entry. Singletons are dropped — they
    // produce no sidecar entry, and `follow_chain` returns just
    // `[entry_idx]` for them.
    let mut chains: Vec<(u64, Vec<u32>)> = root_to_entries
        .into_iter()
        .filter(|(_, members)| members.len() >= 2)
        .map(|(root, mut members)| {
            // Sorted member list (ascending entry_idx) — pins the
            // on-disk layout per chain.
            members.sort_unstable();
            let chain_id = chain_id_from_root(root, entry_body_tokens);
            (chain_id, members)
        })
        .collect();

    // Deterministic order: sort by chain_id. Collisions get a
    // secondary tie-break by min-member entry_idx, which is the
    // only other stable signal.
    chains.sort_by(|a, b| a.0.cmp(&b.0).then(a.1[0].cmp(&b.1[0])));

    // Dedup colliding chain_ids (extremely rare — xxh3_64 collision
    // probability over 5M chains is ≈ 5M²/2⁶⁴ ≈ 6×10⁻⁸). Keep the
    // chain whose first member has the lower entry_idx; merge the
    // other chain's members in, re-sort, and continue. This keeps
    // the binary-search invariant `chains[].chain_id strictly
    // ascending` from the writer's `validate_artifact` check.
    let mut deduped: Vec<(u64, Vec<u32>)> = Vec::with_capacity(chains.len());
    for (chain_id, members) in chains {
        if let Some(last) = deduped.last_mut() {
            if last.0 == chain_id {
                last.1.extend(members);
                last.1.sort_unstable();
                last.1.dedup();
                continue;
            }
        }
        deduped.push((chain_id, members));
    }

    ChainMembership { by_chain: deduped }
}

fn chain_id_from_root(root: u32, entry_body_tokens: &[Option<String>]) -> u64 {
    let body = entry_body_tokens
        .get(root as usize)
        .and_then(|b| b.as_deref())
        .unwrap_or("");
    let mut tokens: Vec<&str> = body.split_whitespace().collect();
    tokens.sort_unstable();
    // Joined-with-space keeps the byte representation simple; the
    // exact encoding is sidecar-internal so cross-tool interop is not
    // a concern. Empty body → empty string → xxh3_64 of empty has a
    // well-defined value; chains of all-empty-body entries collide
    // intentionally (they are byte-identical bodies, by definition
    // "the same code").
    xxh3_64(tokens.join(" ").as_bytes())
}

// =====================================================================
// Phase D — artifact emission
// =====================================================================

fn collect_scores(per_pair_links: &[Vec<Link>]) -> HashMap<u32, f32> {
    // An entry can appear in multiple pair-windows when its rename
    // chain spans more than one commit boundary. Keep the maximum
    // score across all boundaries for the surfaced ForwardEntry
    // value — that's the "strongest evidence" the chain rests on
    // and is what the v1 sidecar reports.
    let mut by_entry: HashMap<u32, f32> = HashMap::new();
    for pair in per_pair_links {
        for link in pair {
            for ent in [link.del, link.add] {
                let prev = by_entry.get(&ent).copied().unwrap_or(0.0);
                if link.score > prev {
                    by_entry.insert(ent, link.score);
                }
            }
        }
    }
    by_entry
}

fn emit_artifact(
    chain_membership: ChainMembership,
    scores_by_entry: HashMap<u32, f32>,
    body_tokens_hash: u64,
    history_tip_sha_prefix: [u8; 20],
) -> RenameChainsArtifact {
    let mut chains: Vec<ChainTableEntry> = Vec::with_capacity(chain_membership.by_chain.len());
    let mut members: Vec<u32> = Vec::new();
    let mut forward_entries: Vec<ForwardEntry> = Vec::new();

    for (chain_id, member_list) in &chain_membership.by_chain {
        let member_offset = members.len() as u32;
        let member_count = member_list.len() as u32;
        chains.push(ChainTableEntry {
            chain_id: *chain_id,
            member_offset,
            member_count,
        });
        members.extend_from_slice(member_list);

        for entry_idx in member_list {
            let score = scores_by_entry.get(entry_idx).copied().unwrap_or(0.0);
            forward_entries.push(ForwardEntry {
                entry_idx: *entry_idx,
                score,
                chain_id: *chain_id,
            });
        }
    }

    // The writer's `validate_artifact` requires forward[] strictly
    // ascending by entry_idx. The chain loop appends in member-list
    // order (ascending within a chain) but two chains may interleave;
    // sort once at the end. With the dedup pass in Phase C, all
    // entry_idx values are unique so the strict-ascending invariant
    // holds.
    forward_entries.sort_by_key(|fe| fe.entry_idx);

    RenameChainsArtifact {
        forward: forward_entries,
        chains,
        members,
        body_tokens_hash,
        history_tip_sha_prefix,
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: u8, first: u32, last: u32) -> HistoryEntry {
        HistoryEntry {
            blob_idx: 0,
            file_offset: 0,
            line: 0,
            signature_offset: 0,
            first_commit_idx: first,
            last_commit_idx: last,
            kind,
            _pad: [0; 3],
        }
    }

    fn ws(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn empty_input_produces_empty_artifact() {
        let input = BuildInput {
            entries: &[],
            entry_body_tokens: &[],
            entry_sig_tokens: &[],
            entry_context_hash: &[],
            body_tokens_hash: 0,
            history_tip_sha_prefix: [0; 20],
            cosine_lookup: None,
        };
        let artifact = build_rename_chains(input).unwrap();
        assert!(artifact.forward.is_empty());
        assert!(artifact.chains.is_empty());
        assert!(artifact.members.is_empty());
    }

    #[test]
    fn mismatched_input_lengths_bail() {
        let entries = vec![entry(1, 0, 0)];
        let bodies = vec![ws("a b c")];
        let sigs: Vec<Option<String>> = vec![];
        let hashes = vec![None];
        let res = build_rename_chains(BuildInput {
            entries: &entries,
            entry_body_tokens: &bodies,
            entry_sig_tokens: &sigs,
            entry_context_hash: &hashes,
            body_tokens_hash: 0,
            history_tip_sha_prefix: [0; 20],
            cosine_lookup: None,
        });
        assert!(res.is_err());
    }

    #[test]
    fn single_entry_is_singleton_no_chain() {
        // One entry can't be in a rename chain. Artifact must be
        // empty (singletons never get a ForwardEntry).
        let entries = vec![entry(1, 0, 0)];
        let bodies = vec![ws("body of entry zero")];
        let sigs = vec![ws("fn foo()")];
        let hashes: Vec<Option<u64>> = vec![None];

        let artifact = build_rename_chains(BuildInput {
            entries: &entries,
            entry_body_tokens: &bodies,
            entry_sig_tokens: &sigs,
            entry_context_hash: &hashes,
            body_tokens_hash: 42,
            history_tip_sha_prefix: [9; 20],
            cosine_lookup: None,
        })
        .unwrap();

        assert!(artifact.forward.is_empty());
        assert!(artifact.chains.is_empty());
        assert!(artifact.members.is_empty());
        assert_eq!(artifact.body_tokens_hash, 42);
        assert_eq!(artifact.history_tip_sha_prefix, [9; 20]);
    }

    #[test]
    fn pair_with_identical_body_links_into_chain() {
        // Entry 0 disappears at commit 0; entry 1 appears at commit 1
        // with the SAME body and signature, SAME kind. Must produce
        // a single chain with both members.
        let body = "let x = compute_total amount return x finalise";
        let sig = "fn compute_total amount i32";
        let entries = vec![entry(1, 0, 0), entry(1, 1, 1)];
        let bodies = vec![ws(body), ws(body)];
        let sigs = vec![ws(sig), ws(sig)];
        let hashes: Vec<Option<u64>> = vec![None, None];

        let artifact = build_rename_chains(BuildInput {
            entries: &entries,
            entry_body_tokens: &bodies,
            entry_sig_tokens: &sigs,
            entry_context_hash: &hashes,
            body_tokens_hash: 0,
            history_tip_sha_prefix: [0; 20],
            cosine_lookup: None,
        })
        .unwrap();

        assert_eq!(artifact.chains.len(), 1, "expected exactly one chain");
        assert_eq!(artifact.chains[0].member_count, 2);
        assert_eq!(artifact.members, vec![0, 1]);
        assert_eq!(artifact.forward.len(), 2);
        // forward[] sorted ascending by entry_idx.
        assert_eq!(artifact.forward[0].entry_idx, 0);
        assert_eq!(artifact.forward[1].entry_idx, 1);
        // Both forward entries point at the same chain_id as in the
        // chains table.
        assert_eq!(artifact.forward[0].chain_id, artifact.chains[0].chain_id);
        assert_eq!(artifact.forward[1].chain_id, artifact.chains[0].chain_id);
        // Perfect body match clears the score gate.
        assert!(artifact.forward[0].score >= GATE_SCORE);
    }

    #[test]
    fn kind_mismatch_blocks_chain() {
        // Same body, different kinds → no link. Catches a
        // function-renamed-to-class style false positive.
        let body = "common body tokens shared between candidates";
        let entries = vec![
            entry(1, 0, 0), // function
            entry(2, 1, 1), // class
        ];
        let bodies = vec![ws(body), ws(body)];
        let sigs = vec![ws("sig0"), ws("sig1")];
        let hashes: Vec<Option<u64>> = vec![None, None];

        let artifact = build_rename_chains(BuildInput {
            entries: &entries,
            entry_body_tokens: &bodies,
            entry_sig_tokens: &sigs,
            entry_context_hash: &hashes,
            body_tokens_hash: 0,
            history_tip_sha_prefix: [0; 20],
            cosine_lookup: None,
        })
        .unwrap();

        assert!(artifact.chains.is_empty(), "kind mismatch should not chain");
    }

    #[test]
    fn extract_method_length_ratio_blocks_chain() {
        // Donor body ≈ 100 chars, candidate body ≈ 20 chars. Length
        // ratio 0.2 < GATE_LEN_RATIO 0.60 → no chain. This is the
        // RefactoringMiner 3.0 false-positive fix.
        let long_body = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho";
        let short_body = "alpha beta gamma";

        let entries = vec![entry(1, 0, 0), entry(1, 1, 1)];
        let bodies = vec![ws(long_body), ws(short_body)];
        let sigs = vec![ws("sig"), ws("sig")];
        let hashes: Vec<Option<u64>> = vec![None, None];

        let artifact = build_rename_chains(BuildInput {
            entries: &entries,
            entry_body_tokens: &bodies,
            entry_sig_tokens: &sigs,
            entry_context_hash: &hashes,
            body_tokens_hash: 0,
            history_tip_sha_prefix: [0; 20],
            cosine_lookup: None,
        })
        .unwrap();

        assert!(
            artifact.chains.is_empty(),
            "length-ratio gate must reject extract-method case"
        );
    }

    #[test]
    fn low_jaccard_blocks_chain() {
        // Identical kinds, identical lengths, but the bodies share
        // only ~2 of ~8 tokens → Jaccard < GATE_JACCARD 0.70 → reject.
        let body_a = "alpha beta gamma delta epsilon zeta eta theta";
        let body_b = "alpha beta one two three four five six";

        let entries = vec![entry(1, 0, 0), entry(1, 1, 1)];
        let bodies = vec![ws(body_a), ws(body_b)];
        let sigs = vec![ws("sig"), ws("sig")];
        let hashes: Vec<Option<u64>> = vec![None, None];

        let artifact = build_rename_chains(BuildInput {
            entries: &entries,
            entry_body_tokens: &bodies,
            entry_sig_tokens: &sigs,
            entry_context_hash: &hashes,
            body_tokens_hash: 0,
            history_tip_sha_prefix: [0; 20],
            cosine_lookup: None,
        })
        .unwrap();

        assert!(
            artifact.chains.is_empty(),
            "low Jaccard should not produce a chain"
        );
    }

    #[test]
    fn three_commit_chain_collapses_via_union_find() {
        // foo @ commit 0 → bar @ commit 1 → baz @ commit 2.
        // Entry 0 spans (0, 0), entry 1 spans (1, 1), entry 2 spans
        // (2, 2). Bodies are byte-identical to force perfect
        // Jaccard at both commit boundaries. UF must merge all three
        // into one chain.
        let body = "let acc = 0 for x in input acc = acc + x return acc";
        let entries = vec![entry(1, 0, 0), entry(1, 1, 1), entry(1, 2, 2)];
        let bodies = vec![ws(body), ws(body), ws(body)];
        let sigs = vec![ws("fn foo"), ws("fn bar"), ws("fn baz")];
        let hashes: Vec<Option<u64>> = vec![None, None, None];

        let artifact = build_rename_chains(BuildInput {
            entries: &entries,
            entry_body_tokens: &bodies,
            entry_sig_tokens: &sigs,
            entry_context_hash: &hashes,
            body_tokens_hash: 0,
            history_tip_sha_prefix: [0; 20],
            cosine_lookup: None,
        })
        .unwrap();

        assert_eq!(artifact.chains.len(), 1, "all three entries → one chain");
        assert_eq!(artifact.chains[0].member_count, 3);
        assert_eq!(artifact.members, vec![0, 1, 2]);
    }

    #[test]
    fn deterministic_artifact_across_runs() {
        // Same input must produce byte-identical artifact across
        // runs. Catches regressions that re-introduce HashMap
        // iteration order or rayon thread-id leak into the result.
        let body_a = "tokens for entry zero appearing in commit zero only";
        let body_b = "tokens for entry zero appearing in commit zero only";
        let entries = vec![entry(1, 0, 0), entry(1, 1, 1)];
        let bodies = vec![ws(body_a), ws(body_b)];
        let sigs = vec![ws("sig"), ws("sig")];
        let hashes: Vec<Option<u64>> = vec![None, None];

        let make_input = || BuildInput {
            entries: &entries,
            entry_body_tokens: &bodies,
            entry_sig_tokens: &sigs,
            entry_context_hash: &hashes,
            body_tokens_hash: 0,
            history_tip_sha_prefix: [0; 20],
            cosine_lookup: None,
        };

        let a = build_rename_chains(make_input()).unwrap();
        let b = build_rename_chains(make_input()).unwrap();
        assert_eq!(a.chains.len(), b.chains.len());
        for (ca, cb) in a.chains.iter().zip(b.chains.iter()) {
            assert_eq!(ca.chain_id, cb.chain_id);
            assert_eq!(ca.member_offset, cb.member_offset);
            assert_eq!(ca.member_count, cb.member_count);
        }
        assert_eq!(a.members, b.members);
        for (fa, fb) in a.forward.iter().zip(b.forward.iter()) {
            assert_eq!(fa.entry_idx, fb.entry_idx);
            assert_eq!(fa.chain_id, fb.chain_id);
            assert!((fa.score - fb.score).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn forward_entries_strictly_ascending_by_entry_idx() {
        // Build a multi-chain case interleaving entry indices so the
        // unsorted concat would violate the writer's invariant.
        // Chain 1: entries 0, 3 (identical bodies "alpha alpha alpha
        // alpha alpha alpha alpha"). Chain 2: entries 1, 2 (identical
        // bodies "beta beta beta beta beta beta beta").
        let body_alpha = "alpha alpha alpha alpha alpha alpha alpha";
        let body_beta = "beta beta beta beta beta beta beta";
        let entries = vec![
            entry(1, 0, 0), // 0 disappears at commit 0
            entry(2, 0, 0), // 1 disappears at commit 0
            entry(2, 1, 1), // 2 appears at commit 1 (beta)
            entry(1, 1, 1), // 3 appears at commit 1 (alpha)
        ];
        let bodies = vec![ws(body_alpha), ws(body_beta), ws(body_beta), ws(body_alpha)];
        let sigs = vec![ws("s1"), ws("s2"), ws("s2"), ws("s1")];
        let hashes: Vec<Option<u64>> = vec![None, None, None, None];

        let artifact = build_rename_chains(BuildInput {
            entries: &entries,
            entry_body_tokens: &bodies,
            entry_sig_tokens: &sigs,
            entry_context_hash: &hashes,
            body_tokens_hash: 0,
            history_tip_sha_prefix: [0; 20],
            cosine_lookup: None,
        })
        .unwrap();

        assert_eq!(artifact.chains.len(), 2);
        // forward[] must be strictly ascending by entry_idx (writer's
        // validate_artifact requires this for binary search).
        for w in artifact.forward.windows(2) {
            assert!(
                w[0].entry_idx < w[1].entry_idx,
                "forward[] not strictly ascending: {} >= {}",
                w[0].entry_idx,
                w[1].entry_idx,
            );
        }
        // chains[] must be strictly ascending by chain_id.
        for w in artifact.chains.windows(2) {
            assert!(
                w[0].chain_id < w[1].chain_id,
                "chains[] not strictly ascending: {:#x} >= {:#x}",
                w[0].chain_id,
                w[1].chain_id,
            );
        }
    }

    #[test]
    fn artifact_round_trips_through_writer() {
        // End-to-end: build a chain, write to disk, open the
        // sidecar, verify the chain is queryable. Catches any drift
        // between the builder's emission order and the writer's
        // validation rules.
        use crate::store::rename_chains::{open, save};

        let body = "perfect match body tokens that exceed the length ratio gate easily";
        let entries = vec![entry(1, 0, 0), entry(1, 1, 1)];
        let bodies = vec![ws(body), ws(body)];
        let sigs = vec![ws("sig"), ws("sig")];
        let hashes: Vec<Option<u64>> = vec![None, None];

        let bt_hash = compute_body_tokens_hash(&bodies);
        let tip_sha = [7u8; 20];

        let artifact = build_rename_chains(BuildInput {
            entries: &entries,
            entry_body_tokens: &bodies,
            entry_sig_tokens: &sigs,
            entry_context_hash: &hashes,
            body_tokens_hash: bt_hash,
            history_tip_sha_prefix: tip_sha,
            cosine_lookup: None,
        })
        .unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("index.rename_chains");
        save(&path, &artifact).unwrap();

        let reader = open(tmp.path(), bt_hash, &tip_sha).unwrap().unwrap();
        assert_eq!(reader.chain_count(), 1);
        assert_eq!(reader.forward_count(), 2);
        assert_eq!(reader.member_count(), 2);
        let chain_id = reader.chain_id_for_entry(0).unwrap();
        let members = reader.members_of(chain_id).unwrap();
        assert_eq!(members, &[0u32, 1u32]);
        assert_eq!(reader.follow_chain(0), vec![0, 1]);
        // Singleton entry not in chain → follow returns just self.
        assert_eq!(reader.follow_chain(99), vec![99]);
    }

    #[test]
    fn body_tokens_hash_helper_is_deterministic() {
        let records = vec![ws("foo bar"), None, ws("")];
        let a = compute_body_tokens_hash(&records);
        let b = compute_body_tokens_hash(&records);
        assert_eq!(a, b);
        // Changing the records changes the hash.
        let other = vec![ws("foo bar"), None, ws("different")];
        let c = compute_body_tokens_hash(&other);
        assert_ne!(a, c);
    }

    #[test]
    fn empty_body_entries_dont_lsh_collide() {
        // Two entries with empty bodies (None and empty string) at
        // distinct commit boundaries must NOT chain together (they
        // never reach the gate stack because LSH skips empty
        // signatures). Pins the Phase 0 empty-body skip.
        let entries = vec![entry(1, 0, 0), entry(1, 1, 1)];
        let bodies = vec![None, ws("")];
        let sigs = vec![ws("sig"), ws("sig")];
        let hashes: Vec<Option<u64>> = vec![None, None];

        let artifact = build_rename_chains(BuildInput {
            entries: &entries,
            entry_body_tokens: &bodies,
            entry_sig_tokens: &sigs,
            entry_context_hash: &hashes,
            body_tokens_hash: 0,
            history_tip_sha_prefix: [0; 20],
            cosine_lookup: None,
        })
        .unwrap();

        assert!(
            artifact.chains.is_empty(),
            "empty bodies must not produce a chain"
        );
    }
}
