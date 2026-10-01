#![no_main]

//! V9-FORMAT P4a (`docs/V9-FORMAT.md` §7, §13 "Tests" — `fuzz_cluster_section`)
//! — fuzz `ClusterSectionReader` against an arbitrary byte soup opened as
//! an index file, mirroring `fuzz_index_reader.rs`'s pattern: write
//! arbitrary bytes to a temp file, try `IndexReader::open`, and on
//! success exercise every `ClusterSectionReader` entry point.
//!
//! `IndexReader::open`'s own structural cluster-section checks (bounds,
//! `assign_len == 4 * symbol_count`, `table_len % ClusterRecord::SIZE ==
//! 0`) already reject plenty of adversarial headers before this target
//! ever reaches `cluster_section_reader()`; the semantic checks inside
//! `ClusterSectionReader::new` (resolution_den != 0, a sane `k`) cover
//! the rest. Goal: no panics, no UB, no out-of-bounds reads — a bad
//! section degrades to `None`/empty, never an error or a crash.

use libfuzzer_sys::fuzz_target;
use std::io::Write;

static FUZZ_DIR: std::sync::LazyLock<tempfile::TempDir> =
    std::sync::LazyLock::new(|| tempfile::tempdir().unwrap());

fuzz_target!(|data: &[u8]| {
    let path = FUZZ_DIR.path().join("fuzz_cluster.vex");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(data).unwrap();
    }

    let reader = match vex::store::reader::IndexReader::open(&path) {
        Ok(r) => r,
        Err(_) => return,
    };

    let _ = reader.has_clusters();

    let Some(csr) = reader.cluster_section_reader() else {
        return;
    };

    let summary = csr.summary();
    let k = csr.k().min(1000); // cap so a crafted huge k can't blow up the loop
    for ord in 0..k {
        let _ = csr.record(ord);
        let members = csr.members(ord);
        if members.len() > 1_000_000 {
            break; // defensive cap, mirrors fuzz_csr's neighbor-count guard
        }
    }
    // One past the end, and the sentinel-adjacent values, must degrade
    // cleanly rather than panic.
    let _ = csr.record(k);
    let _ = csr.record(usize::MAX);
    let _ = csr.members(usize::MAX);

    let n = reader.symbol_count().min(1000);
    for sym_idx in 0..n as u32 {
        let _ = csr.status(sym_idx);
    }
    let _ = csr.status(u32::MAX);
    let _ = summary;
});
