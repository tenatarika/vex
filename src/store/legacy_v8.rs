//! Oracle module for the v8 on-disk encodings that the v9 CSR migration
//! (`docs/V9-FORMAT.md`) deletes from production code.
//!
//! P0 (`docs/V9-FORMAT.md` §9/§13 R20): before the callees FST and the
//! `ref_edges` FST get replaced by [`super::csr`] in a later phase, this
//! module preserves a **faithful copy** of the decimal-string-key
//! encoding they used — `build_u32_keyed_fst` (`call_graph.rs`),
//! `encode_caller_key` / `encode_caller_key_into` (`call_graph.rs`) and
//! `encode_to_sym_key` (`ref_edges.rs`) — so it keeps working as an
//! oracle after those production functions are gone. Production code is
//! untouched by this module; on-disk output is unchanged.
//!
//! Used by:
//! - the P1 `csr` module's proptests, to assert that a CSR group equals
//!   the v8 FST posting list element for element;
//! - the P0 golden tests in `tests/legacy_v8_golden_test.rs`, via
//!   [`build_sample_v8_index`] — a programmatic v8-index builder (no
//!   checked-in binary fixture, R20) that drives the real indexing
//!   pipeline (today's writer IS v8) over a tiny hand-written project.
//!
//! `#[doc(hidden)]`: test/bench-only surface, not part of the public API.
//!
//! This module *is* compiled into the release binary too — `main.rs`
//! duplicates `src/` via its own `mod store;` rather than depending on
//! the `vex` library crate, so every item here exists in both
//! compilation units. It is simply never *called* from `main()` or from
//! any production code path in either one; only `#[cfg(test)]` code and
//! `tests/legacy_v8_golden_test.rs` call it. That makes it dead code by
//! rustc's reachability analysis, which is exactly the same situation as
//! the `__fuzz_*` shims (e.g. `store::rename_chains::__fuzz_rename_chains_bytes`):
//! always compiled, `doc(hidden)`, kept alive only for tests/fuzzing.
//! The module-wide allow suppresses the resulting warning, mirroring
//! `parse/scope/mod.rs` and `index/rename_chains/mod.rs`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Zero-padded 10-digit decimal key encoder. Byte-identical to
/// `call_graph::encode_caller_key` (callees) and to
/// `ref_edges::encode_to_sym_key` (ref_edges), which used the same
/// format — kept as a single copy here since the oracle needs both.
pub fn encode_caller_key(n: u32) -> String {
    format!("{n:010}")
}

/// `ref_edges::encode_to_sym_key`. The v8 `ref_edges` FST used the exact
/// same 10-digit zero-padded encoding as the callees FST's caller key;
/// kept as a distinct name so oracle call sites read like the functions
/// they stand in for.
pub fn encode_to_sym_key(to_sym_idx: u32) -> String {
    encode_caller_key(to_sym_idx)
}

/// Stack-buffer encoder — exact copy of
/// `call_graph::encode_caller_key_into`. Same byte output as
/// [`encode_caller_key`] without the per-key `String` allocation.
pub fn encode_caller_key_into(buf: &mut [u8; 10], mut n: u32) {
    for slot in buf.iter_mut().rev() {
        *slot = b'0' + (n % 10) as u8;
        n /= 10;
    }
}

/// u32-keyed `Vec` → sorted FST — exact copy of
/// `call_graph::build_u32_keyed_fst`. This is the callees-FST oracle:
/// builds the v8 on-disk shape from `(key, edge_idx)` pairs so tests can
/// assert the CSR module's per-key groups equal these posting lists
/// element for element (F4: ascending edge_idx within a group).
pub fn build_u32_keyed_fst(mut entries: Vec<(u32, u32)>) -> Result<(Vec<u8>, Vec<u8>)> {
    entries.sort_unstable();

    let mut posting_data: Vec<u8> = Vec::with_capacity(entries.len() * 4 + entries.len());
    let mut fst_builder = fst::MapBuilder::memory();
    let mut key_buf = [b'0'; 10];

    let mut i = 0;
    while i < entries.len() {
        let key = entries[i].0;
        let mut j = i + 1;
        while j < entries.len() && entries[j].0 == key {
            j += 1;
        }
        let offset = posting_data.len() as u64;
        let group = &mut entries[i..j];
        // Dedup identical (key, edge_idx) pairs across the contiguous
        // group — mirrors the production builder exactly.
        let mut write = 0;
        for read in 0..group.len() {
            if write == 0 || group[read].1 != group[write - 1].1 {
                group.swap(read, write);
                write += 1;
            }
        }
        let count = write as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());
        for slot in group.iter().take(write) {
            posting_data.extend_from_slice(&slot.1.to_le_bytes());
        }
        encode_caller_key_into(&mut key_buf, key);
        fst_builder
            .insert(key_buf, offset)
            .context("fst insert (legacy_v8 callees oracle)")?;
        i = j;
    }

    let fst_bytes = fst_builder
        .into_inner()
        .context("finalize legacy_v8 callees-fst oracle")?;
    Ok((fst_bytes, posting_data))
}

