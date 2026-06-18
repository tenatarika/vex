//! Test-path globs + framework labelling for `vex tests-for`.
//!
//! Pure, table-driven, unit-testable. The CLI layer builds the
//! [`GlobSet`] once and reuses it for the post-filter; framework
//! labelling is a `&str -> &'static str` lookup per surviving row.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

/// Default test-path patterns, language-agnostic. `tests-for` uses
/// these when `--test-pattern` is not supplied. A custom override
/// REPLACES this set entirely (standard CLI override semantics).
pub(crate) const DEFAULT_TEST_PATTERNS: &[&str] = &[
    "**/tests/**",
    "**/__tests__/**",
    "**/test/**",
    "**/*_test.rs",
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
    "**/*Test.java",
    "**/*Tests.java",
    "**/*Test.kt",
    "**/*Tests.kt",
    "**/*Tests.cs",
    "**/*.Tests/**",
    "**/test_*.cc",
    "**/test_*.cpp",
    "**/*_test.cc",
    "**/*_test.cpp",
    "**/conftest.py",
];

/// Build a [`GlobSet`] from `--test-pattern` overrides, or from the
/// default set when overrides is empty. The override list REPLACES
/// defaults (it does not append).
pub(crate) fn build_test_globset(overrides: &[String]) -> Result<GlobSet> {
    let patterns: Vec<&str> = if overrides.is_empty() {
        DEFAULT_TEST_PATTERNS.to_vec()
    } else {
        overrides.iter().map(String::as_str).collect()
    };
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).with_context(|| format!("invalid --test-pattern: {p}"))?);
    }
    b.build().context("building test globset")
}

/// Infer a framework label from a relative path. The label is a
/// short, agent-friendly token used to pick a test runner. Ordering
/// matters — more specific patterns come first.
pub(crate) fn framework_for_path(path: &str) -> &'static str {
    // Rust: `tests/` directory takes precedence over `_test.rs` suffix
    // (cargo integration-test convention vs in-module unit-test
    // convention). Don't reorder these two arms without updating the
    // unit tests below.
    if path.ends_with(".rs") {
        if path.contains("/tests/") || path.starts_with("tests/") {
            return "rust-integration";
        }
        if path.ends_with("_test.rs") {
            return "rust-cargo";
        }
    }
    if path.ends_with("_test.go") {
        return "go-test";
    }
    if path.ends_with(".py") {
        // File-name check avoids matching paths like `latest.py`.
        let fname = path.rsplit('/').next().unwrap_or(path);
        if fname.starts_with("test_") || fname.ends_with("_test.py") || fname == "conftest.py" {
            return "pytest";
        }
        if path.contains("/tests/") || path.contains("/test/") {
            return "pytest";
        }
    }
    if (path.ends_with(".ts")
        || path.ends_with(".tsx")
        || path.ends_with(".js")
        || path.ends_with(".jsx"))
        && (path.contains(".test.") || path.contains(".spec.") || path.contains("__tests__/"))
    {
        return "jest";
    }
    if path.ends_with(".java") && (path.ends_with("Test.java") || path.ends_with("Tests.java")) {
        return "junit";
    }
    if path.ends_with(".kt") && (path.ends_with("Test.kt") || path.ends_with("Tests.kt")) {
        return "kotest";
    }
    if path.ends_with(".cs") && (path.ends_with("Tests.cs") || path.contains(".Tests/")) {
        return "xunit";
    }
    if (path.ends_with(".cc") || path.ends_with(".cpp"))
        && (path.contains("/test_") || path.ends_with("_test.cc") || path.ends_with("_test.cpp"))
    {
        return "gtest";
    }
    "unknown"
}

