use std::sync::LazyLock;

use tree_sitter::Query;

use super::language::Language;

/// Compile a grammar query, returning a stringified error on failure.
///
/// The query is compiled once and cached for the lifetime of the process. We
/// store `Result` (not the raw `Query`) so that an ABI/grammar mismatch
/// surfaces as a normal error instead of poisoning the `LazyLock` and
/// silently breaking every subsequent file of that language. See the C# ABI 15
/// regression: tree-sitter-c-sharp 0.23.5 shipped a grammar newer than what
/// the core could load, and the old `.expect()` left worker threads racing on
/// a poisoned cell with no diagnostic the user could see.
fn compile(
    lang_name: &'static str,
    language: tree_sitter::Language,
    src: &str,
) -> Result<Query, String> {
    Query::new(&language, src).map_err(|e| format!("{lang_name} query: {e}"))
}

static RUST_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    compile(
        "rust",
        tree_sitter_rust::LANGUAGE.into(),
        include_str!("../../queries/rust.scm"),
    )
});

static PYTHON_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    compile(
        "python",
        tree_sitter_python::LANGUAGE.into(),
        include_str!("../../queries/python.scm"),
    )
});

static GO_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    compile(
        "go",
        tree_sitter_go::LANGUAGE.into(),
        include_str!("../../queries/go.scm"),
    )
});

static JAVA_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    compile(
        "java",
        tree_sitter_java::LANGUAGE.into(),
        include_str!("../../queries/java.scm"),
    )
});

static CSHARP_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    compile(
        "csharp",
        tree_sitter_c_sharp::LANGUAGE.into(),
        include_str!("../../queries/csharp.scm"),
    )
});

static RUBY_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    compile(
        "ruby",
        tree_sitter_ruby::LANGUAGE.into(),
        include_str!("../../queries/ruby.scm"),
    )
});

static SWIFT_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    compile(
        "swift",
        tree_sitter_swift::LANGUAGE.into(),
        include_str!("../../queries/swift.scm"),
    )
});

static KOTLIN_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    compile(
        "kotlin",
        tree_sitter_kotlin_ng::LANGUAGE.into(),
        include_str!("../../queries/kotlin.scm"),
    )
});

static TYPESCRIPT_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    // TSX grammar is a superset of TypeScript — handles both .ts and .tsx
    compile(
        "typescript",
        tree_sitter_typescript::LANGUAGE_TSX.into(),
        include_str!("../../queries/typescript.scm"),
    )
});

static SQL_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    compile(
        "sql",
        tree_sitter_sequel::LANGUAGE.into(),
        include_str!("../../queries/sql.scm"),
    )
});

static MARKDOWN_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    compile(
        "markdown",
        tree_sitter_md::LANGUAGE.into(),
        include_str!("../../queries/markdown.scm"),
    )
});

static CPP_QUERY: LazyLock<Result<Query, String>> = LazyLock::new(|| {
    compile(
        "cpp",
        tree_sitter_cpp::LANGUAGE.into(),
        include_str!("../../queries/cpp.scm"),
    )
});

fn lookup(lang: Language) -> &'static Result<Query, String> {
    match lang {
        Language::Rust => &RUST_QUERY,
        Language::Python => &PYTHON_QUERY,
        Language::Go => &GO_QUERY,
        Language::Java => &JAVA_QUERY,
        Language::CSharp => &CSHARP_QUERY,
        Language::Ruby => &RUBY_QUERY,
        Language::Swift => &SWIFT_QUERY,
        Language::Kotlin => &KOTLIN_QUERY,
        Language::TypeScript => &TYPESCRIPT_QUERY,
        Language::Sql => &SQL_QUERY,
        Language::Markdown => &MARKDOWN_QUERY,
        Language::Cpp => &CPP_QUERY,
    }
}

/// Get the compiled tree-sitter query for a language.
///
/// Returns `Ok(&Query)` if the grammar loaded and the query compiled.
/// Returns `Err(message)` if the grammar failed to load — typically an ABI
/// mismatch between the `tree-sitter` core crate and the grammar crate, or
/// a query that references AST nodes that no longer exist in the grammar.
pub fn try_get_query(lang: Language) -> Result<&'static Query, &'static str> {
    match lookup(lang) {
        Ok(q) => Ok(q),
        Err(e) => Err(e.as_str()),
    }
}
