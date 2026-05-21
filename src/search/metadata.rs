//! Symbol metadata filters for `vex search` / `vex show` (11.6).
//!
//! Post-filter run over the rerank output: lets a user narrow results
//! to `--visibility public`, `--async`, `--static`, or `--sealed`
//! without a format bump.
//!
//! Identification is purely lexical against the `signature` field that
//! the parser already captures (first line of the parent declaration,
//! up to 200 chars). For `pub async fn payment_processor()` that
//! string is `"pub async fn payment_processor()"`; for
//! `public sealed class Foo` it's `"public sealed class Foo"`. The
//! filter tokenises on whitespace, checks for a token from a small
//! per-language alias set, and compares.
//!
//! Limits of this approach:
//!   * default visibility per language (Rust = private, TS class member
//!     = public) is **not** inferred — only explicit keywords match.
//!     `--visibility private` won't catch a bare `fn foo()` in Rust.
//!   * decorator-based filters (`@Obsolete`, `[Required]`) and
//!     return-type filters (`--returns 'Task<*>'`) are out of scope for
//!     11.6 — return-type especially is gated behind the 11.1 type
//!     resolver.
//!   * `signature` is None for symbols whose parser didn't capture a
//!     parent node (heading, package, some property kinds); those
//!     symbols are dropped by any metadata filter rather than passed
//!     through, since "no signature" can't prove a positive match.

use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
    Protected,
    Internal,
}

#[derive(Debug, thiserror::Error)]
#[error("unknown --visibility value: \"{0}\". Accepts: public, private, protected, internal")]
pub struct ParseVisibilityError(String);

