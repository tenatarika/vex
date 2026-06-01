//! `vex outline <file>` — print the symbol structure of a single file.
//! Extracted from `cli/mod.rs` in S1 Group B (lift already-named cmd_* fns).

use anyhow::{bail, Context, Result};

use super::args::OutputFormat;
use super::output::print_envelope;
use crate::protocol::{capabilities, MetaEnvelope};

pub(crate) fn cmd_outline(
    file: &std::path::Path,
    kind: Option<&str>,
    format: &OutputFormat,
) -> Result<()> {
    use crate::index::symbols::SymbolKind;

    let kind_filter = kind.map(|k| k.parse::<SymbolKind>()).transpose()?;

    let content =
        std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;

    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .context("file has no extension")?;

    let lang = crate::parse::language::Language::from_extension(ext)
        .with_context(|| format!("unsupported language: .{ext}"))?;

    if let Err(e) = crate::parse::queries::try_get_query(lang) {
        bail!("failed to load grammar for {} (.{ext}): {e}", lang.as_str());
    }

    let rel = file.to_string_lossy().to_string();
    let parsed = crate::parse::parse_file(&rel, &content, lang)?;

    let symbols: Vec<_> = parsed
        .symbols
        .iter()
        // Phase 14.1: synthetic `<module:path>` symbols are invisible to
        // outline regardless of `--kind` filter.
        .filter(|s| s.kind != crate::index::symbols::SymbolKind::Module)
        .filter(|s| kind_filter.is_none_or(|k| s.kind == k))
        .collect();

    print_outline(&symbols, file, kind_filter, format);
    Ok(())
}

fn print_outline(
    symbols: &[&crate::index::symbols::ParsedSymbol],
    file: &std::path::Path,
    kind_filter: Option<crate::index::symbols::SymbolKind>,
    format: &OutputFormat,
) {
    match &format {
        OutputFormat::Json => {
            let json: Vec<serde_json::Value> = symbols
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "kind": s.kind.as_str(),
                        "line": s.line,
                        "signature": s.signature,
                    })
                })
                .collect();
            // outline parses a single file directly without consulting the
            // index, so there's no project root and no manifest to derive
            // index_age from — default meta is the honest answer.
            print_envelope(&json, capabilities::current(), MetaEnvelope::default());
        }
        OutputFormat::Text | OutputFormat::Compact => {
            if symbols.is_empty() {
                if let Some(k) = kind_filter {
                    println!("No {k} symbols found in {}", file.display());
                } else {
                    println!("No symbols found in {}", file.display());
                }
            } else {
                println!("{}", file.display());
                for s in symbols {
                    println!("  {:<12} {:<40} line {}", s.kind.as_str(), s.name, s.line);
                    if let Some(sig) = &s.signature {
                        println!("               {sig}");
                    }
                }
            }
        }
    }
}
