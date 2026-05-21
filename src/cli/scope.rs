//! Per-query path scope filters: `--include <glob>` / `--exclude <glob>`.
//!
//! Compiled once per command invocation from the repeated CLI args and
//! applied at the handler boundary, after the search engine has produced
//! its result list. Keeping the filter outside of the search loop means
//! no index format / search-channel changes are required — every command
//! pays the same flat `O(n_results)` glob match.
//!
//! Semantics:
//!   * `--include` is a *whitelist*. With no include flag the scope
//!     admits everything; with one or more, a path must match at least
//!     one include glob.
//!   * `--exclude` is a *blacklist* and wins over include — a path that
//!     matches any exclude glob is rejected even if it also matches an
//!     include glob.
//!   * Globs use `globset`'s default syntax (`*`, `**`, `?`, character
//!     classes). The leading `!`-negation form is intentionally **not**
//!     supported — that's what `--exclude` is for, and shells eat `!`
//!     anyway.

use anyhow::{bail, Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Defence against CPU exhaustion when scope args arrive from untrusted
/// MCP clients: `globset::GlobSetBuilder::build()` is O(total pattern
/// length), so a flood of long patterns can spike CPU at command start.
/// Caps are well above any realistic interactive workload.
const MAX_PATTERNS: usize = 64;
const MAX_PATTERN_LEN: usize = 256;

#[derive(Debug, Default)]
pub struct PathScope {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl PathScope {
    /// Compile globs from the raw repeated CLI args.
    ///
    /// Returns an error with the offending pattern as context if any glob
    /// fails to parse — surfaces as `error: invalid --include glob: …` at
    /// the CLI boundary, which is the most actionable place for it.
    pub fn from_args(include: &[String], exclude: &[String]) -> Result<Self> {
        Ok(Self {
            include: build_set(include, "include")?,
            exclude: build_set(exclude, "exclude")?,
        })
    }

    /// Returns true when the path passes both the include whitelist (if
    /// any) and the exclude blacklist (if any).
    pub fn accept(&self, path: &str) -> bool {
        if let Some(ref ex) = self.exclude {
            if ex.is_match(path) {
                return false;
            }
        }
        if let Some(ref inc) = self.include {
            if !inc.is_match(path) {
                return false;
            }
        }
        true
    }

    /// True when neither `--include` nor `--exclude` was provided —
    /// callers can short-circuit fetch-limit inflation in that case.
    pub fn is_empty(&self) -> bool {
        self.include.is_none() && self.exclude.is_none()
    }

    /// Accept-decision for a *pair* of paths (used by `vex duplicates`).
    /// Matches the established semantics of `--filter`: a pair is kept
    /// when at least one side matches the include set, and dropped when
    /// at least one side matches the exclude set.
    pub fn accept_pair(&self, a: &str, b: &str) -> bool {
        if let Some(ref ex) = self.exclude {
            if ex.is_match(a) || ex.is_match(b) {
                return false;
            }
        }
        if let Some(ref inc) = self.include {
            if !inc.is_match(a) && !inc.is_match(b) {
                return false;
            }
        }
        true
    }
}

fn build_set(patterns: &[String], label: &str) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    if patterns.len() > MAX_PATTERNS {
        bail!(
            "too many --{label} globs ({} > max {MAX_PATTERNS})",
            patterns.len()
        );
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        if p.len() > MAX_PATTERN_LEN {
            bail!(
                "--{label} glob exceeds {MAX_PATTERN_LEN} chars ({} chars): {p}",
                p.len()
            );
        }
        // `literal_separator(true)` matches gitignore semantics — `*` matches
        // within one path segment, `**` is required to cross `/`. Users
        // copy-paste patterns from `.gitignore` / `.dockerignore` files, so
        // this is the least-surprising default.
        let glob = GlobBuilder::new(p)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid --{label} glob: {p}"))?;
        b.add(glob);
    }
    Ok(Some(
        b.build()
            .with_context(|| format!("build --{label} glob set"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(include: &[&str], exclude: &[&str]) -> PathScope {
        let inc: Vec<String> = include.iter().map(|s| (*s).into()).collect();
        let exc: Vec<String> = exclude.iter().map(|s| (*s).into()).collect();
        PathScope::from_args(&inc, &exc).unwrap()
    }

    #[test]
    fn empty_scope_accepts_everything() {
        let s = PathScope::default();
        assert!(s.is_empty());
        assert!(s.accept("src/main.rs"));
        assert!(s.accept("tests/foo.rs"));
        assert!(s.accept(""));
    }

    #[test]
    fn include_only_whitelists() {
        let s = scope(&["tests/**"], &[]);
        assert!(s.accept("tests/cli_scope_test.rs"));
        assert!(s.accept("tests/sub/dir/x.rs"));
        assert!(!s.accept("src/main.rs"));
    }

    #[test]
    fn exclude_only_blacklists() {
        let s = scope(&[], &["**/generated/**"]);
        assert!(s.accept("src/api.rs"));
        assert!(!s.accept("src/generated/proto.rs"));
        assert!(!s.accept("deep/nested/generated/x.rs"));
    }

    #[test]
    fn exclude_wins_over_include() {
        let s = scope(&["src/**"], &["**/*.gen.rs"]);
        assert!(s.accept("src/api.rs"));
        assert!(!s.accept("src/api.gen.rs"));
        assert!(!s.accept("tests/lib.rs")); // also fails include
    }

    #[test]
    fn multiple_includes_are_union() {
        let s = scope(&["src/**", "crates/**"], &[]);
        assert!(s.accept("src/main.rs"));
        assert!(s.accept("crates/vex-mcp/src/lib.rs"));
        assert!(!s.accept("docs/README.md"));
    }

    #[test]
    fn invalid_glob_surfaces_pattern_in_error() {
        let err = PathScope::from_args(&["src/[".into()], &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--include"), "missing arg label: {msg}");
        assert!(msg.contains("src/["), "missing offending pattern: {msg}");
    }

    #[test]
    fn exclude_star_does_not_cross_segment() {
        // Mirror of `star_does_not_cross_segment` on the exclude side —
        // locks in the literal_separator(true) invariant for both sets.
        let s = scope(&[], &["src/*.gen.rs"]);
        assert!(!s.accept("src/api.gen.rs"));
        assert!(s.accept("src/foo/bar.gen.rs"));
    }

    #[test]
    fn pattern_count_cap_rejects_flood() {
        let many: Vec<String> = (0..200).map(|i| format!("p{i}/**")).collect();
        let err = PathScope::from_args(&many, &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("too many"),
            "expected count-cap error, got: {msg}"
        );
    }

    #[test]
    fn pattern_length_cap_rejects_huge_glob() {
        let huge = "a".repeat(1024);
        let err = PathScope::from_args(&[huge], &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exceeds"),
            "expected length-cap error, got: {msg}"
        );
    }

    #[test]
    fn pair_dropped_when_either_side_excluded() {
        let s = scope(&[], &["generated/**"]);
        assert!(s.accept_pair("src/a.rs", "src/b.rs"));
        assert!(!s.accept_pair("src/a.rs", "generated/b.rs"));
        assert!(!s.accept_pair("generated/a.rs", "src/b.rs"));
    }

    #[test]
    fn pair_kept_when_either_side_included() {
        let s = scope(&["src/api/**"], &[]);
        assert!(s.accept_pair("src/api/a.rs", "tests/b.rs"));
        assert!(s.accept_pair("tests/a.rs", "src/api/b.rs"));
        assert!(!s.accept_pair("tests/a.rs", "tests/b.rs"));
    }

    #[test]
    fn star_does_not_cross_segment() {
        let s = scope(&["src/*.rs"], &[]);
        assert!(s.accept("src/main.rs"));
        // globset's default single-`*` is one segment — `src/foo/bar.rs`
        // requires `**`. Locks in the semantics so future regressions are
        // visible.
        assert!(!s.accept("src/foo/bar.rs"));
    }
}
