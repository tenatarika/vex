#![no_main]

//! v9 CSR format (`docs/V9-FORMAT.md` §7, `fuzz_csr`) — fuzz
//! `vex::store::csr::CsrView::new` with arbitrary `offsets` / `edge_idx`
//! bytes and arbitrary `n` / `m`, in both the callees shape (`edge_idx`
//! present) and the `ref_edges` identity shape (`edge_idx` absent).
//!
//! `CsrView` never casts bytes to `&[u32]` and never panics on
//! malformed input — `new` returns `Err` for anything structurally
//! wrong, and `neighbors` degrades to an empty iterator for any
//! out-of-range symbol, non-monotone offsets, or corrupt edge_idx
//! value. This target pins that contract against byte soup a real
//! mmap could never legitimately contain.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct CsrInput {
    offsets_bytes: Vec<u8>,
    edge_idx_bytes: Vec<u8>,
    /// Whether to exercise the callees shape (`Some(edge_idx_bytes)`) or
    /// the ref_edges identity shape (`None`).
    use_edge_idx: bool,
    n: u32,
    m: u32,
    queries: Vec<u32>,
}

fuzz_target!(|input: CsrInput| {
    let edge_idx_bytes = if input.use_edge_idx {
        Some(input.edge_idx_bytes.as_slice())
    } else {
        None
    };

    let view =
        match vex::store::csr::CsrView::new(&input.offsets_bytes, edge_idx_bytes, input.n, input.m)
        {
            Ok(v) => v,
            Err(_) => return,
        };

    for &s in &input.queries {
        // Must never panic or allocate unboundedly — cap the collected
        // length defensively in case of a pathological (but still
        // "valid by new()'s checks") huge group.
        let mut count = 0usize;
        for _idx in view.neighbors(s) {
            count += 1;
            if count > 1_000_000 {
                break;
            }
        }
    }

    // Edge cases mirroring fuzz_refs_fst / fuzz_unresolved_refs.
    let _ = view.neighbors(0).count();
    let _ = view.neighbors(u32::MAX).count();
    let _ = view.len();
    let _ = view.is_empty();
    let _ = view.edge_count();
});
