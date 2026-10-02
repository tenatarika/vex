//! CSR (compressed sparse row) adjacency: shared builder + zero-copy
//! reader for the v9 callees and `ref_edges` sections
//! (`docs/V9-FORMAT.md` §2.2, §2.3, §3.1, §7 — R1, R4, R16, R18).
//!
//! Two on-disk shapes share one reader:
//!
//! - **Callees** (`build_csr`): `offsets[n+1]` + `edge_idx[m]`. Group `s`
//!   is `edge_idx[offsets[s]..offsets[s+1]]`, ascending within the group
//!   (stable counting sort), holding edge indices into the `CallEdge`
//!   section.
//! - **`ref_edges`** (`build_csr_offsets_sorted`): `offsets[n+1]` only.
//!   Group `s` is the *identity* range `offsets[s]..offsets[s+1]` into
//!   the already-sorted `RefEdge` record array — no `edge_idx` array,
//!   because the records are pre-sorted by key (F5). Sortedness is a v9
//!   format invariant (R1/R2), checked here with a real error, not a
//!   `debug_assert!`.
//!
//! Wired into the writer (`writer.rs`) and reader (`reader.rs`) as of the
//! v9 format bump (P2): `build_csr` produces the callees `offsets` +
//! `edge_idx` arrays, `build_csr_offsets_sorted` produces the `ref_edges`
//! offsets-only shape, and `CsrView` backs both `find_callees_fast` and
//! `IndexReader::find_ref_edges_by_symbol` — including the in-memory CSR
//! built once per `IndexReader` for legacy (v4–v8) files (§13 R19).
//!
//! This module never casts byte slices to `&[u32]` — every value is
//! decoded with `u32::from_le_bytes` on a bounds-checked 4-byte window
//! (§2), so alignment of the underlying mmap is a courtesy, never a
//! safety requirement.

use anyhow::{ensure, Result};

// ---------------------------------------------------------------------------
// Builders (writer side — in-memory `Vec<u32>`, later serialised to LE bytes)
// ---------------------------------------------------------------------------

/// Build a CSR adjacency from an edge list keyed by `keys[e]` (e.g. the
/// `caller_sym_idx` of edge `e`). Returns `(offsets, edge_idx)`:
///
/// - `offsets` has length `n + 1`, `offsets[0] == 0`,
///   `offsets[n] == edge_idx.len()`.
/// - `edge_idx` holds the retained edge indices grouped by key, ascending
///   *within* each group (stable counting sort — the same order
///   `legacy_v8::build_u32_keyed_fst`'s posting lists produce, per F4).
///
/// A key `>= n` is a writer bug, not user input (R18/§3.1): the writer's
/// own symbol numbering produces `keys`, so an out-of-range value means
/// caller and `n` disagree. That edge is filtered out of every group
/// (contributes to no group; it is not `edge_idx.len()`'s problem to
/// solve) rather than failing the whole build — the `edge_idx` array
/// makes this safe (§7: a callee edge simply isn't referenced by any
/// group, and nothing else in the on-disk shape depends on its
/// position). `tracing::warn!` reports it unconditionally;
/// `debug_assert!` additionally turns it into a hard panic in dev/test
/// builds, matching the precedent at `writer.rs:591` (`ModuleSymbol`'s
/// `checked_add`-guarded `wrapping_add`) — a release build degrades
/// gracefully, a dev/test build surfaces the bug immediately.
///
/// Returns `Err` only on arithmetic overflow in the prefix sum
/// (`checked_add`, not `saturating_add` — unreachable in practice since
/// a real `keys.len()` / `n` never approach `u32::MAX`, but a silent
/// wrong result would be worse than a loud, defensive error here).
pub fn build_csr(keys: &[u32], n: u32) -> Result<(Vec<u32>, Vec<u32>)> {
    let (offsets, out_of_range) = count_into_offsets(keys, n)?;
    warn_and_debug_assert_out_of_range(out_of_range, n);
    Ok(scatter(keys, n, offsets))
}

/// [`build_csr`] for keys read back from an index file rather than produced
/// by the writer. Such keys are untrusted input: an out-of-range key is
/// corruption, not a writer bug, so it is dropped with a warning and never
/// trips the debug assertion. Used by the reader's legacy (v4–v8) path.
pub fn build_csr_from_untrusted(keys: &[u32], n: u32) -> Result<(Vec<u32>, Vec<u32>)> {
    let (offsets, out_of_range) = count_into_offsets(keys, n)?;
    if out_of_range > 0 {
        tracing::warn!(
            out_of_range,
            symbol_count = n,
            "dropped {out_of_range} edge(s) whose key >= symbol_count {n} (corrupt index)"
        );
    }
    Ok(scatter(keys, n, offsets))
}

