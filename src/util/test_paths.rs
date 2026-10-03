//! Canonical "is this file a test file?" predicate.
//!
//! One definition shared by `vex tests-for` (default `--test-pattern` set),
//! `--exclude-tests` (scope filter), the `--kind test` rerank selector and
//! the `bundle` PR-impact classifier. Path-only: Rust unit tests inside a
//! `#[cfg(test)] mod tests` block of a non-test file cannot be detected here.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};

/// Default test-path patterns, language-agnostic. A custom
/// `vex tests-for --test-pattern` list REPLACES this set entirely.
pub const DEFAULT_TEST_PATTERNS: &[&str] = &[
    "**/tests/**",
    "**/__tests__/**",
    "**/test/**",
    "**/spec/**",
    "**/tests.rs",
    "**/*_test.rs",
    "**/*_tests.rs",
    "**/*_test.go",
    "**/*_test.py",
    "**/test_*.py",
    "**/*.test.ts",
    "**/*.test.tsx",
    "**/*.test.js",
    "**/*.test.jsx",
    "**/*.spec.ts",
    "**/*.spec.tsx",
    "**/*.spec.js",
    "**/*.spec.jsx",
    "**/*_spec.rb",
    "**/*Test.java",
    "**/*Tests.java",
    "**/*Test.kt",
    "**/*Tests.kt",
    "**/*Tests.cs",
    "**/*Tests.swift",
    "**/*.Tests/**",
    "**/test_*.cc",
    "**/test_*.cpp",
    "**/*_test.cc",
    "**/*_test.cpp",
    "**/conftest.py",
];

/// Build a [`GlobSet`] from overrides, or from [`DEFAULT_TEST_PATTERNS`]
/// when `overrides` is empty (override REPLACES the defaults).
///
/// The built-in defaults use `literal_separator(true)` so `*` stays inside one
/// path segment (`**/test_*.py` must not match `src/test_data/model.py`).
/// User-supplied `--test-pattern` globs keep the historical `Glob::new`
/// semantics (`*` crosses `/`) so existing overrides behave unchanged.
pub fn build_test_globset(overrides: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    if overrides.is_empty() {
        for p in DEFAULT_TEST_PATTERNS {
            let g = GlobBuilder::new(p)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid default test pattern: {p}"))?;
            b.add(g);
        }
    } else {
        for p in overrides {
            b.add(Glob::new(p).with_context(|| format!("invalid --test-pattern: {p}"))?);
        }
    }
    b.build().context("building test globset")
}

fn default_set() -> &'static GlobSet {
    static SET: OnceLock<GlobSet> = OnceLock::new();
    SET.get_or_init(|| build_test_globset(&[]).expect("static patterns"))
}

/// `true` when `path` (an index-relative path, `/` or `\` separated; not
/// absolute) is a test file
/// by [`DEFAULT_TEST_PATTERNS`]. Allocates only for Windows-style paths.
pub fn is_test_path(path: &str) -> bool {
    if path.contains('\\') {
        return default_set().is_match(path.replace('\\', "/"));
    }
    default_set().is_match(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_test_files_across_languages() {
        for p in [
            "tests/foo.rs",
            "crates/x/tests/it.rs",
            "src/foo_test.rs",
            "src/cli/tests.rs",
            "tests.rs",
            "src/store/writer_tests.rs",
            "pkg/foo_test.go",
            "pkg/test_mod.py",
            "pkg/mod_test.py",
            "pkg/conftest.py",
            "web/src/__tests__/a.ts",
            "web/src/util.test.ts",
            "web/src/util.spec.tsx",
            "src/test/java/FooTest.java",
            "app/FooTests.kt",
            "app/spec/models/user_spec.rb",
            "Sources/FooTests.swift",
            "Foo.Tests/Bar.cs",
            "src/test/kotlin/x.kt",
        ] {
            assert!(is_test_path(p), "expected test path: {p}");
        }
    }

    #[test]
    fn rejects_production_files() {
        for p in [
            "src/main.rs",
            "src/cli/args.rs",
            "src/latest.py",
            "src/contest.py",
            "src/attestation.rs",
            "src/testing_utils.rs",
            "web/src/util.ts",
            "README.md",
            "src/test_helpers.rs",
            "src/test_data/model.py",
            "test_utils/helpers.py",
        ] {
            assert!(!is_test_path(p), "expected non-test path: {p}");
        }
    }

    #[test]
    fn windows_separators_are_normalised() {
        assert!(is_test_path("crates\\x\\tests\\it.rs"));
        assert!(!is_test_path("crates\\x\\src\\lib.rs"));
    }

    #[test]
    fn override_replaces_defaults() {
        let gs = build_test_globset(&["**/only_this/**".to_string()]).unwrap();
        assert!(gs.is_match("a/only_this/b.rs"));
        assert!(!gs.is_match("tests/it.rs"));
    }
}
