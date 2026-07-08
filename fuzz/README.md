# vex fuzzing

`cargo-fuzz` (libFuzzer + ASAN) targets covering the two classes of
untrusted input vex handles: **adversarial source text** driven through the
parse → extract → bind path, and **malformed on-disk index sidecars** driven
through their binary loaders.

## Targets

| Target | Exercises |
|---|---|
| `fuzz_kotlin_binder` | Kotlin source → parse → `extract_symbols_and_imports` → `bind_refs` (the full binder walk) |
| `fuzz_pattern_parser` | `vex pattern` AST-pattern parser |
| `fuzz_tokenize_document` | body-token / document tokenizer |
| `fuzz_symbol_fst` / `fuzz_refs_fst` | symbol + reference FST loaders |
| `fuzz_index_reader` | mmap index reader (header + sections) |
| `fuzz_bloom_load` | bloom-filter sidecar loader |
| `fuzz_manifest_load` | `Manifest` JSON loader |
| `fuzz_state_load` | `index.state` (`IncrementalState`) bincode sidecar |
| `fuzz_hash_index_load` | history/hash-index sidecar |
| `fuzz_marker_load` | embedder integrity marker |
| `fuzz_rename_chains_load` | rename-chains sidecar |
| `fuzz_incremental_hnsw` | incremental HNSW graph sidecar |
| `fuzz_unresolved_refs` | cross-repo unresolved-refs section |

Source-text targets drive the public parse API; loader targets use the
`#[doc(hidden)] pub fn __fuzz_*` shim pattern to reach `pub(crate)` loaders
without widening the API.

## Running

`cargo-fuzz` needs a nightly toolchain and libFuzzer's sanitizer runtime.

```bash
# macOS: point the linker at LLVM's clang_rt (CLT clang drifts version dirs).
# Adjust the clang major version to match `ls /opt/homebrew/opt/llvm/lib/clang/`.
export LIBRARY_PATH=/opt/homebrew/opt/llvm/lib/clang/22/lib/darwin

# This repo's `cargo` is Homebrew (not a rustup shim), so `cargo +nightly`
# fails with "no such command" — use `rustup run nightly` instead.
rustup run nightly cargo fuzz run fuzz_kotlin_binder -- -max_total_time=120 -rss_limit_mb=2048

# Whole suite (120 s each):
for t in $(rustup run nightly cargo fuzz list); do
  rustup run nightly cargo fuzz run "$t" -- -max_total_time=120 -rss_limit_mb=2048
done
```

Notes:
- `fuzz/corpus/`, `fuzz/artifacts/`, `fuzz/target/`, and `fuzz/Cargo.lock` are
  git-ignored (local-only). `fuzz/findings/` (minimized regression inputs used
  by unit tests, e.g. `kotlin-grammar-oom.bin`) **is** tracked.
- When capturing a run's output, do **not** pipe through `tail -N`: libFuzzer
  prints the `==ERROR` / panic banner *before* the trailing artifact lines, so
  a tail drops the actual failure. Redirect the full output to a file and grep.
- A saved crash artifact may replay clean in isolation
  (`cargo fuzz run <target> <artifact>`) when the trigger is parser-pool warmth
  (a *sequence* of inputs). Reproduce by replaying the whole corpus through one
  thread instead.

## Findings on record

- **`parse_text` callback budget (v1.23.0-line).** A 451-byte Kotlin input drove
  tree-sitter-kotlin-ng's GLR error recovery to 334 s / >2 GB. `parser_pool::
  parse_text` now caps progress-callback invocations (scaled by input size) and
  bails as `Err`. Regression input: `fuzz/findings/kotlin-grammar-oom.bin`
  (unit test `parser_pool::parse_text_bails_on_pathological_input`).
- **Out-of-range `utf8_text` panic (v1.23.0).** tree-sitter can emit a node whose
  byte range runs one past EOF on malformed input; `Node::utf8_text` panicked
  slicing out of range (`.unwrap_or("")` / `.ok()` cannot catch it). Fixed by
  the bounds-checked `crate::parse::NodeTextExt` accessors used at every
  node-text read site; regression test `parse::node_text_tests`.

## Status

Last full pass (2026-07-08, v1.23.0): **all 14 targets clean at 120 s each**
(~15.4 M executions total), no panics / OOM / leaks.
