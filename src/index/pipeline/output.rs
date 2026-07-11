//! Index output side — building the in-memory sections (BM25, call
//! graph, pattern skeletons), generating embeddings, building the HNSW
//! semantic index, and writing the binary index file.
//!
//! `write_output_locked` is the entry point: it stitches every section
//! together and calls into `store::writer` for the final atomic write.
//! The caller MUST already hold the [`super::IndexLock`] — both `run`
//! and `update` acquire it before this work runs.
//!
//! Isolated from `mod.rs` so the orchestration (manifest re-check,
//! lock handling, decisions about what to rebuild) stays separable
//! from the mechanical "build each section + write" step.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::embed;
use crate::index::manifest::Manifest;
use crate::index::symbols::ParsedFile;
use crate::parse::language::Language;
use crate::store;
use crate::util::config;

use super::IndexOptions;

/// Pick the `vector_dim` to record in the Header.
///
/// Priority: actual length of the first vector → registry lookup by id →
/// default MiniLM dim. The first wins so non-embedded indexes still record a
/// sensible legacy default.
pub(super) fn vector_dim_for(embedder_id: &str, vectors: &[Vec<f32>]) -> u32 {
    if let Some(first) = vectors.first() {
        return first.len() as u32;
    }
    embed::embedder_dim(embedder_id).unwrap_or(embed::MINILM_DIM)
}

/// Build the BM25 index from per-symbol term bags.
///
/// Each symbol becomes a document whose terms are drawn from:
/// - `name` (split on non-alnum, lowercased)
/// - `signature` (same split)
/// - `body_tokens` (already extracted, space-separated)
/// - `doc` (docstring; same split)
///
/// Returns `(fst_bytes, postings_bytes, stats_bytes)`. The triple is empty
/// when there are no documents (no symbols across all parsed files).
pub(super) fn build_bm25_index(parsed: &[ParsedFile]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    // `doc_count` counts every `ParsedSymbol`, including synthetic
    // `SymbolKind::Module` rows (Phase 14.1) that the inner loop skips
    // when populating term bags. The Module slots still reserve a
    // zero-length doc-length entry in BM25 stats so that `sym_idx` —
    // which is incremented for *every* symbol regardless of kind — stays
    // aligned with `SymbolRecord` positions in the records section. Net
    // overhead: 2 bytes per indexed file in the stats footer.
    let doc_count: usize = parsed.iter().map(|f| f.symbols.len()).sum();
    if doc_count == 0 {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    let mut builder = crate::store::bm25::Bm25IndexBuilder::new(doc_count);
    let mut sym_idx: u32 = 0;
    for file in parsed {
        for sym in &file.symbols {
            // Phase 14.1: synthetic Module symbols carry only call edges,
            // never BM25 content. Skip the bag-building but advance
            // `sym_idx` so the doc-id alignment with `SymbolRecord`
            // positions stays correct (doc slot stays zero-length).
            if sym.kind == crate::index::symbols::SymbolKind::Module {
                sym_idx += 1;
                continue;
            }
            let mut bag = String::with_capacity(256);
            bag.push_str(&sym.name);
            bag.push(' ');
            if let Some(s) = &sym.signature {
                bag.push_str(s);
                bag.push(' ');
            }
            if let Some(b) = &sym.body_tokens {
                bag.push_str(b);
                bag.push(' ');
            }
            if let Some(d) = &sym.doc {
                bag.push_str(d);
            }
            let terms = crate::store::bm25::tokenize_document(&bag);
            builder.add_document(sym_idx, &terms);
            sym_idx += 1;
        }
    }
    builder.build()
}

/// Resolve each [`RawCallEdge`] to a [`CallEdgeBuilder`] with a concrete
/// `caller_sym_idx`. Edges whose caller doesn't match any symbol in the
/// parsed-file iteration order are dropped — this happens for languages
/// where the call-graph query produces function names that the symbol
/// extractor doesn't register (e.g. inner closures, anonymous fns).
///
/// The key is `(file_path, fn_name, definition_line)` — the `line` is
/// load-bearing because a single file can contain multiple functions
/// sharing a name (overloaded methods, duplicate `impl` blocks, methods
/// with the same name across nested modules). Keying only on `(path, name)`
/// would cause every duplicate to inherit the first occurrence's symbol
/// index, attributing call sites to the wrong caller.
///
/// The iteration order MUST match what `writer::write_index_to` uses to
/// assign `symbol_idx` (`parsed.iter().flat_map(|f| f.symbols.iter())`)
/// otherwise the resulting indices will be wrong.
pub(super) fn resolve_call_edges(
    parsed: &[ParsedFile],
) -> Vec<crate::store::call_graph::CallEdgeBuilder> {
    let mut sym_idx_of: HashMap<(&str, &str, usize), u32> = HashMap::new();
    let mut next_idx: u32 = 0;
    for file in parsed {
        for sym in &file.symbols {
            // Preserve the first symbol when `(path, name, line)` collides
            // (genuinely identical entries — duplicate symbols at the same
            // line are a parser bug, not a real case). The line component
            // makes overloaded/same-name siblings distinct.
            sym_idx_of
                .entry((file.path.as_str(), sym.name.as_str(), sym.line))
                .or_insert(next_idx);
            next_idx += 1;
        }
    }

    // Phase 14.1: per-file synthetic Module symbol name lookup key. Only
    // allocate the `<module:path>` string when the file actually contains a
    // sentinel edge — empty `caller_fn_name` + `caller_fn_line == 0` is the
    // marker emitted by `callgraph::extract_call_edges` for module-scope
    // calls. By tree-sitter grammar invariants no real function definition
    // has an empty name or line 0, so this conjunction is collision-free.
    let mut out = Vec::new();
    for file in parsed {
        let module_sym_name = file
            .call_edges
            .iter()
            .any(|e| e.caller_fn_name.is_empty() && e.caller_fn_line == 0)
            .then(|| format!("<module:{}>", file.path));
        for edge in &file.call_edges {
            let (caller_name, caller_line) =
                if edge.caller_fn_name.is_empty() && edge.caller_fn_line == 0 {
                    // `module_sym_name` is `Some` whenever any sentinel
                    // exists in this file — proven by the `any(...)` above.
                    (module_sym_name.as_deref().unwrap_or(""), 1)
                } else {
                    (edge.caller_fn_name.as_str(), edge.caller_fn_line)
                };
            let Some(&caller_sym_idx) =
                sym_idx_of.get(&(file.path.as_str(), caller_name, caller_line))
            else {
                continue;
            };
            out.push(crate::store::call_graph::CallEdgeBuilder {
                caller_sym_idx,
                callee_name: edge.callee_name.clone(),
                line: edge.line as u32,
            });
        }
    }
    out
}

/// Output of [`collect_pattern_skeletons`] — the flattened skeleton tuples
/// the writer consumes, paired with the per-language grammar fingerprints
/// Inc 5 will compare against the live grammar.
type SkeletonsForWriter = (
    Vec<(u32, crate::pattern::skeleton::Skeleton)>,
    Vec<(u8, u32)>,
);

/// Flatten per-file pattern skeletons into the `(file_id, Skeleton)`
/// shape the writer expects, and compute grammar fingerprints for the
/// distinct T1 languages that contributed at least one skeleton. The
/// fingerprint lets Inc 5 detect grammar drift between index build and
/// query time and fall back to live-scan when the on-disk skeletons no
/// longer agree with the live grammar.
pub(super) fn collect_pattern_skeletons(parsed: &[ParsedFile]) -> SkeletonsForWriter {
    let mut skeletons: Vec<(u32, crate::pattern::skeleton::Skeleton)> = Vec::new();
    let mut langs_with_skeletons: HashSet<Language> = HashSet::new();
    for (file_id, file) in parsed.iter().enumerate() {
        if file.skeletons.is_empty() {
            continue;
        }
        // Recover the language from the file extension — ParsedFile
        // does not carry it explicitly, but the extension is canonical
        // (parse_files uses Language::from_extension).
        let lang = std::path::Path::new(&file.path)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension);
        if let Some(lang) = lang {
            langs_with_skeletons.insert(lang);
        }
        for sk in &file.skeletons {
            skeletons.push((file_id as u32, sk.clone()));
        }
    }
    let fingerprints: Vec<(u8, u32)> = langs_with_skeletons
        .into_iter()
        .map(|lang| {
            (
                lang.lang_id(),
                crate::store::pattern_skeletons::grammar_fingerprint_for_lang(lang),
            )
        })
        .collect();
    (skeletons, fingerprints)
}

