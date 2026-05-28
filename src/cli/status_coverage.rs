//! `vex status --coverage` diagnostic (FU-5, v1.10.1).
//!
//! Walks the project with the same `walk_builder` the indexer uses,
//! cross-references the discovered set against the index's `file_paths()`,
//! and reports three buckets:
//!
//!   * `indexed` — files present in the file table, broken down by
//!     detected language.
//!   * `discovered_not_indexed` — files the walker emitted but the
//!     pipeline filtered out before parsing. The reason is one of
//!     `unsupported_extension` (no `Language::from_extension` match) or
//!     `too_large` (> 1 MiB — matches the indexer's size cap).
//!   * `missing_from_disk` — paths in the index that no longer exist
//!     on disk (auto_update would clean them on next run).
//!
//! Designed to answer "what's on disk but unindexed?" — useful when a
//! new file type appears or `auto_update` quietly skips files.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::parse::language::Language;
use crate::store::reader::IndexReader;

/// Mirrors `pipeline::discover_files`'s 1 MiB cap so the same files
/// surface here as `too_large` that the indexer would skip silently.
const MAX_INDEXABLE_SIZE: u64 = 1_048_576;

/// Number of sample paths emitted per non-empty bucket. Caps the JSON
/// envelope size on pathological repos while still giving the user
/// something concrete to act on.
const SAMPLE_CAP: usize = 25;

/// `pub(crate)`: the serialised field names are the wire contract
/// (locked by `tests/cli_status_coverage_test.rs`); the Rust struct is
/// not part of the public API.
#[derive(Debug, Serialize)]
pub(crate) struct CoverageReport {
    pub indexed_files: usize,
    pub by_language: BTreeMap<String, usize>,
    pub discovered_not_indexed: SampledBucket,
    pub missing_from_disk: SampledBucket,
}

#[derive(Debug, Serialize)]
pub(crate) struct SampledBucket {
    pub count: usize,
    pub samples: Vec<SampleEntry>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SampleEntry {
    pub path: String,
    /// Only present for `discovered_not_indexed` entries; `None` for
    /// `missing_from_disk` (the reason is implicit: file deleted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// Collect coverage data for `vex status --coverage`.
pub(crate) fn collect(
    root: &Path,
    reader: &IndexReader,
    excludes: &[String],
) -> Result<CoverageReport> {
    let mut by_language: BTreeMap<String, usize> = BTreeMap::new();
    let indexed_paths: Vec<String> = reader.file_paths();

    for rel in &indexed_paths {
        if let Some(lang) = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension)
        {
            *by_language.entry(lang.as_str().to_string()).or_insert(0) += 1;
        } else {
            // Indexed file with no detectable language — defensive
            // bucket so the user sees the count instead of it
            // silently disappearing from the per-language breakdown.
            *by_language.entry("unknown".to_string()).or_insert(0) += 1;
        }
    }

    // `HashSet` for O(1) membership in the walker loop. Constructed
    // once over the same `Vec<String>` we iterate elsewhere — review
    // FU-5 noted the previous BTreeSet + HashSet pair allocated the
    // path set twice.
    let indexed_set: HashSet<&str> = indexed_paths.iter().map(String::as_str).collect();

    let mut discovered_not_indexed = SampledBucket {
        count: 0,
        samples: Vec::new(),
    };
    let walker = crate::util::walk::walk_builder(root, excludes)
        .context("build walker for coverage report")?
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            // A walker error (permission denied, transient I/O) shouldn't
            // sink the whole report — coverage is a best-effort diagnostic.
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let abs = entry.into_path();
        let rel = match abs.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if indexed_set.contains(rel.as_str()) {
            continue;
        }

        let ext = abs.extension().and_then(|e| e.to_str());
        let reason: &'static str = match ext.and_then(Language::from_extension) {
            None => "unsupported_extension",
            Some(_) => {
                let size = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
                if size > MAX_INDEXABLE_SIZE {
                    "too_large"
                } else {
                    // The walker emitted it AND it has a known language
                    // AND it's under the size cap — must be a brand-new
                    // file `auto_update` hasn't picked up yet (or
                    // `--no-index` was passed at last index time).
                    "not_yet_indexed"
                }
            }
        };

        discovered_not_indexed.count += 1;
        if discovered_not_indexed.samples.len() < SAMPLE_CAP {
            discovered_not_indexed.samples.push(SampleEntry {
                path: rel,
                reason: Some(reason),
            });
        }
    }

    let mut missing_from_disk = SampledBucket {
        count: 0,
        samples: Vec::new(),
    };
    for rel in &indexed_paths {
        let candidate: PathBuf = root.join(rel);
        if candidate.exists() {
            continue;
        }
        missing_from_disk.count += 1;
        if missing_from_disk.samples.len() < SAMPLE_CAP {
            missing_from_disk.samples.push(SampleEntry {
                path: rel.clone(),
                reason: None,
            });
        }
    }

    Ok(CoverageReport {
        indexed_files: indexed_paths.len(),
        by_language,
        discovered_not_indexed,
        missing_from_disk,
    })
}

/// Render a human-readable summary on stdout. Caller is responsible
/// for the leading blank line separation if needed.
pub(crate) fn render_text(c: &CoverageReport) {
    println!();
    println!("Coverage:");
    println!("  Indexed files:     {}", c.indexed_files);
    if !c.by_language.is_empty() {
        println!("  By language:");
        for (lang, count) in &c.by_language {
            println!("    {lang:<14} {count}");
        }
    }
    println!(
        "  Discovered, not indexed: {}",
        c.discovered_not_indexed.count
    );
    for s in &c.discovered_not_indexed.samples {
        let reason = s.reason.unwrap_or("");
        println!("    [{reason}] {}", s.path);
    }
    println!("  Missing from disk:       {}", c.missing_from_disk.count);
    for s in &c.missing_from_disk.samples {
        println!("    {}", s.path);
    }
}
