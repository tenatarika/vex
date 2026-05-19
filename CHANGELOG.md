# Changelog

All notable changes to vex are documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [1.6.4] - 2026-05-19

Feature + UX polish release. Headline change is the new indexing-time
opt-out flags (`--no-call-graph` / `--no-bm25`) that let monorepos and
CI hot-paths skip the call-graph and BM25 build steps that v1.5.0
added unconditionally — recovering most of the v1.4.3-era indexing
speed when the user does not need those search channels.

### Added
- **`vex index --no-call-graph` / `--no-bm25`** (also on `update` and `watch`). v1.5.0 made `vex index` ~6× slower than v1.4.3 because the persistent call-graph and BM25 sections were always built. Users who only need structural + vector search can now opt out per-build or globally via `.vex.toml`. With `--no-call-graph` set, `vex callers` / `vex callees` transparently fall back to the live tree-sitter scan they always supported; with `--no-bm25`, hybrid search drops the BM25 channel and uses structural (+ semantic if enabled). The opt-out is recorded in the manifest so a subsequent `vex update` does not silently re-add the section.
- **`.vex.toml` keys `call_graph` and `bm25`** for project-wide opt-out. Resolution precedence: CLI `--no-...` flag > `.vex.toml` > previous manifest > default (true).
- **`vex callers` / `vex callees` accept `--auto-update` and `--no-stale-check`**, symmetric with the other index-backed commands. With `--auto-update`, a missing index is bootstrapped on first call so the persistent call-graph FST (~4 ms) becomes available immediately; the existing live-scan fallback is preserved when no index and no auto-update are in play. MCP wrappers gained the matching `auto_update` schema field.

### Fixed
- **`auto_update`-driven bootstrap no longer re-runs the staleness probe** on the freshly built index. A just-bootstrapped manifest is guaranteed `Freshness::Fresh`, so the manifest read + git HEAD probe that fired immediately after the bootstrap was pure waste. Behaviourally invisible; just trims the first-search latency on `auto_update = true` projects.
- **`IndexReader::open` errors now carry the index file path** in every `bail!` and `File::open`/`Mmap::map` context. Previously a corrupt or version-mismatched index surfaced as a path-less "index file is corrupted (bad magic)" — users had to grep stderr or read source to find the file. Same treatment for the embedder-mismatch and stale-embedder-switch bails (now include the manifest path), and the cache-dir `..`-traversal warning (now shows both the raw config value and the tilde-expanded form).
- **`vex callers` / `vex callees` live-scan fallback reason is now printed via `eprintln!`** when an index exists but fails to open. Previously logged via `tracing::warn!` which `RUST_LOG` hides by default — users saw the ~seconds live-scan latency without knowing why the fast path was skipped.
- **Bootstrap `pipeline::run` failure now includes the project root** via `.with_context()`. Bare pipeline errors during first-run bootstrap no longer drop the root path.
- **CI release-zip Deflate guard** introduced in v1.6.3 contained an f-string syntax error inside a PowerShell here-string that crashed on the next tag push. The dict lookup is now pulled into a local variable so the f-string only references a bare identifier. (Affects only the release pipeline; published binaries on v1.6.3 are unchanged.)

### Internal
- New `pipeline::IndexOptions { with_embeddings, with_call_graph, with_bm25 }` struct replaces the bare `with_embeddings: bool` parameter on `pipeline::run` / `pipeline::update`. Default is `with_embeddings = false, with_call_graph = true, with_bm25 = true`. All seven library-test callsites migrated to `IndexOptions::default()`.
- `Manifest` gains `call_graph: Option<bool>` and `bm25: Option<bool>` fields (back-compat: `None` on pre-10.4 manifests is treated as enabled).
- New `resolve_section_enabled(cli_no_flag, cfg_value, manifest_value) -> bool` helper in `cli::mod` encodes the precedence rule in one place.
- `ensure_index_exists` now returns `IndexAvail { path, just_bootstrapped }` instead of bare `PathBuf`. A new `ensure_index_ready` wrapper composes ensure + staleness, skipping the latter on a fresh bootstrap. Six existing call sites (search/show/usages/check/similar/duplicates) collapsed from a pair of calls to a single helper.
- `cmd_callgraph` gains `auto_update`, `no_stale_check`, `local_cache_active`, `cfg` parameters and a bootstrap-or-staleness block that runs before the existing fast-path / live-scan branching. The live-scan fallback when neither an index nor auto-update is available is preserved exactly.
- `Update` / `Watch` arms canonicalize `root` once at the top of the arm and `?`-propagate `Manifest::load` errors so a partially-written manifest does not silently re-enable opted-out sections.
- 11 new tests: 4 integration tests for callers/callees auto-update bootstrap and live-scan fallback, 4 integration tests for the opt-out persistence + sticky-update invariant, 2 behavioural tests proving callers/search still work with the opt-out, 1 unit test suite (4 cases) for `resolve_section_enabled` covering all precedence levels.

## [1.6.3] - 2026-05-19

Windows-only patch for the `vex self-update` failure reported on v1.6.2.