/// Build the index sections and write them out. The caller MUST already hold
/// the [`IndexLock`]; both `run` and `update` acquire it before the expensive
/// parse + embed so concurrent instances serialize and skip redundant rebuilds
/// instead of all embedding in parallel.
#[allow(clippy::too_many_arguments)] // mirrors the writer entry shape
pub(super) fn write_output_locked(
    root: &Path,
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    vector_dim: u32,
    file_hashes: &[(String, u64)],
    embedder_id: Option<String>,
    opts: IndexOptions,
    is_full_rebuild: bool,
    // Phase 11.1.9 (Q4-A) cross-stage handoff: reconstructed ref-edges
    // + old-index file_paths flow from `pipeline::update` into the
    // writer's second-pass resolution as one bundle. Empty default on
    // a full rebuild (`IndexBuildArtefacts::default()`).
    artefacts: &crate::index::types::IndexBuildArtefacts,
) -> Result<()> {
    let index_path = config::index_path(root);
    let cache_dir = index_path.parent().context("index path has no parent")?;
    std::fs::create_dir_all(cache_dir).context("create cache directory")?;

    let git_head = crate::index::staleness::read_git_head(root);
    let indexed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Skip the corresponding build work entirely when the section is
    // opted out — the reader gates these via `*_len == 0`, so passing
    // empty slices / `None` is a valid disabled state in the format.
    let call_edges = if opts.with_call_graph {
        resolve_call_edges(parsed)
    } else {
        Vec::new()
    };

    let bm25_built = if opts.with_bm25 {
        Some(build_bm25_index(parsed)?)
    } else {
        None
    };
    let bm25 = bm25_built.as_ref().and_then(|(fst, posts, stats)| {
        if fst.is_empty() {
            None
        } else {
            Some((fst.as_slice(), posts.as_slice(), stats.as_slice()))
        }
    });
    // 11.4 Inc 4 — collect (file_id, Skeleton) tuples and compute
    // per-language grammar fingerprints. The empty-opt-out path
    // still produces a v6 index, just with all-zero header fields.
    let (pattern_skeletons, lang_fingerprints) = if opts.with_pattern_index {
        collect_pattern_skeletons(parsed)
    } else {
        (Vec::new(), Vec::new())
    };
    let writer_meta = store::writer::write_index_with_call_graph_and_skeletons_and_fingerprints(
        parsed,
        vectors,
        vector_dim,
        &call_edges,
        bm25,
        &pattern_skeletons,
        &lang_fingerprints,
        &artefacts.reconstructed_refs,
        &artefacts.old_file_paths,
        &artefacts.reconstructed_unresolved_refs,
        &index_path,
    )
    .context("write index")?;

    // v1.12.0 T4 — build + persist the bloom sidecar. Failure is
    // non-fatal: callers fall back to direct FST lookups when the
    // sidecar is absent or corrupt, so a write error must not block
    // the index run. NOTE: ordering matters — `index.vex` is already
    // atomically in place. A failed bloom write leaves either no
    // sidecar (fresh first run) or a stale sidecar from a prior run.
    // Both are safe: bloom only adds false positives, never false
    // negatives, so even a stale sidecar can't make `vex check` miss a
    // present symbol.
    let bloom_path = config::bloom_path(root);
    let bloom = crate::search::bloom::SymbolBloom::from_parsed_files(parsed);
    if let Err(e) = bloom.save(&bloom_path) {
        tracing::warn!(
            path = %bloom_path.display(),
            error = %e,
            "failed to persist bloom sidecar (stale sidecar may remain); \
             cmd_check will fall back to FST or use the stale bitmap"
        );
    }

    // Phase 14.8 — git_history sidecar with three branches:
    //   1. `drop_history`: delete sidecar + null manifest fields
    //      (`vex update --no-history` sticky-drop).
    //   2. `with_history` + tip unchanged + sidecar present: fast path
    //      — skip rebuild, reuse stats, refresh `indexed_at`. Pins the
    //      "no-op `vex update` is fast" contract (acceptance C of
    //      `.claude/Task/PHASE14.8-history-index.md`).
    //   3. `with_history`: full rebuild via builder + sidecar write.
    //      Force-push (prior_tip exists but isn't ancestor of HEAD)
    //      triggers a warning + full rebuild (architect H3).
    //
    // Outcome captured in `history_manifest_fields` — the manifest
    // block below records fields ONLY on a successful write or
    // fast-path reuse. The drop branch leaves `history_manifest_fields
    // = None`, which the manifest construction propagates as Nones
    // across all four `history_*` fields → next `vex update` sees a
    // clean slate.
    let history_path = config::git_history_path(root);
    let prior_manifest_for_history = if opts.with_history || opts.drop_history {
        crate::index::manifest::Manifest::load(&config::manifest_path(root)).ok()
    } else {
        None
    };
    let mut history_manifest_fields: Option<HistoryManifestFields> = None;
    // Phase 14.10 — tri-state record of whether rename_chains were built
    // this run: `None` = chain detection wasn't reached (history not
    // indexed, drop branch, or tip-SHA parse failure); `Some(true)` =
    // sidecar written successfully; `Some(false)` = builder reached
    // but write failed. Mirrors body_tokens_persisted gating so the
    // manifest stays honest with what's on disk.
    let mut rename_chains_built: Option<bool> = None;
    // Phase 14.10 — count of MiniLM tie-breaker decisions emitted by
    // the rename-chain builder this run. `None` until the build path
    // overwrites it (drop branch and non-history runs keep `None`);
    // `Some(0)` is a meaningful signal that the cosine path ran but
    // never decided a borderline pair.
    let mut rename_chains_minilm_tiebreak_hits: Option<u32> = None;
    if opts.drop_history {
        // Best-effort: missing file is fine (idempotent); permission
        // errors warn but don't block the rest of the index write.
        if history_path.exists() {
            if let Err(e) = std::fs::remove_file(&history_path) {
                tracing::warn!(
                    path = %history_path.display(),
                    error = %e,
                    "failed to drop git_history sidecar; manifest will still null \
                     the fields so cmd_history falls back to walker"
                );
            } else {
                tracing::info!(
                    path = %history_path.display(),
                    "dropped git_history sidecar per --no-history"
                );
            }
        }
        // Phase 14.10 — rename_chains sidecar is coupled to git_history
        // via the tip-SHA guard, so it's stale the moment history is
        // dropped. Remove it alongside so `vex status` doesn't keep
        // surfacing a chain count for a section that no longer exists.
        let rename_chains_path = config::rename_chains_path(root);
        if rename_chains_path.exists() {
            if let Err(e) = std::fs::remove_file(&rename_chains_path) {
                tracing::warn!(
                    path = %rename_chains_path.display(),
                    error = %e,
                    "failed to drop rename_chains sidecar alongside --no-history; \
                     stale sidecar may surface in `vex status` until next rebuild"
                );
            }
        }
        // history_manifest_fields stays None — manifest serialises all
        // four `history_*` fields as None. rename_chains_built stays
        // None — pre-14.10 semantics ("not run").
    } else if opts.with_history {
        let prior_tip = prior_manifest_for_history
            .as_ref()
            .and_then(|m| m.state.history_tip_sha.clone());
        let prior_stats = prior_manifest_for_history
            .as_ref()
            .and_then(|m| m.state.history.clone());
        let prior_depth = prior_manifest_for_history
            .as_ref()
            .and_then(|m| m.state.history_depth);
        let current_tip = rev_parse_head(root);

        // Branch 2: no-op fast path. Requires (sidecar present) AND
        // (prior_tip == current_tip) AND (depth opt didn't change).
        // The depth check is important: a user who re-runs with
        // `--history-depth N` (different from prior) MUST get a full
        // rebuild — fast-path reuse would silently honour the old cap.
        let depth_unchanged = opts.history_depth == prior_depth;
        let tip_unchanged = matches!(
            (&prior_tip, &current_tip),
            (Some(a), Some(b)) if a == b
        );
        let sidecar_present = history_path.exists();

        if sidecar_present && tip_unchanged && depth_unchanged {
            tracing::debug!(
                path = %history_path.display(),
                tip = ?current_tip,
                "git_history fast-path: tip + depth unchanged, reusing existing sidecar"
            );
            history_manifest_fields = Some(HistoryManifestFields {
                indexed_at: today_iso_date(),
                tip_sha: current_tip,
                depth: opts.history_depth.or(prior_depth),
                stats: prior_stats.unwrap_or_default(),
            });
            // Phase 14.10 — fast path reuses the on-disk sidecar
            // verbatim, so the provenance must reuse the prior
            // manifest's values rather than regress to `None`. Without
            // this every no-op `vex update` would forget that the
            // sidecar exists, and `vex status` would prompt a re-index
            // even though the file is still valid on disk. Mirrors
            // how `prior_stats` is reused above.
            rename_chains_built = prior_manifest_for_history
                .as_ref()
                .and_then(|m| m.rename_chains_built);
            rename_chains_minilm_tiebreak_hits = prior_manifest_for_history
                .as_ref()
                .and_then(|m| m.rename_chains_minilm_tiebreak_hits);
        } else {
            // Phase 14.8 Step 5c — three sub-branches for the
            // "rebuild" case, picking the cheapest viable path:
            //
            //   3a. Force-push detected (prior_tip exists but is
            //       NOT an ancestor of HEAD): full rebuild, warn.
            //   3b. Linear history with new commits (prior_tip
            //       exists, IS an ancestor of HEAD, sidecar
            //       present): INCREMENTAL — walk only
            //       <prior_tip>..HEAD and merge into the prior
            //       section. Avoids re-walking the entire history
            //       for every commit added.
            //   3c. Otherwise (no prior tip, depth change, sidecar
            //       missing): full rebuild via the from-scratch
            //       builder. The cleanest semantic — no merge
            //       state to honour.
            let force_push = matches!(
                (&prior_tip, &current_tip),
                (Some(p), Some(c)) if p != c && !is_ancestor(root, p, c)
            );
            let can_incremental = !force_push
                && sidecar_present
                && depth_unchanged
                && matches!(
                    (&prior_tip, &current_tip),
                    (Some(p), Some(c)) if p != c
                );

            if force_push {
                if let (Some(prior), Some(current)) = (&prior_tip, &current_tip) {
                    tracing::warn!(
                        prior_tip = %prior,
                        current_tip = %current,
                        "phase 14.8: prior history tip is not an ancestor of HEAD \
                         (force-push or rebase detected). Full git_history rebuild forced."
                    );
                }
            }

            let build_result = if can_incremental {
                // Branch 3b: load prior + walk delta + merge.
                let prior_tip_sha = prior_tip
                    .as_deref()
                    .expect("can_incremental implies prior_tip Some");
                build_incremental(root, prior_tip_sha, opts.history_depth, &history_path)
            } else {
                // Branch 3a or 3c: from-scratch full rebuild.
                crate::index::history_builder::build_history_section_with_names(
                    &crate::index::history_builder::BuildConfig {
                        repo_root: root.to_path_buf(),
                        tip: "HEAD".to_string(),
                        depth: opts.history_depth,
                    },
                )
            };

            match build_result {
                Ok((section, entry_names)) => {
                    let input = crate::store::git_history::WriterInput {
                        section: &section,
                        entry_names: &entry_names,
                    };
                    if let Err(e) = crate::store::git_history::write_sidecar(&history_path, input) {
                        tracing::warn!(
                            path = %history_path.display(),
                            error = %e,
                            "failed to persist git_history sidecar; \
                             vex history will fall back to query-time walker"
                        );
                    } else {
                        tracing::debug!(
                            path = %history_path.display(),
                            entries = section.entries.len(),
                            commits = section.commits.len(),
                            blobs = section.blobs.len(),
                            depth_capped = section.was_depth_capped,
                            mode = if can_incremental { "incremental" } else { "full" },
                            "wrote git_history sidecar"
                        );

                        // Phase 14.10 — best-effort rename-chains sidecar.
                        // Paired with the git_history sidecar via the tip
                        // SHA + body_tokens_hash stale-guards; absent or
                        // stale sidecar = `vex history` falls back to
                        // singleton chains (v1.16 behaviour). Outcome is
                        // captured in `rename_chains_built` so the
                        // manifest below records the actual disk state
                        // (None = not attempted; Some(true) = sidecar
                        // written; Some(false) = tried but failed).
                        //
                        // MiniLM tie-breaker: enabled when this build
                        // emitted semantic vectors. Sym_idx-aligned
                        // `hashes` are recomputed inside the helper via
                        // `compute_hashes_for`; vectors are already
                        // L2-normalized post-v1.13 so the dot-product
                        // fast path engages.
                        if let Some(tip_bytes) = current_tip.as_deref().and_then(parse_tip_sha_20) {
                            let outcome = write_rename_chains_sidecar(
                                root,
                                &section,
                                &entry_names,
                                tip_bytes,
                                parsed,
                                vectors,
                                embedder_id.as_deref(),
                                !vectors.is_empty(),
                            );
                            rename_chains_built = Some(outcome.sidecar_written);
                            rename_chains_minilm_tiebreak_hits = outcome.minilm_tiebreak_hits;
                        }

                        history_manifest_fields = Some(HistoryManifestFields {
                            indexed_at: today_iso_date(),
                            tip_sha: current_tip,
                            depth: opts.history_depth,
                            stats: crate::index::manifest::HistoryStats {
                                commit_count: section.commits.len() as u32,
                                blob_count: section.blobs.len() as u32,
                                entry_count: section.entries.len() as u32,
                                depth_capped: Some(section.was_depth_capped),
                            },
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        root = %root.display(),
                        error = %e,
                        "git_history builder failed; sidecar not written. \
                         vex history will use the query-time walker."
                    );
                }
            }
        }
    }

    // v1.15.0 B1.2 — persist body_tokens sidecar in sym_idx order so a
    // subsequent `vex update` can restore body_tokens for unchanged
    // symbols (`reconstruct_unchanged`). Without this, reconstructed
    // symbols would produce body-less `context_hash` values that drift
    // from the fresh `vex index` baseline, defeating the B1.2 HNSW
    // incremental-update diff. Failure is non-fatal: missing sidecar
    // gracefully degrades to the pre-v1.15 reconstruct path (every
    // body_tokens is `None`), at the cost of incremental HNSW falling
    // back to full rebuild on the next update. The save outcome gates
    // `Manifest::body_tokens_persisted` below so `vex status` and the
    // next update's diagnostics stay accurate (without this gate, a
    // failed write would still record `Some(true)` and `vex status`
    // would report "Body tokens: yes" for a missing file).
    let body_tokens_path = config::body_tokens_path(root);
    let body_token_strings: Vec<Option<String>> = parsed
        .iter()
        .flat_map(|f| f.symbols.iter().map(|s| s.body_tokens.clone()))
        .collect();
    let body_tokens_saved =
        match crate::store::body_tokens::save(&body_tokens_path, &body_token_strings) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    path = %body_tokens_path.display(),
                    error = %e,
                    "failed to persist body_tokens sidecar; next `vex update` will \
                     fall back to body-less context_hash for unchanged symbols and \
                     trigger a full HNSW rebuild"
                );
                false
            }
        };

    // grep trigram skip-index sidecar (STORAGE-RESEARCH §2). One record
    // per code file: the presence bloom built in `parse_files` paired
    // with the `(len, mtime)` the file has right now, so `vex grep` can
    // skip files that provably cannot contain the pattern's literal —
    // guarded by a staleness check against edits made after this index.
    // Failure is non-fatal: a missing/partial sidecar just makes grep
    // full-walk (never a false negative).
    //
    // Two provenance classes in `parsed`:
    //   - `trigram_bloom = Some` ⟺ freshly parsed this run (read path or
    //     blob-cache hit). Emit a fresh record: pair the bloom with a
    //     live `stat()` for `(len, mtime)`.
    //   - `trigram_bloom = None` ⟺ reconstructed from the prior index on
    //     `vex update` (no bytes read). Carry the OLD sidecar record
    //     forward verbatim. A changed-but-unparseable file is dropped
    //     from `parsed` entirely (absent → grep full-reads → safe), so a
    //     `None` here is only ever a genuinely-unchanged file whose old
    //     bloom is still valid — never a stale bloom for changed content.
    let trigram_path = config::trigram_path(root);
    let needs_carry_forward = parsed.iter().any(|f| f.trigram_bloom.is_none());
    let old_trigram: HashMap<String, crate::store::trigram::TrigramRecord> = if needs_carry_forward
    {
        crate::store::trigram::load(&trigram_path)
            .map(|recs| recs.into_iter().map(|r| (r.rel_path.clone(), r)).collect())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    let mut trigram_records = Vec::with_capacity(parsed.len());
    for pf in parsed {
        match &pf.trigram_bloom {
            Some(bloom) => {
                // Live stat for the staleness guard. A stat failure
                // (deleted/renamed between parse and now) drops the
                // record → grep full-reads that path → safe.
                let full = root.join(&pf.path);
                if let Ok(meta) = std::fs::metadata(&full) {
                    if let Ok(mtime) = meta.modified() {
                        let (mtime_secs, mtime_nanos) = crate::store::trigram::mtime_parts(mtime);
                        trigram_records.push(crate::store::trigram::TrigramRecord {
                            rel_path: pf.path.clone(),
                            bloom: *bloom,
                            len: meta.len(),
                            mtime_secs,
                            mtime_nanos,
                        });
                    }
                }
            }
            None => {
                if let Some(old) = old_trigram.get(&pf.path) {
                    trigram_records.push(old.clone());
                }
                // else: absent → grep full-reads this path → safe.
            }
        }
    }
    let trigram_persisted = match crate::store::trigram::save(&trigram_path, &trigram_records) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                path = %trigram_path.display(),
                error = %e,
                "failed to persist trigram skip-index sidecar; `vex grep` will \
                 full-walk every file until the next successful index"
            );
            false
        }
    };

    let manifest_path = config::manifest_path(root);
    let manifest = Manifest {
        files: file_hashes.iter().cloned().collect::<HashMap<_, _>>(),
        git_head,
        indexed_at: Some(indexed_at),
        embedder_id,
        // Persist explicit `Some(false)` for opt-outs so `vex update`
        // can detect them. `Some(true)` rather than `None` for the
        // default case so post-10.3 manifests are unambiguous.
        call_graph: Some(opts.with_call_graph),
        bm25: Some(opts.with_bm25),
        pattern_index: Some(opts.with_pattern_index),
        // 11.4 Inc 5: `pattern_index_full` distinguishes a full
        // `vex index` from a `vex update`. The reader checks this
        // before using the indexed prefilter — incremental builds
        // produce a partial section (only re-parsed files have
        // skeletons) and would silently drop matches in unchanged
        // files. `is_update` is plumbed by the writer wrapper.
        pattern_index_full: Some(is_full_rebuild),
        // v1.13 P5: vectors are L2-normalized by `pipeline::run` /
        // `pipeline::update` before they reach this writer. Only
        // meaningful when vectors are present; `None` for the
        // no-embeddings case keeps pre-1.13 readers happy and avoids
        // a misleading "normalized: true" for an empty vector array.
        vectors_normalized: (!vectors.is_empty()).then_some(true),
        // v1.24+ — gated on the actual `index.trigram` save outcome so
        // `vex status` provenance matches disk (same pattern as
        // `rename_chains_built`).
        trigram_persisted: Some(trigram_persisted),
        // Phase 14.10 — gated on the actual sidecar write outcome (see
        // `rename_chains_built` initialisation comment above). `None`
        // when chain detection wasn't reached, `Some(true)` on a
        // successful write, `Some(false)` when the builder ran but the
        // write failed. Mirrors `body_tokens_persisted` semantics so
        // `vex status` provenance matches disk state.
        rename_chains_built,
        rename_chains_minilm_tiebreak_hits,
        // v1.21 — incremental-rebuild state, persisted to the
        // `index.state` sidecar (NOT this JSON). See `Manifest::state`.
        state: crate::index::incremental_state::IncrementalState {
            // v1.14: unconditional `Some(true)` — every index written by
            // this build performed Pass-2 C++ include resolution. Version
            // marker, not a project-content predicate (pure-Rust projects
            // still get `Some(true)`). Pre-1.14 indexes have `None`.
            cpp_includes_processed: Some(true),
            // v1.15.0 B1.2: gated on the actual sidecar save outcome.
            // `Some(true)` when the file is on disk; `Some(false)` when
            // the save failed; `None` only for pre-v1.15 indexes. Either
            // `Some(false)` or `None` triggers the same fallback on the
            // next `vex update`: body_tokens reconstructed as `None`,
            // embed-cache misses for unchanged symbols, full HNSW rebuild.
            body_tokens_persisted: Some(body_tokens_saved),
            // Phase 14.8 — populated only on successful sidecar write
            // (gated by `history_manifest_fields.is_some()`). Sticky
            // sentinel: `history_indexed_at.is_some()` IS the predicate
            // `vex status` / `vex update` use to decide "section present
            // and usable" (architect L3).
            history_indexed_at: history_manifest_fields
                .as_ref()
                .map(|f| f.indexed_at.clone()),
            history_tip_sha: history_manifest_fields
                .as_ref()
                .and_then(|f| f.tip_sha.clone()),
            history_depth: history_manifest_fields.as_ref().and_then(|f| f.depth),
            history: history_manifest_fields.as_ref().map(|f| f.stats.clone()),
            // Phase 11.1.10 (Q4-B) — reverse import map for cascade.
            // Empty for full-rebuild + binder-less projects; populated
            // whenever the writer's resolution loop or Q4-A reconstruction
            // observed at least one cross-file edge.
            imported_by: writer_meta.imported_by,
            // Sentinel: this writer ran the Q4-B path. Distinguishes
            // pre-11.1.10 indexes (`None`) from a Q4-B-aware writer that
            // observed no edges (`Some(true)` + empty `imported_by`).
            imported_by_built: Some(true),
        },
    };
    manifest.save(&manifest_path)?;
    Ok(())
}

