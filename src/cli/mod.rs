pub mod args;
pub mod output;

use std::time::Instant;

use anyhow::{bail, Context, Result};
use args::{Cli, Commands, OutputFormat};

use crate::embed::Embedder;
use crate::index::pipeline;
use crate::search::{fusion, semantic, structural};
use crate::store::reader::IndexReader;
use crate::util::config;

fn resolve_root(path: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match path {
        Some(p) => Ok(p),
        None => std::env::current_dir().context("get working directory"),
    }
}

fn filter_by_path(
    results: Vec<crate::search::SearchResult>,
    filter: Option<&str>,
) -> Vec<crate::search::SearchResult> {
    match filter {
        Some(fp) => results
            .into_iter()
            .filter(|r| r.path.contains(fp))
            .collect(),
        None => results,
    }
}

pub fn dispatch(cli: Cli) -> Result<()> {
    let format = &cli.format;

    match cli.command {
        Commands::Index { path, semantic } => {
            let root = resolve_root(path)?;
            let start = Instant::now();
            let count = pipeline::run(&root, semantic)?;
            let elapsed = start.elapsed();
            let index_path = config::index_path(&root.canonicalize()?);

            match format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "symbols": count,
                        "elapsed_ms": elapsed.as_millis(),
                        "embeddings": semantic,
                        "index": index_path.to_string_lossy(),
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    println!("Indexed {count} symbols in {elapsed:.2?}");
                    if semantic {
                        println!("Embeddings: enabled");
                    }
                    println!("Index: {}", index_path.display());
                }
            }
            Ok(())
        }
        Commands::Search {
            query,
            limit,
            semantic,
            filter_path,
        } => {
            let root = resolve_root(None)?.canonicalize()?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                bail!(
                    "No index found. Run `vex index` first.\nExpected: {}",
                    index_path.display()
                );
            }

            let reader = IndexReader::open(&index_path).context("open index")?;

            // Fetch more results when filtering, then truncate after filter
            let fetch_limit = if filter_path.is_some() {
                reader.symbol_count()
            } else {
                limit
            };

            let structural_results = structural::search(&reader, &query, fetch_limit);

            let results = if semantic && reader.has_vectors() {
                let mut embedder = Embedder::new().context("load embedding model")?;
                let hnsw_path = config::hnsw_path(&root);
                let semantic_results = semantic::search_with_embedder(
                    &reader,
                    &mut embedder,
                    &query,
                    fetch_limit,
                    &hnsw_path,
                )?;
                fusion::fuse(structural_results, semantic_results, limit)
            } else {
                if semantic && !reader.has_vectors() {
                    eprintln!("Warning: no embeddings in index. Run `vex index --semantic` first.");
                }
                structural_results
            };

            let results: Vec<_> = filter_by_path(results, filter_path.as_deref())
                .into_iter()
                .take(limit)
                .collect();

            if results.is_empty() {
                match format {
                    OutputFormat::Json => println!("[]"),
                    OutputFormat::Text | OutputFormat::Compact => {
                        println!("No results for \"{query}\"")
                    }
                }
            } else {
                output::print_results(&results, format);
            }
            Ok(())
        }
        Commands::Usages {
            name,
            limit,
            filter_path,
        } => {
            let root = resolve_root(None)?.canonicalize()?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                bail!(
                    "No index found. Run `vex index` first.\nExpected: {}",
                    index_path.display()
                );
            }

            let reader = IndexReader::open(&index_path).context("open index")?;
            let ref_reader = reader
                .ref_reader()
                .context("no refs in index — re-run `vex index` to rebuild")?;
            let file_paths = reader.file_paths();

            let entries = ref_reader.find(&name);
            let entries: Vec<_> = entries
                .into_iter()
                .filter(|e| {
                    if let Some(ref fp) = filter_path {
                        file_paths
                            .get(e.file_id as usize)
                            .is_some_and(|p| p.contains(fp.as_str()))
                    } else {
                        true
                    }
                })
                .collect();
            let total = entries.len();
            let entries: Vec<_> = entries.into_iter().take(limit).collect();

            match format {
                OutputFormat::Json => {
                    let json: Vec<serde_json::Value> = entries
                        .iter()
                        .map(|e| {
                            let path = file_paths
                                .get(e.file_id as usize)
                                .map(|s| s.as_str())
                                .unwrap_or("?");
                            serde_json::json!({
                                "path": path,
                                "line": e.line,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    if entries.is_empty() {
                        println!("No usages found for \"{name}\"");

                        let prefix_results = ref_reader.find_by_prefix(&name);
                        if !prefix_results.is_empty() {
                            println!("\nDid you mean:");
                            for (n, refs) in prefix_results.iter().take(5) {
                                println!("  {n} ({} usages)", refs.len());
                            }
                        }
                    } else {
                        println!("{name}: {total} usages (showing {})", entries.len());
                        for e in &entries {
                            let path = file_paths
                                .get(e.file_id as usize)
                                .map(|s| s.as_str())
                                .unwrap_or("?");
                            println!("  {path}:{}", e.line);
                        }
                    }
                }
            }
            Ok(())
        }
        Commands::Pattern {
            pattern,
            lang,
            path,
            limit,
        } => {
            let root = resolve_root(path)?;
            let language = crate::parse::language::Language::from_extension(&lang)
                .or(match lang.as_str() {
                    "rust" => Some(crate::parse::language::Language::Rust),
                    "python" => Some(crate::parse::language::Language::Python),
                    "go" => Some(crate::parse::language::Language::Go),
                    "java" => Some(crate::parse::language::Language::Java),
                    "csharp" | "cs" => Some(crate::parse::language::Language::CSharp),
                    "ruby" | "rb" => Some(crate::parse::language::Language::Ruby),
                    "swift" => Some(crate::parse::language::Language::Swift),
                    _ => None,
                })
                .with_context(|| format!("unknown language: {lang}"))?;

            let start = Instant::now();
            let matches = crate::pattern::scan(&root, &pattern, language, limit)?;
            let elapsed = start.elapsed();

            match format {
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
                    println!("{}", serde_json::to_string_pretty(&json)?);
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
            Ok(())
        }
        Commands::Update { path, semantic } => {
            let root = resolve_root(path)?;
            let start = Instant::now();
            let (total, changed, deleted) = pipeline::update(&root, semantic)?;
            let elapsed = start.elapsed();

            match format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "symbols": total,
                        "changed": changed,
                        "deleted": deleted,
                        "elapsed_ms": elapsed.as_millis(),
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    if changed == 0 && deleted == 0 {
                        println!("Index up to date ({total} symbols)");
                    } else {
                        println!("Updated in {elapsed:.2?}: {changed} changed, {deleted} deleted, {total} total symbols");
                    }
                }
            }
            Ok(())
        }
        Commands::Outline { file, kind } => cmd_outline(&file, kind.as_deref(), format),
        Commands::Watch { path, semantic } => {
            let root = resolve_root(path)?;
            crate::watch::handler::watch(&root, semantic)?;
            Ok(())
        }
        Commands::Show {
            symbols,
            limit,
            context,
            filter_path,
        } => {
            let root = resolve_root(None)?.canonicalize()?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                bail!(
                    "No index found. Run `vex index` first.\nExpected: {}",
                    index_path.display()
                );
            }

            let reader = IndexReader::open(&index_path).context("open index")?;
            let fetch_limit = if filter_path.is_some() {
                reader.symbol_count()
            } else {
                limit
            };
            let mut json_items: Vec<serde_json::Value> = Vec::new();
            let mut printed = 0usize;

            for symbol in &symbols {
                let results = structural::search(&reader, symbol, fetch_limit);
                let results: Vec<_> = filter_by_path(results, filter_path.as_deref())
                    .into_iter()
                    .take(limit)
                    .collect();

                if results.is_empty() {
                    match format {
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

                    let body =
                        if let Some(lang) = crate::parse::language::Language::from_extension(ext) {
                            crate::parse::body::extract_symbol_body_ts(
                                &content,
                                result.line,
                                lang,
                                context,
                            )?
                        } else {
                            crate::parse::body::extract_symbol_body(&content, result.line, context)?
                        };

                    match format {
                        OutputFormat::Json => {
                            json_items.push(serde_json::json!({
                                "name": result.name,
                                "kind": result.kind,
                                "path": result.path,
                                "start_line": body.start_line,
                                "end_line": body.end_line,
                                "lines": body.lines,
                                "body": body.body,
                            }));
                        }
                        OutputFormat::Text => {
                            if printed > 0 {
                                println!();
                            }
                            println!(
                                "── {} ({}) {}:{}-{}",
                                result.name,
                                result.kind,
                                result.path,
                                body.start_line,
                                body.end_line
                            );
                            for (n, line) in body.body.lines().enumerate() {
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
                            println!("{}", body.body);
                            printed += 1;
                        }
                    }
                }
            }

            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&json_items)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    if printed == 0 {
                        println!("No symbols found");
                    }
                }
            }
            Ok(())
        }
        Commands::Status { path } => {
            let root = resolve_root(path)?
                .canonicalize()
                .context("canonicalize root")?;
            let index_path = config::index_path(&root);

            if !index_path.exists() {
                match format {
                    OutputFormat::Json => {
                        println!("{}", serde_json::json!({"error": "no index found"}));
                    }
                    OutputFormat::Text | OutputFormat::Compact => {
                        println!("No index found for {}", root.display());
                        println!("Run `vex index` to build one.");
                    }
                }
                return Ok(());
            }

            let meta = std::fs::metadata(&index_path)?;
            let reader = IndexReader::open(&index_path)?;

            match format {
                OutputFormat::Json => {
                    let json = serde_json::json!({
                        "project": root.to_string_lossy(),
                        "index": index_path.to_string_lossy(),
                        "size_bytes": meta.len(),
                        "symbols": reader.symbol_count(),
                        "embeddings": reader.has_vectors(),
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text | OutputFormat::Compact => {
                    println!("Project:    {}", root.display());
                    println!("Index:      {}", index_path.display());
                    println!("Size:       {:.1} KB", meta.len() as f64 / 1024.0);
                    println!("Symbols:    {}", reader.symbol_count());
                    println!(
                        "Embeddings: {}",
                        if reader.has_vectors() { "yes" } else { "no" }
                    );
                }
            }
            Ok(())
        }
        Commands::Grep {
            pattern,
            limit,
            filter_path,
            path,
        } => {
            let root = resolve_root(path)?;
            let matches = crate::grep::search(&root, &pattern, filter_path.as_deref(), limit)?;

            match format {
                OutputFormat::Json => {
                    let json: Vec<serde_json::Value> = matches
                        .iter()
                        .map(|m| {
                            serde_json::json!({
                                "path": m.path,
                                "line": m.line,
                                "text": m.text,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text => {
                    if matches.is_empty() {
                        println!("No matches for \"{pattern}\"");
                    } else {
                        println!("{} matches\n", matches.len());
                        for m in &matches {
                            println!("{}:{}", m.path, m.line);
                            println!("  {}", m.text);
                        }
                    }
                }
                OutputFormat::Compact => {
                    for m in &matches {
                        println!("{}:{}  {}", m.path, m.line, m.text);
                    }
                }
            }
            Ok(())
        }
        Commands::Implementations { name, path, limit } => {
            let root = resolve_root(path)?;
            let start = Instant::now();
            let matches = crate::hierarchy::find_implementations(&root, &name, limit)?;
            let elapsed = start.elapsed();

            match format {
                OutputFormat::Json => {
                    let json: Vec<serde_json::Value> = matches
                        .iter()
                        .map(|m| {
                            serde_json::json!({
                                "name": m.name,
                                "base": m.base,
                                "relation": m.relation,
                                "path": m.path,
                                "line": m.line,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
                OutputFormat::Text => {
                    if matches.is_empty() {
                        println!("No implementations of \"{name}\" in {elapsed:.2?}");
                    } else {
                        println!(
                            "{name}: {} implementations in {elapsed:.2?}\n",
                            matches.len()
                        );
                        for m in &matches {
                            println!("  {:<40} ({})  {}:{}", m.name, m.relation, m.path, m.line);
                        }
                    }
                }
                OutputFormat::Compact => {
                    for m in &matches {
                        println!("{} {} {} {}:{}", m.relation, m.base, m.name, m.path, m.line);
                    }
                }
            }
            Ok(())
        }
    }
}

fn cmd_outline(file: &std::path::Path, kind: Option<&str>, format: &OutputFormat) -> Result<()> {
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

    if crate::parse::queries::get_query(lang).is_none() {
        bail!(
            "language .{ext} is recognized but has no tree-sitter query yet (Kotlin, TypeScript pending)"
        );
    }

    let rel = file.to_string_lossy().to_string();
    let parsed = crate::parse::parse_file(&rel, &content, lang)?;

    let symbols: Vec<_> = parsed
        .symbols
        .iter()
        .filter(|s| kind_filter.map_or(true, |k| s.kind == k))
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
    match format {
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
            // unwrap: serializing simple JSON values cannot fail
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
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
