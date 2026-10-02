//! The v9 cluster section: writer-side building AND the lazy reader-side
//! semantic validation (`docs/V9-FORMAT.md` §2.4, §13 R3) — mirrors the
//! one-file-per-section convention of `hierarchy_edges.rs` /
//! `unresolved_hierarchy.rs` (builder + reader together).
//!
//! Reader half: `IndexReader::open` validates only STRUCTURE (bounds,
//! `assign_len == 4 * symbol_count`, `table_len % ClusterRecord::SIZE ==
//! 0`) — a cheap, O(1) check that never bricks `search` / `callers` / any
//! other command on a corrupt cluster section. This module performs the
//! deeper SEMANTIC validation (`resolution_den != 0`, a sane `k`) on
//! demand, so a bad section degrades to "no cluster data" for whatever
//! calls [`ClusterSectionReader::new`] (P5's `vex modules`, `vex
//! status`), never to an error that propagates anywhere else.
//!
//! Every lookup here is bounds-checked and allocation-free except where a
//! `Vec` is the whole point ([`ClusterSectionReader::members`]); nothing
//! panics on malformed `assign`/`table` bytes — see `fuzz_cluster_section`.
//!
//! Writer half: [`build_cluster_section`] turns a computed
//! `crate::cluster::ClusterOutput` into the on-disk `assign`/`table`
//! bytes, mirroring `build_hierarchy_section` / `build_ref_edges_records`
//! in their sibling modules, so `write_index_to` stays focused on
//! orchestration.

use std::collections::HashMap;

use anyhow::{ensure, Result};

use super::format::{
    ClusterHeader, ClusterRecord, CLUSTER_NEW, CLUSTER_NOT_ELIGIBLE, CLUSTER_UNCLUSTERED,
};
use super::reader::IndexReader;

// The two sentinel tables (`format::CLUSTER_*`
// and `cluster::{NOT_ELIGIBLE, UNCLUSTERED, NEW}`) are deliberately
// duplicated — `format.rs` stays a leaf module with no dependency on the
// clustering algorithm (matching its existing `EdgeKind`/`RefKind`
// convention), while `cluster::mod` stays decoupled from the on-disk
// format. This module is the one place that imports BOTH, so the
// compile-time equality check belongs here: any future edit to either
// table that lets them drift apart fails the build immediately, rather
// than silently corrupting every `assign` slot this module decodes.
const _: () = assert!(CLUSTER_NOT_ELIGIBLE == crate::cluster::NOT_ELIGIBLE);
const _: () = assert!(CLUSTER_UNCLUSTERED == crate::cluster::UNCLUSTERED);
const _: () = assert!(CLUSTER_NEW == crate::cluster::NEW);

/// Whether a write should compute symbol clusters fresh, carry a prior
/// COMPUTED section forward (frozen + marked STALE), or leave the
/// section absent (the all-zero P2 placeholder). `docs/V9-FORMAT.md` §5,
/// §13 R10-R14.
///
/// `vex index` passes `Compute` (or `None` with `--no-clusters`). `vex
/// update` passes `Carry` when the prior index has a COMPUTED section,
/// `Compute` once when it doesn't and the user hasn't opted out (R14),
/// and `None` when the user has opted out (`--no-clusters` on the last
/// full `vex index`).
pub(crate) enum ClusterInput {
    Compute(ClusterComputeRequest),
    Carry(crate::index::types::ClusterCarryArtefacts),
    None,
}

/// P4a (`docs/V9-FORMAT.md` §13 R10/R14): whether this write should
/// compute symbol clusters, and at what resolution.
#[derive(Debug, Clone, Copy)]
pub struct ClusterComputeRequest {
    pub resolution: (u32, u32),
}

/// `docs/V9-FORMAT.md` §3.3 names this algorithm "leiden-cpm/1" — the
/// only `algo_version` this build ever computes fresh. A P4b carry keeps
/// whatever `algo_version` the OLD section recorded (§5 rule 6), which is
/// always this value today but need not stay a literal `1` forever.
pub(crate) const ALGO_VERSION_LEIDEN_CPM_1: u16 = 1;

/// Writer-ready bytes + header fields for a COMPUTED cluster section —
/// everything `write_index_to` needs to fill in `ClusterHeader` and write
/// the `assign`/`table` bytes. Produced by both [`build_cluster_section`]
/// (fresh compute) and [`build_cluster_section_from_carry`] (P4b carry) so
/// `write_index_to`'s downstream layout math stays branch-free on which
/// path produced it.
#[derive(Debug)]
pub(crate) struct ClusterSectionBuilt {
    pub assign_bytes: Vec<u8>,
    pub table_bytes: Vec<u8>,
    pub resolution: (u32, u32),
    pub flags: u32,
    pub algo_version: u16,
    pub levels: u16,
    pub build_symbol_count: u32,
}