/// Manifest fields populated only on successful Phase 14.8 sidecar
/// write. Grouped so the `Manifest { … }` literal below stays
/// readable instead of carrying four parallel `if let Some()` ladders.
struct HistoryManifestFields {
    indexed_at: String,
    tip_sha: Option<String>,
    depth: Option<usize>,
    stats: crate::index::manifest::HistoryStats,
}

/// Today's UTC date in ISO `YYYY-MM-DD`. Same Howard Hinnant
/// civil-date arithmetic as `cmd_history::unix_seconds_to_iso_date` —
/// kept inline here to avoid a `pub use` from a CLI module into the
/// pipeline layer.
fn today_iso_date() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);
    let days = (now / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = z - era * 146_097; // already i64
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y_civil = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (if m <= 2 { y_civil + 1 } else { y_civil }) as i32;
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Parse a 40-char hex SHA into raw 20 bytes. Returns `None` on any
/// shape error — used by the rename-chains tip-SHA guard, which
/// degrades gracefully to "no sidecar written" if the conversion fails.
fn parse_tip_sha_20(hex: &str) -> Option<[u8; 20]> {
    if hex.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, byte) in out.iter_mut().enumerate() {
        let s = hex.get(i * 2..i * 2 + 2)?;
        *byte = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

/// Phase 14.10 — best-effort rename-chains sidecar emit. Caller should
/// invoke this only AFTER the git_history sidecar has been written
/// successfully (the two sidecars share a tip-SHA guard).
///
/// Failures are non-fatal: a warn-log is emitted and the existing
/// rename_chains sidecar (if any) stays on disk. The next `vex history`
/// call will discard it via the stale-guard when its tip SHA disagrees
/// with the freshly-written `index.git_history`.
///
/// Outcome of a [`write_rename_chains_sidecar`] call. The boolean
/// reports the disk state (`true` = sidecar present and valid); the
/// hit count is `Some(n)` only when the MiniLM tie-breaker actually
/// ran (cosine path active) — `None` distinguishes "no semantic
/// embeddings available" from "ran but nothing was decisive".
struct RenameChainsWriteOutcome {
    sidecar_written: bool,
    minilm_tiebreak_hits: Option<u32>,
}

/// Returns whether the sidecar was persisted and how many MiniLM
/// tie-breaker decisions fired during the build. The caller threads
/// both into the manifest so `vex status` reports the same outcome
/// users observe on disk.
// Eight scalar/slice borrows from the caller; grouping them into a
// struct would just move the same argument list one level down.
// Acceptable for a single private callee with one caller.
//
// `assume_normalized_post_v1_13` records the pipeline invariant that
// every vector reaching this site has been L2-normalized by
// `pipeline::run`/`pipeline::update` (v1.13 P5). The caller passes
// `!vectors.is_empty()` because the only branch that yields a
// non-empty `vectors` slice is the post-v1.13 normalize-on-write
// path. If a future caller bypasses that path the parameter must
// flip to `false` so `CosineLookup` runs the full cosine formula
// instead of the dot-product fast path.
#[allow(clippy::too_many_arguments)]
fn write_rename_chains_sidecar(
    root: &Path,
    section: &crate::index::history_builder::HistorySection,
    entry_names: &[String],
    tip_sha: [u8; 20],
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    embedder_id: Option<&str>,
    assume_normalized_post_v1_13: bool,
) -> RenameChainsWriteOutcome {
    use crate::index::rename_chains::{
        build_rename_chains_with_stats, compute_body_tokens_hash, score::CosineLookup, BuildInput,
    };
    use crate::store::rename_chains as store_rc;

    let path = config::rename_chains_path(root);

    // The chain builder demands entry-keyed body/sig/context_hash
    // slices the same length as `entries`. The build path populates
    // body/sig from the parser (`HistorySection::entry_*_tokens`); a
    // merge-from-disk pads with None for the prior side (limitation
    // documented in `merge_history_sections`).
    let entry_count = section.entries.len();
    let body = if section.entry_body_tokens.len() == entry_count {
        section.entry_body_tokens.clone()
    } else {
        vec![None; entry_count]
    };
    let sig = if section.entry_sig_tokens.len() == entry_count {
        section.entry_sig_tokens.clone()
    } else {
        vec![None; entry_count]
    };
    let body_tokens_hash = compute_body_tokens_hash(&body);

    // MiniLM tie-breaker wiring (Phase 14.10 closure step). Active
    // only when this build produced semantic vectors *and* the names
    // slice matches the entries slice — both are caller invariants
    // but we guard defensively so a future refactor doesn't silently
    // disable the cosine path on length mismatch.
    let tip_hashes = if let Some(id) = embedder_id {
        if !vectors.is_empty() && vectors.len() == count_parsed_symbols(parsed) {
            match compute_hashes_for(parsed, id) {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!(
                        embedder_id = id,
                        error = %e,
                        "rename_chains: failed to compute current-tip context hashes; \
                         falling back to structural-only path"
                    );
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // Per-entry context_hash. Only entries whose `(path, name, kind)`
    // matches a current-tip symbol get a hash — historical-only
    // entries (the renamed-away pre-image side) get `None` and the
    // cosine helper returns 0.0 for missing hashes, which the gate
    // logic already handles. The map collapses overloads at the same
    // `(path, name, kind)` to the last-written hash; the resulting
    // ambiguity matches what `CosineLookup` already does with
    // duplicate vectors (keep-first).
    let (entry_context_hash, cosine_lookup) = if let Some(hashes) = tip_hashes.as_deref() {
        let key_to_hash = build_tip_hash_lookup(parsed, hashes);
        let resolved = resolve_entry_context_hashes(section, entry_names, &key_to_hash);
        let lookup =
            CosineLookup::from_hashed_vectors(vectors, hashes, assume_normalized_post_v1_13);
        (resolved, Some(lookup))
    } else {
        (vec![None; entry_count], None)
    };

    let input = BuildInput {
        entries: &section.entries,
        entry_body_tokens: &body,
        entry_sig_tokens: &sig,
        entry_context_hash: &entry_context_hash,
        body_tokens_hash,
        history_tip_sha_prefix: tip_sha,
        cosine_lookup: cosine_lookup.as_ref(),
    };

    let (artifact, stats) = match build_rename_chains_with_stats(input) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "rename_chains builder failed; sidecar not written"
            );
            return RenameChainsWriteOutcome {
                sidecar_written: false,
                // Builder failed before scoring → no semantic decisions
                // were made, so the count is `None` rather than `Some(0)`
                // (zero would falsely imply the path ran).
                minilm_tiebreak_hits: None,
            };
        }
    };

    // Capture stats up front so `Some(0)` survives even if the write
    // step fails — provenance is "did MiniLM decide anything", not
    // "did the sidecar land".
    let recorded_hits = cosine_lookup.as_ref().map(|_| stats.minilm_tiebreak_hits);

    let chain_count = artifact.chains.len();
    let forward_count = artifact.forward.len();
    let sidecar_written = match store_rc::save(&path, &artifact) {
        Ok(()) => {
            tracing::debug!(
                path = %path.display(),
                chains = chain_count,
                forward = forward_count,
                minilm_tiebreak_hits = stats.minilm_tiebreak_hits,
                cosine_active = cosine_lookup.is_some(),
                "wrote rename_chains sidecar"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to persist rename_chains sidecar; \
                 vex history will use singleton chains"
            );
            false
        }
    };

    RenameChainsWriteOutcome {
        sidecar_written,
        minilm_tiebreak_hits: recorded_hits,
    }
}

/// Total parsed symbol count — used to validate that the `vectors`
/// slice is sym_idx-aligned before we trust the parallel
/// `compute_hashes_for` output.
fn count_parsed_symbols(parsed: &[ParsedFile]) -> usize {
    parsed.iter().map(|f| f.symbols.len()).sum()
}

/// Build `(path, name, kind) -> context_hash` for current-tip
/// symbols. `hashes` is in `sym_idx` order, parallel to the flattened
/// `(file, sym)` iterator over `parsed` — same contract as
/// `compute_hashes_for`. Overload collisions at the same key keep the
/// FIRST-WRITTEN hash to mirror `CosineLookup::from_hashed_vectors`'s
/// keep-first dedup policy (score.rs:184). If both maps used the same
/// key but different sides of the collision, a `key_to_hash` lookup
/// could land on a hash that isn't in `CosineLookup`, silently
/// degrading every overload-affected entry to a 0.0 cosine — a
/// systematic miss instead of a graceful one.
fn build_tip_hash_lookup(
    parsed: &[ParsedFile],
    hashes: &[u64],
) -> std::collections::HashMap<(String, String, u8), u64> {
    let mut map: std::collections::HashMap<(String, String, u8), u64> =
        std::collections::HashMap::with_capacity(hashes.len());
    let mut idx = 0usize;
    for file in parsed {
        for sym in &file.symbols {
            // Defensive: don't panic on a hashes-slice shorter than
            // expected — the calling path already gated on lengths
            // matching but a future refactor could regress.
            if let Some(&h) = hashes.get(idx) {
                // Keep-first parity with CosineLookup.
                map.entry((file.path.clone(), sym.name.clone(), sym.kind as u8))
                    .or_insert(h);
            }
            idx += 1;
        }
    }
    map
}

/// Walk `section.entries` and look up each entry's `(file, name,
/// kind)` in the tip-side map. The file path is decoded from the
/// in-memory strings table; missing offsets and historical-only
/// entries surface as `None`.
fn resolve_entry_context_hashes(
    section: &crate::index::history_builder::HistorySection,
    entry_names: &[String],
    key_to_hash: &std::collections::HashMap<(String, String, u8), u64>,
) -> Vec<Option<u64>> {
    let mut out = Vec::with_capacity(section.entries.len());
    for (i, e) in section.entries.iter().enumerate() {
        let file = decode_string_at(&section.strings, e.file_offset);
        let Some(name) = entry_names.get(i) else {
            out.push(None);
            continue;
        };
        let hit = key_to_hash
            .get(&(file.to_string(), name.clone(), e.kind))
            .copied();
        out.push(hit);
    }
    out
}

/// Decode a length-prefixed UTF-8 string from the build-time strings
/// table (same layout as `StringTable::intern` produces). Returns
/// `""` for the empty-string sentinel at offset 0 and for any out-
/// of-range / non-UTF-8 read. Build-side mirror of
/// `HistoryReader::string`; kept private since callers only need the
/// `&str` view, not the offset arithmetic.
///
/// **Encoding contract** (must stay in lock-step with
/// `StringTable::intern` + `HistoryReader::string`): `[u32_le
/// byte_len][UTF-8 bytes; byte_len]`, offset 0 = empty sentinel. If
/// the on-disk encoding ever changes, update all three call sites
/// and the `decode_string_at_round_trips_build_time_strings` test —
/// the test pins our half of the contract.
fn decode_string_at(strings: &[u8], offset: u32) -> &str {
    let off = offset as usize;
    if off + 4 > strings.len() {
        return "";
    }
    let len_bytes: [u8; 4] = match strings[off..off + 4].try_into() {
        Ok(b) => b,
        Err(_) => return "",
    };
    let len = u32::from_le_bytes(len_bytes) as usize;
    let start = off + 4;
    if start + len > strings.len() {
        return "";
    }
    std::str::from_utf8(&strings[start..start + len]).unwrap_or("")
}

fn rev_parse_head(repo: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Phase 14.8 Step 5c — incremental sidecar rebuild on linear
/// history. Loads the existing sidecar, walks ONLY
/// `<prior_tip>..HEAD`, merges via [`merge_history_sections`]. Falls
/// back to a from-scratch full rebuild on any error (sidecar
/// corruption, range-walk failure) so the caller never sees a missing
/// section as a result of incremental failure.
fn build_incremental(
    root: &Path,
    prior_tip: &str,
    depth: Option<usize>,
    history_path: &Path,
) -> Result<(crate::index::history_builder::HistorySection, Vec<String>)> {
    use crate::index::history_builder::{
        build_history_section_for_range, build_history_section_with_names, merge_history_sections,
        BuildConfig,
    };
    use crate::store::git_history::HistoryReader;

    let cfg = BuildConfig {
        repo_root: root.to_path_buf(),
        tip: "HEAD".to_string(),
        depth,
    };

    // Defensive: any failure to load prior section → fall back to
    // full rebuild. The on-disk format hasn't changed under us today
    // (HISTORY_SECTION_VERSION = 1) but a future-version sidecar
    // we don't understand should still degrade cleanly.
    let prior = match HistoryReader::open(history_path) {
        Ok(Some(r)) => match r.extract_owned() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "incremental: failed to load prior section, full rebuild");
                return build_history_section_with_names(&cfg);
            }
        },
        _ => {
            tracing::warn!("incremental: sidecar missing/unreadable, full rebuild");
            return build_history_section_with_names(&cfg);
        }
    };

    // Walk only the delta. Empty delta (no new commits) reaches
    // this path only if `prior_tip != current_tip` but the range is
    // semantically empty (e.g. only merge commits filtered out); in
    // that case we return prior section as-is.
    let (delta_section, delta_names) = build_history_section_for_range(&cfg, prior_tip)?;
    if delta_section.entries.is_empty() && delta_section.commits.is_empty() {
        tracing::debug!("incremental: delta range produced no new commits; reusing prior section");
        return Ok(prior);
    }

    let prior_commit_count = prior.0.commits.len();
    let delta_commit_count = delta_section.commits.len();
    let merged = merge_history_sections(prior.0, prior.1, delta_section, delta_names);
    tracing::info!(
        prior_commits = prior_commit_count,
        delta_commits = delta_commit_count,
        merged_commits = merged.0.commits.len(),
        "phase 14.8: incremental git_history update"
    );
    Ok(merged)
}