### Fixed
- **Windows zip archive now uses standard Deflate (method 8) instead of Deflate64.** PowerShell's `Compress-Archive` on the `windows-latest` runner emits Deflate64 in some configurations — .NET can decompress it, but the `zip` crate inside `self_update 0.42` cannot, surfacing as `Compression method not supported` when `vex self-update` tries to extract the new release. Release packaging now invokes `7z a -tzip -mm=Deflate` (preinstalled on `windows-latest`) which forces compatible compression. zipsign continues to preserve the method, so signed archives are also Deflate-compressed end-to-end.
- **Migration**: v1.6.1 → v1.6.2 self-update on Windows failed for users on v1.6.1 trying to land on v1.6.2 (and v1.6.0 → v1.6.1 in retrospect was likely affected too). v1.6.2 → v1.6.3 should self-update normally because v1.6.3 ships standard Deflate, which the existing v1.6.2 binary can read. If `vex self-update` from v1.6.2 still fails for any reason, download `vex-x86_64-pc-windows-msvc.zip` from the v1.6.3 release page once — from 1.6.3 onwards updates are clean.

## [1.6.2] - 2026-05-19

UX/perf hotfix on top of v1.6.1, motivated by Windows users hitting two
sharp edges as soon as they tried `auto_update = true` in `.vex.toml`.

### Fixed
- **`auto_update = true` now bootstraps a missing index on first use.** The previous code path bailed with "No index found" before the auto-update logic ever ran, and `handle_staleness` could only do incremental updates anyway. A bare `vex search` / `show` / `usages` / `check` / `similar` / `duplicates` in a fresh project will now print "Bootstrapping…" and run the equivalent of `vex index` before continuing. `Similar` and `Duplicates` bootstrap with `--semantic` automatically since they require embeddings
- **Improved "no index" error message** when `auto_update` is *not* set — now lists both fixes: run `vex index` or set `auto_update = true` in `.vex.toml`
- **MiniLM embedding model is now cached globally**, not in `./.fastembed_cache/` relative to the working directory. The ~86 MB ONNX model lives at `<platform-cache>/vex/embeddings/` and is shared across every project. Previously every project re-downloaded the same model and dropped a `.fastembed_cache/` folder into the project tree. Existing `.fastembed_cache/` directories inside user projects can be deleted safely — the model will not be re-downloaded
- **`local_cache = true` + bootstrap now writes the `.gitignore` safeguard**. The auto-bootstrap path previously skipped the project-`.gitignore` creation that the explicit `vex index` command performs, so users with both flags set could end up committing the binary index. Now the bootstrap mirrors the `Commands::Index` behaviour
- **Semantic bootstrap announces the model download.** `vex similar` / `vex duplicates` on a fresh machine print "Note: first semantic index downloads the MiniLM ONNX model (~86 MB)…" before fastembed's progress bar appears
- **Clearer embed cache errors.** `MiniLMEmbedder::new` now wraps `create_dir_all` with `.with_context(...)`, so a failed cache-dir create surfaces with the actual path instead of hiding behind fastembed's generic "failed to load MiniLM" message

### Fixed (CI)
- Replaced `orhun/git-cliff-action@v3` with a direct install via `taiki-e/install-action`. The Docker image used by the action was based on debian:buster which is EOL — `apt-get update` returned 404 and the release job failed before the body could be generated. v1.6.1 release shipped via this fix; documenting here for the changelog trail

### Internal
- New `ensure_index_exists(root, auto_update_flag, needs_semantic, local_cache_active, cfg)` helper in `cli::mod` replaces the same six-line `if !index_path.exists() { bail!(...) }` block that appeared in every index-backed command. Returns the resolved `index.vex` path so callers do not recompute it
- New `util::config::embed_cache_dir()` returns `<cache-root>/embeddings/` without appending the project-hash subdir — every project that shares the platform cache root also shares the model. Users on `local_cache = true` consciously opt in to a portable layout and pay the per-project cost
- 9 new CLI integration tests in `tests/cli_bootstrap_test.rs` drive the actual binary via `assert_cmd`. Cover the auto-bootstrap path, the helpful-error path, the `.gitignore` safeguard, path-traversal rejection end-to-end, the `--no-stale-check` interaction, and the `--check` / `--yes` mutual exclusion. Each test scopes its cache to a tempdir so the shared platform cache is never touched

## [1.6.1] - 2026-05-18

Follow-up patch for v1.6.0. Adds the `vex self-update` subcommand that
Windows users needed (no Homebrew tap), and fixes the two Windows path
issues that surfaced once CI started running on `windows-latest` for the
first time.

### Added
- **`vex self-update` subcommand** — fetches the latest GitHub release, picks the archive for the running target triple, and replaces the binary in place. Works on Linux, macOS, and Windows. `--check` reports the latest version without modifying anything; `-y/--yes` skips the interactive confirmation. Closes the upgrade-flow gap on platforms without a package manager
- **Windows install section in README** — step-by-step for downloading `vex-x86_64-pc-windows-msvc.zip`, extracting `vex.exe`, and putting it on `PATH`

### Fixed
- **`is_test_path` on Windows** — the test-file down-rank heuristic in `search::rerank` checked for `/test/`, `/tests/`, etc. as substrings. Indexed paths preserve the host separator, so on Windows the heuristic silently never matched and test files received no rank penalty. Now normalizes backslashes before the scan, allocating the extra string only when needed
- **`grep::tests::grep_with_path_filter`** asserted `matches[0].path.contains("api/")` which fails on Windows (paths are `api\routes.py`). Assertion now contains "api" without the trailing slash, OS-agnostic