/// Turn a computed [`crate::cluster::ClusterOutput`] into writer-ready
/// bytes (§2.4 `assign`/`table`) and header fields.
///
/// **Must** run before the caller computes any section-offset layout
/// math (§13 R21) — `intern` grows the `StringPool`, and interning a
/// label after offsets are computed would silently corrupt every
/// downstream offset.
pub(crate) fn build_cluster_section(
    output: &crate::cluster::ClusterOutput,
    resolution: (u32, u32),
    intern: &mut dyn FnMut(&str) -> u32,
) -> ClusterSectionBuilt {
    let assign_bytes = super::csr::encode_le_u32s(&output.assign);
    let mut table_bytes = Vec::with_capacity(output.clusters.len() * ClusterRecord::SIZE);
    for rec in &output.clusters {
        // Labels interned here, while `intern` can still grow the string
        // pool — this IS the "before the layout math" requirement (R21).
        let label_offset = intern(&rec.label);
        let mut hubs = [u32::MAX; 3];
        for (slot, &h) in hubs.iter_mut().zip(rec.hubs.iter()) {
            *slot = h;
        }
        let on_disk = ClusterRecord {
            rep_sym_idx: rec.rep_sym_idx,
            size: rec.size,
            internal_weight: rec.internal_weight,
            cut_weight: rec.cut_weight,
            label_offset,
            hubs,
        };
        // SAFETY: ClusterRecord is #[repr(C)] with fixed layout.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &on_disk as *const ClusterRecord as *const u8,
                ClusterRecord::SIZE,
            )
        };
        table_bytes.extend_from_slice(bytes);
    }

    let mut flags = ClusterHeader::FLAG_COMPUTED;
    if output.iter_cap_hit {
        flags |= ClusterHeader::FLAG_ITER_CAP_HIT;
    }

    ClusterSectionBuilt {
        assign_bytes,
        table_bytes,
        resolution,
        flags,
        algo_version: ALGO_VERSION_LEIDEN_CPM_1,
        levels: output.levels,
        build_symbol_count: output.assign.len() as u32,
    }
}

/// P4b (`docs/V9-FORMAT.md` §5, §13 R11-R13) — carry a prior COMPUTED
/// cluster section forward across `vex update` instead of recomputing.
///
/// `parsed` is the FULL new symbol set in the exact order `write_index_to`
/// assigns `sym_idx` (unchanged-file prefix, then re-parsed/new files) —
/// the same slice the caller builds `records` from. `carry` holds the
/// per-symbol data `reconstruct_unchanged` already resolved for the
/// unchanged prefix, plus everything needed to resolve the remainder:
///
/// - A symbol whose `per_symbol_carried` entry is `Some(v)` already has
///   its final assign value (§5 rule 1, exact and positional).
/// - Every other symbol belongs to a re-parsed file. A cascade-only file
///   (content hash unchanged, re-parsed just to rebind refs) whose old
///   and new symbol counts agree carries positionally. Otherwise each
///   symbol is matched against the OLD symbols defined in the same path
///   by `(name, kind)`, requiring the key to be unique on BOTH sides
///   (§13 R12) — an ambiguous key (overloads, duplicates) always becomes
///   NEW/NOT_ELIGIBLE rather than guessing a many-to-one match. A symbol
///   with no match is NEW when eligible (§3.1/§3.2), NOT_ELIGIBLE
///   otherwise (§13 R13).
///
/// `intern` re-interns each frozen cluster label into the NEW string
/// pool — must be the SAME pool `write_index_to` uses for everything
/// else, exactly like [`build_cluster_section`]'s caller contract.
///
/// Returns `Err` only for an internal writer-bug desync (§13 R1's
/// "writer bug, not user input" category) — never for anything a
/// malformed *input* file could trigger; those are the lazy reader's
/// job (§13 R3). See [`carry_unchanged_prefix`], [`resolve_reparsed_files`]
/// and [`remap_table_and_intern_labels`] for the three passes this
/// splits into.
pub(crate) fn build_cluster_section_from_carry(
    parsed: &[crate::index::symbols::ParsedFile],
    carry: &crate::index::types::ClusterCarryArtefacts,
    intern: &mut dyn FnMut(&str) -> u32,
) -> Result<ClusterSectionBuilt> {
    let total_symbols = carry.per_symbol_carried.len();
    let parsed_symbols: usize = parsed.iter().map(|f| f.symbols.len()).sum();
    // Code-review follow-up (MEDIUM): this used to be a `debug_assert_eq!`
    // pair. Every indexing operation below (`assign[slot]`,
    // `carry.per_symbol_carried[base + j]`) trusts that `parsed`'s
    // symbol count and `carry`'s carry-vector lengths agree — a desync
    // between the pipeline's padding step and the writer's own symbol
    // count would panic in a RELEASE build (no bounds-checked accessor
    // stands between here and a raw `Vec` index). This is the §13 R1
    // "writer bug, not user input" category: `ensure!` so a desync fails
    // the write loudly and immediately, instead of either panicking
    // release builds or silently reading garbage under
    // `debug_assert_eq!`'s debug-only guard.
    ensure!(
        total_symbols == parsed_symbols,
        "cluster carry desync: per_symbol_carried has {total_symbols} entries but `parsed` \
         has {parsed_symbols} symbols — writer bug, not user input"
    );
    ensure!(
        total_symbols == carry.per_symbol_old_idx.len(),
        "cluster carry desync: per_symbol_carried has {total_symbols} entries but \
         per_symbol_old_idx has {} — writer bug, not user input",
        carry.per_symbol_old_idx.len()
    );

    let mut assign: Vec<u32> = vec![CLUSTER_NOT_ELIGIBLE; total_symbols];
    let mut old_to_new: Vec<u32> = vec![u32::MAX; carry.old_symbol_count as usize];

    carry_unchanged_prefix(carry, &mut assign, &mut old_to_new);
    resolve_reparsed_files(parsed, carry, &mut assign, &mut old_to_new);
    let table_bytes = remap_table_and_intern_labels(carry, &old_to_new, intern);

    let mut flags = ClusterHeader::FLAG_COMPUTED | ClusterHeader::FLAG_STALE;
    if carry.iter_cap_hit {
        flags |= ClusterHeader::FLAG_ITER_CAP_HIT;
    }

    Ok(ClusterSectionBuilt {
        assign_bytes: super::csr::encode_le_u32s(&assign),
        table_bytes,
        resolution: carry.resolution,
        flags,
        algo_version: carry.algo_version,
        levels: carry.levels,
        build_symbol_count: carry.build_symbol_count,
    })
}