/// Architect H3 force-push detector. Returns `true` when `prior` is
/// an ancestor of `current` (linear history, incremental update would
/// be safe). Returns `false` when `prior` was rewritten out of the
/// reachable history (force-push / rebase / cherry-pick).
///
/// `git merge-base --is-ancestor <A> <B>` exits 0 when A is an
/// ancestor of B, 1 otherwise. We treat any non-zero (incl. "object
/// not found", which happens after a hard reset) as non-ancestor.
fn is_ancestor(repo: &Path, prior: &str, current: &str) -> bool {
    std::process::Command::new("git")
        .current_dir(repo)
        .args(["merge-base", "--is-ancestor", prior, current])
        .status()
        .ok()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// v1.13 E2b: persistent embedding cache. Contexts whose
/// `xxh3(embedder_id || \0 || ctx)` hits the on-disk cache reuse the
/// stored vector and skip the embed step. ONNX model load is deferred
/// until at least one miss — `vex update` runs with no embedding-
/// affecting changes (e.g. comment-only edits on already-indexed
/// files, or no-op re-runs) skip the ~80 MB model load entirely.
///
/// Cache is updated in-place with newly-embedded vectors and saved
/// atomically before return. Cache load failures degrade silently
/// to the all-miss path — never block the embedding run.
/// Returns `(vectors, hashes)` paired by sym_idx. `hashes[i]` is the
/// `context_hash` that gates `vectors[i]` in the embed cache AND that
/// the HNSW index keys by (v1.14.1 B1.1). Callers thread `hashes`
/// through to `build_hnsw` so the search path can map HNSW results
/// back to sym_idx via the `hash_index` sidecar.
pub(super) fn generate_embeddings(
    parsed: &[ParsedFile],
    embedder_id: &str,
    root: &Path,
    device: crate::embed::Device,
    gpu_explicit: bool,
) -> Result<(Vec<Vec<f32>>, Vec<u64>)> {
    let total_start = Instant::now();

    // Resolve embedder dim + char budget WITHOUT touching the ONNX
    // model — these are compile-time consts for each known embedder.
    // Lets us pre-build contexts and probe the cache before deciding
    // whether the model load is needed at all.
    let dim = embed::embedder_dim(embedder_id)
        .with_context(|| format!("unknown embedder `{embedder_id}` — no recorded dim"))?;
    let budget = embed::embedder_char_budget(embedder_id)
        .with_context(|| format!("unknown embedder `{embedder_id}` — no recorded char budget"))?;

    // Step 1: build context strings + hashes in output order.
    //
    // v1.14.1 — parallelised via rayon. `build_context` (string assembly
    // + identifier tokenisation + path-keyword extraction) plus
    // `xxh3_64` over the result averages ~5μs/symbol; at 50k symbols
    // the sequential loop cost ~250ms on a warm M1, an outright second
    // on slower laptops. `par_iter().unzip()` preserves source order
    // (rayon contract) so the resulting `(contexts, hashes)` Vecs stay
    // sym_idx-aligned with everything downstream (cache lookup,
    // `hash_index` sidecar, vector slot). Pure CPU, no model load —
    // safe to run before the ONNX init decision below.
    let step1_start = Instant::now();
    let pairs: Vec<(&ParsedFile, &crate::index::symbols::ParsedSymbol)> = parsed
        .iter()
        .flat_map(|f| f.symbols.iter().map(move |s| (f, s)))
        .collect();
    let (contexts, hashes): (Vec<String>, Vec<u64>) = {
        use rayon::prelude::*;
        pairs
            .par_iter()
            .map(|(file, sym)| {
                let ctx = embed::build_context(
                    sym.kind.as_str(),
                    &sym.name,
                    &file.path,
                    sym.signature.as_deref(),
                    sym.doc.as_deref(),
                    sym.body_tokens.as_deref(),
                    budget,
                );
                let h = embed::cache::context_hash(embedder_id, &ctx);
                (ctx, h)
            })
            .unzip()
    };
    let total = contexts.len();
    tracing::debug!(
        symbols = total,
        elapsed = ?step1_start.elapsed(),
        "embed: step 1 parallel context+hash build complete"
    );

    // Step 2: load cache + partition into hits / misses.
    let cache_path = config::embed_cache_path(root, embedder_id);
    let mut cache = embed::cache::EmbedCache::load(&cache_path, embedder_id, dim);

    let mut all_vectors: Vec<Vec<f32>> = vec![Vec::new(); total];
    let mut miss_indices: Vec<usize> = Vec::new();
    for (i, &h) in hashes.iter().enumerate() {
        match cache.get(h) {
            Some(cached) => all_vectors[i] = cached.to_vec(),
            None => miss_indices.push(i),
        }
    }

    let hits = total - miss_indices.len();
    let misses = miss_indices.len();
    tracing::info!(
        total,
        hits,
        misses,
        cache_size_before = cache.len(),
        "embed cache partition"
    );

    // Step 3: if everything hit, return immediately — no ONNX load.
    if misses == 0 {
        tracing::info!(
            total,
            elapsed = ?total_start.elapsed(),
            "embedding complete (all cached, model load skipped)"
        );
        return Ok((all_vectors, hashes));
    }

    // Step 4: embed misses only.
    //
    // Miss-count gate (docs/GPU_SUPPORT.md §3.4): `Auto` that wasn't an
    // explicit `--gpu`/`--device` request stays on CPU for tiny update sets,
    // where per-run GPU/EP warm-up would dominate. The threshold is
    // model-aware — a heavier model has a far higher per-symbol CPU cost, so
    // its GPU break-even is at *fewer* misses (e.g. jina-code ~32 vs MiniLM
    // ~256). An explicit request bypasses the gate;
    // `Device::Cpu`/`Cuda`/`DirectMl`/`CoreMl` pass through unchanged. The
    // 0-miss early return above already covers no-ops.
    let gpu_auto_min_misses = crate::embed::embedder_gpu_auto_min_misses(embedder_id);
    let effective_device =
        if device == crate::embed::Device::Auto && !gpu_explicit && misses < gpu_auto_min_misses {
            crate::embed::Device::Cpu
        } else {
            device
        };
    let model_start = Instant::now();
    tracing::info!(
        embedder = embedder_id,
        device = ?effective_device,
        "loading embedding model"
    );
    // Graceful EP fallback (`strict = false`): if the GPU provider can't
    // register, ORT quietly serves CPU — an index build must never fail just
    // because the GPU is misconfigured (`vex gpu` is the strict diagnostic).
    // Concurrency: the boxed embedder wraps a `fastembed::TextEmbedding`,
    // which is `Send` but NOT `Sync` (ort 2.0.0-rc.12). The embed step below
    // must stay on this one thread — do not parallelise it (e.g. rayon) over
    // a shared embedder without redesigning ownership.
    let mut embedder = embed::make_embedder_with_device(embedder_id, effective_device, false)?;
    tracing::info!(
        elapsed = ?model_start.elapsed(),
        model = embedder.id(),
        dim = embedder.dim(),
        "model loaded"
    );

    let embed_start = Instant::now();
    // Collect miss contexts as owned `String`s; `embed_batch` takes `&[String]`.
    // Pass the WHOLE miss set in one call so the embedder can batch globally:
    // the GPU path (`batching::embed_length_aware`) length-sorts across all
    // misses to bound VRAM and minimise padding waste, while the CPU path falls
    // back to fastembed's internal batching. (Previously this chunked at a flat
    // `EMBED_BATCH_SIZE`, which on the GPU padded short contexts up to a mixed
    // batch's longest and ballooned attention memory — see docs/GPU_SUPPORT.md.)
    let miss_contexts: Vec<String> = miss_indices.iter().map(|&i| contexts[i].clone()).collect();
    let miss_vectors: Vec<Vec<f32>> = embedder.embed_batch(&miss_contexts)?;
    tracing::info!(
        misses,
        elapsed = ?embed_start.elapsed(),
        "embedding misses complete"
    );

    // Step 5: place miss vectors into output + update cache.
    for (j, &out_idx) in miss_indices.iter().enumerate() {
        let vec = miss_vectors[j].clone();
        cache.insert(hashes[out_idx], vec.clone());
        all_vectors[out_idx] = vec;
    }

    // NOTE: E3 mark-and-sweep used to live here but ran against `hashes`,
    // which in the `vex update` path is the **changed-files-only** subset
    // — it would evict every unchanged symbol's cache entry, defeating
    // the cache on the very next update. Sweep is now hoisted to
    // `prune_embed_cache`, called from the pipeline orchestrator
    // (`pipeline::run` / `pipeline::update`) after the full set of live
    // hashes is known. See `prune_embed_cache` below.

    // Step 6: persist cache. Failure is non-fatal — vectors are still
    // returned; next run will re-embed and re-attempt to persist.
    if let Err(e) = cache.save(&cache_path) {
        tracing::warn!(
            path = %cache_path.display(),
            error = %e,
            "embed cache: save failed; next run will rehash misses"
        );
    } else {
        tracing::info!(
            cache_size_after = cache.len(),
            path = %cache_path.display(),
            "embed cache persisted"
        );
    }

    tracing::info!(
        total,
        elapsed = ?total_start.elapsed(),
        "embedding complete"
    );
    Ok((all_vectors, hashes))
}

/// v1.14.1 E3 — load the embed cache, sweep orphan entries against the
/// **full** live-hash set, and save it back. Called by the pipeline
/// orchestrator (`pipeline::run` and `pipeline::update`) once the full
/// set of currently-indexed `context_hash`es is known. NOT called from
/// inside `generate_embeddings` because the update path invokes it with
/// only the changed-files slice — sweeping against that subset would
/// silently evict every unchanged symbol's cache entry, defeating the
/// cache on the next update (the bug the original E3 inadvertently
/// introduced; rust-reviewer found it before the change landed).
///
/// Cache-load failure (missing/malformed sidecar) is the cold-start
/// path: `EmbedCache::load` returns empty and there's nothing to sweep.
/// Save failure is non-fatal — we log and continue; next run rehashes.
pub(super) fn prune_embed_cache(
    root: &Path,
    embedder_id: &str,
    dim: u32,
    live_hashes: &[u64],
) -> Result<()> {
    let cache_path = config::embed_cache_path(root, embedder_id);
    let mut cache = embed::cache::EmbedCache::load(&cache_path, embedder_id, dim);
    if cache.is_empty() {
        return Ok(());
    }
    let before = cache.len();
    let swept = cache.sweep_to(live_hashes);
    if swept == 0 {
        return Ok(());
    }
    tracing::info!(
        swept,
        cache_size_before = before,
        cache_size_after = cache.len(),
        "embed cache: reclaimed orphan entries (E3 mark-and-sweep)"
    );
    if let Err(e) = cache.save(&cache_path) {
        tracing::warn!(
            path = %cache_path.display(),
            error = %e,
            "embed cache: post-sweep save failed; orphans will reappear next run"
        );
    }
    Ok(())
}

/// Compute `context_hash` for every symbol in `parsed`, in sym_idx
/// order. Used by the v1.14.1 B1.1 update path which already has the
/// merged `all_vectors` (unchanged + freshly-embedded) but needs the
/// matching `hashes` slice to key the HNSW. `generate_embeddings`
/// produces hashes only for changed files; this helper covers the
/// whole index in one pass.
///
/// **Stability across builds (v1.15.0 B1.2 closure):** `body_tokens`
/// participates in the hash. Reconstructed symbols load body_tokens
/// from the `index.bodytokens` sidecar via
/// `parse_files::reconstruct_unchanged`, so the hash a `vex update`
/// computes for an unchanged symbol matches what a fresh `vex index`
/// would produce. Pre-v1.15 indexes lack the sidecar; reconstructed
/// symbols get `body_tokens: None` and `build_hnsw_incremental`
/// falls back to full rebuild for that one update cycle (the next
/// `vex index` writes the sidecar).
pub(super) fn compute_hashes_for(parsed: &[ParsedFile], embedder_id: &str) -> Result<Vec<u64>> {
    let budget = embed::embedder_char_budget(embedder_id)
        .with_context(|| format!("unknown embedder `{embedder_id}` — no recorded char budget"))?;
    // Parallel mirror of `generate_embeddings` Step 1. The update path
    // calls this over the full merged corpus (unchanged + newly parsed),
    // potentially 50k+ symbols — sequential would re-introduce the
    // ~250ms wall-clock bottleneck Step 1 was parallelised away from.
    // Same ordered-unzip contract: rayon preserves source order so the
    // resulting Vec stays sym_idx-aligned with `all_vectors` and the
    // `index.hashes` sidecar.
    use rayon::prelude::*;
    let pairs: Vec<(&ParsedFile, &crate::index::symbols::ParsedSymbol)> = parsed
        .iter()
        .flat_map(|f| f.symbols.iter().map(move |s| (f, s)))
        .collect();
    let hashes: Vec<u64> = pairs
        .par_iter()
        .map(|(file, sym)| {
            let ctx = embed::build_context(
                sym.kind.as_str(),
                &sym.name,
                &file.path,
                sym.signature.as_deref(),
                sym.doc.as_deref(),
                sym.body_tokens.as_deref(),
                budget,
            );
            embed::cache::context_hash(embedder_id, &ctx)
        })
        .collect();
    Ok(hashes)
}

/// v1.14.1 B1.1: build HNSW keyed by `context_hash` (not by sym_idx).
/// Content-based keys are stable across `vex update` runs — the
/// prerequisite for B1.2 incremental update. The pairing sidecar
/// `index.hashes` is written alongside so the query path can map HNSW
/// results back to sym_idx via `search::hash_index::load`.
///
/// Pre-1.14.1 indexes keyed by sym_idx are rebuilt on next `vex
/// index`; `HnswHandle::open` requires the sidecar to be present so a
/// stale numeric-keyed `index.hnsw` without a sidecar degrades to
/// brute-force search (same as a missing HNSW altogether).
pub(super) fn build_hnsw(root: &Path, vectors: &[Vec<f32>], hashes: &[u64]) -> Result<()> {
    let hnsw_path = config::hnsw_path(root);
    let hash_index_path = config::hash_index_path(root);
    build_hnsw_at(&hnsw_path, &hash_index_path, vectors, hashes)
}

// v1.15.0 B1.2 — `build_hnsw_at` and `build_hnsw_incremental_at` are
// exposed `pub` (re-exported via `#[doc(hidden)]` from `pipeline::mod`)
// so the bench (`benches/perf_b12.rs`) and the integration test
// (`tests/cli_incremental_hnsw_test.rs`) drive the EXACT same code
// path production does, instead of inlining an "equivalent" copy
// that could silently drift on parameter changes (e.g. usearch
// `IndexOptions` tweaks). The shim convention matches the v1.12.0
// `__fuzz_*` doc-hidden exports — keeps the user-facing public
// surface minimal while letting bench/test/fuzz reach the real impl.

/// v1.15.0 C — two-phase atomic commit for the HNSW + hash-index
/// sidecar pair. Both files are written to their `.tmp` siblings
/// (with fsync after each write so the bytes are durable on disk),
/// then renamed back-to-back. A process kill between the two rename
/// syscalls leaves the on-disk state in a "HNSW new, sidecar old"
/// configuration that `HnswHandle::open`'s size-check catches and
/// falls back to brute force — same self-heal path the v1.14.1 build
/// had, but the inconsistency window shrinks from ~ms (between two
/// content writes for an MB-sized HNSW + sidecar) to ~μs (between
/// two adjacent `rename` syscalls).
///
/// Ordering: HNSW renames first, sidecar last. The intermediate state
/// (HNSW new, sidecar old) is handled by `HnswHandle::open` already;
/// inverting the order would leave a window where the sidecar
/// references hashes that aren't in the HNSW yet, which could produce
/// wrong (empty) search results instead of a clean brute-force
/// fallback. Last-rename-wins is the safe direction.
///
/// On HNSW rename failure: the sidecar tmp is left in place (best-
/// effort cleanup) — the previous build's files remain untouched.
/// On sidecar rename failure after HNSW rename succeeded: HNSW is
/// already in the new state on disk; the error bubbles up to the
/// orchestrator so the user sees it loudly. (Same `Err`-propagates-
/// loudly contract as the inline form had.)
fn commit_hnsw_and_sidecar(
    index: usearch::Index,
    hnsw_path: &Path,
    hash_index_path: &Path,
    hashes: &[u64],
) -> Result<()> {
    let hnsw_tmp = hnsw_path.with_extension("hnsw.tmp");
    let hash_tmp = hash_index_path.with_extension("hashes.tmp");

    // Phase 1: write both tmps (durable on disk after fsync). usearch
    // doesn't expose an fsync hook on its save path, but a buggy or
    // crashed write would manifest as a partial tmp file that the
    // subsequent rename can't fix — the read path's size-check still
    // catches it, falling back to brute force.
    let hnsw_tmp_str = hnsw_tmp
        .to_str()
        .context("HNSW tmp path contains non-UTF-8 characters")?;
    index
        .save(hnsw_tmp_str)
        .with_context(|| format!("save HNSW to {}", hnsw_tmp.display()))?;
    if let Err(e) = crate::search::hash_index::save_to_tmp(&hash_tmp, hashes) {
        // Sidecar tmp failed → HNSW tmp leaked; clean it up before
        // returning so the next run starts from a clean state.
        let _ = std::fs::remove_file(&hnsw_tmp);
        return Err(e.context("save hash-index sidecar to tmp"));
    }

    // v1.15.1 Windows fix: drop the usearch handle BEFORE the rename
    // over `hnsw_path`. The incremental path calls `index.load(hnsw_path)`
    // earlier, which on Windows holds an exclusive file handle on the
    // loaded file. `std::fs::rename` cannot replace a file that another
    // handle in the same process has open (`ERROR_ACCESS_DENIED` /
    // os error 5) — Linux tolerates this via the unix unlink-while-open
    // semantics. Taking `index` by value lets us release it here, after
    // the tmp save (which only writes to `hnsw_tmp`, a different path)
    // and before the rename targets `hnsw_path`. The full-rebuild path
    // through `build_hnsw_at` is also safe: it constructs `index` with
    // `new_index()` and never loads from disk, so the drop just
    // releases process memory there.
    drop(index);

    // Phase 2: atomic renames back-to-back. The inconsistency window
    // here is the smallest the kernel allows — two adjacent rename
    // syscalls on local FS take ~μs each. On Windows we retry briefly
    // (see [`rename_with_windows_retry`]) because Windows surfaces
    // transient `ERROR_SHARING_VIOLATION` after `index.save()` until
    // the usearch C++ FFI's file handle is fully released and
    // antivirus / search-indexer real-time scans relinquish their own
    // brief read locks.
    if let Err(e) = rename_with_windows_retry(&hnsw_tmp, hnsw_path) {
        // HNSW rename failed → both tmps still present, both finals
        // unchanged. Clean both tmps so the next run isn't confused
        // by stale fixtures.
        let _ = std::fs::remove_file(&hnsw_tmp);
        let _ = std::fs::remove_file(&hash_tmp);
        return Err(e)
            .with_context(|| format!("rename {} → {}", hnsw_tmp.display(), hnsw_path.display()));
    }
    if let Err(e) = rename_with_windows_retry(&hash_tmp, hash_index_path) {
        // HNSW rename succeeded but sidecar rename failed — disk is
        // now in the "HNSW new, sidecar old" state. `HnswHandle::open`
        // size-check will catch this and brute-force; next successful
        // update self-heals. Surface the error loudly so the user
        // knows to re-run.
        let _ = std::fs::remove_file(&hash_tmp);
        return Err(e).with_context(|| {
            format!(
                "rename {} → {} (HNSW already committed; next update self-heals)",
                hash_tmp.display(),
                hash_index_path.display()
            )
        });
    }

    Ok(())
}

/// v1.15.2 Windows hardening: `std::fs::rename` on Windows fails with
/// `ERROR_ACCESS_DENIED` (os error 5) or `ERROR_SHARING_VIOLATION`
/// (os error 32) when any process — including antivirus / Windows
/// Defender / the search indexer — holds a handle on either the source
/// or destination file. The v1.15.1 `drop(index)` fix above releases
/// usearch's own handle on the loaded file, but the underlying C++ FFI
/// close + the OS-level handle release are not synchronous, and on a
/// freshly-written file Defender can grab a read handle for content
/// scanning within microseconds of `save()`. Both windows close out
/// quickly; a short retry with backoff masks them without changing
/// semantics on Linux/macOS (those targets get a single rename and a
/// hard error on real failure).
///
/// Total budget: up to ~1.1s across 10 attempts (20ms, 40ms, …,
/// 200ms). The first 4 attempts cost <200ms and cover ~all observed
/// races; the rest is paranoia for slow CI runners.
fn rename_with_windows_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    if !cfg!(windows) {
        return std::fs::rename(from, to);
    }
    const MAX_ATTEMPTS: u32 = 10;
    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..MAX_ATTEMPTS {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let retryable = matches!(e.raw_os_error(), Some(5) | Some(32));
                if !retryable || attempt + 1 == MAX_ATTEMPTS {
                    return Err(e);
                }
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20 * (attempt as u64 + 1)));
            }
        }
    }
    // Unreachable in practice — the loop above returns on every path.
    // Keep an explicit fallback so a future refactor that drops the
    // `attempt + 1 == MAX_ATTEMPTS` guard doesn't silently loop.
    Err(last_err.unwrap_or_else(|| std::io::Error::other("rename retry exhausted")))
}

