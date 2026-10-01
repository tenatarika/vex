#![no_main]

//! V9-FORMAT P3 (`docs/V9-FORMAT.md` §7, §13 "Tests" — `fuzz_leiden`) —
//! drives `vex::cluster::leiden::__fuzz_leiden_bytes` with arbitrary
//! bytes: decodes a <= 256-node graph, runs deterministic Leiden-CPM
//! twice and asserts identical output plus "every cluster induces a
//! connected subgraph" (the shim itself panics via `assert_eq!` /
//! `assert!` on a violation, which libfuzzer reports as a crash).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    vex::cluster::leiden::__fuzz_leiden_bytes(data);
});