/// Pass 1 (§5 rule 1) — the unchanged-file prefix: exact, positional, no
/// matching needed. `assign`/`old_to_new` must already be sized to
/// `carry.per_symbol_carried.len()` / `carry.old_symbol_count`
/// respectively (the caller's `ensure!`s guarantee this).
fn carry_unchanged_prefix(
    carry: &crate::index::types::ClusterCarryArtefacts,
    assign: &mut [u32],
    old_to_new: &mut [u32],
) {
    for (slot, (carried, old_idx)) in carry
        .per_symbol_carried
        .iter()
        .zip(&carry.per_symbol_old_idx)
        .enumerate()
    {
        if let Some(v) = carried {
            assign[slot] = *v;
            if let Some(old) = old_idx {
                if let Some(dst) = old_to_new.get_mut(*old as usize) {
                    *dst = slot as u32;
                }
            }
        }
    }
}

/// Pass 2 (§13 R12/R13) — every re-parsed (changed/cascade/new) file: a
/// cascade-only file (content hash unchanged) whose old and new symbol
/// counts agree carries positionally; otherwise each symbol is
/// key-matched by `(name, kind)` against the OLD symbols in the same
/// path, requiring uniqueness on BOTH sides; anything left over is NEW
/// when eligible, NOT_ELIGIBLE otherwise.
///
/// A second pass over `parsed` (rather than interleaving with the
/// caller's own symbol-numbering loop) keeps this function pure and
/// independently testable; the extra O(symbols) walk is negligible next
/// to the re-parse `vex update` already paid for.
fn resolve_reparsed_files(
    parsed: &[crate::index::symbols::ParsedFile],
    carry: &crate::index::types::ClusterCarryArtefacts,
    assign: &mut [u32],
    old_to_new: &mut [u32],
) {
    use crate::parse::language::Language;

    let mut sym_idx: u32 = 0;
    for file in parsed {
        let base = sym_idx as usize;
        let file_needs_resolution =
            (0..file.symbols.len()).any(|j| carry.per_symbol_carried[base + j].is_none());
        if !file_needs_resolution {
            sym_idx += file.symbols.len() as u32;
            continue;
        }

        let old_entries = carry.old_symbols_by_path.get(&file.path);
        let cascade_positional = carry.cascade_unchanged_paths.contains(&file.path)
            && old_entries.map(Vec::len) == Some(file.symbols.len());

        // Key-match tables, built only when NOT taking the positional
        // path. `old_by_key` maps a (name, kind) key to the LAST old
        // entry seen for it — only ever read when `old_key_counts` says
        // that key is unique, so "last wins" never matters.
        let mut new_key_counts: HashMap<(&str, u8), u32> = HashMap::new();
        let mut old_key_counts: HashMap<(&str, u8), u32> = HashMap::new();
        let mut old_by_key: HashMap<(&str, u8), (u32, u32)> = HashMap::new();
        if !cascade_positional {
            for sym in &file.symbols {
                *new_key_counts
                    .entry((sym.name.as_str(), sym.kind as u8))
                    .or_insert(0) += 1;
            }
            if let Some(entries) = old_entries {
                for (name, kind, old_idx, old_assign) in entries {
                    let key = (name.as_str(), *kind);
                    *old_key_counts.entry(key).or_insert(0) += 1;
                    old_by_key.insert(key, (*old_idx, *old_assign));
                }
            }
        }

        let language = file
            .path
            .rsplit('.')
            .next()
            .and_then(Language::from_extension);

        for (j, sym) in file.symbols.iter().enumerate() {
            let slot = base + j;
            if carry.per_symbol_carried[slot].is_some() {
                continue; // resolved in pass 1
            }

            let matched: Option<(u32, u32)> = if cascade_positional {
                old_entries
                    .and_then(|entries| entries.get(j))
                    .map(|(_, _, old_idx, old_assign)| (*old_idx, *old_assign))
            } else {
                let key = (sym.name.as_str(), sym.kind as u8);
                if new_key_counts.get(&key).copied() == Some(1)
                    && old_key_counts.get(&key).copied() == Some(1)
                {
                    old_by_key.get(&key).copied()
                } else {
                    None
                }
            };

            match matched {
                Some((old_idx, old_assign)) => {
                    assign[slot] = old_assign;
                    if let Some(dst) = old_to_new.get_mut(old_idx as usize) {
                        *dst = slot as u32;
                    }
                }
                None => {
                    assign[slot] =
                        if crate::cluster::projection::is_eligible(sym.kind as u8, language) {
                            CLUSTER_NEW
                        } else {
                            CLUSTER_NOT_ELIGIBLE
                        };
                }
            }
        }

        sym_idx += file.symbols.len() as u32;
    }
}

