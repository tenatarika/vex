//! `vex pattern` — AST-pattern matching across files. Extracted from
//! `cli/mod.rs` in S1 Group D.

use std::time::Instant;

use anyhow::{Context, Result};

use super::args::{DiffFilterArgs, OutputFormat, ScopeArgs};
use super::common::{diff_filter_meta, resolve_diff_filter, resolve_root, CmdCtx};
use super::output::print_envelope;
use super::scope;
use crate::protocol::{capabilities, MetaEnvelope};

#[allow(clippy::too_many_arguments)]
pub(crate) fn pattern(
    ctx: &CmdCtx<'_>,
    pattern: String,
    lang: String,
    path: Option<std::path::PathBuf>,
    limit: usize,
    why: bool,
    scope: ScopeArgs,
    diff: DiffFilterArgs,
) -> Result<()> {
    let path_scope = scope::PathScope::from_args(&scope.include, &scope.exclude)?;
    let root = resolve_root(path)?;
    // Resolve diff filter against the project root. Pattern uses a
    // non-canonicalized root; that's fine for git, which accepts any
    // dir inside the work tree.
    let changed_paths = resolve_diff_filter(&root, &diff)?;
    let language = crate::parse::language::Language::from_extension(&lang)
        .or(match lang.as_str() {
            "rust" => Some(crate::parse::language::Language::Rust),
            "python" => Some(crate::parse::language::Language::Python),
            "go" => Some(crate::parse::language::Language::Go),
            "java" => Some(crate::parse::language::Language::Java),
            "csharp" | "cs" => Some(crate::parse::language::Language::CSharp),
            "ruby" | "rb" => Some(crate::parse::language::Language::Ruby),
            "swift" => Some(crate::parse::language::Language::Swift),
            "kotlin" | "kt" => Some(crate::parse::language::Language::Kotlin),
            "typescript" | "ts" | "tsx" => Some(crate::parse::language::Language::TypeScript),
            "sql" => Some(crate::parse::language::Language::Sql),
            "markdown" | "md" => Some(crate::parse::language::Language::Markdown),
            "cpp" | "c++" | "cxx" | "c" => Some(crate::parse::language::Language::Cpp),
            "php" | "phtml" => Some(crate::parse::language::Language::Php),
            "bash" | "sh" | "shell" => Some(crate::parse::language::Language::Bash),
            "lua" => Some(crate::parse::language::Language::Lua),
            "css" => Some(crate::parse::language::Language::Css),
            "html" | "htm" => Some(crate::parse::language::Language::Html),
            "yaml" | "yml" => Some(crate::parse::language::Language::Yaml),
            "toml" => Some(crate::parse::language::Language::Toml),
            _ => None,
        })
        .with_context(|| format!("unknown language: {lang}"))?;

    let start = Instant::now();
    // Over-fetch when scope filters are active so post-filter truncation
    // does not silently drop matches the user expects to see. Diff
    // filter is treated identically — see Search handler note.
    let fetch_limit = if path_scope.is_empty() && changed_paths.is_none() {
        limit
    } else {
        usize::MAX
    };

    let (raw_matches, trace) =
        crate::pattern::scan_with_mode(&root, &pattern, language, fetch_limit, ctx.excludes)?;

    // Apply scope first, then diff filter. Track counts for the
    // `--why` diff_filter trace.
    let pre_diff: Vec<_> = raw_matches
        .into_iter()
        .filter(|m| path_scope.accept(&m.path))
        .collect();
    let pre_diff_count = pre_diff.len();
    let post_diff: Vec<_> = if let Some(ref cp) = changed_paths {
        pre_diff
            .into_iter()
            .filter(|m| cp.contains(&m.path))
            .collect()
    } else {
        pre_diff
    };
    let diff_retained = post_diff.len();
    let diff_dropped = pre_diff_count.saturating_sub(diff_retained);
    let matches: Vec<_> = post_diff.into_iter().take(limit).collect();
    let elapsed = start.elapsed();

    match ctx.format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = matches
                .iter()
                .map(|m| {
                    let mut obj = serde_json::json!({
                        "path": m.path,
                        "line": m.line,
                        "text": m.matched_text.lines().next().unwrap_or(""),
                    });
                    if !m.captures.is_empty() {
                        obj["captures"] = serde_json::json!(m
                            .captures
                            .iter()
                            .map(|(k, v)| serde_json::json!({k: v}))
                            .collect::<Vec<_>>());
                    }
                    obj
                })
                .collect();
            let meta = MetaEnvelope {
                diff_filter: diff_filter_meta(
                    &diff,
                    changed_paths.as_ref(),
                    diff_retained,
                    diff_dropped,
                ),
                ..MetaEnvelope::default()
            };
            print_envelope(&json, capabilities::current(), meta);
        }
        OutputFormat::Text | OutputFormat::Compact => {
            if matches.is_empty() {
                println!("No matches for pattern in {elapsed:.2?}");
            } else {
                println!("{} matches in {elapsed:.2?}\n", matches.len());
                for m in &matches {
                    let first_line = m.matched_text.lines().next().unwrap_or("");
                    println!("{}:{}", m.path, m.line);
                    println!("  {first_line}");
                    for (name, value) in &m.captures {
                        println!("  ${name} = {value}");
                    }
                    println!();
                }
            }
        }
    }

    if why {
        // stderr keeps stdout a pure result stream — mirrors
        // `vex search --why` so `vex pattern 'pat' --why | jq` works.
        crate::cli::trace::emit_why_trace(&trace)?;
        if let Some(df) =
            diff_filter_meta(&diff, changed_paths.as_ref(), diff_retained, diff_dropped)
        {
            crate::cli::trace::emit_diff_filter(&df)?;
        }
    }

    Ok(())
}
