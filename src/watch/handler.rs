use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebouncedEvent};

use crate::index::pipeline;
use crate::parse::language::Language;

const DEBOUNCE_MS: u64 = 500;

/// Watch a project directory for changes and trigger incremental re-indexing.
/// Blocks until SIGINT (Ctrl+C).
pub fn watch(root: &Path, with_embeddings: bool, excludes: &[String]) -> Result<()> {
    let root = root.canonicalize().context("canonicalize root")?;

    println!("Building initial index...");
    let count = pipeline::run(&root, with_embeddings, excludes)?;
    println!(
        "Watching {} ({count} symbols). Press Ctrl+C to stop.",
        root.display()
    );

    let (tx, rx) = mpsc::channel();

    let mut debouncer = new_debouncer(
        Duration::from_millis(DEBOUNCE_MS),
        None,
        move |result: std::result::Result<Vec<DebouncedEvent>, Vec<notify::Error>>| match result {
            Ok(events) => {
                let _ = tx.send(events);
            }
            Err(errors) => {
                for e in errors {
                    eprintln!("Watch error: {e}");
                }
            }
        },
    )
    .context("create file watcher")?;

    debouncer
        .watch(&root, RecursiveMode::Recursive)
        .context("start watching")?;

    while let Ok(events) = rx.recv() {
        let relevant = events.iter().any(|e| {
            e.event.paths.iter().any(|p| {
                p.extension()
                    .and_then(|ext| ext.to_str())
                    .and_then(Language::from_extension)
                    .is_some()
            })
        });

        if relevant {
            let start = std::time::Instant::now();
            match pipeline::update(&root, with_embeddings, excludes) {
                Ok((total, changed, deleted)) => {
                    if changed > 0 || deleted > 0 {
                        println!(
                            "[{:.1?}] Updated: {changed} changed, {deleted} deleted, {total} total",
                            start.elapsed()
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Update error: {e:#}");
                }
            }
        }
    }

    Ok(())
}