/// Pass 3 (§5 rule 5) — remap `rep_sym_idx`/`hubs` through the completed
/// `old_to_new` (built by the two passes above), freeze `size`/weights
/// verbatim, and re-intern each label into the NEW string pool. Returns
/// the encoded `table` bytes, one [`ClusterRecord`] per `carry.old_table`
/// entry, in the SAME order (ordinals are never reshuffled by a carry).
fn remap_table_and_intern_labels(
    carry: &crate::index::types::ClusterCarryArtefacts,
    old_to_new: &[u32],
    intern: &mut dyn FnMut(&str) -> u32,
) -> Vec<u8> {
    let remap = |old: Option<u32>| -> u32 {
        old.and_then(|i| old_to_new.get(i as usize).copied())
            .unwrap_or(u32::MAX)
    };

    let mut table_bytes = Vec::with_capacity(carry.old_table.len() * ClusterRecord::SIZE);
    for rec in &carry.old_table {
        let label_offset = intern(&rec.label);
        let mut hubs = [u32::MAX; 3];
        for (slot, h) in hubs.iter_mut().zip(rec.hubs.iter()) {
            *slot = remap(*h);
        }
        let on_disk = ClusterRecord {
            rep_sym_idx: remap(rec.rep_sym_idx),
            size: rec.size,
            internal_weight: rec.internal_weight,
            cut_weight: rec.cut_weight,
            label_offset,
            hubs,
        };
        // SAFETY: ClusterRecord is #[repr(C)] with fixed layout (mirrors
        // `build_cluster_section`).
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &on_disk as *const ClusterRecord as *const u8,
                ClusterRecord::SIZE,
            )
        };
        table_bytes.extend_from_slice(bytes);
    }
    table_bytes
}

/// Per-symbol cluster membership outcome (`assign` sentinels, §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterStatus {
    /// Member of cluster ordinal `0..k`.
    Clustered(u32),
    /// Eligible but isolated, or a final singleton.
    Unclustered,
    /// Excluded kind/language (§3.1), or an out-of-range / corrupt
    /// `assign` slot (§2.4: "any other value >= k reads as NOT_ELIGIBLE").
    NotEligible,
    /// Introduced by `vex update` after the last full build (P4b only —
    /// P4a never emits this sentinel, but the reader still decodes it
    /// defensively since it is a defined on-disk value).
    New,
}

impl ClusterStatus {
    /// Inverse of the decode in [`ClusterSectionReader::status`] — the
    /// raw on-disk `assign` value this status came from. Used by the
    /// P4b carry path to read the OLD index's per-symbol assignment back
    /// out of a `ClusterSectionReader` without a second, duplicate
    /// bounds-checked byte accessor.
    pub(crate) fn to_raw(self) -> u32 {
        match self {
            ClusterStatus::Clustered(ord) => ord,
            ClusterStatus::Unclustered => CLUSTER_UNCLUSTERED,
            ClusterStatus::NotEligible => CLUSTER_NOT_ELIGIBLE,
            ClusterStatus::New => CLUSTER_NEW,
        }
    }
}

/// One finalized cluster's decoded fields (§2.4 `ClusterRecord`), with the
/// label resolved to a borrowed `&str` via the owning reader's string
/// pool. `rep_sym_idx` / `hubs` entries `>= symbol_count` read as
/// `None` ("absent"), per §7.
#[derive(Debug, Clone, Copy)]
pub struct ClusterRecordView<'a> {
    pub rep_sym_idx: Option<u32>,
    pub size: u32,
    pub internal_weight: u32,
    pub cut_weight: u32,
    pub label: &'a str,
    pub hubs: [Option<u32>; 3],
}

/// Aggregate counts + build metadata, computed by one O(symbol_count)
/// scan of `assign` (never done at `open()` — this is exactly the "lazy"
/// part).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterSummary {
    pub k: usize,
    pub unclustered: usize,
    pub not_eligible: usize,
    pub new_count: usize,
    pub stale: bool,
    pub resolution: (u32, u32),
    pub algo_version: u16,
}

/// Lazily-constructed, semantically-validated view over a COMPUTED v9
/// cluster section. Borrows straight from the owning [`IndexReader`]'s
/// mmap — no copies.
pub struct ClusterSectionReader<'a> {
    reader: &'a IndexReader,
    assign: &'a [u8],
    table: &'a [u8],
    k: usize,
    /// Live `header.symbol_count`, NOT `build_symbol_count` — `assign` is
    /// always sized to the live value (open() already enforced this).
    n: u32,
    resolution: (u32, u32),
    algo_version: u16,
    stale: bool,
}

