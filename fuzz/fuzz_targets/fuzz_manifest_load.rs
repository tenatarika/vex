#![no_main]

//! Fuzz `Manifest::load` — the JSON manifest reader that vex consults
//! on every search-shaped command (staleness check, semantic embedder
//! lookup, section-opt-out replay). The file sits next to `index.vex`
//! and is editable by anyone with write access to the cache dir, so
//! crafted bytes must not panic the CLI before it can decide whether
//! to ignore them and fall through.
//!
//! Goal: any input results in either `Ok(Manifest)` or `Err`; no
//! panics deep in serde or our deserialize helpers.

use libfuzzer_sys::fuzz_target;
use std::io::Write as _;

use vex::index::manifest::Manifest;

fuzz_target!(|data: &[u8]| {
    // Reuse a single tmp file across iterations — libfuzzer hammers
    // this at millions/sec, so allocator/inode pressure matters.
    let path = std::env::temp_dir().join("__vex_fuzz_manifest.json");
    {
        let Ok(mut f) = std::fs::File::create(&path) else {
            return;
        };
        if f.write_all(data).is_err() {
            return;
        }
    }
    let _ = Manifest::load(&path);
});