/// Signal-B name filter: keep symbols whose name looks like a test
/// function (`test_*`, `*_test`, `Test*`, `*Test`, `*Tests`). When
/// `--include-fixtures` is on this filter is bypassed entirely.
pub(crate) fn looks_like_test_name(name: &str) -> bool {
    name.starts_with("test_")
        || name.ends_with("_test")
        || name.starts_with("Test")
        || name.ends_with("Test")
        || name.ends_with("Tests")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_dir_matches_default_set() {
        let gs = build_test_globset(&[]).unwrap();
        assert!(gs.is_match("tests/integration.rs"));
        assert!(gs.is_match("crates/foo/tests/it.rs"));
    }

    #[test]
    fn pytest_test_prefix_matches() {
        let gs = build_test_globset(&[]).unwrap();
        assert!(gs.is_match("tests/test_mod.py"));
        assert!(gs.is_match("pkg/test_helpers.py"));
    }

    #[test]
    fn dotted_test_ts_matches() {
        let gs = build_test_globset(&[]).unwrap();
        assert!(gs.is_match("src/util.test.ts"));
        assert!(gs.is_match("packages/foo/util.spec.tsx"));
    }

    #[test]
    fn framework_pytest_for_test_py() {
        assert_eq!(framework_for_path("tests/test_mod.py"), "pytest");
        assert_eq!(framework_for_path("pkg/mod_test.py"), "pytest");
        assert_eq!(framework_for_path("conftest.py"), "pytest");
    }

    #[test]
    fn framework_rust_integration_in_tests_dir() {
        assert_eq!(
            framework_for_path("tests/integration.rs"),
            "rust-integration"
        );
        assert_eq!(
            framework_for_path("crates/foo/tests/it.rs"),
            "rust-integration"
        );
    }

    #[test]
    fn framework_unknown_for_random_path() {
        assert_eq!(framework_for_path("src/main.rs"), "unknown");
        assert_eq!(framework_for_path("README.md"), "unknown");
    }

    #[test]
    fn framework_rust_cargo_for_test_rs_outside_tests_dir() {
        // `_test.rs` outside `tests/` is the unit-test convention; `tests/`
        // takes precedence (asserted by the integration test above).
        assert_eq!(framework_for_path("src/foo_test.rs"), "rust-cargo");
        assert_eq!(
            framework_for_path("crates/bar/src/baz_test.rs"),
            "rust-cargo"
        );
    }

    #[test]
    fn framework_go_test_label() {
        assert_eq!(framework_for_path("pkg/foo_test.go"), "go-test");
        assert_eq!(framework_for_path("internal/util_test.go"), "go-test");
        assert_eq!(framework_for_path("pkg/foo.go"), "unknown");
    }

    #[test]
    fn framework_junit_for_java() {
        assert_eq!(framework_for_path("src/test/java/FooTest.java"), "junit");
        assert_eq!(framework_for_path("src/test/java/FooTests.java"), "junit");
        assert_eq!(framework_for_path("src/main/java/Foo.java"), "unknown");
    }

    #[test]
    fn framework_kotest_for_kotlin() {
        assert_eq!(framework_for_path("src/test/kotlin/FooTest.kt"), "kotest");
        assert_eq!(framework_for_path("src/test/kotlin/FooTests.kt"), "kotest");
        assert_eq!(framework_for_path("src/main/kotlin/Foo.kt"), "unknown");
    }

    #[test]
    fn framework_xunit_for_csharp() {
        assert_eq!(framework_for_path("MyProject.Tests/FooTests.cs"), "xunit");
        assert_eq!(framework_for_path("Foo.Tests/Bar.cs"), "xunit");
        assert_eq!(framework_for_path("MyProject/Foo.cs"), "unknown");
    }

    #[test]
    fn framework_gtest_for_cpp() {
        assert_eq!(framework_for_path("tests/test_foo.cc"), "gtest");
        assert_eq!(framework_for_path("test/test_foo.cpp"), "gtest");
        assert_eq!(framework_for_path("src/foo_test.cc"), "gtest");
        assert_eq!(framework_for_path("src/foo_test.cpp"), "gtest");
        assert_eq!(framework_for_path("src/foo.cc"), "unknown");
    }

    #[test]
    fn custom_override_replaces_defaults() {
        let gs = build_test_globset(&["**/spec/**".to_string()]).unwrap();
        assert!(gs.is_match("spec/foo_spec.rs"));
        assert!(!gs.is_match("tests/integration.rs"));
    }

    #[test]
    fn looks_like_test_name_signal_b() {
        assert!(looks_like_test_name("test_foo"));
        assert!(looks_like_test_name("foo_test"));
        assert!(looks_like_test_name("TestFoo"));
        assert!(looks_like_test_name("FooTest"));
        assert!(looks_like_test_name("FooTests"));
        assert!(!looks_like_test_name("fixture_helper"));
        assert!(!looks_like_test_name("helper"));
    }
}
