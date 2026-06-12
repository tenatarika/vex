#![no_main]

//! Fuzz the Phase 14.10 `rename_chains` sidecar parser
//! (`<index_dir>/index.rename_chains`, magic `VEXR` v1).
//!
//! The sidecar is opened at every `vex history <Symbol>` (chain
//! expansion) and `vex status` invocation. A malformed sidecar must
//! NEVER panic — corruption returns `Ok(None)` (degrade to singleton
//! chains) or `Err(SidecarError::*)` (caller drops the file).
//!
//! Surfaces under test: 48-byte header parse with hand-packed
//! `#[repr(C)]` field offsets, magic + version + stale-guard checks,
//! `forward_count` / `chain_count` / `member_count` bounds, three
//! mmap-backed `from_raw_parts` slices (ForwardEntry / ChainTableEntry
//! / u32 members), per-chain `member_offset + member_count`
//! validation at open time. The reader transmutes slices to
//! `'static` lifetime backed by the owned `Mmap`; any panic on
//! arbitrary input would surface a real safety hole.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    vex::store::rename_chains::__fuzz_rename_chains_bytes(data);
});