/// Second counting-sort pass: place each in-range edge index into its group.
fn scatter(keys: &[u32], n: u32, offsets: Vec<u32>) -> (Vec<u32>, Vec<u32>) {
    let n_usize = n as usize;
    let m = offsets[n_usize] as usize;
    let mut cursor = offsets[..n_usize].to_vec();
    let mut edge_idx = vec![0u32; m];
    for (e, &k) in keys.iter().enumerate() {
        if k >= n {
            continue;
        }
        let slot = &mut cursor[k as usize];
        edge_idx[*slot as usize] = e as u32;
        *slot += 1;
    }

    (offsets, edge_idx)
}

/// Build the `offsets`-only CSR for a key sequence that is already
/// sorted ascending (ties allowed) — the `ref_edges` shape (§2.3, F5).
///
/// Unlike [`build_csr`], **both** invariants here are enforced with a
/// real error (`ensure!`), never `debug_assert!`:
///
/// - sortedness (R1/R2): eliding `edge_idx` makes "records sorted by
///   key" a **v9 format invariant**, and a writer that violates it must
///   fail loudly, not ship a corrupt file;
/// - every key `< n` (unlike [`build_csr`]): this shape has no
///   `edge_idx` array to drop a bad edge from. The on-disk group for
///   symbol `s` is the *positional* range `offsets[s]..offsets[s+1]`
///   into the physical `RefEdge` record array — filtering an
///   out-of-range key out of the `offsets` bucket counts (as
///   [`build_csr`] does) would leave that record still physically
///   present at its original position but uncounted by any `offsets`
///   boundary, so every later record's group would shift by one and
///   silently point at the wrong records. E.g. keys `[0, 5, 1]` with
///   `n = 3`: dropping key `5`'s count would make symbol `1`'s group
///   `offsets[1]..offsets[2]` land on the record that actually holds
///   key `5`'s data, not key `1`'s. R18's "filter, don't bail" does not
///   transfer to this shape — it only works because [`build_csr`] has
///   an explicit `edge_idx` indirection to simply omit the bad edge
///   from.
pub fn build_csr_offsets_sorted(keys: &[u32], n: u32) -> Result<Vec<u32>> {
    for w in keys.windows(2) {
        ensure!(
            w[0] <= w[1],
            "build_csr_offsets_sorted: keys not sorted ascending ({} appears after {})",
            w[1],
            w[0]
        );
    }
    for &k in keys {
        ensure!(
            k < n,
            "build_csr_offsets_sorted: key {k} >= symbol_count {n} — this shape has no \
             edge_idx array to drop the bad record from, so filtering would misalign every \
             later group against the physical record array; this is a writer bug"
        );
    }

    let (offsets, out_of_range) = count_into_offsets(keys, n)?;
    debug_assert_eq!(
        out_of_range, 0,
        "unreachable: out-of-range keys are rejected above"
    );
    Ok(offsets)
}

/// Shared counting-sort degree pass: returns `(offsets, out_of_range_count)`.
/// `offsets[n]` after the prefix sum equals the number of **retained**
/// (in-range) keys. `Err` only on `checked_add` overflow (§3.1: overflow-
/// safe), which requires a `keys.len()` or per-key degree near `u32::MAX`
/// — unreachable for any real index, but never silently wrapped/saturated.
fn count_into_offsets(keys: &[u32], n: u32) -> Result<(Vec<u32>, usize)> {
    let n_usize = n as usize;
    let mut offsets = vec![0u32; n_usize + 1];
    let mut out_of_range = 0usize;

    for &k in keys {
        if k >= n {
            out_of_range += 1;
            continue;
        }
        // `k < n == n_usize` (as u32 -> usize is lossless here), so
        // `k as usize + 1 <= n_usize`, within `offsets`' length.
        let slot = &mut offsets[k as usize + 1];
        *slot = slot
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("csr degree overflow at key {k}"))?;
    }

    for i in 0..n_usize {
        offsets[i + 1] = offsets[i + 1]
            .checked_add(offsets[i])
            .ok_or_else(|| anyhow::anyhow!("csr offsets prefix-sum overflow at index {i}"))?;
    }

    Ok((offsets, out_of_range))
}

