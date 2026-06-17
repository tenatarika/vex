#![no_main]

//! Fuzz the v1.18 (audit C1) incremental-state sidecar parser
//! (`<index_dir>/index.state`, magic `VEXS` v1).
//!
//! The sidecar is opened on every `Manifest::load` (i.e. every `vex
//! update`, `vex status`, `vex search` against a stale index). A
//! malformed sidecar must NEVER panic — corruption returns `Err` and
//! the loader logs a warning + falls back to the JSON manifest's
//! inline state fields.
//!
//! Surfaces under test: 12-byte header (magic + version + payload_len),
//! `MAX_PAYLOAD_BYTES` cap before the `Vec<u8>` allocation,
//! bincode-decoded payload (`IncrementalState`). Every other binary
//! sidecar in vex has shipped a fuzz target; this closes the gap.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    vex::index::incremental_state::__fuzz_state_bytes(data);
});
