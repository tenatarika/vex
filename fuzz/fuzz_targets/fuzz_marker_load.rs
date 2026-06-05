#![no_main]

//! Fuzz the v1.13.0 P2 marker-cache parser (`<onnx>.sha256.marker`).
//!
//! The marker is a text sidecar holding `(mtime_ns, size, sha256_hex)`
//! next to the ONNX file. `verify_with_marker` trusts the recorded SHA
//! when the on-disk file's mtime + size match, skipping the 86 MiB
//! rehash. A malicious or corrupted marker must NEVER panic — the
//! parser's contract is "any error → fall through to slow path", and
//! the slow path itself is the trust anchor.
//!
//! Goal: no panics, no UB, no out-of-bounds reads on any byte
//! sequence the parser is fed. Same risk class as the v1.12.0 bloom
//! sidecar harness, which surfaced two real defects in 60 seconds.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    vex::embed::integrity::__fuzz_marker_bytes(data);
});
