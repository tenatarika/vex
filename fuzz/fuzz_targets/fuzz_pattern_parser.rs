#![no_main]

//! Fuzz `parse_composite_pattern` — the user-facing pattern parser
//! behind `vex pattern '...'`. The input is a raw UTF-8 string fed
//! from the CLI, so any panic / non-graceful failure is reachable by
//! a single bad invocation. Phase 11.4 introduced metavar substitution
//! (`$X`, `$$ARGS`, `$$$BODY`), composition (`&&`, `||`), and
//! string/comment-aware delimiter splitting — all custom code paths
//! worth pressure-testing.
//!
//! Goal: no panics, no allocator abuse, no infinite loops on any
//! UTF-8 string up to libfuzzer's default size cap. The parser is
//! expected to return `Err` on malformed input; `Err` is fine, panic
//! is not.

use libfuzzer_sys::fuzz_target;
use vex::parse::language::Language;
use vex::pattern::matcher::parse_composite_pattern;

fuzz_target!(|data: &[u8]| {
    // The parser takes &str — reject non-UTF8 inputs cheaply so the
    // fuzzer concentrates on parse-logic mutations rather than UTF-8
    // boundary noise (which `std::str::from_utf8` already handles).
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    // A fixed language pins the surface; the parser is language-agnostic
    // at this layer (Language only flows through to AST matching).
    let _ = parse_composite_pattern(input, Language::Rust);
});
