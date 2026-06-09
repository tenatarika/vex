//! Identifier-shape predicate, shared between commands.
//!
//! Promoted from `cli/cmd_search.rs` in Phase 14.9 so `cmd_history`'s
//! prefix-FST fallback (Tier B.8) can reuse the same conservative
//! definition without a CLI-module dependency.

/// True when `query` is shaped like a single bare identifier
/// (`compile_query`, `Foo`, `_internal`, `my_fn`). Used by callers
/// that need to distinguish "exact-symbol lookup gone fuzzy" from a
/// genuine relevance query.
///
/// Conservative: requires the first char to be ASCII letter or
/// underscore and every subsequent char to be ASCII alphanumeric or
/// underscore. Multi-word queries (`payment processor`), qualified
/// paths (`Foo::bar`), generics (`Foo<T>`), globs, and non-ASCII
/// names all fall through — those are clearly relevance queries, not
/// exact-symbol lookups.
pub fn is_identifier_shaped(query: &str) -> bool {
    let mut bytes = query.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::is_identifier_shaped;

    #[test]
    fn identifier_shaped_accepts_typical_symbols() {
        for q in [
            "compile_query",
            "Foo",
            "_internal",
            "my_fn",
            "PaymentProcessor",
            "X",
            "_",
            "snake_case_name",
            "CamelCaseName",
            "fn123",
        ] {
            assert!(is_identifier_shaped(q), "should accept {q:?}");
        }
    }

    #[test]
    fn identifier_shaped_rejects_relevance_queries() {
        for q in [
            "payment processor", // multi-word
            "Foo::bar",          // qualified path
            "Foo.bar",           // member access
            "Foo<T>",            // generic
            "1Foo",              // starts with digit
            "",                  // empty
            "пример",            // non-ASCII identifier (precision-preserving — non-ASCII
            // names exist but rg-style symbol lookup is rare)
            "Foo-bar", // hyphen
            "Foo Bar", // space
            "*foo*",   // glob
            "/regex/", // regex
        ] {
            assert!(!is_identifier_shaped(q), "should reject {q:?}");
        }
    }
}