fn warn_and_debug_assert_out_of_range(out_of_range: usize, n: u32) {
    if out_of_range == 0 {
        return;
    }
    tracing::warn!(
        out_of_range,
        symbol_count = n,
        "build_csr: dropped {out_of_range} edge(s) whose key >= symbol_count {n} (writer bug)"
    );
    debug_assert!(
        out_of_range == 0,
        "build_csr: {out_of_range} key(s) >= symbol_count {n} (writer bug) — \
         a release build degrades gracefully (edges dropped, warn! logged); \
         dev/test builds fail loudly so the writer bug is caught before it ships"
    );
}

/// Serialise a `u32` array to little-endian bytes — used for both
/// `offsets` and `edge_idx` (§2: "written with `to_le_bytes`"). Named
/// `encode_*` rather than `to_le_bytes` to avoid reading like a shadow
/// of `u32::to_le_bytes` at call sites.
pub fn encode_le_u32s(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for &v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// Reader (mmap side — never casts to &[u32], §2)
// ---------------------------------------------------------------------------

/// Bounds-checked, allocation-free view over a CSR adjacency backed by
/// raw LE-`u32` byte slices (§7: `CsrView::neighbors(s)` is the per-lookup
/// validation boundary; [`CsrView::new`] does only the O(1) structural
/// checks required at `open()`).
#[derive(Debug, Clone, Copy)]
pub struct CsrView<'a> {
    offsets: &'a [u8],
    edge_idx: Option<&'a [u8]>,
    n: u32,
    m: u32,
}

impl<'a> CsrView<'a> {
    /// Validate and construct a view. `offsets_bytes` must be exactly
    /// `4 * (n + 1)` bytes. `edge_idx_bytes`:
    /// - `Some(bytes)` (callees shape) must be exactly `4 * m` bytes;
    /// - `None` (ref_edges identity shape) — neighbours are the implicit
    ///   range `offsets[s]..offsets[s+1]`, capped at `m`.
    ///
    /// O(1) checks only (R3/R4): lengths match, `offsets[0] == 0`, and
    /// `offsets[n] == m`. Monotonicity of the interior is **not**
    /// checked here — a non-monotone array degrades to empty groups at
    /// lookup time (`neighbors`), never a panic and never an open-time
    /// O(n) scan (§7).
    pub fn new(
        offsets_bytes: &'a [u8],
        edge_idx_bytes: Option<&'a [u8]>,
        n: u32,
        m: u32,
    ) -> Result<Self> {
        let want_offsets_len = (n as usize)
            .checked_add(1)
            .and_then(|v| v.checked_mul(4))
            .ok_or_else(|| anyhow::anyhow!("CsrView::new: offsets length overflow for n={n}"))?;
        ensure!(
            offsets_bytes.len() == want_offsets_len,
            "CsrView::new: offsets length {} != expected {want_offsets_len} (n={n})",
            offsets_bytes.len()
        );

        if let Some(bytes) = edge_idx_bytes {
            let want_edge_len = (m as usize).checked_mul(4).ok_or_else(|| {
                anyhow::anyhow!("CsrView::new: edge_idx length overflow for m={m}")
            })?;
            ensure!(
                bytes.len() == want_edge_len,
                "CsrView::new: edge_idx length {} != expected {want_edge_len} (m={m})",
                bytes.len()
            );
        }

        let off0 = read_u32_le(offsets_bytes, 0)
            .ok_or_else(|| anyhow::anyhow!("CsrView::new: offsets too short to read offsets[0]"))?;
        ensure!(off0 == 0, "CsrView::new: offsets[0] must be 0, got {off0}");

        let off_n = read_u32_le(offsets_bytes, (n as usize) * 4)
            .ok_or_else(|| anyhow::anyhow!("CsrView::new: offsets too short to read offsets[n]"))?;
        ensure!(off_n == m, "CsrView::new: offsets[n] ({off_n}) != m ({m})");

        Ok(Self {
            offsets: offsets_bytes,
            edge_idx: edge_idx_bytes,
            n,
            m,
        })
    }

    /// Number of groups (e.g. `symbol_count`).
    #[allow(dead_code)] // exercised by this module's tests; documented public API
    pub fn len(&self) -> u32 {
        self.n
    }