/// `ref_edges::build_ref_edges_section`'s FST-building portion — exact
/// copy of the loop that turns already-sorted `(to_sym_idx, edge_idx)`
/// pairs (F5: the writer sorts `RefEdge` records by `to_sym_idx`, so
/// entries arrive pre-grouped, no extra sort needed here either) into
/// `(fst_bytes, posting_bytes)`. No dedup: v8 ref_edges postings are
/// never deduped — each `edge_idx` is the unique position of its record.
pub fn build_ref_edges_fst(entries: &[(u32, u32)]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut posting_data: Vec<u8> = Vec::new();
    let mut fst_builder = fst::MapBuilder::memory();
    let mut key_buf = [b'0'; 10];

    let mut i = 0;
    while i < entries.len() {
        let key = entries[i].0;
        let mut j = i + 1;
        while j < entries.len() && entries[j].0 == key {
            j += 1;
        }
        let offset = posting_data.len() as u64;
        let count = (j - i) as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());
        for slot in &entries[i..j] {
            posting_data.extend_from_slice(&slot.1.to_le_bytes());
        }
        encode_caller_key_into(&mut key_buf, key);
        fst_builder
            .insert(key_buf, offset)
            .context("fst insert (legacy_v8 ref-edges oracle)")?;
        i = j;
    }

    let fst_bytes = fst_builder
        .into_inner()
        .context("finalize legacy_v8 ref-edges-fst oracle")?;
    Ok((fst_bytes, posting_data))
}

/// Zero-copy reader over an oracle FST — the same shape and behaviour as
/// `call_graph::CallGraphFstReader`, kept as its own copy so this module
/// has no dependency on code a later phase may delete.
pub struct FstOracleReader<'a> {
    fst_map: fst::Map<&'a [u8]>,
    posting_data: &'a [u8],
}

impl<'a> FstOracleReader<'a> {
    pub fn new(fst_bytes: &'a [u8], posting_bytes: &'a [u8]) -> Result<Self> {
        let fst_map = fst::Map::new(fst_bytes)
            .map_err(|e| anyhow::anyhow!("fst load (legacy_v8 oracle): {e}"))?;
        Ok(Self {
            fst_map,
            posting_data: posting_bytes,
        })
    }

    /// Posting list for a zero-padded 10-digit decimal key. Empty when
    /// the key is absent or the posting bytes are truncated.
    pub fn find_decimal_key(&self, n: u32) -> Vec<u32> {
        let key = encode_caller_key(n);
        match self.fst_map.get(key.as_bytes()) {
            Some(offset) => self.read_posting_list(offset),
            None => Vec::new(),
        }
    }

    fn read_posting_list(&self, offset: u64) -> Vec<u32> {
        let offset = offset as usize;
        if offset + 4 > self.posting_data.len() {
            return Vec::new();
        }
        let count = u32::from_le_bytes(
            self.posting_data[offset..offset + 4]
                .try_into()
                .unwrap_or([0; 4]),
        ) as usize;
        let max_entries = self.posting_data.len().saturating_sub(offset + 4) / 4;
        let mut out = Vec::with_capacity(count.min(max_entries));
        let mut pos = offset + 4;
        for _ in 0..count {
            if pos + 4 > self.posting_data.len() {
                break;
            }
            let idx =
                u32::from_le_bytes(self.posting_data[pos..pos + 4].try_into().unwrap_or([0; 4]));
            out.push(idx);
            pos += 4;
        }
        out
    }
}

// ---------------------------------------------------------------------
// R20: programmatic v8-index builder for library-level golden tests.
// ---------------------------------------------------------------------