/// Core HNSW + sidecar builder with explicit paths. The `build_hnsw`
/// wrapper above resolves them via `config::hnsw_path` /
/// `config::hash_index_path`; this layer is split out so unit tests can
/// drive the same code path without touching the `set_cache_override`
/// `OnceLock` (which under `cargo test` is shared across thread-parallel
/// sibling tests and produces the wrong cache dir for whichever ran
/// second). Production callers should always go through `build_hnsw`.
pub fn build_hnsw_at(
    hnsw_path: &Path,
    hash_index_path: &Path,
    vectors: &[Vec<f32>],
    hashes: &[u64],
) -> Result<()> {
    use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};

    anyhow::ensure!(
        vectors.len() == hashes.len(),
        "build_hnsw: vectors/hashes length mismatch ({} vs {})",
        vectors.len(),
        hashes.len(),
    );

    let dim = vectors[0].len(); // guaranteed non-empty by caller

    let options = IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 0,     // auto
        expansion_add: 0,    // auto
        expansion_search: 0, // auto
        multi: false,
    };

    let index = new_index(&options).context("create HNSW index")?;
    index
        .reserve(vectors.len())
        .context("reserve HNSW capacity")?;

    // v1.15.1: dedup on hash key before HNSW insert ONLY. `context_hash`
    // collapses (kind, name, path, signature, doc, body_tokens) into a
    // u64 without a byte-offset disambiguator (see `compute_hashes_for`
    // above), so two C++ symbols with identical signatures in the same
    // file (forward decl + def, overloads, anonymous-namespace clones)
    // produce the same key. usearch's high-level `Index::add` opens
    // with `multi: false` and treats a duplicate key as a hard error,
    // which — pre-fix — aborted the whole index build mid-corpus and
    // left no on-disk HNSW. We keep the first occurrence (skip-and-warn)
    // so a single collision can't bring down the semantic channel.
    //
    // The on-disk hash sidecar stays sym_idx-aligned (full `hashes`
    // slice, duplicates and all) — `src/search/semantic.rs:156` checks
    // `hashes.len() == expected_symbols` at query open and bails to
    // brute force if they disagree. The reader at the same site already
    // dedups duplicate hashes via `entry().or_insert` (keeps first
    // sym_idx, logs `collisions` count), so the duplicate-symbol case
    // is handled end-to-end without dropping any record from the index.
    // Single `first_idx: HashMap<u64, usize>` maps each hash to the
    // sym_idx of its FIRST occurrence — `entry().or_insert(i)` collapses
    // the "have we seen this hash?" check and the "what was the first
    // sym_idx?" lookup into one operation, so the warn log never needs
    // a fallback for a missing key.
    let mut first_idx: std::collections::HashMap<u64, usize> =
        std::collections::HashMap::with_capacity(hashes.len());
    let mut inserted: usize = 0;
    let mut skipped: usize = 0;
    for (i, (vec, &h)) in vectors.iter().zip(hashes.iter()).enumerate() {
        use std::collections::hash_map::Entry;
        match first_idx.entry(h) {
            Entry::Occupied(o) => {
                tracing::warn!(
                    hash = format_args!("0x{h:016x}"),
                    first_sym_idx = *o.get(),
                    duplicate_sym_idx = i,
                    "HNSW build: duplicate context_hash — skipping second occurrence"
                );
                skipped += 1;
            }
            Entry::Vacant(v) => {
                v.insert(i);
                index.add(h, vec).context("add vector to HNSW index")?;
                inserted += 1;
            }
        }
    }

    // v1.15.0 C — two-phase atomic commit. Write both files to .tmp
    // paths, fsync, then rename both back-to-back. See
    // `commit_hnsw_and_sidecar` for the full rationale. `index` is
    // moved (v1.15.1 Windows fix — released before rename targets the
    // loaded file).
    commit_hnsw_and_sidecar(index, hnsw_path, hash_index_path, hashes)?;

    if skipped > 0 {
        tracing::warn!(
            skipped_duplicates = skipped,
            inserted,
            input = hashes.len(),
            "HNSW build: completed with duplicate-hash skips (v1.15.1 dedup)"
        );
    }

    tracing::info!(
        vectors = inserted,
        sidecar_entries = hashes.len(),
        path = %hnsw_path.display(),
        sidecar = %hash_index_path.display(),
        "HNSW index built"
    );

    Ok(())
}