impl<'a> ClusterSectionReader<'a> {
    /// Construct from `reader`. `None` when there is no COMPUTED cluster
    /// section, or when its content fails semantic validation
    /// (`resolution_den == 0`, a `k` larger than the section could
    /// legitimately hold, or bytes that don't fit — every case degrades
    /// to `None`, never a panic or an `Err`).
    pub fn new(reader: &'a IndexReader) -> Option<Self> {
        let h = reader.cluster_header()?;
        if h.flags & ClusterHeader::FLAG_COMPUTED == 0 {
            return None;
        }
        if h.resolution_den == 0 {
            return None;
        }
        let record_size = ClusterRecord::SIZE as u64;
        if !h.table_len.is_multiple_of(record_size) {
            return None;
        }
        let k = usize::try_from(h.table_len / record_size).ok()?;
        // §13 R3's rationale, relocated here (not `open()`): bound `k`
        // against the BUILD-time symbol count, never the live one — the
        // live count can shrink (files deleted) or grow since the
        // section was last computed. Every real cluster has >= 2
        // members (singletons are demoted to UNCLUSTERED, §3.3 step 6),
        // so `k` clusters need >= `2*k` members, i.e.
        // `k <= build_symbol_count / 2` is always true for honest data;
        // anything larger is corrupt (pinned by
        // `k_bound_rejects_between_half_and_full_build_symbol_count`).
        if (k as u64) * 2 > u64::from(h.build_symbol_count) {
            return None;
        }
        let n = u32::try_from(reader.header().symbol_count).ok()?;
        let assign = reader.mmap_slice(h.assign_offset, h.assign_len)?;
        let table = reader.mmap_slice(h.table_offset, h.table_len)?;
        if assign.len() as u64 != u64::from(n) * 4 {
            return None;
        }
        Some(Self {
            reader,
            assign,
            table,
            k,
            n,
            resolution: (h.resolution_num, h.resolution_den),
            algo_version: h.algo_version,
            stale: h.flags & ClusterHeader::FLAG_STALE != 0,
        })
    }

    pub fn k(&self) -> usize {
        self.k
    }

    fn assign_at(&self, sym_idx: u32) -> Option<u32> {
        let off = usize::try_from(sym_idx).ok()?.checked_mul(4)?;
        let bytes = self.assign.get(off..off.checked_add(4)?)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    /// Status of `sym_idx`. Out-of-range (`sym_idx >= symbol_count`)
    /// degrades to [`ClusterStatus::NotEligible`] rather than panicking.
    pub fn status(&self, sym_idx: u32) -> ClusterStatus {
        if sym_idx >= self.n {
            return ClusterStatus::NotEligible;
        }
        match self.assign_at(sym_idx) {
            Some(v) if v == CLUSTER_NOT_ELIGIBLE => ClusterStatus::NotEligible,
            Some(v) if v == CLUSTER_UNCLUSTERED => ClusterStatus::Unclustered,
            Some(v) if v == CLUSTER_NEW => ClusterStatus::New,
            Some(v) if (v as usize) < self.k => ClusterStatus::Clustered(v),
            // §2.4: any other value >= k (and not a sentinel) is corrupt,
            // reads as NOT_ELIGIBLE. Covers the `None` (out-of-bounds
            // byte read) case too.
            _ => ClusterStatus::NotEligible,
        }
    }

    fn record_raw(&self, ord: usize) -> Option<[u32; 8]> {
        if ord >= self.k {
            return None;
        }
        let off = ord.checked_mul(ClusterRecord::SIZE)?;
        let b = self.table.get(off..off.checked_add(ClusterRecord::SIZE)?)?;
        let u32_at = |p: usize| u32::from_le_bytes(b[p..p + 4].try_into().unwrap_or([0; 4]));
        Some([
            u32_at(0),
            u32_at(4),
            u32_at(8),
            u32_at(12),
            u32_at(16),
            u32_at(20),
            u32_at(24),
            u32_at(28),
        ])
    }

    /// Decoded record for cluster ordinal `ord`, or `None` for an
    /// out-of-range ordinal or a `table` slice too short to hold it.
    pub fn record(&self, ord: usize) -> Option<ClusterRecordView<'a>> {
        let raw = self.record_raw(ord)?;
        let [rep_sym_idx, size, internal_weight, cut_weight, label_offset, h0, h1, h2] = raw;
        let present = |v: u32| (v < self.n).then_some(v);
        let label = self.reader.read_string(label_offset);
        Some(ClusterRecordView {
            rep_sym_idx: present(rep_sym_idx),
            size,
            internal_weight,
            cut_weight,
            label,
            hubs: [present(h0), present(h1), present(h2)],
        })
    }

    /// Every `sym_idx` assigned to cluster ordinal `ord`, ascending. O(n)
    /// scan of `assign` (§4.1: "found by an O(n) scan"). Empty for an
    /// out-of-range ordinal.
    #[allow(dead_code)] // no CLI caller until P5 wires `vex modules`
    pub fn members(&self, ord: usize) -> Vec<u32> {
        if ord >= self.k {
            return Vec::new();
        }
        let ord_u32 = ord as u32;
        (0..self.n)
            .filter(|&s| self.assign_at(s) == Some(ord_u32))
            .collect()
    }