    #[allow(dead_code)] // exercised by this module's tests; documented public API
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Total edge count (`m`).
    #[allow(dead_code)] // exercised by this module's tests; documented public API
    pub fn edge_count(&self) -> u32 {
        self.m
    }

    /// Neighbours of group `s`, in ascending edge/record-index order.
    /// Zero-allocation: decodes `u32` LE lazily from the borrowed byte
    /// slice (callees shape) or counts up a plain range (identity
    /// shape). Never panics: an out-of-range `s`, a non-monotone
    /// `start > end`, or `end > m` all yield an empty iterator (§7).
    pub fn neighbors(&self, s: u32) -> CsrNeighbors<'a> {
        if s >= self.n {
            return CsrNeighbors::Empty;
        }
        let Some(start) = read_u32_le(self.offsets, (s as usize) * 4) else {
            return CsrNeighbors::Empty;
        };
        let Some(end) = read_u32_le(self.offsets, (s as usize + 1) * 4) else {
            return CsrNeighbors::Empty;
        };
        if start > end || end > self.m {
            return CsrNeighbors::Empty;
        }

        match self.edge_idx {
            Some(bytes) => {
                let byte_start = (start as usize) * 4;
                let byte_end = (end as usize) * 4;
                let Some(slice) = bytes.get(byte_start..byte_end) else {
                    return CsrNeighbors::Empty;
                };
                CsrNeighbors::Edges {
                    bytes: slice,
                    pos: 0,
                    bound: self.m,
                }
            }
            None => CsrNeighbors::Identity { next: start, end },
        }
    }
}

/// Zero-allocation iterator returned by [`CsrView::neighbors`]. Borrows
/// from the `CsrView`'s byte slice in the `Edges` case; the `Identity`
/// case needs no bytes at all (the range is the value).
#[derive(Debug, Clone)]
pub enum CsrNeighbors<'a> {
    Edges {
        bytes: &'a [u8],
        pos: usize,
        bound: u32,
    },
    Identity {
        next: u32,
        end: u32,
    },
    Empty,
}

impl Iterator for CsrNeighbors<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        match self {
            CsrNeighbors::Edges { bytes, pos, bound } => loop {
                let window = bytes.get(*pos..*pos + 4)?;
                *pos += 4;
                // SAFETY-free: bounds-checked 4-byte window, never casts
                // the underlying slice to `&[u32]` (§2).
                let idx = u32::from_le_bytes([window[0], window[1], window[2], window[3]]);
                if idx < *bound {
                    return Some(idx);
                }
                // §7: a decoded edge_idx >= m is corrupt — skip it and
                // keep scanning the rest of the group.
            },
            CsrNeighbors::Identity { next, end } => {
                if *next >= *end {
                    None
                } else {
                    let v = *next;
                    *next += 1;
                    Some(v)
                }
            }
            CsrNeighbors::Empty => None,
        }
    }
}