### Security
- **ed25519 signature verification** for `vex self-update`. Every release archive is signed in CI via [`zipsign`](https://github.com/Kijewski/zipsign); the 32-byte verifying key is embedded in the binary at compile time. The download path refuses to extract an archive whose signature does not match. Mitigates the "compromised GitHub account / CDN tamper" scenario in the v1.6.0 self-update threat model
- `vex self-update` continues to use rustls (no openssl C dep), enforces certificate validation by default, and refuses to apply a downgrade
- Key rotation and one-time setup documented in [`docs/RELEASING.md`](docs/RELEASING.md)

### Internal
- `--check` and `-y/--yes` are mutually exclusive at the clap layer (the combination would have silently dropped one flag)
- `self_update::version::bump_is_greater` parse failures surface via `?` instead of `.unwrap_or(false)` so an unexpectedly-tagged release is reported as an error, not a silent "no action needed"
- Named-argument comments at the `cmd_self_update` call site guard against a future refactor swapping the two `bool` positionals

## [1.6.0] - 2026-05-18

Configurable cache location, first-class Windows support, and a thread-count
limit for parallel indexing. The original trigger was a Windows-only MCP
bug — `dirs_home()` had no Windows branch and fell back to `/tmp`, producing
mangled paths like `/tmp\.cache\vex\<hash>`. Fixing that path resolver turned
into a broader rework of cache management.

### Added
- **`--cache-dir <PATH>` global flag** plus `$VEX_CACHE_DIR` env var to override the cache root per-invocation
- **`cache_dir = "..."` in `.vex.toml`** — accepts absolute paths, `~/...`, and paths relative to the config file (e.g. `"./.vex/cache"`)
- **`local_cache = true` in `.vex.toml`** — store the index at `<project>/.vex_cache/` without a project-hash subdirectory. vex auto-writes a `.gitignore` so the cache is not committed. Useful when the cache should travel with the project (renames, copies, moves)
- **`-j/--jobs N` flag** on `index`, `update`, `watch` to cap the worker pool. Mirrored by `$VEX_JOBS` and `jobs = N` in `.vex.toml`
- **80% default thread count** (rounded up, floor 1) when no explicit jobs setting is supplied. Leaves headroom for the editor / browser / language server sharing the machine. Pass `0` to keep using every core
- **Windows cache locations**: `%LOCALAPPDATA%\vex` (with `%USERPROFILE%\AppData\Local\vex` and `$HOME\AppData\Local\vex` as fallbacks). No more `/tmp` literal anywhere in the resolver

### Fixed
- **MCP server now forwards `--auto-update`** to the seven index-backed commands (`search`, `find_symbol`, `find_similar`, `show`, `usages`, `check`, `similar`, `duplicates`). Previously a stale index surfaced as a tool failure to the MCP client even though the bare CLI handled the same condition transparently
- **MCP cache path mangling on Windows** — `HOME` fell back to `/tmp` and downstream paths became `/tmp\.cache\vex\<hash>\index.vex`. Fixed by the new platform-aware resolver

### Changed
- `.vex.toml` now accepts `cache_dir`, `local_cache`, and `jobs` fields (all optional). Existing configs continue to parse unchanged
- Default worker count for parallel indexing dropped from "all cores" to "ceil(80%)". To restore the previous behaviour, set `jobs = 0` in `.vex.toml`, `$VEX_JOBS=0`, or pass `-j 0`. The change only affects indexing commands; one-shot search/show calls do not eagerly initialize the rayon pool

### Security
- **Path-traversal blocker** for `cache_dir`. A `.vex.toml` shared via a monorepo cannot redirect index writes outside the project root via `..` segments, including post-tilde-expansion cases like `~/../etc/evil`. Rejected paths produce a warning and fall back to the platform default
- **Atomic `.gitignore` creation** for `local_cache` uses `OpenOptions::create_new` so a planted symlink at `.vex_cache/.gitignore` cannot be overwritten

### Internal
- `VexConfig` records the directory of the `.vex.toml` that produced it (`source_dir`) so relative `cache_dir` values resolve against the config file, not the cwd
- Cache override is installed once at `dispatch()` via `OnceLock<CacheLayout>`, avoiding a per-call-site parameter through 20+ index/manifest/hnsw path accessors
- New helpers in `util::config`: `resolve_cache_root` (`ResolvedCache { root, skip_hash_subdir }`), `resolve_jobs`, `resolve_explicit_jobs`, `default_thread_count`, `expand_user`, `write_local_cache_gitignore`, `set_cache_override`, `init_rayon_pool`
- Test env helper uses an RAII `Drop` guard so a panicking test cannot leak mutated env into the next; poisoned-mutex recovery via `unwrap_or_else(|e| e.into_inner())`
- 17 new unit tests covering the cache-resolution priority chain, tilde expansion, path-traversal rejection (relative and tilde-bypass), local_cache layout, VEX_JOBS opt-ins, and the 80%-default formula

## [1.5.0] - 2026-05-16

Phase 9 ships hybrid search v2: three orthogonal channels (structural FST,
BM25 over body tokens, semantic HNSW) fused via 3-way RRF; persistent call
graph delivering ~1000× speedups on `callers`/`callees`; a pluggable embedder
trait that unblocks future code-specific models; and two new commands
(`similar`, `duplicates`) on top of the existing vector index. The on-disk
format bumps to v4 with backwards-compatible read of v3.

- 19 commands (+2), 17 MCP tools (+2), 649 tests (+108 vs v1.4.3)

### Added — Phase 9.4 (BM25 channel in hybrid search)
- **BM25 inverted index** as the third channel in `vex search` alongside structural FST and semantic HNSW. Closes the gap between "exact symbol name" (structural) and "general meaning" (semantic) — finds rare body terms like `timeout`, `singlestore`, `idempotency_key` that aren't part of the symbol name
- **`vex::store::bm25` module** — `Bm25IndexBuilder` (per-symbol term bags), `Bm25Reader` (zero-copy scoring), `tokenize_query`/`tokenize_document`, Okapi BM25 with `K1 = 1.2`, `B = 0.75`. Minimal English stop-word filter applied at query time only (keeps the index size unchanged when the stop-word list evolves)
- **`vex::search::bm25::search(reader, query, top_k)`** — adapter that converts BM25 hits to `SearchResult` tagged with new `MatchType::Bm25`
- **3-way RRF fusion** — `vex::search::fusion::{fuse3, fuse_many}`. `Hybrid` label when a result appears in ≥2 channels; original `MatchType` preserved when in just one
- **CLI integration** — BM25 auto-on when the index has BM25 data. Flag `--no-bm25` to disable on a per-call basis
- **Pipeline integration** — `pipeline::build_bm25_index` aggregates per-symbol terms from `name + signature + body_tokens + doc` and emits the FST/posting/stats triple
- **20 new integration tests** in `tests/bm25_test.rs` plus 7 BM25 unit tests in `src/store/bm25.rs` — format size, writer/reader roundtrip (with and without BM25), pipeline emission, IDF discrimination (rare term ranks above common), short-doc preference under same TF, 3-way fusion with Hybrid labelling, MatchType tagging, Unicode tokenization, empty/stopword-only query handling

### Changed — Phase 9.4
- `CallGraphHeader` extended from 80 → 128 bytes (10 call-graph fields + 6 BM25 fields: fst/postings/stats offset+len). Internal name kept for back-compat with 9.3 callers; will be renamed to a generic `V4SectionHeader` in a follow-up. **No version bump v4 → v5** — v4 was unreleased outside this repo, and extending the existing v4 envelope avoids a second migration event
- `writer::write_index_with_call_graph` signature gains `Option<Bm25Sections>` parameter (a `(&[u8], &[u8], &[u8])` triple of pre-built BM25 bytes)
- `search::fusion::fuse` retained as a back-compat 2-way wrapper around `fuse_many`; new code uses `fuse3` / `fuse_many`
- `MatchType` gains the `Bm25` variant

### Note — format compatibility
- v3 indexes still readable; `has_bm25() == false` → search falls back to structural-only behavior (plus semantic if `--semantic` and vectors present)
- v4 indexes built between Phase 9.3 and 9.4 are also readable; `bm25_*_len == 0` → BM25 channel disabled until next `vex index` rebuilds

### Added — Phase 9.3 (persistent call graph + format v4)
- **Format bump v3 → v4**: new sections persisted at index time — `CallEdge` records, callers FST, callees FST + posting lists. The `Header` struct stays byte-identical to v3; a new `CallGraphHeader` (80 B) is placed at offset `Header::SIZE` when `version >= 4`. v3 indexes still open (back-compat read path)
- **`vex callers <name>` / `vex callees <name>` fast path** — FST lookup (~4ms) on v4 indexes. Falls back to live tree-sitter scan when (a) no index exists, (b) index is v3, (c) v4 index has no call graph, or (d) index exists but fails to open (with a warning logged so corrupt/locked indexes don't silently degrade to the slow path)
- **`crate::callgraph::extract_call_edges(content, lang)`** — returns `(caller_fn_name, caller_fn_line, callee_name, call_line)` quadruples. Used by `pipeline::parse_file` to populate `ParsedFile.call_edges`
- **`store::call_graph` module** — `CallEdgeBuilder`, `build_callers_fst`, `build_callees_fst`, `CallGraphFstReader`, `find_callers_fast`, `find_callees_fast`, `encode_caller_key`
- **Incremental update preserves call edges** — `reconstruct_unchanged` re-emits edges from the old index for files whose hash didn't change; only changed/deleted files get fresh extraction
- **25 new integration tests** in `tests/call_graph_test.rs` — format sizes (Header::SIZE = 144, CallGraphHeader::SIZE = 80, CallEdge::SIZE = 16), writer/reader roundtrip, callers/callees FST fast paths with case-insensitivity and dedup, same-name-across-files isolation via `caller_sym_idx` keying, end-to-end pipeline → fast path, `extract_call_edges` for Rust + unsupported langs, incremental update preserves edges, regression test for same-name-within-file disambiguation (CRITICAL fix from Phase 9.3 review)

### Changed — Phase 9.3
- `ParsedFile` gains `call_edges: Vec<RawCallEdge>` — populated at parse time, consumed by writer
- `pipeline::resolve_call_edges` keys on `(path, name, line)` (not just `(path, name)`) so two functions sharing a name in the same file (overloaded methods, duplicate `impl` blocks) get distinct caller symbol indices
- `IndexReader::open` accepts versions in `[MIN_SUPPORTED_VERSION..=VERSION]` (v3..v4) plus legacy v2 — section-bounds validation extended for the new v4 sections
- Writer aligns `call_edges_offset` to a 4-byte boundary so `CallEdge` records can be safely mmap-cast on strict-alignment platforms
- `vex::store::writer::write_index_full` now delegates to a new `write_index_with_call_graph` (used by `pipeline`)

### Fixed — Phase 9.3 (review findings)
- **CRITICAL**: same-name symbols within a single file used to all resolve to the first occurrence's `caller_sym_idx`, attributing every later duplicate's call sites to the wrong caller. Fixed by including the symbol's definition line in the resolution key. Regression test in `tests/call_graph_test.rs::duplicate_function_name_in_same_file_resolves_to_correct_caller`
- **HIGH**: `find_callers_fast` / `find_callees_fast` now early-return on `!has_call_graph()` instead of relying on callers to check the invariant
- **HIGH**: `cmd_callgraph` logs a `tracing::warn!` when an existing index fails to open (corrupt/locked) instead of silently falling back to the slow path
- Writer alignment bug surfaced during test authoring — `call_edges_offset` rounded up to a 4-byte boundary so subsequent mmap reads succeed alignment checks

### Note — format compatibility
- v3 indexes remain readable. `has_call_graph()` returns false → CLI falls back to live scan. Run `vex index` to rebuild as v4 and unlock fast paths.

### Added — Phase 9.1 (pluggable embedder backend)
- **`Embedder` trait** — `id()`, `dim()`, `char_budget()`, `embed()`, `embed_batch()`. Unblocks future code-specific embedders (BGE, GraphCodeBERT, CodeT5+)
- **Registry**: `embed::make_embedder(id)`, `embed::embedder_dim(id)`, `embed::known_embedders()`. Unknown ID returns an error listing known IDs
- **`MiniLMEmbedder`** — current default and only implementation. Output 384-dim, char budget 1100, ID `"minilm-l6-v2"`. Replaces the previous concrete `Embedder` struct
- **`embedder` option** in `.vex.toml` and `--embedder <id>` flag for `index`/`update`/`watch`. Priority: CLI > config > `DEFAULT_EMBEDDER` (=`minilm-l6-v2`)
- **`Manifest.embedder_id: Option<String>`** — records the embedder a semantic index was built with. Missing field on pre-9.1 manifests is interpreted as the default for back-compat
- **Embedder mismatch detection** at `vex search --semantic`: when the requested embedder differs from `manifest.embedder_id`, search bails with a rebuild hint instead of producing nonsense results
- **`writer::write_index_full`** now takes `vector_dim: u32` instead of hardcoding 384. `Header.vector_dim` is filled from the embedder, opening the door to non-384 models
- **21 new integration tests** in `tests/embedder_test.rs` covering trait registry, dim lookup, resolve priority, mismatch detection (including back-compat with missing `embedder_id`), manifest roundtrip + skip-serializing-when-none, config parsing, `build_context` budget, writer variable dim + length validation

### Changed — Phase 9.1
- `embed::build_context` signature: takes `budget: usize` instead of hardcoded `EMBEDDING_CHAR_BUDGET`. Callers pass `embedder.char_budget()` to fit the model's token window
- `semantic::search_with_embedder` now accepts `&mut dyn Embedder` (was `&mut Embedder` concrete struct)
- `pipeline::run` and `pipeline::update` now take `embedder_id: &str`
- `watch::watch` takes `embedder_id: &str`
- `src/embed/model.rs` slimmed to `build_context` only; the model struct lives in the new `src/embed/minilm.rs`

### Note — format compatibility
- **No binary format bump in 9.1.** Embedder identification lives in `manifest.json`, not the index Header. Format v4 will land with Phase 9.3 (persistent call graph) when binary section additions are actually needed. This avoids breaking v3 read-back.

### Added — Phase 9.2 (semantic similarity)
- **`vex similar <symbol>`** — find symbols semantically close to a given one. Resolves the symbol's stored embedding, runs HNSW (or brute-force fallback), returns nearest neighbors with cosine similarity. Flags: `--limit`, `--threshold`, `--filter`, `--auto-update`. Requires `vex index --semantic`
- **`vex duplicates`** — list pairs of near-duplicate symbols by embedding similarity. Canonical pair dedup `(min_idx, max_idx)` — never both `(A, B)` and `(B, A)`. Flags: `--threshold` (default 0.9), `--limit` (default 50), `--min-body-lines` (default 5, filters trivial 1-liner matches via approximated body length), `--filter`
- **MCP tools**: `similar` (by existing symbol — distinct from existing `find_similar` which queries by description) and `duplicates`
- **18 new integration tests** in `tests/similar_test.rs` covering self-exclusion, threshold filtering, canonical pair dedup, body-length filtering, empty index, missing vectors, unknown symbol errors, descending sort

### Changed
- `cosine_similarity` promoted from `fn` to `pub(crate) fn` in `src/search/semantic.rs` so `src/search/similar.rs` shares the same implementation

## [1.4.3] - 2026-05-14

### Added
- **+7 languages** (12 → 19): PHP, Bash, Lua, CSS, HTML, YAML, TOML. Each ships a tree-sitter grammar crate, an `.scm` query, and a per-language regression test (`tests/<lang>_query_test.rs`) on the v1.4.2 pattern
  - PHP: class, interface, trait, enum, function, method, class constant, `use` import
  - Bash: function definitions, `source`/`.` imports
  - Lua: function (top-level, local, `Mod.fn`, `Class:method`), `require` imports
  - CSS: class selectors, `#id` selectors, `@keyframes`, custom properties (`--var`)
  - HTML: `id="..."` attribute values, hyphenated custom-element tags
  - YAML: document-root mapping keys (anchored to avoid nested-key noise)
  - TOML: table headers (`[name]`, `[[name]]`), key/value pairs
- **`vex implementations` for PHP and Ruby** — 7 → 10 OOP languages supported
  - PHP: `extends`, `implements` (multi-arg, qualified names), `interface extends`, **PHP 8.1+ `enum implements`**, **trait `use TraitName`** (in class & trait bodies, labelled `(uses)`)
  - Ruby: `class Foo < Bar` (labelled `(inherits)`), `include`/`extend`/`prepend Mixin` inside class and module bodies (labelled `(include)`)
  - Relation labels dispatched via `tree_sitter::QueryMatch::pattern_index` with named thresholds (`PHP_TRAIT_PATTERN_START`, `RUBY_MIXIN_PATTERN_START`)
- **Multi-case identifier scanner** in the refs FST — `extract_references` now picks up `snake_case`, `camelCase`, and `SCREAMING_SNAKE_CASE` identifiers in addition to `PascalCase`. `vex usages process_order` now works in Python/Rust/Go/Lua codebases. Filter keeps plain lowercase words (`total`, `amount`) out of the FST to prevent prose-noise bloat
- 41 new tests (451 → 541): per-language grammar regressions for the new 7, hierarchy tests for PHP/Ruby/traits, scanner case-style coverage, symmetric threshold guards, negative tests against silent query degradation

### Fixed
- **Ruby `#match?` predicate misplaced** — the include/extend/prepend filter sat OUTSIDE the enclosing `(class ...)` S-expression, which tree-sitter silently treated as a no-op. The query degraded to "any method call inside a class body with a Constant argument", which would falsely match `assert_equal Mixin, foo.class` and `describe Mixin do`. Caught during code review; `ruby_non_mixin_call_does_not_match` now guards it
- **PHP `use Foo as Log;` alias was duplicated** in the imports index because two `namespace_use_clause` patterns matched the same `name` node — pattern 2 (bare `(name)`) fired alongside pattern 3 (`alias:` field). Added `!alias` negative-field guard to pattern 2
- **Lua `require("util")` stored quotes** — `string_content` is absent for strings without escape sequences in tree-sitter-lua 0.5, so the query captured the whole `(string)` node verbatim. New `strip_import_quotes` in `extract_symbols_and_imports` peels off `"..."`, `'...'`, `<...>` (C/C++ system includes), and `[[...]]` (Lua long brackets) from any `import.name` capture. `vex usages util` now resolves the `require` site
- **Markdown heading kind glyph in compact output** was `?` because `cli/output.rs::compact_kind` had no arm for `"heading"`; now emits `H`
- **`clippy::collapsible_match`** failure on `relation_label` PHP arm — refactored to match guards

### Changed
- **Documentation** — `docs/SUPPORTED_LANGUAGES.md` adds an `implementations` support column and lists `src/hierarchy/mod.rs` and `src/cli/mod.rs::Pattern` as match sites to keep in sync; README badges updated to 19 languages / 17 commands / 541 tests
- `extract_references` filter — only structurally-shaped identifiers (`contains '_'` or mixed-case) reach the FST; plain lowercase words like `total`/`amount` are skipped to keep the refs FST tight

## [1.4.2] - 2026-05-13

### Fixed
- **C# parsing was silently broken** in 1.4.1 — `tree-sitter-c-sharp` 0.23.5 shipped grammar ABI 15 while `tree-sitter` 0.24 only supported up to ABI 14. The `LazyLock` initializer panicked on the first `.cs` file, every subsequent file hit a poisoned cell, and the failure was buried in `tracing::warn!` that `RUST_LOG` filters out by default. Result: 0 C# symbols extracted, no user-visible error. Reported externally — 487 `.cs` files indexed as empty
- **Swift parsing was silently broken** since the language was added — `queries/swift.scm` referenced `enum_declaration`, a node that does not exist in `tree-sitter-swift` 0.7. Same `LazyLock`-poisoned-cell failure mode as C#. Discovered only after adding the new per-language regression tests
- **Grammar load failures no longer poison the parser** — `queries::get_query` returned `LazyLock<Query>` and panicked on init; replaced with `LazyLock<Result<Query, String>>` and new `try_get_query` so a compile failure is cached as an error and surfaces as `GrammarLoadError` to the indexing pipeline, not as a thread panic
- **Grammar failures are now visible** — `index/pipeline.rs` aggregates per-language failures and emits a single structured `tracing::warn!` at the end of a run (`language=<X> skipped=<N> error=<reason>`) so users see why their language has zero symbols
- **TOCTOU on file size check** — `discover_files` did `fs::metadata().len() <= 1MB` then `fs::read_to_string()`; a file that grew between the two could exhaust memory. New `read_capped` uses `File::open` + `Read::take(1MB+1)` so the cap is enforced on the actual read
- **Mutex poison handling** in `parse_files` — both lock sites now use `.unwrap_or_else(|e| e.into_inner())` instead of `.unwrap()`, so an unrelated worker panic cannot block aggregation
- Stale CLI message in `vex show` referenced "Kotlin, TypeScript pending" — both have had queries for releases; replaced with a generic grammar-load error
- `WalkBuilder::follow_links(false)` is now set explicitly (was relying on the crate default) — defensive: a malicious symlink cannot smuggle in `../../.ssh/id_rsa`

### Added
- **`docs/SUPPORTED_LANGUAGES.md`** — version matrix listing all 12 grammars, their crate versions, extracted symbol kinds, plus runbooks for upgrading a grammar and adding a new language
- **Per-language regression tests** (11 new files, `tests/<lang>_query_test.rs`) — each verifies the grammar loads on empty input and that core symbol kinds are extracted with the right `SymbolKind`. The first regression check fires *before* any real source file is indexed. Total suite: 451 tests (up from 243)
- `GrammarLoadError` typed error in `parse::extractor` — `pipeline.rs` downcasts to it via `anyhow::Error::downcast_ref`, replacing the previous string-matching heuristic
- Negative-assertion tests for Swift enum/struct/class (regression guard against tree-sitter's no-short-circuit query semantics — multiple patterns matching the same node would double-index)

### Changed
- **Grammar version bumps** (all to crates.io latest as of 2026-05-13):
  - `tree-sitter`: 0.24 → 0.26 (adds ABI 15 support)
  - `tree-sitter-rust`: 0.23 → 0.24
  - `tree-sitter-python`: 0.23 → 0.25
  - `tree-sitter-go`: 0.23 → 0.25
  - `tree-sitter-md`: 0.3 → 0.5
- README: updated test count (243 → 451), added per-language grammar regression section, linked `docs/SUPPORTED_LANGUAGES.md`, refreshed Swift row in the language table to reflect actual extracted kinds

### Removed
- `queries::get_query` — dead-weight back-compat wrapper after CLI gate migrated to `try_get_query`. Removing it prevents future callers from accidentally silently swallowing grammar-load errors

## [1.4.1] - 2026-05-11

### Added
- **Staleness detection** — `vex search` warns when the index is stale (git HEAD changed or files modified since last index); uses a single `git rev-parse` call (~0.1ms) with mtime fallback for non-git repos
- `--auto-update` flag for search/show/usages/check — runs `vex update` inline before search when stale
- `--no-stale-check` flag — skip staleness check entirely
- `auto_update` option in `.vex.toml` — enable auto-update by default
- **Context-aware reranking** — `--kind fn` boosts results matching a specific symbol kind, `--context-path src/file.rs` boosts results near the file you are editing (path overlap + module proximity)
- **90 new tests** (172 → 243):
  - Property-based (proptest): rerank preserves length, sorted output, no NaN/negative, fusion commutativity
  - Reranking stress: NaN/Infinity/zero scores, 10K results, edge context paths
  - Adversarial binary format: 20 crafted index tests — overflow offsets, alignment attacks, truncated records
  - Unicode/encoding: BOM, mixed CRLF, unicode identifiers, null bytes, empty files
  - Incremental update: file rename, symbol move between files, empty file, new file added
  - Concurrency: parallel index (lock serialization), concurrent readers, read during reindex
  - Cross-language: same name across 3 languages, wrong extension, 1K-symbol file, deep nesting, error recovery
  - Path edges: spaces, 20-level nesting, symlinks, absolute vs relative, Windows backslashes
- `proptest` added to dev-dependencies

### Fixed
- **NaN propagation in reranker** — `NaN * boost = NaN` silently corrupted scores; now sanitized to 0.0
- **Infinity overflow in reranker** — `f64::MAX * boost = +inf`; now clamped to `f64::MAX / 2.0`
- **Single-result NaN bypass** — `rerank()` early-returned for len ≤ 1, skipping sanitization
- Kind hint mismatch was too aggressive (0.7x demotion) — changed to neutral 1.0 (boost-only, no penalty)
- Path overlap double-counted filename as shared component — now compares directory components only
- Module proximity boost no longer overlaps with path overlap for same-directory results
- `read_git_head()` moved outside advisory lock in `write_output` to reduce lock hold time

### Changed
- `Freshness` enum is `#[must_use]`; `changed_count` is `Option<usize>` (None = count not computed)
- `RerankContext` struct with `kind_hint: Option<SymbolKind>` and `context_path: Option<&str>`
- `rerank()` signature extended: `rerank(query, &RerankContext, results)`
- Manifest extended with `git_head` and `indexed_at` fields (backward-compatible via `serde(default)`)
- README: updated commands table, test count (243), added Staleness Detection section, updated CLAUDE.md integration rules

## [1.4.0] - 2026-05-08

### Added
- C/C++ support (12th language) — classes, structs, functions, methods, templates, enums, `#include` refs
- Heuristic search result reranking — PascalCase → type boost, snake_case → function boost, exact name match boost, test path demotion
- Fuzz testing suite — 3 targets (index reader, refs FST, symbol FST) covering all `unsafe` code paths in the binary format reader
- 21 new integration tests — binary format roundtrip, corrupted index handling, vector roundtrip, fuzzy search, refs FST, incremental update, multi-language parsing, RRF fusion
- CI test gate on release — `cargo fmt` + `cargo clippy` + `cargo test` must pass before build
- Live CI badge in README (GitHub Actions)

### Fixed
- **3 bugs found by fuzzing**: out-of-bounds read on crafted `symbol_count`, misaligned pointer dereference on odd `symbols_offset`, unchecked section offsets exceeding file size
- `IndexReader::open()` now validates all section offsets against actual file size — rejects corrupted/truncated index files instead of crashing
- `symbol()` and `vector()` use runtime alignment checks (not just `debug_assert`) — returns `None` on misaligned data instead of UB
- Overflow-safe arithmetic in `symbol()`, `vector()`, `read_string()`
- `file_paths()` caps allocation to what fits in mmap — prevents OOM on crafted headers
- Unicode-safe string truncation in body token extraction and doc comments
- Real incremental update — `vex update` now only re-parses changed files and reconstructs unchanged symbols from the existing index (previously did a full rebuild despite detecting changes)

### Changed
- `SymbolKind`: added `TryFrom<u8>` — replaces hardcoded `symbol_kind_str()` magic numbers in search modules
- Eliminated code duplication: merged `indices_to_results` / `indices_to_results_typed`, extracted shared `discover_source_files()` from callgraph and hierarchy modules
- README: removed fake test count badge, added honest comparison notes (pre-built index vs full scan tradeoff), added Testing section with fuzz test documentation
- Release workflow now requires tests to pass before building binaries

## [1.3.0] - 2026-05-08

### Added
- Body-aware semantic search — embeddings now include identifiers and string literals extracted from symbol bodies, not just names/signatures/docstrings
- `.vex.toml` config file — exclude patterns, default format, semantic on/off
- `vex init` — generates default config with commented examples
- `--no-semantic` flag to override config-enabled semantic mode
- Fuzzy search — automatic Levenshtein fallback when exact + prefix returns nothing ("IndxReader" finds "IndexReader")
- Markdown support (11th language) — headings indexed as symbols, `vex outline README.md` shows document structure, `vex show "Installation"` extracts full section

### Changed
- `SymbolKind` now uses `#[repr(u8)]` with explicit discriminants for binary format stability
- File walking centralized via `util::walk` module with configurable exclude patterns
- Config loaded from project root (resolved `--path`), not cwd

## [1.1.0] - 2026-05-08

### Added
- Shell completions for bash, zsh, fish (`vex completions <shell>`)
- Release script for version bumps (`scripts/release.sh`)

### Fixed
- Atomic index writes — write to temp file, rename on success (no corruption on crash)
- Advisory file locking during `vex index` / `vex update` (prevents concurrent writes)
- Skip binary and minified files by content heuristic (control chars, long lines)
- Catch tree-sitter panics in rayon workers instead of crashing the indexer
- Inline index validation with specific error messages (size, magic, version)
- Bounds-check `read_string` to prevent panic on corrupt index

### Changed
- README updated: 15 commands, 115 tests, full MCP tools list, completions docs

## [1.0.1] - 2026-05-07

### Fixed
- Drop x86_64-apple-darwin from release CI matrix

## [1.0.0] - 2026-05-07

### Added
- `vex check` — fast symbol existence check via FST
- `vex callers` / `vex callees` — callgraph navigation (5 languages)
- `vex implementations` — find types extending a base class/trait/interface (7 languages)
- `vex show A B C` — bulk symbol body extraction
- `vex outline --kind fn` — filter outline by symbol kind
- `vex search --filter`, `vex show --filter` — path-based result filtering
- Semantic search quality: tokenized names, path keywords, docstrings in embeddings
- MCP server expanded to 15 tools (full CLI parity)
- CI/CD: GitHub Actions for tests and releases
- CLAUDE.md integration instructions in README

### Fixed
- Pattern matching enabled for Kotlin, TypeScript, and SQL

## [0.2.0] - 2026-05-07

### Added
- `vex grep` — regex content search with `--filter` path support
- Homebrew installation (`brew install tenatarika/tap/vex`)

## [0.1.0] - 2026-05-07

Initial release.

### Added
- Structural search — FST-based symbol lookup in ~4ms
- Semantic search — MiniLM-L6-v2 embeddings + HNSW index + RRF fusion
- `vex index` / `vex update` / `vex watch` — full, incremental, and watch-mode indexing
- `vex search`, `vex usages`, `vex show`, `vex outline`, `vex status`
- `vex pattern` — AST pattern matching with metavariables ($NAME, $$$)
- 10 languages: Rust, Python, Go, Java, C#, Ruby, Swift, Kotlin, TypeScript, SQL
- MCP server (`vex-mcp`) for AI agent integration
- Custom mmap'd binary format — zero-copy reads, 10x smaller than SQLite
- Parallel parsing via rayon, incremental updates via content hashing
- Compact output format (`--format compact`) for LLM token efficiency
- JSON output (`--format json`) for tool integration

[Unreleased]: https://github.com/tenatarika/vex/compare/v1.5.0...HEAD
[1.5.0]: https://github.com/tenatarika/vex/compare/v1.4.3...v1.5.0
[1.4.3]: https://github.com/tenatarika/vex/compare/v1.4.2...v1.4.3
[1.4.2]: https://github.com/tenatarika/vex/compare/v1.4.1...v1.4.2
[1.4.1]: https://github.com/tenatarika/vex/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/tenatarika/vex/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/tenatarika/vex/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/tenatarika/vex/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/tenatarika/vex/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/tenatarika/vex/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/tenatarika/vex/compare/v0.2.0...v1.0.0
[0.2.0]: https://github.com/tenatarika/vex/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tenatarika/vex/releases/tag/v0.1.0
