use std::sync::LazyLock;

use tree_sitter::Query;

use super::language::Language;

static RUST_QUERY: LazyLock<Query> = LazyLock::new(|| {
    let src = include_str!("../../queries/rust.scm");
    Query::new(&tree_sitter_rust::LANGUAGE.into(), src).expect("failed to compile rust query")
});

static PYTHON_QUERY: LazyLock<Query> = LazyLock::new(|| {
    let src = include_str!("../../queries/python.scm");
    Query::new(&tree_sitter_python::LANGUAGE.into(), src).expect("failed to compile python query")
});

static GO_QUERY: LazyLock<Query> = LazyLock::new(|| {
    let src = include_str!("../../queries/go.scm");
    Query::new(&tree_sitter_go::LANGUAGE.into(), src).expect("failed to compile go query")
});

// TODO: add Kotlin and TypeScript queries once grammars stabilize

/// Get the compiled tree-sitter query for a language.
pub fn get_query(lang: Language) -> Option<&'static Query> {
    match lang {
        Language::Rust => Some(&RUST_QUERY),
        Language::Python => Some(&PYTHON_QUERY),
        Language::Go => Some(&GO_QUERY),
        Language::Kotlin | Language::TypeScript => None, // TODO
    }
}