fn read_u32_le(bytes: &[u8], byte_offset: usize) -> Option<u32> {
    let window = bytes.get(byte_offset..byte_offset + 4)?;
    Some(u32::from_le_bytes([
        window[0], window[1], window[2], window[3],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // build_csr
    // -----------------------------------------------------------------

    #[test]
    fn empty_input_produces_offsets_only_zeros() {
        let (offsets, edge_idx) = build_csr(&[], 5).unwrap();
        assert_eq!(offsets, vec![0, 0, 0, 0, 0, 0]);
        assert!(edge_idx.is_empty());
    }

    #[test]
    fn n_zero_produces_single_zero_offset_and_no_edges() {
        let (offsets, edge_idx) = build_csr(&[], 0).unwrap();
        assert_eq!(offsets, vec![0]);
        assert!(edge_idx.is_empty());
    }

    #[test]
    fn groups_are_stable_ascending_by_edge_index() {
        // keys: edge 0 -> group 1, edge 1 -> group 0, edge 2 -> group 1
        let (offsets, edge_idx) = build_csr(&[1, 0, 1], 2).unwrap();
        assert_eq!(offsets, vec![0, 1, 3]);
        // group 0 (offsets[0]..offsets[1]) = edge 1
        assert_eq!(&edge_idx[0..1], &[1]);
        // group 1 (offsets[1]..offsets[2]) = edges 0, 2 in ascending order
        assert_eq!(&edge_idx[1..3], &[0, 2]);
    }

    /// Direct test of the private counting-sort helper (bypassing
    /// `build_csr`'s `debug_assert!` trip-wire entirely, since calling
    /// `count_into_offsets` never trips it) — this is what actually
    /// verifies the release-mode graceful path: an out-of-range key is
    /// dropped from the bucket counts, not just "the process doesn't
    /// abort after panicking" (which is all `catch_unwind` around
    /// `build_csr` could prove).
    #[test]
    fn count_into_offsets_filters_out_of_range_keys_gracefully() {
        // n=3: valid keys are 0,1,2. Key 5 is out of range and must be
        // dropped from the bucket counts, not counted anywhere.
        let (offsets, out_of_range) = count_into_offsets(&[0, 5, 1], 3).unwrap();
        assert_eq!(out_of_range, 1, "exactly one key (5) was out of range");
        // key 0 -> bucket 0 (count 1), key 1 -> bucket 1 (count 1),
        // bucket 2 (key 2) never touched -> count 0.
        assert_eq!(offsets, vec![0, 1, 2, 2]);
    }

    /// The `debug_assert!` trip-wire in `build_csr` (via
    /// `warn_and_debug_assert_out_of_range`) must fire in a dev/test
    /// build when a key is out of range — R18: this is a writer bug,
    /// not user input, and dev/test builds are exactly where we want it
    /// caught loudly. A release build (debug_assertions off) never
    /// reaches this assert and returns the graceful, filtered result
    /// verified directly by `count_into_offsets_filters_out_of_range_keys_gracefully`.
    #[test]
    #[should_panic(expected = "key(s) >= symbol_count")]
    fn out_of_range_keys_trip_debug_assert_in_dev_test_builds() {
        let _ = build_csr(&[0, 5, 1], 3);
    }

    /// Keys decoded from an on-disk legacy index are untrusted input, not a
    /// writer bug: a corrupt `caller_sym_idx` / `to_sym_idx` must be dropped
    /// without the `build_csr` debug trip-wire (found by fuzz_index_reader).
    #[test]
    fn untrusted_out_of_range_keys_are_dropped_without_panicking() {
        let (offsets, edge_idx) = build_csr_from_untrusted(&[0, 5, 1], 3).unwrap();
        assert_eq!(offsets, vec![0, 1, 2, 2]);
        assert_eq!(edge_idx, vec![0, 2]);
    }

    #[test]
    fn checked_add_overflow_safe_large_n() {
        // n large but no keys — must not panic or overflow.
        let n: u32 = 1 << 20;
        let (offsets, edge_idx) = build_csr(&[], n).unwrap();
        assert_eq!(offsets.len(), (1usize << 20) + 1);
        assert!(edge_idx.is_empty());
    }

    // -----------------------------------------------------------------
    // build_csr_offsets_sorted (ref_edges identity shape)
    // -----------------------------------------------------------------

    #[test]
    fn offsets_sorted_builds_from_sorted_keys() {
        let offsets = build_csr_offsets_sorted(&[0, 0, 1, 1, 1, 3], 4).unwrap();
        assert_eq!(offsets, vec![0, 2, 5, 5, 6]);
    }

    #[test]
    fn offsets_sorted_rejects_unsorted_keys() {
        let err = build_csr_offsets_sorted(&[0, 2, 1], 3).unwrap_err();
        assert!(err.to_string().contains("not sorted"), "{err}");
    }

    /// Unlike `build_csr`, an out-of-range key here must be a hard
    /// `Err`, never a filtered/graceful result — this shape has no
    /// `edge_idx` array to drop the bad record from, so silently
    /// dropping its count would misalign every later group against the
    /// physical (already-sorted) record array. Keys are sorted ascending
    /// (0 <= 1 <= 5) so this isolates the out-of-range check from the
    /// sortedness check above — the review's original example, `[0, 5,
    /// 1]` with `n=3`, is unsorted and would trip that check first.
    #[test]
    fn offsets_sorted_rejects_out_of_range_key() {
        let err = build_csr_offsets_sorted(&[0, 1, 5], 3).unwrap_err();
        assert!(
            err.to_string().contains("symbol_count"),
            "expected an out-of-range-key error, got: {err}"
        );
    }

    #[test]
    fn offsets_sorted_empty_and_n_zero() {
        assert_eq!(build_csr_offsets_sorted(&[], 0).unwrap(), vec![0]);
        assert_eq!(build_csr_offsets_sorted(&[], 5).unwrap(), vec![0; 6]);
    }

    // -----------------------------------------------------------------
    // CsrView
    // -----------------------------------------------------------------

    #[test]
    fn view_roundtrips_callees_shape() {
        let (offsets, edge_idx) = build_csr(&[1, 0, 1, 2], 3).unwrap();
        let offsets_bytes = encode_le_u32s(&offsets);
        let edge_bytes = encode_le_u32s(&edge_idx);
        let m = edge_idx.len() as u32;
        let view = CsrView::new(&offsets_bytes, Some(&edge_bytes), 3, m).unwrap();
        assert_eq!(view.neighbors(0).collect::<Vec<_>>(), vec![1]);
        assert_eq!(view.neighbors(1).collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(view.neighbors(2).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn view_roundtrips_identity_shape() {
        let offsets = build_csr_offsets_sorted(&[0, 0, 2], 3).unwrap();
        let offsets_bytes = encode_le_u32s(&offsets);
        let m = *offsets.last().unwrap();
        let view = CsrView::new(&offsets_bytes, None, 3, m).unwrap();
        assert_eq!(view.neighbors(0).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(view.neighbors(1).collect::<Vec<_>>(), Vec::<u32>::new());
        assert_eq!(view.neighbors(2).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn new_rejects_wrong_offsets_length() {
        let offsets_bytes = encode_le_u32s(&[0, 1, 2]);
        assert!(CsrView::new(&offsets_bytes, None, 5, 2).is_err());
    }

    #[test]
    fn new_rejects_offsets_zero_not_zero() {
        let offsets_bytes = encode_le_u32s(&[1, 1, 2]);
        assert!(CsrView::new(&offsets_bytes, None, 2, 2).is_err());
    }

    #[test]
    fn new_rejects_offsets_n_mismatch_with_m() {
        let offsets_bytes = encode_le_u32s(&[0, 1, 2]);
        assert!(CsrView::new(&offsets_bytes, None, 2, 99).is_err());
    }

    #[test]
    fn new_rejects_truncated_edge_idx() {
        let offsets_bytes = encode_le_u32s(&[0, 1, 2]);
        let short_edges = vec![0u8; 4]; // claims m=2 but only 1 entry worth of bytes
        assert!(CsrView::new(&offsets_bytes, Some(&short_edges), 2, 2).is_err());
    }

    #[test]
    fn lookup_out_of_range_symbol_is_empty_never_panics() {
        let offsets_bytes = encode_le_u32s(&[0, 1, 2]);
        let edge_bytes = encode_le_u32s(&[10, 20]);
        let view = CsrView::new(&offsets_bytes, Some(&edge_bytes), 2, 2).unwrap();
        assert!(view.neighbors(2).collect::<Vec<_>>().is_empty());
        assert!(view.neighbors(u32::MAX).collect::<Vec<_>>().is_empty());
    }

    #[test]
    fn lookup_non_monotone_offsets_is_empty_never_panics() {
        // offsets[0]=0, offsets[1]=5 (> m=2!), offsets[2]=2 — start > end
        // for group 0 is not the failure here; group 0 is start=0,end=5
        // which is > m so it's rejected; group 1 is start=5,end=2: start>end.
        let offsets_bytes = encode_le_u32s(&[0, 5, 2]);
        let edge_bytes = encode_le_u32s(&[10, 20]);
        let view = CsrView::new(&offsets_bytes, Some(&edge_bytes), 2, 2).unwrap();
        assert!(view.neighbors(0).collect::<Vec<_>>().is_empty());
        assert!(view.neighbors(1).collect::<Vec<_>>().is_empty());
    }

    #[test]
    fn lookup_skips_corrupt_edge_idx_values_at_or_past_m() {
        // m=2 (two valid edges, 0 and 1), but the stored bytes hold a
        // corrupt third value (99, >= m) mixed into group 0 — §7:
        // "decoded edge_idx values >= m skipped", never returned, never
        // a panic.
        let offsets_bytes = encode_le_u32s(&[0, 2]);
        let edge_bytes = encode_le_u32s(&[0, 99]);
        let view = CsrView::new(&offsets_bytes, Some(&edge_bytes), 1, 2).unwrap();
        assert_eq!(view.neighbors(0).collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn empty_view_len_and_edge_count() {
        let offsets_bytes = encode_le_u32s(&[0]);
        let view = CsrView::new(&offsets_bytes, None, 0, 0).unwrap();
        assert!(view.is_empty());
        assert_eq!(view.len(), 0);
        assert_eq!(view.edge_count(), 0);
    }

    // -----------------------------------------------------------------
    // Proptest oracle equivalence (§8): for every symbol s, the CSR
    // group must equal the v8 FST posting list, element for element.
    // `n` itself is varied (including 0) so empty groups and boundary
    // keys get property-tested, not just a fixed symbol count.
    // -----------------------------------------------------------------

    /// `(n, keys)` where every key is `< n` by construction (`n == 0`
    /// forces an empty `keys`, since there is no valid key when there
    /// are no symbols) — this keeps the generated input always valid,
    /// so the proptests below exercise the CSR-vs-oracle equivalence
    /// itself, not `build_csr`'s out-of-range debug_assert trip-wire
    /// (covered separately and directly above).
    fn n_and_keys_strategy() -> impl proptest::strategy::Strategy<Value = (u32, Vec<u32>)> {
        use proptest::strategy::{Just, Strategy};
        (0u32..64).prop_flat_map(|n| {
            let keys = if n == 0 {
                Just(Vec::new()).boxed()
            } else {
                proptest::collection::vec(0..n, 0..200).boxed()
            };
            (Just(n), keys)
        })
    }

    proptest::proptest! {
        /// `build_csr` (callees shape) vs `legacy_v8::build_u32_keyed_fst`
        /// (the v8 callees-FST oracle) over random `(caller, idx)` edge
        /// lists and a random symbol count `n` — every group must match
        /// element for element (F4: ascending edge_idx within a group).
        #[test]
        fn callees_csr_matches_legacy_v8_oracle((n, keys) in n_and_keys_strategy()) {
            let oracle_entries: Vec<(u32, u32)> = keys
                .iter()
                .enumerate()
                .map(|(i, &k)| (k, i as u32))
                .collect();
            let (oracle_fst, oracle_posts) =
                crate::store::legacy_v8::build_u32_keyed_fst(oracle_entries).unwrap();
            let oracle_reader =
                crate::store::legacy_v8::FstOracleReader::new(&oracle_fst, &oracle_posts).unwrap();

            let (offsets, edge_idx) = build_csr(&keys, n).unwrap();
            let offsets_bytes = encode_le_u32s(&offsets);
            let edge_bytes = encode_le_u32s(&edge_idx);
            let m = edge_idx.len() as u32;
            let view = CsrView::new(&offsets_bytes, Some(&edge_bytes), n, m).unwrap();

            for s in 0..n {
                let csr_group: Vec<u32> = view.neighbors(s).collect();
                let oracle_group = oracle_reader.find_decimal_key(s);
                proptest::prop_assert_eq!(csr_group, oracle_group, "diverged for symbol {}", s);
            }
        }

        /// `build_csr_offsets_sorted` (ref_edges identity shape) vs
        /// `legacy_v8::build_ref_edges_fst` over random sorted key lists
        /// and a random symbol count `n` — same element-for-element
        /// equivalence, but the identity range stands in for the
        /// (elided) `edge_idx` array.
        #[test]
        fn ref_edges_csr_matches_legacy_v8_oracle((n, mut keys) in n_and_keys_strategy()) {
            keys.sort_unstable();

            let oracle_entries: Vec<(u32, u32)> = keys
                .iter()
                .enumerate()
                .map(|(i, &k)| (k, i as u32))
                .collect();
            let (oracle_fst, oracle_posts) =
                crate::store::legacy_v8::build_ref_edges_fst(&oracle_entries).unwrap();
            let oracle_reader =
                crate::store::legacy_v8::FstOracleReader::new(&oracle_fst, &oracle_posts).unwrap();

            let offsets = build_csr_offsets_sorted(&keys, n).unwrap();
            let offsets_bytes = encode_le_u32s(&offsets);
            let m = *offsets.last().unwrap();
            let view = CsrView::new(&offsets_bytes, None, n, m).unwrap();

            for s in 0..n {
                let csr_group: Vec<u32> = view.neighbors(s).collect();
                let oracle_group = oracle_reader.find_decimal_key(s);
                proptest::prop_assert_eq!(csr_group, oracle_group, "diverged for symbol {}", s);
            }
        }
    }
}