/// v1.15.0 B1.2: tombstone threshold for incremental HNSW updates.
/// When removed hashes exceed this fraction of the old index, the
/// incremental path bails and the caller falls back to a full rebuild —
/// at high churn the per-key `remove()` cost (HNSW relinks neighbours)
/// plus the lingering tombstone overhead in the on-disk file outweighs
/// the rebuild. 25% is the same heuristic used by usearch's internal
/// compaction guidance and matches our SHOULD-FIX/SHOULD-bench number.
/// Expressed as integer arithmetic (`removed * 4 > old_len`) so the
/// check is exact across small `old_len` values where a float
/// comparison could disagree with intuition.
pub(super) const INCREMENTAL_TOMBSTONE_NUMERATOR: usize = 1;
pub(super) const INCREMENTAL_TOMBSTONE_DENOMINATOR: usize = 4;

/// v1.15.0 B1.2: try an incremental HNSW update. Returns `Ok(true)` when
/// the incremental path succeeded; the caller MUST NOT call `build_hnsw`
/// after a `true` result (the index + sidecar are already on disk).
/// Returns `Ok(false)` for the "expected fallback" reasons: no prior
/// HNSW or hash-index sidecar on disk (cold start / pre-v1.14.1 index),
/// tombstone threshold exceeded, or vectors slice empty. Returns `Err`
/// only for true I/O / usearch errors the caller should surface.
///
/// **Why a bool instead of an enum**: the caller pattern is
/// `if !try_incremental { full_rebuild }` — every fallback reason
/// degrades to the same recovery action. Reasons are surfaced via
/// `tracing::debug!` for diagnostics, not via the return type.
pub(super) fn build_hnsw_incremental(
    root: &Path,
    new_vectors: &[Vec<f32>],
    new_hashes: &[u64],
) -> Result<bool> {
    let hnsw_path = config::hnsw_path(root);
    let hash_index_path = config::hash_index_path(root);
    build_hnsw_incremental_at(&hnsw_path, &hash_index_path, new_vectors, new_hashes)
}

/// Core incremental builder with explicit paths. Mirrors `build_hnsw_at`
/// for testability: lets unit tests target a temp directory without
/// touching the `set_cache_override` `OnceLock`. Production callers go
/// through `build_hnsw_incremental`.
pub fn build_hnsw_incremental_at(
    hnsw_path: &Path,
    hash_index_path: &Path,
    new_vectors: &[Vec<f32>],
    new_hashes: &[u64],
) -> Result<bool> {
    use std::collections::HashSet;
    use usearch::{new_index, IndexOptions, MetricKind, ScalarKind};

    anyhow::ensure!(
        new_vectors.len() == new_hashes.len(),
        "build_hnsw_incremental: vectors/hashes length mismatch ({} vs {})",
        new_vectors.len(),
        new_hashes.len(),
    );

    if new_vectors.is_empty() {
        tracing::debug!("HNSW incremental: empty corpus → fall back to caller cleanup");
        return Ok(false);
    }

    if !hnsw_path.exists() {
        tracing::debug!(
            path = %hnsw_path.display(),
            "HNSW incremental: no prior HNSW file → cold start, fall back to full rebuild"
        );
        return Ok(false);
    }
    if !hash_index_path.exists() {
        tracing::debug!(
            path = %hash_index_path.display(),
            "HNSW incremental: no prior hash-index sidecar → pre-v1.14.1 index, fall back"
        );
        return Ok(false);
    }

    let old_hashes = match crate::search::hash_index::load(hash_index_path) {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!(
                path = %hash_index_path.display(),
                error = %e,
                "HNSW incremental: hash-index sidecar load failed → fall back"
            );
            return Ok(false);
        }
    };

    let old_set: HashSet<u64> = old_hashes.iter().copied().collect();
    let new_set: HashSet<u64> = new_hashes.iter().copied().collect();

    let to_remove: Vec<u64> = old_set.difference(&new_set).copied().collect();
    let to_add_indices: Vec<usize> = new_hashes
        .iter()
        .enumerate()
        .filter(|(_, h)| !old_set.contains(h))
        .map(|(i, _)| i)
        .collect();

    // Tombstone threshold: `removed * DENOM > old_len * NUM` i.e.
    // `removed / old_len > NUM / DENOM` without the float comparison.
    // Overflow safety: both operands are bounded by `hash_index::MAX_COUNT`
    // = 10M; `10M * 4` = 40M, well within `usize::MAX` even on 32-bit
    // targets (4G). No `checked_mul` needed under that invariant.
    if to_remove.len() * INCREMENTAL_TOMBSTONE_DENOMINATOR
        > old_hashes.len() * INCREMENTAL_TOMBSTONE_NUMERATOR
    {
        // Threshold-exceeded is an expected performance-tuning event,
        // not an error — log at `debug` so it doesn't spam RUST_LOG=info
        // output during normal large-refactor runs.
        tracing::debug!(
            removed = to_remove.len(),
            old_size = old_hashes.len(),
            "HNSW incremental: tombstone threshold exceeded ({}/{} > {}/{}) → full rebuild",
            to_remove.len(),
            old_hashes.len(),
            INCREMENTAL_TOMBSTONE_NUMERATOR,
            INCREMENTAL_TOMBSTONE_DENOMINATOR,
        );
        return Ok(false);
    }

    // Open a mutable index, then `load()` reads the existing graph into
    // it. Dim mismatch (e.g. embedder changed) surfaces here as a load
    // error — `Ok(false)` falls back to full rebuild which writes a
    // fresh index with the right dim.
    let dim = new_vectors[0].len();
    let options = IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    };
    let index = match new_index(&options) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "HNSW incremental: new_index failed → full rebuild");
            return Ok(false);
        }
    };

    let path_str = hnsw_path
        .to_str()
        .context("HNSW path contains non-UTF-8 characters")?;
    if let Err(e) = index.load(path_str) {
        tracing::warn!(
            path = %hnsw_path.display(),
            error = %e,
            "HNSW incremental: usearch load failed → full rebuild"
        );
        return Ok(false);
    }

    // Reserve up-front for the post-mutation size so usearch doesn't
    // re-grow its arena mid-loop. `to_add_indices.len() + old_size`
    // is the upper bound; once we remove the orphans the real size is
    // `new_vectors.len()`, but over-reserving is cheap and avoids the
    // off-by-one of computing it before `remove()` runs.
    let post_capacity = old_hashes.len() + to_add_indices.len();
    if let Err(e) = index.reserve(post_capacity) {
        tracing::warn!(error = %e, "HNSW incremental: reserve failed → full rebuild");
        return Ok(false);
    }

    let removed_count = to_remove.len();
    for h in &to_remove {
        // `remove` returns the count of points removed. Zero is the
        // "key not present" path — defensive but expected if the
        // sidecar drifted from the HNSW. Swallow rather than bail; the
        // worst case is a stale tombstone that the next compaction
        // (full rebuild) sweeps.
        if let Err(e) = index.remove(*h) {
            tracing::debug!(hash = h, error = %e, "HNSW incremental: remove key error (ignored)");
        }
    }

    // v1.15.1: dedup add-candidates so duplicates inside the new batch
    // (two C++ symbols with the same context_hash) don't trip usearch's
    // `multi: false` duplicate-key error. Two collision sources exist;
    // `to_add_indices` is built from `.filter(|h| !old_set.contains(h))`
    // above, which handles the new-vs-existing case — leaving only the
    // new-vs-new case for this loop to guard. The `entry()` dance
    // mirrors the full-rebuild path so the warn log always has a
    // first-occurrence sym_idx.
    let mut add_first: std::collections::HashMap<u64, usize> =
        std::collections::HashMap::with_capacity(to_add_indices.len());
    let mut add_skipped: usize = 0;
    let mut added_count: usize = 0;
    for &i in &to_add_indices {
        let h = new_hashes[i];
        use std::collections::hash_map::Entry;
        match add_first.entry(h) {
            Entry::Occupied(o) => {
                tracing::warn!(
                    hash = format_args!("0x{h:016x}"),
                    first_sym_idx = *o.get(),
                    duplicate_sym_idx = i,
                    "HNSW incremental: duplicate context_hash in new batch — skipping"
                );
                add_skipped += 1;
            }
            Entry::Vacant(v) => {
                v.insert(i);
                if let Err(e) = index.add(h, &new_vectors[i]) {
                    // `index.save` has not run yet at this point, so the
                    // on-disk HNSW is still the original pre-load
                    // snapshot. Safe to ask caller to do a full rebuild
                    // — it will overwrite cleanly.
                    tracing::warn!(
                        sym_idx = i,
                        hash = h,
                        error = %e,
                        "HNSW incremental: add failed mid-batch → full rebuild"
                    );
                    return Ok(false);
                }
                added_count += 1;
            }
        }
    }

    // v1.15.0 C — two-phase atomic commit. Same `commit_hnsw_and_sidecar`
    // helper the full-rebuild path uses; the incremental mutation is
    // already in the `index` handle, so we just need to publish both
    // files together. Pre-1.15.0 this was two separate calls
    // (`index.save` then `hash_index::save`) with a ~ms gap between
    // them where the on-disk state could be observed in a "HNSW new,
    // sidecar old" mismatch. The two-phase commit shrinks that window
    // to ~μs (two adjacent rename syscalls).
    //
    // `commit_hnsw_and_sidecar` returns Err iff one of the renames
    // failed — that's the only path that bubbles up, matching the
    // Err contract documented at the function head.
    commit_hnsw_and_sidecar(index, hnsw_path, hash_index_path, new_hashes)
        .context("HNSW incremental: two-phase commit")?;

    if add_skipped > 0 {
        tracing::warn!(
            skipped_duplicates = add_skipped,
            added = added_count,
            "HNSW incremental: completed with duplicate-hash skips in new batch (v1.15.1 dedup)"
        );
    }

    tracing::info!(
        added = added_count,
        removed = removed_count,
        new_size = new_vectors.len(),
        old_size = old_hashes.len(),
        "HNSW incremental update applied"
    );

    Ok(true)
}