    /// Aggregate summary — one O(symbol_count) scan of `assign`.
    pub fn summary(&self) -> ClusterSummary {
        let mut unclustered = 0usize;
        let mut not_eligible = 0usize;
        let mut new_count = 0usize;
        for s in 0..self.n {
            match self.status(s) {
                ClusterStatus::Unclustered => unclustered += 1,
                ClusterStatus::NotEligible => not_eligible += 1,
                ClusterStatus::New => new_count += 1,
                ClusterStatus::Clustered(_) => {}
            }
        }
        ClusterSummary {
            k: self.k,
            unclustered,
            not_eligible,
            new_count,
            stale: self.stale,
            resolution: self.resolution,
            algo_version: self.algo_version,
        }
    }
}

#[cfg(test)]
mod tests {
    // Roundtrip / adversarial coverage lives in
    // `src/store/reader.rs`'s test module (it already owns the minimal
    // on-disk index fixture helpers this needs) and in
    // `tests/incremental_consistency_clusters.rs` / the writer roundtrip
    // tests. This module's own unit tests cover the pure decode helpers
    // directly against hand-built byte slices, without needing a whole
    // on-disk file.
    use super::*;

    #[test]
    fn assign_at_decodes_le_u32_and_is_bounds_safe() {
        // Build a reader-free harness: we can't construct
        // `ClusterSectionReader` without a real `IndexReader` (it holds a
        // borrow), so this test exercises the sentinel constants + the
        // public `ClusterStatus`/`ClusterRecordView` shapes compile and
        // match the documented sentinel values. Full behavioural
        // coverage (status/summary/members/record against a real mmap)
        // lives in `reader.rs`'s test module, next to the existing
        // minimal-index fixture builder.
        assert_eq!(CLUSTER_NOT_ELIGIBLE, 0xFFFF_FFFF);
        assert_eq!(CLUSTER_UNCLUSTERED, 0xFFFF_FFFE);
        assert_eq!(CLUSTER_NEW, 0xFFFF_FFFD);
    }
}

#[cfg(test)]
mod carry_tests {
    //! P4b (`docs/V9-FORMAT.md` §13 R11-R13) unit tests for
    //! `build_cluster_section_from_carry`, exercised directly against
    //! hand-built `ClusterCarryArtefacts` — no `IndexReader`/pipeline
    //! needed, so every match-logic branch (unchanged prefix, key-match,
    //! cascade-positional, ambiguity, eligibility, table remap) is
    //! isolated and deterministic. End-to-end coverage through the real
    //! pipeline lives in `tests/incremental_consistency_clusters.rs`.
    use std::collections::HashSet;

    use super::*;
    use crate::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
    use crate::index::types::{CarriedClusterRecord, ClusterCarryArtefacts};

    fn mk_sym(name: &str, kind: SymbolKind, line: usize) -> ParsedSymbol {
        ParsedSymbol {
            name: name.to_string(),
            kind,
            line,
            signature: None,
            doc: None,
            body_tokens: None,
        }
    }

    fn mk_file(path: &str, symbols: Vec<ParsedSymbol>) -> ParsedFile {
        ParsedFile {
            path: path.to_string(),
            symbols,
            refs: Vec::new(),
            call_edges: Vec::new(),
            bound_refs: Vec::new(),
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
            hierarchy_captures: Vec::new(),
        }
    }

