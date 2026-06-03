use super::language::Language;

mod body;
mod refs;
mod symbols;

pub use symbols::extract_symbols_and_imports;
// `extract_references_ast` is the in-crate consumer (called from
// `parse::mod`). `extract_references` is the plain-text fallback —
// no current caller, but referenced by name in `parse::language`'s
// `extract_references_ast` doc, so we keep the historical path
// `crate::parse::extractor::extract_references` resolvable.
#[allow(unused_imports)]
pub use refs::extract_references;
pub use refs::extract_references_ast;

/// Sentinel error type for grammar / query compilation failures.
///
/// The indexing pipeline downcasts to this type to detect grammar-level
/// failures (ABI mismatch, renamed AST node) and aggregate them into a
/// per-language summary, instead of treating them like per-file parse
/// errors. Plain string matching on the error message would couple the two
/// sides at the wrong layer.
#[derive(Debug, thiserror::Error)]
#[error("failed to load {lang:?} grammar: {reason}")]
pub struct GrammarLoadError {
    pub lang: Language,
    pub reason: String,
}

/// Common language keywords to exclude from body token extraction.
pub(super) fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "self"
            | "Self"
            | "return"
            | "if"
            | "else"
            | "while"
            | "for"
            | "in"
            | "let"
            | "mut"
            | "fn"
            | "pub"
            | "const"
            | "static"
            | "impl"
            | "struct"
            | "enum"
            | "trait"
            | "use"
            | "mod"
            | "crate"
            | "super"
            | "true"
            | "false"
            | "None"
            | "Some"
            | "Ok"
            | "Err"
            | "match"
            | "def"
            | "class"
            | "import"
            | "from"
            | "pass"
            | "with"
            | "as"
            | "var"
            | "val"
            | "func"
            | "nil"
            | "null"
            | "void"
            | "new"
            | "try"
            | "catch"
            | "throw"
            | "throws"
            | "this"
            | "async"
            | "await"
            | "yield"
            | "break"
            | "continue"
            | "where"
            | "type"
            | "interface"
            | "extends"
            | "implements"
            | "override"
            | "private"
            | "public"
            | "protected"
            | "internal"
            | "open"
            | "final"
            | "abstract"
            | "default"
            | "package"
            | "object"
            | "namespace"
            | "template"
            | "typename"
            | "virtual"
            | "explicit"
            | "constexpr"
            | "noexcept"
            | "nullptr"
            | "sizeof"
            | "delete"
            | "inline"
            | "auto"
            | "volatile"
            | "mutable"
            | "extern"
            | "typedef"
            | "using"
            | "friend"
            | "operator"
            | "include"
    )
}

/// Decide whether an identifier token is worth indexing as a ref.
///
/// Filters tuned to balance recall (catch real symbol uses across all
/// supported case conventions) against FST bloat (skip prose nouns and
/// trivial locals).
pub(crate) fn is_meaningful_identifier(word: &str) -> bool {
    if word.len() < 3 || word.bytes().all(|b| b == b'_') {
        return false;
    }
    if is_keyword(word) {
        return false;
    }

    let has_underscore = word.contains('_');
    let has_upper = word.bytes().any(|b| b.is_ascii_uppercase());
    let has_lower = word.bytes().any(|b| b.is_ascii_lowercase());

    // Accept any identifier with structural shape: either it carries a
    // case boundary (mixed case) or an explicit word separator (`_`).
    // Pure lowercase words like `total` or `amount` are skipped — they
    // dominate prose and trivial locals and would drown out real refs.
    has_underscore || (has_upper && has_lower)
}

#[cfg(test)]
mod tests;