/// v1.15.0 B1.2 libFuzzer shim — drives `build_hnsw_incremental_at`
/// against a baked baseline HNSW + sidecar, with the input bytes
/// decoded as the `new_hashes` slice. Goal: no panic on any byte
/// sequence the diff/mutate path sees, since the function is invoked
/// on every `vex update --semantic` run with content the user has
/// no direct control over (the hash set is derived from
/// `compute_hashes_for` over freshly parsed code).
///
/// Risk surface this catches:
///   - HashSet construction on adversarial duplicate-heavy `new_hashes`
///   - tombstone-threshold arithmetic at boundary inputs
///   - usearch's `add(k, v)` / `remove(k)` reaction to corner cases
///     (collisions with already-removed keys, multi-remove of the
///     same key, add of a key that was just removed)
///   - the sidecar-rewrite-after-HNSW-save error path
///
/// Same convention as `crate::search::hash_index::__fuzz_hash_index_bytes`
/// and the v1.12.0 bloom / v1.13.0 marker harnesses — `pub fn` under a
/// `#[doc(hidden)]` umbrella so the fuzz crate can reach it without
/// widening the user-facing API.
#[doc(hidden)]
pub fn __fuzz_incremental_hnsw_bytes(data: &[u8]) {
    use std::sync::OnceLock;

    // One-time baseline build. 8 vectors at dim 8 — small enough that
    // each fuzz iteration's HNSW load + mutate completes in <1ms,
    // dense enough to exercise usearch's neighbour-link relaxation.
    static BASELINE: OnceLock<(std::path::PathBuf, std::path::PathBuf, usize)> = OnceLock::new();
    let (baseline_hnsw, baseline_hash, dim) = BASELINE.get_or_init(|| {
        const FUZZ_DIM: usize = 8;
        let baseline_dir = std::env::temp_dir().join("__vex_fuzz_inc_hnsw_baseline");
        // Idempotent setup: clear any stale baseline from a prior crashed
        // run so we don't accidentally seed from a corrupt fixture.
        let _ = std::fs::remove_dir_all(&baseline_dir);
        std::fs::create_dir_all(&baseline_dir).expect("baseline dir create");
        let baseline_hnsw = baseline_dir.join("index.hnsw");
        let baseline_hash = baseline_dir.join("index.hashes");

        let vectors: Vec<Vec<f32>> = (0..8)
            .map(|i| {
                let mut v = vec![0.0_f32; FUZZ_DIM];
                v[i] = 1.0;
                v
            })
            .collect();
        let hashes: Vec<u64> = (0..8).map(|i| 0xCAFE_0000_u64 + i as u64).collect();
        build_hnsw_at(&baseline_hnsw, &baseline_hash, &vectors, &hashes)
            .expect("baseline build_hnsw_at — fuzz harness setup");
        (baseline_hnsw, baseline_hash, FUZZ_DIM)
    });

    // Per-iteration scratch — copy of the baseline so the mutation
    // path can't corrupt the fixture for subsequent iterations.
    //
    // Keyed by `process::id()`: libFuzzer is single-threaded per
    // worker process, so iterations within one process serialise here
    // and don't collide. `cargo fuzz run --jobs N` spawns N separate
    // processes with different PIDs — they get distinct scratch dirs
    // by construction. If a worker is killed mid-iteration (signal /
    // OOM) the scratch dir leaks until the next process with the same
    // PID (after OS PID recycling) hits the leading `remove_dir_all`
    // below — that's the self-cleanup contract. Don't replace this
    // with a `LazyLock`-scoped path: that would skip the per-iter
    // wipe and the mutation would carry over.
    let scratch_dir =
        std::env::temp_dir().join(format!("__vex_fuzz_inc_hnsw_iter_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch_dir);
    if std::fs::create_dir_all(&scratch_dir).is_err() {
        // Disk failure (full / permission) — libfuzzer will move on.
        return;
    }
    let scratch_hnsw = scratch_dir.join("index.hnsw");
    let scratch_hash = scratch_dir.join("index.hashes");
    if std::fs::copy(baseline_hnsw, &scratch_hnsw).is_err() {
        return;
    }
    if std::fs::copy(baseline_hash, &scratch_hash).is_err() {
        return;
    }

    // Decode the fuzz input as a `Vec<u64>` of new_hashes. Cap at 256
    // to keep iteration time bounded — at higher counts the HNSW
    // `add()` loop dominates and reduces effective fuzz throughput.
    const MAX_HASHES: usize = 256;
    let n_hashes = (data.len() / 8).min(MAX_HASHES);
    let mut new_hashes: Vec<u64> = Vec::with_capacity(n_hashes);
    for i in 0..n_hashes {
        let chunk = &data[i * 8..i * 8 + 8];
        // `try_into` on an exact-8-byte slice can't fail; this is just
        // appeasing the compiler. `unwrap_or` keeps the shim total.
        let arr: [u8; 8] = chunk.try_into().unwrap_or([0; 8]);
        new_hashes.push(u64::from_le_bytes(arr));
    }

    // Matching synthetic vectors — deterministic one-hot at slot
    // `i % dim`. With `n_hashes > FUZZ_DIM` multiple entries share
    // the same vector by design; the shim's only job is "no panic",
    // and vector-shape coverage of `add()` is exercised by the
    // property test, not here. Don't "fix" this to use the bench's
    // PRNG — wasted entropy on a path that doesn't care.
    let new_vectors: Vec<Vec<f32>> = (0..new_hashes.len())
        .map(|i| {
            let mut v = vec![0.0_f32; *dim];
            v[i % *dim] = 1.0;
            v
        })
        .collect();

    // Drive incremental. Result discarded — Ok(true), Ok(false), or
    // any Err is acceptable. Only a panic / abort signals a real
    // defect. The function MUST be total over byte-sequence input.
    let _ = build_hnsw_incremental_at(&scratch_hnsw, &scratch_hash, &new_vectors, &new_hashes);

    let _ = std::fs::remove_dir_all(&scratch_dir);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{cache::context_hash, MINILM_CHAR_BUDGET, MINILM_DIM, MINILM_ID};
    use crate::index::symbols::{ParsedSymbol, SymbolKind};

    fn mk_sym(name: &str, line: usize) -> ParsedSymbol {
        ParsedSymbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            line,
            signature: Some(format!("fn {name}()")),
            doc: None,
            body_tokens: None,
        }
    }

    /// End-to-end E2b check: when every context hashes to a cache hit,
    /// `generate_embeddings` returns the cached vectors and does NOT
    /// touch the ONNX model. The proof-of-no-model-load is implicit
    /// (this test runs in < 100 ms vs > 1 s for an ONNX load + first
    /// embed) but the public-API behavior — correct vectors returned
    /// in symbol order — is what users actually depend on.
    #[test]
    fn all_hit_returns_cached_vectors_without_model_load() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let cache_root = root.join(".vex_cache");
        std::fs::create_dir_all(&cache_root).unwrap();
        std::fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
        crate::util::config::set_cache_override(cache_root, false);

        let parsed = vec![ParsedFile {
            path: "a.rs".to_string(),
            symbols: vec![mk_sym("foo", 1), mk_sym("bar", 10)],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
            hierarchy_captures: Vec::new(),
        }];

        // Pre-seed the cache with synthetic vectors keyed by the same
        // hash function `generate_embeddings` will use. Using
        // distinguishable per-symbol vectors so we can prove the
        // results came back in the right output order.
        let ctx_foo = embed::build_context(
            "function",
            "foo",
            "a.rs",
            Some("fn foo()"),
            None,
            None,
            MINILM_CHAR_BUDGET,
        );
        let ctx_bar = embed::build_context(
            "function",
            "bar",
            "a.rs",
            Some("fn bar()"),
            None,
            None,
            MINILM_CHAR_BUDGET,
        );
        let h_foo = context_hash(MINILM_ID, &ctx_foo);
        let h_bar = context_hash(MINILM_ID, &ctx_bar);
        let mut v_foo = vec![0.0_f32; MINILM_DIM as usize];
        v_foo[0] = 1.0;
        let mut v_bar = vec![0.0_f32; MINILM_DIM as usize];
        v_bar[1] = 1.0;

        let mut cache = embed::cache::EmbedCache::empty(MINILM_ID, MINILM_DIM);
        cache.insert(h_foo, v_foo.clone());
        cache.insert(h_bar, v_bar.clone());
        let cache_path = crate::util::config::embed_cache_path(root, MINILM_ID);
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        cache.save(&cache_path).unwrap();

        // The call must NOT load ONNX (no network, no model file in
        // tmp). If the all-hit early-return ever regresses, the test
        // either hangs on download or panics in `make_embedder`.
        let (out, hashes) =
            generate_embeddings(&parsed, MINILM_ID, root, crate::embed::Device::Cpu, false)
                .expect("generate_embeddings");

        assert_eq!(out.len(), 2);
        assert_eq!(out[0], v_foo, "foo position 0");
        assert_eq!(out[1], v_bar, "bar position 1");
        // v1.14.1 B1.1: the returned hashes must be sym_idx-ordered
        // and identical to the cache keys — that's the contract
        // `build_hnsw` and the `index.hashes` sidecar rely on.
        assert_eq!(hashes, vec![h_foo, h_bar]);
    }

    /// v1.14.1 B1.1 end-to-end: build a hash-keyed HNSW from a tiny
    /// fixture, open it via `HnswHandle`, query the v_foo vector and
    /// confirm the top result maps back to sym_idx 0. This is the
    /// query path the rest of vex relies on for `vex search
    /// --semantic` / `vex similar` / `vex duplicates` once the index
    /// carries v1.14.1 sidecars. Catches regressions in either the
    /// builder (sidecar / HNSW pair drift) or the handle's
    /// `hash → sym_idx` translation.
    #[test]
    fn build_hnsw_with_hashes_and_query_returns_sym_idx() {
        // Uses `build_hnsw_at` with explicit paths so this test never
        // touches `crate::util::config::set_cache_override` — that's an
        // `OnceLock<CacheLayout>` and under `cargo test` the sibling
        // `all_hit_returns_cached_vectors_without_model_load` test
        // racing for it would leave whichever ran second pointing at a
        // dropped TempDir. The production wrapper `build_hnsw(root, ..)`
        // does the config lookup; `build_hnsw_at` covers the same
        // builder + sidecar path with zero process-wide state.
        let tmp = tempfile::TempDir::new().unwrap();
        let hnsw_path = tmp.path().join("index.hnsw");
        // `HnswHandle::open` derives the sidecar as `hnsw_path.parent()
        // .join("index.hashes")`, so co-locating the two files mirrors
        // the production layout.
        let hash_index_path = tmp.path().join("index.hashes");

        // Two orthogonal MiniLM-shaped vectors at sym_idx 0 and 1 with
        // synthetic but plausible context hashes. Orthogonal so the
        // top-1 result is unambiguous (cosine = 1 for the query vector
        // matching its own stored copy, ~0 for the other).
        let dim = MINILM_DIM as usize;
        let mut v_foo = vec![0.0_f32; dim];
        v_foo[0] = 1.0;
        let mut v_bar = vec![0.0_f32; dim];
        v_bar[1] = 1.0;
        let h_foo: u64 = 0xFEED_C0FF_EEC0_DE42;
        let h_bar: u64 = 0x00BA_0BAB_DEAD_BEEF;

        build_hnsw_at(
            &hnsw_path,
            &hash_index_path,
            &[v_foo.clone(), v_bar.clone()],
            &[h_foo, h_bar],
        )
        .expect("build_hnsw_at");

        // Both files must exist after the build — `HnswHandle::open`
        // bails on a missing sidecar even if the HNSW is fine.
        assert!(hnsw_path.exists(), "HNSW file must exist");
        assert!(hash_index_path.exists(), "hash-index sidecar must exist");

        // Open the handle: expected_symbols = 2 (matches both HNSW size
        // and sidecar length). Should succeed; missing sidecar would
        // return None.
        let handle =
            crate::search::semantic::HnswHandle::open(&hnsw_path, dim, 2).expect("open HnswHandle");

        // Query with v_foo — top-1 must be sym_idx 0 with similarity
        // ~1.0 (the stored vector under hash h_foo).
        let results = handle.search(&v_foo, 1).expect("search succeeds");
        assert_eq!(results.len(), 1);
        let (sym_idx, sim) = results[0];
        assert_eq!(sym_idx, 0, "top-1 must resolve to sym_idx 0 via hash map");
        assert!(sim > 0.99, "self-similarity should be ~1.0, got {sim}");
    }

    // ---- v1.15.0 B1.2 incremental HNSW tests ----

    /// Build a fixture HNSW with two synthetic vectors, then add/remove
    /// via the incremental path and verify the query still works. The
    /// helper returns the temp dir handle so the caller's drop closes
    /// it cleanly at scope exit.
    fn make_seed_hnsw(
        tmp: &tempfile::TempDir,
        hashes: &[u64],
        vectors: &[Vec<f32>],
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let hnsw_path = tmp.path().join("index.hnsw");
        let hash_index_path = tmp.path().join("index.hashes");
        build_hnsw_at(&hnsw_path, &hash_index_path, vectors, hashes).expect("seed build_hnsw_at");
        (hnsw_path, hash_index_path)
    }

    #[test]
    fn incremental_returns_false_when_no_prior_hnsw() {
        // Cold-start path: no prior HNSW means the caller must do a
        // full rebuild. The fallback contract is `Ok(false)`, not Err.
        let tmp = tempfile::TempDir::new().unwrap();
        let hnsw_path = tmp.path().join("index.hnsw");
        let hash_index_path = tmp.path().join("index.hashes");
        let dim = MINILM_DIM as usize;
        let v = vec![vec![1.0_f32; dim]];
        let h = vec![0xAA_u64];
        let applied = build_hnsw_incremental_at(&hnsw_path, &hash_index_path, &v, &h).unwrap();
        assert!(!applied, "no prior HNSW → fall back");
    }

    #[test]
    fn incremental_returns_false_when_no_prior_sidecar() {
        // HNSW present but sidecar gone → can't diff against old
        // hashes → fall back. Mimics a partial-cleanup state on disk.
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = MINILM_DIM as usize;
        let v_seed = vec![vec![1.0_f32; dim]];
        let h_seed = vec![0xAA_u64];
        let (hnsw_path, hash_index_path) = make_seed_hnsw(&tmp, &h_seed, &v_seed);

        std::fs::remove_file(&hash_index_path).unwrap();

        let v_new = vec![vec![1.0_f32; dim]];
        let h_new = vec![0xAA_u64];
        let applied =
            build_hnsw_incremental_at(&hnsw_path, &hash_index_path, &v_new, &h_new).unwrap();
        assert!(!applied, "missing sidecar → fall back");
    }

    #[test]
    fn incremental_returns_false_on_empty_corpus() {
        // The orchestrator separately drops both files when corpus is
        // empty; the incremental path must NOT touch disk in that case.
        // Use a tempdir to ensure the bail happens before any file open.
        let tmp = tempfile::TempDir::new().unwrap();
        let hnsw_path = tmp.path().join("index.hnsw");
        let hash_index_path = tmp.path().join("index.hashes");
        let applied = build_hnsw_incremental_at(&hnsw_path, &hash_index_path, &[], &[]).unwrap();
        assert!(!applied, "empty vectors → caller handles cleanup");
    }

    #[test]
    fn incremental_returns_false_when_tombstone_threshold_exceeded() {
        // 4 old hashes, 3 removed = 75% removal → far past the 25%
        // threshold. The incremental code must bail BEFORE opening the
        // HNSW so the caller's full-rebuild path runs against fresh
        // data instead of a half-mutated index.
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = MINILM_DIM as usize;
        let mut v_seed = Vec::new();
        let mut h_seed = Vec::new();
        for i in 0..4 {
            let mut v = vec![0.0_f32; dim];
            v[i] = 1.0;
            v_seed.push(v);
            h_seed.push(0x100_u64 + i as u64);
        }
        let (hnsw_path, hash_index_path) = make_seed_hnsw(&tmp, &h_seed, &v_seed);

        // New corpus keeps only the first vector + adds two new ones —
        // that's 3 removes vs old=4, well above threshold.
        let mut v_new = vec![v_seed[0].clone()];
        let mut h_new = vec![h_seed[0]];
        for i in 0..2 {
            let mut v = vec![0.0_f32; dim];
            v[10 + i] = 1.0;
            v_new.push(v);
            h_new.push(0x200_u64 + i as u64);
        }
        let applied =
            build_hnsw_incremental_at(&hnsw_path, &hash_index_path, &v_new, &h_new).unwrap();
        assert!(!applied, "75% removal must exceed 25% tombstone threshold");
    }

    #[test]
    fn incremental_applies_small_add_and_remove_and_keeps_search_correct() {
        // Build a 3-symbol HNSW, then remove one and add one (delta of
        // 1 remove + 1 add against old_size 3 = 33% remove → just over
        // the 25% line. Bump old_size to 5 so we're at 1/5 = 20% and
        // the incremental path applies. Then query for the surviving
        // vectors — both pre-existing and new — and verify the
        // sidecar's new sym_idx mapping is correct.
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = MINILM_DIM as usize;
        let mut v_seed = Vec::new();
        let mut h_seed = Vec::new();
        for i in 0..5 {
            let mut v = vec![0.0_f32; dim];
            v[i] = 1.0;
            v_seed.push(v);
            h_seed.push(0x1000_u64 + i as u64);
        }
        let (hnsw_path, hash_index_path) = make_seed_hnsw(&tmp, &h_seed, &v_seed);

        // New corpus: keep entries 0..4, drop entry 4, add a new
        // distinct vector. sym_idx layout (5 entries):
        //   0..=3 = old entries (hashes 0x1000..0x1003)
        //   4 = new entry (hash 0x2000)
        let mut v_new = v_seed[0..4].to_vec();
        let mut h_new = h_seed[0..4].to_vec();
        let mut v_added = vec![0.0_f32; dim];
        v_added[20] = 1.0;
        v_new.push(v_added.clone());
        h_new.push(0x2000_u64);

        let applied =
            build_hnsw_incremental_at(&hnsw_path, &hash_index_path, &v_new, &h_new).unwrap();
        assert!(
            applied,
            "1-remove-1-add at old_size=5 must apply incrementally"
        );

        // Sidecar must reflect the new sym_idx ordering.
        let loaded = crate::search::hash_index::load(&hash_index_path).unwrap();
        assert_eq!(loaded, h_new);

        // Query for the surviving old vector at sym_idx 1: top-1 must
        // be sym_idx 1 (sidecar position of h_seed[1]).
        let handle = crate::search::semantic::HnswHandle::open(&hnsw_path, dim, h_new.len())
            .expect("open handle");
        let results = handle.search(&v_seed[1], 1).expect("search v_seed[1]");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1, "surviving old vector at new sym_idx 1");

        // Query for the freshly-added vector: top-1 must be sym_idx 4.
        let results = handle.search(&v_added, 1).expect("search v_added");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 4, "freshly-added vector at new sym_idx 4");
    }

    #[test]
    fn incremental_at_exact_25_percent_threshold_does_not_fall_back() {
        // Pins the strict-GT semantics of the tombstone check: at old=4
        // / removed=1 (exactly 25%), the incremental path MUST apply.
        // A future refactor that flips `>` to `>=` would silently
        // regress this boundary; this test catches it before commit.
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = MINILM_DIM as usize;
        let mut v_seed = Vec::new();
        let mut h_seed = Vec::new();
        for i in 0..4 {
            let mut v = vec![0.0_f32; dim];
            v[i] = 1.0;
            v_seed.push(v);
            h_seed.push(0x5000_u64 + i as u64);
        }
        let (hnsw_path, hash_index_path) = make_seed_hnsw(&tmp, &h_seed, &v_seed);

        // Drop entry 3, keep 0..=2 — that's 1 remove out of 4 = 25%.
        let v_new = v_seed[0..3].to_vec();
        let h_new = h_seed[0..3].to_vec();
        let applied =
            build_hnsw_incremental_at(&hnsw_path, &hash_index_path, &v_new, &h_new).unwrap();
        assert!(
            applied,
            "exactly 25% removal must NOT trigger fallback (strict-GT)"
        );
    }

    #[test]
    fn two_phase_commit_cleans_up_tmps_on_hnsw_rename_failure() {
        // Pre-create a directory at the HNSW path so `rename(hnsw_tmp,
        // hnsw_path)` fails (can't rename a file over a non-empty
        // dir). Verifies the cleanup branch removes both tmps. The
        // existing `index.hashes.tmp` from a prior aborted run would
        // confuse the next iteration if left behind.
        let tmp = tempfile::TempDir::new().unwrap();
        let hnsw_path = tmp.path().join("index.hnsw");
        let hash_index_path = tmp.path().join("index.hashes");
        // Drop a non-empty dir at hnsw_path to wedge the rename.
        std::fs::create_dir(&hnsw_path).unwrap();
        std::fs::write(hnsw_path.join("blocker"), b"in-the-way").unwrap();

        let dim = MINILM_DIM as usize;
        let v = vec![vec![1.0_f32; dim]];
        let h = vec![0xAA_u64];
        let result = build_hnsw_at(&hnsw_path, &hash_index_path, &v, &h);
        assert!(
            result.is_err(),
            "build_hnsw_at should fail when HNSW destination is wedged"
        );

        // Both tmps must be cleaned up — neither `.hnsw.tmp` nor
        // `.hashes.tmp` should leak.
        let hnsw_tmp = hnsw_path.with_extension("hnsw.tmp");
        let hash_tmp = hash_index_path.with_extension("hashes.tmp");
        assert!(
            !hnsw_tmp.exists(),
            "HNSW tmp leaked after rename failure: {}",
            hnsw_tmp.display()
        );
        assert!(
            !hash_tmp.exists(),
            "sidecar tmp leaked after rename failure: {}",
            hash_tmp.display()
        );
    }

    #[test]
    fn incremental_returns_false_on_corrupt_sidecar() {
        // A crafted bad-magic sidecar must trigger the fallback path,
        // not bubble up an error. The caller's full-rebuild will
        // overwrite the bad sidecar with a fresh one.
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = MINILM_DIM as usize;
        let v_seed = vec![vec![1.0_f32; dim]];
        let h_seed = vec![0xAA_u64];
        let (hnsw_path, hash_index_path) = make_seed_hnsw(&tmp, &h_seed, &v_seed);

        std::fs::write(&hash_index_path, b"GARBAGE").unwrap();

        let v_new = vec![vec![1.0_f32; dim]];
        let h_new = vec![0xAA_u64];
        let applied =
            build_hnsw_incremental_at(&hnsw_path, &hash_index_path, &v_new, &h_new).unwrap();
        assert!(!applied, "corrupt sidecar → graceful fallback");
    }

    // ---------------------------------------------------------------
    // decode_string_at — encoding-contract guard with StringTable
    // ---------------------------------------------------------------

    /// Round-trip: bytes produced by the canonical `StringTable::intern`
    /// must be decodable by our build-side `decode_string_at`. If
    /// `StringTable` ever changes its on-disk encoding (e.g. length
    /// field width), this test breaks at the seam and the helper's
    /// docstring already points the reader at the fix locations
    /// (`HistoryReader::string` + `decode_string_at`). Cheaper than
    /// constructing a full sidecar + opening it via `HistoryReader`.
    #[test]
    fn decode_string_at_round_trips_build_time_strings() {
        use crate::store::git_history::StringTable;

        let mut st = StringTable::new();
        let off_empty = st.intern("");
        let off_a = st.intern("src/foo.rs");
        let off_b = st.intern("fn frobnicate(x: u32) -> u32");
        let off_a_dup = st.intern("src/foo.rs");

        let bytes = st.as_bytes();

        // Sentinel: empty string at offset 0.
        assert_eq!(off_empty, 0);
        assert_eq!(decode_string_at(bytes, 0), "");

        // Round-trips for two distinct interns.
        assert_eq!(decode_string_at(bytes, off_a), "src/foo.rs");
        assert_eq!(
            decode_string_at(bytes, off_b),
            "fn frobnicate(x: u32) -> u32"
        );

        // Dedup: re-interning returns the same offset.
        assert_eq!(off_a, off_a_dup);

        // Defensive: out-of-range offset yields "" instead of panicking.
        assert_eq!(decode_string_at(bytes, u32::MAX), "");
        assert_eq!(decode_string_at(bytes, bytes.len() as u32), "");
    }
}