impl FromStr for Visibility {
    type Err = ParseVisibilityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "public" | "pub" => Ok(Self::Public),
            "private" | "priv" => Ok(Self::Private),
            "protected" => Ok(Self::Protected),
            "internal" => Ok(Self::Internal),
            other => Err(ParseVisibilityError(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MetadataFilter {
    pub visibility: Option<Visibility>,
    /// `Some(true)` keeps only async/suspend symbols; `Some(false)`
    /// keeps only non-async. `None` is "don't filter on this axis".
    pub async_required: Option<bool>,
    /// `Some(true)` from `--static-only`. The `Some(false)` branch is
    /// reserved for a future `--no-static` CLI flag — `build_metadata_filter`
    /// never produces it today, but `matches()` handles it correctly
    /// so the future flag drops in without core changes.
    pub static_required: Option<bool>,
    /// `Some(true)` from `--sealed-only`. Same `Some(false)` reservation
    /// as `static_required`.
    pub sealed_required: Option<bool>,
}

impl MetadataFilter {
    pub fn is_empty(&self) -> bool {
        self.visibility.is_none()
            && self.async_required.is_none()
            && self.static_required.is_none()
            && self.sealed_required.is_none()
    }

    /// True when the signature passes every active filter. Symbols
    /// whose signature is `None` fail any active filter — see module
    /// doc for the rationale.
    pub fn matches(&self, signature: Option<&str>) -> bool {
        if self.is_empty() {
            return true;
        }
        let Some(sig) = signature else {
            return false;
        };
        let tokens = tokenise(sig);
        if let Some(v) = self.visibility {
            if !has_visibility(&tokens, v) {
                return false;
            }
        }
        if let Some(want) = self.async_required {
            if has_token_any(&tokens, &["async", "suspend"]) != want {
                return false;
            }
        }
        if let Some(want) = self.static_required {
            if has_token_any(&tokens, &["static"]) != want {
                return false;
            }
        }
        if let Some(want) = self.sealed_required {
            // `sealed` is unambiguous (Kotlin/C#); `final` is overloaded
            // — Java uses `final class Foo` for the same concept, but
            // the same keyword also annotates method parameters
            // (`void f(final String x)`) and fields (`private final
            // int id`). Match `final` only when it co-occurs with a
            // type-introducing keyword so a parameter doesn't false-
            // positive into the sealed bucket.
            let is_sealed_token = has_token_any(&tokens, &["sealed"])
                || (has_token_any(&tokens, &["final"])
                    && has_token_any(&tokens, &["class", "interface", "enum", "record"]));
            if is_sealed_token != want {
                return false;
            }
        }
        true
    }
}

fn tokenise(sig: &str) -> Vec<&str> {
    // Split on whitespace and on common punctuation that separates
    // modifiers from each other in C-family signatures. The fast path
    // `split_whitespace` already covers Rust/Python/TS/Kotlin where
    // modifiers are space-separated.
    sig.split(|c: char| c.is_whitespace() || c == '(' || c == '<' || c == ':')
        .filter(|s| !s.is_empty())
        .collect()
}

fn has_token_any(tokens: &[&str], any: &[&str]) -> bool {
    tokens.iter().any(|t| any.contains(t))
}

fn has_visibility(tokens: &[&str], v: Visibility) -> bool {
    match v {
        Visibility::Public => {
            // Rust: `pub`, `pub(crate)`, `pub(super)`. Java/C#/TS:
            // `public`. JS/TS modules: `export`. The tokeniser splits
            // on `(`, so `pub(crate)` is already two tokens with `pub`
            // matching directly.
            has_token_any(tokens, &["pub", "public", "export"])
        }
        Visibility::Private => has_token_any(tokens, &["private"]),
        Visibility::Protected => has_token_any(tokens, &["protected"]),
        // Kotlin's `internal` and C#'s `internal` share the keyword.
        Visibility::Internal => has_token_any(tokens, &["internal"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_passes_everything() {
        let f = MetadataFilter::default();
        assert!(f.is_empty());
        assert!(f.matches(Some("pub fn foo()")));
        assert!(f.matches(Some("private async def bar")));
        assert!(f.matches(None));
    }

    #[test]
    fn missing_signature_fails_any_active_filter() {
        let f = MetadataFilter {
            visibility: Some(Visibility::Public),
            ..Default::default()
        };
        assert!(!f.matches(None));
    }

    #[test]
    fn visibility_public_matches_rust_and_java() {
        let f = MetadataFilter {
            visibility: Some(Visibility::Public),
            ..Default::default()
        };
        assert!(f.matches(Some("pub fn foo()")));
        assert!(f.matches(Some("pub(crate) fn bar()")));
        assert!(f.matches(Some("public class Foo")));
        assert!(f.matches(Some("export function baz()")));
        assert!(!f.matches(Some("fn private_default()")));
        assert!(!f.matches(Some("private class Hidden")));
    }

    #[test]
    fn visibility_private_does_not_infer_default() {
        // Per module doc: bare `fn foo()` in Rust is private-by-default,
        // but the filter does not infer that — it only matches an
        // explicit `private` keyword. This locks in the documented
        // limitation so a future tightening is a visible change.
        let f = MetadataFilter {
            visibility: Some(Visibility::Private),
            ..Default::default()
        };
        assert!(f.matches(Some("private class Hidden")));
        assert!(!f.matches(Some("fn definitely_private_in_rust()")));
    }

    #[test]
    fn async_required_filters_async_only() {
        let f = MetadataFilter {
            async_required: Some(true),
            ..Default::default()
        };
        assert!(f.matches(Some("async fn fetch()")));
        assert!(f.matches(Some("async def handle(self):")));
        assert!(f.matches(Some("suspend fun process()")));
        assert!(!f.matches(Some("fn synchronous()")));
    }

    #[test]
    fn async_excluded_filters_to_non_async() {
        let f = MetadataFilter {
            async_required: Some(false),
            ..Default::default()
        };
        assert!(f.matches(Some("fn sync_only()")));
        assert!(!f.matches(Some("async fn never_match()")));
    }

    #[test]
    fn static_required_catches_class_member_modifier() {
        let f = MetadataFilter {
            static_required: Some(true),
            ..Default::default()
        };
        assert!(f.matches(Some("public static void main(String[] args)")));
        assert!(f.matches(Some("static factory(): Foo")));
        assert!(!f.matches(Some("public void instance()")));
    }

    #[test]
    fn sealed_required_rejects_final_in_method_parameter() {
        // Regression guard: Java methods can take `final` parameters
        // (`void process(final String x)`). Before the type-keyword
        // co-requirement, that signature matched `--sealed-only`
        // because `final` alone was treated as a sealing modifier.
        let f = MetadataFilter {
            sealed_required: Some(true),
            ..Default::default()
        };
        assert!(!f.matches(Some("public void process(final String input)")));
        assert!(!f.matches(Some("private final int id;")));
    }

    #[test]
    fn sealed_required_matches_kotlin_csharp_and_java_final() {
        let f = MetadataFilter {
            sealed_required: Some(true),
            ..Default::default()
        };
        assert!(f.matches(Some("sealed class Result")));
        assert!(f.matches(Some("public sealed class Money")));
        assert!(f.matches(Some("public final class Token")));
        assert!(!f.matches(Some("class Open")));
    }

    #[test]
    fn multiple_filters_combine_with_and() {
        let f = MetadataFilter {
            visibility: Some(Visibility::Public),
            async_required: Some(true),
            ..Default::default()
        };
        assert!(f.matches(Some("pub async fn fetch()")));
        assert!(!f.matches(Some("pub fn sync_only()")));
        assert!(!f.matches(Some("async fn missing_visibility()")));
    }

    #[test]
    fn tokeniser_splits_pub_crate_into_separable_tokens() {
        // The key invariant for the filter: `pub(crate)` must yield a
        // `pub` token so `--visibility public` matches restricted-pub
        // declarations. Splitting on `(` accomplishes that. We don't
        // assert anything stronger about generics — the goal is filter
        // accuracy on visibility keywords, not a perfect lexer.
        let toks = tokenise("pub(crate) async fn fetch<T>(arg: T) -> Result<()>");
        assert!(toks.contains(&"pub"), "expected `pub` token: {toks:?}");
        assert!(toks.contains(&"async"));
        assert!(toks.contains(&"fn"));
        assert!(toks.contains(&"fetch"));
    }

    #[test]
    fn visibility_from_str_aliases() {
        assert_eq!("public".parse::<Visibility>().unwrap(), Visibility::Public);
        assert_eq!("pub".parse::<Visibility>().unwrap(), Visibility::Public);
        assert_eq!(
            "private".parse::<Visibility>().unwrap(),
            Visibility::Private
        );
        assert_eq!("priv".parse::<Visibility>().unwrap(), Visibility::Private);
        assert!("frobnicate".parse::<Visibility>().is_err());
    }
}
