use std::path::Path;

use anyhow::{Context, Result};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

/// Build a configured file walker with exclude patterns applied.
pub fn walk_builder(root: &Path, excludes: &[String]) -> Result<WalkBuilder> {
    let mut builder = WalkBuilder::new(root);
    builder.hidden(true).max_depth(Some(50));

    if !excludes.is_empty() {
        let mut ov = OverrideBuilder::new(root);
        for pattern in excludes {
            anyhow::ensure!(
                !pattern.starts_with('!'),
                "exclude pattern {pattern:?} must not start with '!' — use the pattern without prefix"
            );
            ov.add(&format!("!{pattern}"))
                .with_context(|| format!("invalid exclude pattern: {pattern}"))?;
        }
        builder.overrides(ov.build()?);
    }

    Ok(builder)
}