/// Writes a tiny multi-file Rust project under `project_root` and runs
/// the real indexing pipeline over it, producing a genuine v8 index —
/// no checked-in binary fixture (R20). `project_root` must already
/// exist and, on macOS, should be pre-canonicalized by the caller (the
/// pipeline and `util::config::index_path` must agree on the canonical
/// path — see the cache-path writer/reader-symmetry note).
///
/// Layout, chosen so callees, callers and `ref_edges` (the `--strict`
/// binder-resolved refs) are all non-empty:
/// - `src/a.rs` defines `helper_fn`.
/// - `src/b.rs` has `use crate::a::helper_fn;` and defines `caller_fn`, which
///   calls it — a cross-file, binder-resolved (`Imported`) reference,
///   plus a name-based call-graph edge.
/// - `src/lib.rs` declares `pub mod a; pub mod b;` — the Rust binder
///   needs the module tree declared to resolve `crate::a::helper_fn`
///   cross-file (see `tests/incremental_consistency_ref_edges.rs`).
///
/// Returns the index path (`util::config::index_path(project_root)`).
pub fn build_sample_v8_index(project_root: &Path) -> Result<PathBuf> {
    let src_dir = project_root.join("src");
    std::fs::create_dir_all(&src_dir).context("create src dir")?;
    std::fs::write(
        src_dir.join("a.rs"),
        "pub fn helper_fn() -> i32 {\n    42\n}\n",
    )
    .context("write a.rs")?;
    std::fs::write(
        src_dir.join("b.rs"),
        "use crate::a::helper_fn;\n\npub fn caller_fn() -> i32 {\n    helper_fn()\n}\n",
    )
    .context("write b.rs")?;
    std::fs::write(src_dir.join("lib.rs"), "pub mod a;\npub mod b;\n").context("write lib.rs")?;

    crate::index::pipeline::run(
        project_root,
        crate::index::pipeline::IndexOptions::default(),
        "minilm-l6-v2",
        &[],
    )
    .context("run indexing pipeline")?;

    Ok(crate::util::config::index_path(project_root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::call_graph::{
        build_callees_fst, encode_caller_key as prod_encode_caller_key, CallEdgeBuilder,
        CallGraphFstReader,
    };
    use crate::store::ref_edges::{build_ref_edges_section, RefEdgeBuilder};

    // -------------------------------------------------------------
    // Sanity: the oracle copy is byte-identical to the production
    // functions it stands in for, today. This is what makes it a
    // valid oracle — if these ever diverge, the copy is stale.
    // -------------------------------------------------------------

    #[test]
    fn encode_caller_key_matches_production() {
        for n in [0u32, 1, 42, 999_999, u32::MAX] {
            assert_eq!(encode_caller_key(n), prod_encode_caller_key(n));
        }
    }

    #[test]
    fn encode_caller_key_into_matches_production() {
        for n in [0u32, 1, 9, 10, 12345, u32::MAX] {
            let mut oracle_buf = [b'0'; 10];
            let mut prod_buf = [b'0'; 10];
            encode_caller_key_into(&mut oracle_buf, n);
            crate::store::call_graph::encode_caller_key_into(&mut prod_buf, n);
            assert_eq!(oracle_buf, prod_buf, "diverged for n={n}");
        }
    }

    #[test]
    fn build_u32_keyed_fst_matches_build_callees_fst() {
        let edges = vec![
            CallEdgeBuilder {
                caller_sym_idx: 5,
                callee_name: "alpha".into(),
                line: 1,
            },
            CallEdgeBuilder {
                caller_sym_idx: 5,
                callee_name: "beta".into(),
                line: 2,
            },
            CallEdgeBuilder {
                caller_sym_idx: 7,
                callee_name: "gamma".into(),
                line: 3,
            },
        ];
        let entries: Vec<(u32, u32)> = edges
            .iter()
            .enumerate()
            .map(|(i, e)| (e.caller_sym_idx, i as u32))
            .collect();

        let (prod_fst, prod_posts) = build_callees_fst(&edges).unwrap();
        let (oracle_fst, oracle_posts) = build_u32_keyed_fst(entries).unwrap();

        let prod_reader = CallGraphFstReader::new(&prod_fst, &prod_posts).unwrap();
        let oracle_reader = FstOracleReader::new(&oracle_fst, &oracle_posts).unwrap();

        for caller in [5u32, 7, 99] {
            assert_eq!(
                prod_reader.find(&prod_encode_caller_key(caller)),
                oracle_reader.find_decimal_key(caller),
                "diverged for caller={caller}"
            );
        }
    }

    #[test]
    fn build_ref_edges_fst_matches_production() {
        let edges = vec![
            RefEdgeBuilder {
                to_sym_idx: 2,
                from_file_id: 0,
                line: 10,
                col: 1,
                kind: 2,
            },
            RefEdgeBuilder {
                to_sym_idx: 2,
                from_file_id: 1,
                line: 20,
                col: 2,
                kind: 1,
            },
            RefEdgeBuilder {
                to_sym_idx: 4,
                from_file_id: 0,
                line: 30,
                col: 3,
                kind: 0,
            },
        ];
        let (_prod_edge_bytes, prod_fst, prod_posts) = build_ref_edges_section(&edges).unwrap();

        // Rebuild the same sorted (to_sym_idx, edge_idx) entries the
        // production builder derives internally, to feed the oracle.
        let mut sorted: Vec<&RefEdgeBuilder> = edges.iter().collect();
        sorted.sort_by_key(|e| (e.to_sym_idx, e.from_file_id, e.line, e.col));
        let entries: Vec<(u32, u32)> = sorted
            .iter()
            .enumerate()
            .map(|(idx, e)| (e.to_sym_idx, idx as u32))
            .collect();
        let (oracle_fst, oracle_posts) = build_ref_edges_fst(&entries).unwrap();

        let prod_reader = CallGraphFstReader::new(&prod_fst, &prod_posts).unwrap();
        let oracle_reader = FstOracleReader::new(&oracle_fst, &oracle_posts).unwrap();

        for sym in [2u32, 4, 99] {
            assert_eq!(
                prod_reader.find(&prod_encode_caller_key(sym)),
                oracle_reader.find_decimal_key(sym),
                "diverged for to_sym_idx={sym}"
            );
        }
    }

    // -------------------------------------------------------------
    // Parity proptests: the oracle builders vs the production ones
    // they stand in for, over RANDOM inputs (not just the handful of
    // hand-picked edges above). These exist only to prove the oracle
    // is faithful *today* — once P2 deletes the production FST-based
    // `build_callees_fst` / `build_ref_edges_section` FST path, there
    // is nothing left to compare against, so these two proptests are
    // deleted alongside them (they are not part of the CSR-vs-oracle
    // equivalence proptests in `store::csr`, which survive P2).
    // -------------------------------------------------------------

    proptest::proptest! {
        #[test]
        fn callees_oracle_matches_production_over_random_edges(
            raw in proptest::collection::vec(
                (0u32..20, "[a-z]{1,8}", 0u32..1000),
                0..50,
            )
        ) {
            let edges: Vec<CallEdgeBuilder> = raw
                .iter()
                .map(|(caller, name, line)| CallEdgeBuilder {
                    caller_sym_idx: *caller,
                    callee_name: name.clone(),
                    line: *line,
                })
                .collect();
            let entries: Vec<(u32, u32)> = edges
                .iter()
                .enumerate()
                .map(|(i, e)| (e.caller_sym_idx, i as u32))
                .collect();

            let (prod_fst, prod_posts) = build_callees_fst(&edges).unwrap();
            let (oracle_fst, oracle_posts) = build_u32_keyed_fst(entries).unwrap();
            let prod_reader = CallGraphFstReader::new(&prod_fst, &prod_posts).unwrap();
            let oracle_reader = FstOracleReader::new(&oracle_fst, &oracle_posts).unwrap();

            for caller in 0u32..20 {
                proptest::prop_assert_eq!(
                    prod_reader.find(&prod_encode_caller_key(caller)),
                    oracle_reader.find_decimal_key(caller),
                    "diverged for caller={}", caller
                );
            }
        }

        #[test]
        fn ref_edges_oracle_matches_production_over_random_edges(
            raw in proptest::collection::vec(
                (0u32..20, 0u32..10, 0u32..1000, 0u32..200, 0u8..4),
                0..50,
            )
        ) {
            let edges: Vec<RefEdgeBuilder> = raw
                .iter()
                .map(|&(to, file, line, col, kind)| RefEdgeBuilder {
                    to_sym_idx: to,
                    from_file_id: file,
                    line,
                    col,
                    kind,
                })
                .collect();
            if edges.is_empty() {
                // `build_ref_edges_section` special-cases zero edges by
                // returning literally empty byte vectors (not a valid
                // minimal FST, unlike `build_callees_fst`'s always-build-
                // through-the-FST-builder path) — nothing to compare.
                return Ok(());
            }
            let (_prod_edge_bytes, prod_fst, prod_posts) = build_ref_edges_section(&edges).unwrap();

            // Rebuild the same sorted (to_sym_idx, edge_idx) entries the
            // production builder derives internally, to feed the oracle.
            let mut sorted: Vec<&RefEdgeBuilder> = edges.iter().collect();
            sorted.sort_by_key(|e| (e.to_sym_idx, e.from_file_id, e.line, e.col));
            let entries: Vec<(u32, u32)> = sorted
                .iter()
                .enumerate()
                .map(|(idx, e)| (e.to_sym_idx, idx as u32))
                .collect();
            let (oracle_fst, oracle_posts) = build_ref_edges_fst(&entries).unwrap();

            let prod_reader = CallGraphFstReader::new(&prod_fst, &prod_posts).unwrap();
            let oracle_reader = FstOracleReader::new(&oracle_fst, &oracle_posts).unwrap();

            for sym in 0u32..20 {
                proptest::prop_assert_eq!(
                    prod_reader.find(&prod_encode_caller_key(sym)),
                    oracle_reader.find_decimal_key(sym),
                    "diverged for to_sym_idx={}", sym
                );
            }
        }
    }
}
