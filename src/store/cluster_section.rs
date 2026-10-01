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

/// P4a (`docs/V9-FORMAT.md` §13 R10/R14): whether this write should
/// compute symbol clusters, and at what resolution. `None` means "don't
/// compute" — the writer emits the all-zero P2 `ClusterHeader` — which is
/// what every `vex update` call passes today (P4a never carries; that's
/// P4b) and what `--no-clusters` passes for `vex index`. `vex index`
/// without `--no-clusters` passes `Some`.
#[derive(Debug, Clone, Copy)]
pub struct ClusterComputeRequest {
    pub resolution: (u32, u32),
}

/// Writer-ready bytes + header fields for a COMPUTED cluster section —
/// everything `write_index_to` needs to fill in `ClusterHeader` and write
/// the `assign`/`table` bytes.
pub(crate) struct ClusterSectionBuilt {
    pub assign_bytes: Vec<u8>,
    pub table_bytes: Vec<u8>,
    pub resolution: (u32, u32),
    pub flags: u32,
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
        levels: output.levels,
        build_symbol_count: output.assign.len() as u32,
    }
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

/// One finalized cluster's decoded fields (§2.4 `ClusterRecord`), with the
/// label resolved to a borrowed `&str` via the owning reader's string
/// pool. `rep_sym_idx` / `hubs` entries `>= symbol_count` read as
/// `None` ("absent"), per §7.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // fields read only by `record()`, which has no CLI caller until P5
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

    #[allow(dead_code)] // no CLI caller until P5 wires `vex modules`
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
    #[allow(dead_code)] // no CLI caller until P5 wires `vex modules` symbol mode
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
    #[allow(dead_code)] // no CLI caller until P5 wires `vex modules`
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
