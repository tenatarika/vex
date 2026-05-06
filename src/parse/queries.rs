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

static JAVA_QUERY: LazyLock<Query> = LazyLock::new(|| {
    let src = include_str!("../../queries/java.scm");
    Query::new(&tree_sitter_java::LANGUAGE.into(), src).expect("failed to compile java query")
});

static CSHARP_QUERY: LazyLock<Query> = LazyLock::new(|| {
    let src = include_str!("../../queries/csharp.scm");
    Query::new(&tree_sitter_c_sharp::LANGUAGE.into(), src).expect("failed to compile csharp query")
});

static RUBY_QUERY: LazyLock<Query> = LazyLock::new(|| {
    let src = include_str!("../../queries/ruby.scm");
    Query::new(&tree_sitter_ruby::LANGUAGE.into(), src).expect("failed to compile ruby query")
});

static SWIFT_QUERY: LazyLock<Query> = LazyLock::new(|| {
    let src = include_str!("../../queries/swift.scm");
    Query::new(&tree_sitter_swift::LANGUAGE.into(), src).expect("failed to compile swift query")
});

/// Get the compiled tree-sitter query for a language.
pub fn get_query(lang: Language) -> Option<&'static Query> {
    match lang {
        Language::Rust => Some(&RUST_QUERY),
        Language::Python => Some(&PYTHON_QUERY),
        Language::Go => Some(&GO_QUERY),
        Language::Java => Some(&JAVA_QUERY),
        Language::CSharp => Some(&CSHARP_QUERY),
        Language::Ruby => Some(&RUBY_QUERY),
        Language::Swift => Some(&SWIFT_QUERY),
        Language::Kotlin | Language::TypeScript => None, // TODO: grammar API differences
    }
}
