#![no_main]

//! Fuzz the Kotlin scope binder end-to-end: raw bytes → tree-sitter parse →
//! symbol extraction → `bind_refs`. The binder (`src/parse/scope/kotlin.rs`)
//! walks the Kotlin AST emitting refs and bindings; it touches many node
//! kinds (imports incl. `as`-aliases, classes/objects/companions, enum
//! entries, lambdas, primary/secondary constructors, `${…}` string
//! interpolation) and uses index-based child skipping and raw-text scans —
//! all worth pressure-testing against malformed / adversarial source.
//!
//! Goal: no panics on any UTF-8 input up to libfuzzer's size cap. The binder
//! is expected to return `Err` (or empty refs) on garbage; `Err` is fine,
//! panic is not. Mirrors `fuzz_pattern_parser` — both drive a source-text
//! parser through the public API rather than a binary-loader shim.

use libfuzzer_sys::fuzz_target;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;
use vex::parse::scope::bind_refs;

fuzz_target!(|data: &[u8]| {
    // Reject non-UTF8 cheaply so the fuzzer spends its budget on Kotlin
    // grammar / binder-walk mutations rather than UTF-8 boundary noise.
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    // Extract symbols first (the binder resolves refs against them), then
    // run the full bind. Both steps reparse with the Kotlin grammar; either
    // may return Err on malformed input — only a panic is a finding.
    if let Ok((symbols, _imports)) = extract_symbols_and_imports(input, Language::Kotlin) {
        let _ = bind_refs(input, Language::Kotlin, &symbols);
    }
});
