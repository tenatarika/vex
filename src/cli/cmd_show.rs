//! `vex show <SYMBOL>+` — extract symbol bodies, with optional
//! truncation modes. Extracted from `cli/mod.rs` in S1 Group D.2.

use anyhow::{Context, Result};

use super::args::{MetadataArgs, OutputFormat, ScopeArgs};
use super::common::{apply_path_filters, build_metadata_filter, resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use super::{scope, show_truncate};
use crate::protocol::capabilities;
use crate::search::rerank::RerankContext;
use crate::search::{structural, SearchResult};
use crate::store::reader::IndexReader;

/// The definitions `vex show` prints for one `symbol`: rank a candidate pool
/// with [`structural::search_ranked`] (order-independent), filter, then cut
/// to `limit`. Truncating before the rerank made the default `--limit 1`
/// depend on filesystem walk order.
fn select_definitions(
    reader: &IndexReader,
    symbol: &str,
    limit: usize,
    rerank_ctx: &RerankContext<'_>,
    filter_path: Option<&str>,
    path_scope: &scope::PathScope,
    metadata_filter: &crate::search::metadata::MetadataFilter,
) -> Vec<SearchResult> {
    let pool = if filter_path.is_some() || !path_scope.is_empty() {
        reader.symbol_count()
    } else {
        limit.max(structural::RERANK_POOL)
    };
    let ranked = structural::search_ranked(reader, symbol, pool, rerank_ctx);
    apply_path_filters(ranked, filter_path, path_scope)
        .into_iter()
        .filter(|r| metadata_filter.matches(r.signature.as_deref()))
        .take(limit)
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn show(
    ctx: &CmdCtx<'_>,
    symbols: Vec<String>,
    limit: usize,
    context: usize,
    filter_path: Option<String>,
    kind: Vec<String>,
    context_path: Option<String>,
    auto_update: bool,
    no_stale_check: bool,
    signature_only: bool,
    head: Option<usize>,
    no_body: bool,
    collapsed: bool,
    meta: MetadataArgs,
    scope: ScopeArgs,
) -> Result<()> {
    // Phase 13.3 — resolve the truncation mode once. Clap's
    // `conflicts_with_all` already guarantees at most one flag
    // is set; this just maps the booleans into an `Option`.
    let truncation_mode: Option<show_truncate::TruncationMode> = if signature_only {
        Some(show_truncate::TruncationMode::SignatureOnly)
    } else if head.is_some() {
        Some(show_truncate::TruncationMode::Head)
    } else if no_body {
        Some(show_truncate::TruncationMode::NoBody)
    } else if collapsed {
        Some(show_truncate::TruncationMode::Collapsed)
    } else {
        None
    };
    if collapsed {
        // Single emission via stderr — tracing isn't always
        // initialized (e.g. under the CLI integration tests),
        // and emitting twice would risk drift if a test asserts
        // on exact-string output. The integration test pins the
        // `pending` substring on stderr, so this stays
        // observable for both human and automated callers.
        eprintln!("warning: --collapsed pending language-aware implementation; emitting full body");
    }
    let path_scope = scope::PathScope::from_scope_args(&scope)?;
    let metadata_filter = build_metadata_filter(&meta)?;
    let root = resolve_root(None)?.canonicalize()?;
    let index_path = ensure_index_ready(
        &root,
        auto_update,
        no_stale_check,
        false,
        ctx.local_cache_active,
        ctx.cfg,
    )?;

    let reader = IndexReader::open(&index_path).context("open index")?;
    let mut json_items: Vec<serde_json::Value> = Vec::new();
    // `printed` counts text/compact blocks (a "No symbol found" line
    // included) and only drives blank-line separators; `found` counts real
    // definitions in any format and alone decides the exit code.
    let mut printed = 0usize;
    let mut found = 0usize;

    let rerank_ctx = RerankContext {
        kind_hints: crate::search::rerank::KindSelector::parse_many(&kind)?,
        context_path: context_path.as_deref(),
    };

    for symbol in &symbols {
        let results = select_definitions(
            &reader,
            symbol,
            limit,
            &rerank_ctx,
            filter_path.as_deref(),
            &path_scope,
            &metadata_filter,
        );

        if results.is_empty() {
            match ctx.format {
                OutputFormat::Json => {}
                OutputFormat::Text | OutputFormat::Compact => {
                    if printed > 0 {
                        println!();
                    }
                    println!("No symbol found: \"{symbol}\"");
                    printed += 1;
                }
            }
            continue;
        }

        found += results.len();
        for result in &results {
            let content = std::fs::read_to_string(&result.path)
                .with_context(|| format!("read {}", result.path))?;

            let ext = std::path::Path::new(&result.path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");

            let body = if result.kind == "heading" {
                crate::parse::body::extract_heading_body(&content, result.line, context)?
            } else if let Some(lang) = crate::parse::language::Language::from_extension(ext) {
                crate::parse::body::extract_symbol_body_ts(&content, result.line, lang, context)?
            } else {
                crate::parse::body::extract_symbol_body(&content, result.line, context)?
            };

            // Phase 13.3 — apply optional truncation to the
            // extracted body. The struct returned by the
            // helpers carries metadata that we surface in the
            // JSON envelope per result; text/compact output
            // stays clean (just the truncated body).
            let truncation = truncation_mode.map(|mode| match mode {
                show_truncate::TruncationMode::SignatureOnly => {
                    show_truncate::signature_only(&body.body)
                }
                show_truncate::TruncationMode::Head => {
                    // `head` Option already validated as Some
                    // when mode is Head.
                    let n = head.unwrap_or(usize::MAX);
                    show_truncate::head_n(&body.body, n)
                }
                show_truncate::TruncationMode::NoBody => show_truncate::no_body(&body.body),
                show_truncate::TruncationMode::Collapsed => show_truncate::collapsed(&body.body),
            });
            let display_body: &str = truncation
                .as_ref()
                .map(|t| t.body.as_str())
                .unwrap_or(body.body.as_str());

            match ctx.format {
                OutputFormat::Json => {
                    let mut item = serde_json::json!({
                        "name": result.name,
                        "kind": result.kind,
                        "path": result.path,
                        "start_line": body.start_line,
                        "end_line": body.end_line,
                        "lines": body.lines,
                        "body": display_body,
                    });
                    if let Some(t) = &truncation {
                        item["truncation"] = serde_json::json!({
                            "mode": t.mode.as_str(),
                            "original_lines": t.original_lines,
                            "kept_lines": t.kept_lines,
                        });
                    }
                    json_items.push(item);
                }
                OutputFormat::Text => {
                    if printed > 0 {
                        println!();
                    }
                    println!(
                        "── {} ({}) {}:{}-{}",
                        result.name, result.kind, result.path, body.start_line, body.end_line
                    );
                    for (n, line) in display_body.lines().enumerate() {
                        println!("{:>4} | {}", body.start_line + n, line);
                    }
                    printed += 1;
                }
                OutputFormat::Compact => {
                    if printed > 0 {
                        println!();
                    }
                    println!(
                        "# {}:{}-{} ({})",
                        result.path, body.start_line, body.end_line, result.kind
                    );
                    println!("{}", display_body);
                    printed += 1;
                }
            }
        }
    }

    match ctx.format {
        OutputFormat::Json => {
            print_envelope(
                &json_items,
                capabilities::current(),
                super::output::default_meta_for(&root),
            );
        }
        OutputFormat::Text | OutputFormat::Compact => {
            if printed == 0 {
                println!("No symbols found");
            }
        }
    }
    // v1.12.0 S8.2 — exit 1 when no symbol resolved, in every format. This
    // used to test `printed`, which the per-symbol "No symbol found" line
    // bumps, so a miss exited 0 in text/compact but 1 in JSON.
    if found == 0 {
        crate::cli::exit_code::signal_no_results();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! `show` must pick the same definitions whatever order the files were
    //! discovered in. Index order is `readdir` order, which differs between
    //! NTFS (sorted) and APFS/ext4 (hash order); these tests write the index
    //! in both orders explicitly so the guard fails on every OS.
    use super::*;
    use crate::index::symbols::{ParsedFile, ParsedSymbol, SymbolKind};
    use crate::store::writer::write_index;

    fn file_defining(path: &str, name: &str) -> ParsedFile {
        ParsedFile {
            path: path.to_string(),
            symbols: vec![ParsedSymbol {
                name: name.to_string(),
                kind: SymbolKind::Function,
                line: 1,
                signature: Some(format!("pub fn {name}() {{}}")),
                doc: None,
                body_tokens: None,
            }],
            refs: vec![],
            call_edges: vec![],
            bound_refs: vec![],
            skeletons: Vec::new(),
            cpp_includes: Vec::new(),
            trigram_bloom: None,
            hierarchy_captures: Vec::new(),
        }
    }

    /// Paths `select_definitions` returns for `shared_helper_fn` over an
    /// index whose files were written in `order`.
    fn selected(order: &[&str], limit: usize, scope: &scope::PathScope) -> Vec<String> {
        let tmp = tempfile::TempDir::new().unwrap();
        let out = tmp.path().join("index.vex");
        let parsed: Vec<ParsedFile> = order
            .iter()
            .map(|p| file_defining(p, "shared_helper_fn"))
            .collect();
        write_index(&parsed, &out).unwrap();
        let reader = IndexReader::open(&out).unwrap();
        let ctx = RerankContext {
            kind_hints: Vec::new(),
            context_path: None,
        };
        let filter = crate::search::metadata::MetadataFilter::default();
        select_definitions(
            &reader,
            "shared_helper_fn",
            limit,
            &ctx,
            None,
            scope,
            &filter,
        )
        .into_iter()
        .map(|r| r.path)
        .collect()
    }

    const TEST_FIRST: [&str; 3] = ["tests/integration.rs", "src/b.rs", "src/a.rs"];
    const PROD_FIRST: [&str; 3] = ["src/a.rs", "src/b.rs", "tests/integration.rs"];

    #[test]
    fn default_limit_is_independent_of_index_order() {
        let none = scope::PathScope::default();
        let a = selected(&TEST_FIRST, 1, &none);
        let b = selected(&PROD_FIRST, 1, &none);
        // The test file is demoted and the two equal-score production
        // definitions tie-break by path, so `src/a.rs` wins in both orders.
        assert_eq!(a, vec!["src/a.rs".to_string()]);
        assert_eq!(a, b);
    }

    #[test]
    fn full_listing_is_identical_across_index_orders() {
        let none = scope::PathScope::default();
        let a = selected(&TEST_FIRST, 10, &none);
        assert_eq!(a, ["src/a.rs", "src/b.rs", "tests/integration.rs"]);
        assert_eq!(a, selected(&PROD_FIRST, 10, &none));
    }

    #[test]
    fn exclude_tests_scope_drops_the_test_definition() {
        let scoped = scope::PathScope::default().with_exclude_tests(true);
        assert_eq!(selected(&TEST_FIRST, 10, &scoped), ["src/a.rs", "src/b.rs"],);
    }
}
