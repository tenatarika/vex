//! `vex show <SYMBOL>+` — extract symbol bodies, with optional
//! truncation modes. Extracted from `cli/mod.rs` in S1 Group D.2.

use anyhow::{Context, Result};

use super::args::{MetadataArgs, OutputFormat, ScopeArgs};
use super::common::{apply_path_filters, build_metadata_filter, resolve_root, CmdCtx};
use super::index_management::ensure_index_ready;
use super::output::print_envelope;
use super::{scope, show_truncate};
use crate::protocol::capabilities;
use crate::search::structural;
use crate::store::reader::IndexReader;

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
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
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
    let fetch_limit = if filter_path.is_some() || !path_scope.is_empty() {
        reader.symbol_count()
    } else {
        limit
    };
    let mut json_items: Vec<serde_json::Value> = Vec::new();
    let mut printed = 0usize;

    let rerank_ctx = crate::search::rerank::RerankContext {
        kind_hints: crate::search::rerank::KindSelector::parse_many(&kind)?,
        context_path: context_path.as_deref(),
    };

    for symbol in &symbols {
        let results = structural::search_with_fuzzy(&reader, symbol, fetch_limit);
        let results = crate::search::rerank::rerank(symbol, &rerank_ctx, results);
        let results: Vec<_> = apply_path_filters(results, filter_path.as_deref(), &path_scope)
            .into_iter()
            .filter(|r| metadata_filter.matches(r.signature.as_deref()))
            .take(limit)
            .collect();

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
    Ok(())
}
