use std::path::Path;

use crate::parse::language::Language;
use anyhow::{Context, Result};
use rayon::prelude::*;

pub mod bfs;
pub mod indegree;
pub mod stdlib_filter;
pub mod test_patterns;

mod extractor;
mod queries;

// `extract_call_edges` is the self-parsing public entry point (index time uses
// `extract_call_edges_with_tree`); `callers_in_source_with_facts` / `callees_in_source_with_facts` are
// internal call-site helpers used by `find_callers` / `find_callees` below. `callgraph_query` is reused here as the
// language-filter predicate (we only walk files the engine has a
// query for) — `extractor.rs` imports it independently for query
// compilation, so the two `use`s are not duplicates.
// `#[allow]`: the `vex` binary target compiles these modules directly rather
// than linking the library, so a re-export whose only in-crate callers are
// tests reads as unused there. `parse_file` goes through
// `extract_call_edges_with_tree`; this stays as the public API and as the
// reference implementation the shared-tree equivalence test diffs against.
#[allow(unused_imports)]
pub use extractor::extract_call_edges;
// Shared-tree core behind `extract_call_edges`, called by `parse_file` with the
// tree it already parsed. `pub(crate)`: no external consumer, and exporting it
// would put `tree_sitter::Tree` in the public API.
pub(crate) use extractor::extract_call_edges_with_tree;
pub(crate) use extractor::extract_call_edges_with_tree_raw;
use extractor::{callees_in_source_with_facts, callers_in_source_with_facts};
use queries::callgraph_query;

fn dart_project_facts(
    files: &[(std::path::PathBuf, Language)],
) -> Result<crate::parse::extractor::dart::ProjectFacts> {
    let mut facts = crate::parse::extractor::dart::ProjectFacts::default();
    for (path, lang) in files {
        if *lang != Language::Dart {
            continue;
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("read Dart source {}", path.display()))?;
        facts.merge(crate::parse::extractor::dart::ProjectFacts::from_source(
            &content,
        )?);
    }
    Ok(facts)
}

/// Per-step cap when binding `find_callers_fast` as the BFS
/// `callers_of` closure. Far above any realistic fan-in but bounded
/// for safety; saturation should surface a stderr warning so an
/// incomplete walk is visible. Shared across `vex paths`,
/// `vex reachable`, and `vex bundle --mode pr-impact`.
pub const CALLERS_FETCH_CAP: usize = 1024;

/// A caller→callee relationship found in source code.
#[derive(Debug, Clone)]
pub struct CallMatch {
    /// Function that contains the call (caller) or is being called (callee)
    pub name: String,
    pub path: String,
    pub line: usize,
}

/// Find all functions that call `target_name`.
pub fn find_callers(
    root: &Path,
    target_name: &str,
    limit: usize,
    excludes: &[String],
) -> Result<Vec<CallMatch>> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files: Vec<_> = crate::util::walk::discover_source_files(&root, excludes)?
        .into_iter()
        .filter(|(_, lang)| callgraph_query(*lang).is_some())
        .collect();
    let dart_facts = dart_project_facts(&files)?;

    let matches: Vec<CallMatch> = files
        .par_iter()
        .flat_map(|(path, lang)| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            callers_in_source_with_facts(
                &content,
                *lang,
                &rel,
                target_name,
                (*lang == Language::Dart).then_some(&dart_facts),
            )
        })
        .collect();

    Ok(matches.into_iter().take(limit).collect())
}

/// Find all functions called by `target_name`.
pub fn find_callees(
    root: &Path,
    target_name: &str,
    limit: usize,
    excludes: &[String],
) -> Result<Vec<CallMatch>> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files: Vec<_> = crate::util::walk::discover_source_files(&root, excludes)?
        .into_iter()
        .filter(|(_, lang)| callgraph_query(*lang).is_some())
        .collect();
    let dart_facts = dart_project_facts(&files)?;

    let matches: Vec<CallMatch> = files
        .par_iter()
        .flat_map(|(path, lang)| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            callees_in_source_with_facts(
                &content,
                *lang,
                &rel,
                target_name,
                (*lang == Language::Dart).then_some(&dart_facts),
            )
        })
        .collect();

    Ok(matches.into_iter().take(limit).collect())
}

#[cfg(test)]
mod tests;