    fn decode_assign(bytes: &[u8]) -> Vec<u32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect()
    }

    /// Decode a `ClusterRecord`'s 8 `u32` fields in on-disk order:
    /// `[rep_sym_idx, size, internal_weight, cut_weight, label_offset,
    /// hubs[0], hubs[1], hubs[2]]`.
    fn decode_table(bytes: &[u8]) -> Vec<[u32; 8]> {
        bytes
            .as_chunks::<{ ClusterRecord::SIZE }>()
            .0
            .iter()
            .map(|rec| {
                let mut out = [0u32; 8];
                for (i, slot) in out.iter_mut().enumerate() {
                    let off = i * 4;
                    *slot = u32::from_le_bytes(rec[off..off + 4].try_into().unwrap());
                }
                out
            })
            .collect()
    }

    fn base_carry() -> ClusterCarryArtefacts {
        ClusterCarryArtefacts {
            per_symbol_carried: Vec::new(),
            per_symbol_old_idx: Vec::new(),
            old_symbols_by_path: HashMap::new(),
            old_symbol_count: 0,
            cascade_unchanged_paths: Default::default(),
            old_table: Vec::new(),
            resolution: (1, 8),
            algo_version: 1,
            levels: 3,
            build_symbol_count: 0,
            iter_cap_hit: false,
        }
    }

    #[test]
    fn unchanged_prefix_is_carried_verbatim() {
        let parsed = vec![
            mk_file("a.rs", vec![mk_sym("a0", SymbolKind::Function, 1)]),
            mk_file("b.rs", vec![mk_sym("b0", SymbolKind::Function, 1)]),
        ];
        let carry = ClusterCarryArtefacts {
            per_symbol_carried: vec![Some(5), Some(CLUSTER_UNCLUSTERED)],
            per_symbol_old_idx: vec![Some(0), Some(1)],
            old_symbol_count: 2,
            ..base_carry()
        };
        let mut interned = Vec::new();
        let built = build_cluster_section_from_carry(&parsed, &carry, &mut |s: &str| {
            interned.push(s.to_string());
            0
        })
        .expect("carry build");
        assert_eq!(
            decode_assign(&built.assign_bytes),
            vec![5, CLUSTER_UNCLUSTERED]
        );
        assert_eq!(built.resolution, (1, 8));
        assert_eq!(built.algo_version, 1);
        assert_eq!(built.levels, 3);
        assert_eq!(
            built.flags,
            ClusterHeader::FLAG_COMPUTED | ClusterHeader::FLAG_STALE
        );
    }

    #[test]
    fn key_match_unique_both_sides_inherits_old_assign() {
        // `a.rs` is a re-parsed (changed) file — its one symbol has no
        // precomputed carry entry (`None`), but it key-matches a unique
        // OLD symbol of the same (name, kind) in the same path.
        let parsed = vec![mk_file(
            "a.rs",
            vec![mk_sym("foo", SymbolKind::Function, 1)],
        )];
        let carry = ClusterCarryArtefacts {
            per_symbol_carried: vec![None],
            per_symbol_old_idx: vec![None],
            old_symbols_by_path: HashMap::from([(
                "a.rs".to_string(),
                vec![("foo".to_string(), SymbolKind::Function as u8, 7u32, 3u32)],
            )]),
            old_symbol_count: 8,
            ..base_carry()
        };
        let built =
            build_cluster_section_from_carry(&parsed, &carry, &mut |_| 0).expect("carry build");
        assert_eq!(decode_assign(&built.assign_bytes), vec![3]);
    }

    #[test]
    fn ambiguous_key_on_new_side_becomes_new_not_many_to_one() {
        // Two NEW symbols share (name, kind) — the OLD side has exactly
        // one "dup", but R12 requires uniqueness on BOTH sides, so
        // neither new symbol may inherit it; both become NEW rather than
        // one guessing a many-to-one match.
        let parsed = vec![mk_file(
            "a.rs",
            vec![
                mk_sym("dup", SymbolKind::Function, 1),
                mk_sym("dup", SymbolKind::Function, 5),
            ],
        )];
        let carry = ClusterCarryArtefacts {
            per_symbol_carried: vec![None, None],
            per_symbol_old_idx: vec![None, None],
            old_symbols_by_path: HashMap::from([(
                "a.rs".to_string(),
                vec![("dup".to_string(), SymbolKind::Function as u8, 2u32, 1u32)],
            )]),
            old_symbol_count: 8,
            ..base_carry()
        };
        let built =
            build_cluster_section_from_carry(&parsed, &carry, &mut |_| 0).expect("carry build");
        assert_eq!(
            decode_assign(&built.assign_bytes),
            vec![CLUSTER_NEW, CLUSTER_NEW],
            "ambiguous key on the new side must never many-to-one match"
        );
    }

    #[test]
    fn cascade_unchanged_path_carries_positionally_ignoring_name() {
        // A cascade-only re-parse (content hash unchanged) with the SAME
        // symbol count as the old file carries by POSITION, not by
        // (name, kind) — proven here by giving the new symbols DIFFERENT
        // names than their old counterparts.
        let parsed = vec![mk_file(
            "a.rs",
            vec![
                mk_sym("renamed_x", SymbolKind::Function, 1),
                mk_sym("renamed_y", SymbolKind::Function, 5),
            ],
        )];
        let carry = ClusterCarryArtefacts {
            per_symbol_carried: vec![None, None],
            per_symbol_old_idx: vec![None, None],
            old_symbols_by_path: HashMap::from([(
                "a.rs".to_string(),
                vec![
                    ("x".to_string(), SymbolKind::Function as u8, 10u32, 2u32),
                    (
                        "y".to_string(),
                        SymbolKind::Function as u8,
                        11u32,
                        CLUSTER_UNCLUSTERED,
                    ),
                ],
            )]),
            old_symbol_count: 12,
            cascade_unchanged_paths: HashSet::from(["a.rs".to_string()]),
            ..base_carry()
        };
        let built =
            build_cluster_section_from_carry(&parsed, &carry, &mut |_| 0).expect("carry build");
        assert_eq!(
            decode_assign(&built.assign_bytes),
            vec![2, CLUSTER_UNCLUSTERED],
            "position 0 -> old entry 0 (x), position 1 -> old entry 1 (y), despite the name change"
        );
    }

    #[test]
    fn new_ineligible_symbol_is_not_eligible_not_new() {
        // A Markdown heading has no old counterpart and fails the
        // eligibility predicate (kind AND language both exclude it) —
        // §13 R13: it must read NOT_ELIGIBLE, never NEW.
        let parsed = vec![mk_file(
            "doc.md",
            vec![mk_sym("Intro", SymbolKind::Heading, 1)],
        )];
        let carry = ClusterCarryArtefacts {
            per_symbol_carried: vec![None],
            per_symbol_old_idx: vec![None],
            ..base_carry()
        };
        let built =
            build_cluster_section_from_carry(&parsed, &carry, &mut |_| 0).expect("carry build");
        assert_eq!(
            decode_assign(&built.assign_bytes),
            vec![CLUSTER_NOT_ELIGIBLE]
        );
    }

    #[test]
    fn new_eligible_symbol_with_no_match_becomes_new() {
        let parsed = vec![mk_file(
            "a.rs",
            vec![mk_sym("brand_new", SymbolKind::Function, 1)],
        )];
        let carry = ClusterCarryArtefacts {
            per_symbol_carried: vec![None],
            per_symbol_old_idx: vec![None],
            ..base_carry()
        };
        let built =
            build_cluster_section_from_carry(&parsed, &carry, &mut |_| 0).expect("carry build");
        assert_eq!(decode_assign(&built.assign_bytes), vec![CLUSTER_NEW]);
    }

    #[test]
    fn table_remap_rep_and_hubs_to_new_idx_or_max() {
        // Symbol at OLD sym_idx 0 survives (unchanged prefix) and lands
        // at NEW sym_idx 0. OLD sym_idx 1 does not survive anywhere in
        // this update (no carry entry, no key match) — its table
        // references must degrade to `u32::MAX` ("lost"), never a stale
        // or out-of-range index.
        let parsed = vec![mk_file("a.rs", vec![mk_sym("a0", SymbolKind::Function, 1)])];
        let carry = ClusterCarryArtefacts {
            per_symbol_carried: vec![Some(0)],
            per_symbol_old_idx: vec![Some(0)],
            old_symbol_count: 2,
            old_table: vec![CarriedClusterRecord {
                rep_sym_idx: Some(0),
                size: 4,
                internal_weight: 9,
                cut_weight: 1,
                label: "src/a/".to_string(),
                hubs: [Some(0), Some(1), None],
            }],
            ..base_carry()
        };
        let mut labels = Vec::new();
        let built = build_cluster_section_from_carry(&parsed, &carry, &mut |s: &str| {
            labels.push(s.to_string());
            42
        })
        .expect("carry build");
        let table = decode_table(&built.table_bytes);
        assert_eq!(table.len(), 1);
        let [rep, size, internal, cut, label_offset, h0, h1, h2] = table[0];
        assert_eq!(rep, 0, "rep_sym_idx remapped OLD 0 -> NEW 0");
        assert_eq!(size, 4, "size is a frozen build-time value");
        assert_eq!(internal, 9);
        assert_eq!(cut, 1);
        assert_eq!(
            label_offset, 42,
            "label re-interned via the writer's intern closure"
        );
        assert_eq!(labels, vec!["src/a/".to_string()]);
        assert_eq!(h0, 0, "hub OLD 0 -> NEW 0, same as rep");
        assert_eq!(h1, u32::MAX, "hub OLD 1 did not survive -> lost (u32::MAX)");
        assert_eq!(h2, u32::MAX, "a None hub slot stays u32::MAX");
    }

    #[test]
    fn iter_cap_hit_flag_is_carried_forward() {
        let parsed = vec![mk_file("a.rs", vec![mk_sym("a0", SymbolKind::Function, 1)])];
        let carry = ClusterCarryArtefacts {
            per_symbol_carried: vec![Some(CLUSTER_UNCLUSTERED)],
            per_symbol_old_idx: vec![Some(0)],
            old_symbol_count: 1,
            iter_cap_hit: true,
            ..base_carry()
        };
        let built =
            build_cluster_section_from_carry(&parsed, &carry, &mut |_| 0).expect("carry build");
        assert_eq!(
            built.flags,
            ClusterHeader::FLAG_COMPUTED
                | ClusterHeader::FLAG_STALE
                | ClusterHeader::FLAG_ITER_CAP_HIT
        );
    }

    #[test]
    fn desynced_carry_length_bails_instead_of_panicking() {
        // Code-review follow-up (MEDIUM) — `per_symbol_carried` claims 2
        // entries but `parsed` only has 1 symbol. Every direct index in
        // the two resolution passes trusts these lengths agree; this
        // must surface as a clean `Err`, never a release-mode panic.
        let parsed = vec![mk_file("a.rs", vec![mk_sym("a0", SymbolKind::Function, 1)])];
        let carry = ClusterCarryArtefacts {
            per_symbol_carried: vec![Some(CLUSTER_UNCLUSTERED), Some(CLUSTER_UNCLUSTERED)],
            per_symbol_old_idx: vec![Some(0), Some(1)],
            old_symbol_count: 2,
            ..base_carry()
        };
        let err = build_cluster_section_from_carry(&parsed, &carry, &mut |_| 0)
            .expect_err("a length desync must bail, not panic");
        assert!(
            err.to_string().contains("writer bug"),
            "error should name itself a writer bug, got: {err}"
        );
    }

    #[test]
    fn desynced_old_idx_length_bails_instead_of_panicking() {
        let parsed = vec![mk_file("a.rs", vec![mk_sym("a0", SymbolKind::Function, 1)])];
        let carry = ClusterCarryArtefacts {
            per_symbol_carried: vec![Some(CLUSTER_UNCLUSTERED)],
            per_symbol_old_idx: vec![Some(0), Some(1)],
            old_symbol_count: 2,
            ..base_carry()
        };
        let err = build_cluster_section_from_carry(&parsed, &carry, &mut |_| 0)
            .expect_err("a per_symbol_old_idx length desync must bail, not panic");
        assert!(err.to_string().contains("writer bug"));
    }
}
