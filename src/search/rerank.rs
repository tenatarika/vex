use std::str::FromStr;

use crate::index::symbols::SymbolKind;

use super::SearchResult;

// Boost factors — easy to tune, all in one place
const KIND_BOOST_TYPE: f64 = 1.3; // class, struct, interface, trait, enum
const KIND_BOOST_FUNC: f64 = 1.2; // function, method
const KIND_BOOST_CONST: f64 = 1.1; // constant
const KIND_DEMOTE_IMPL: f64 = 0.7; // impl blocks (usually noise)
const KIND_DEMOTE_HEADING: f64 = 0.6; // Markdown headings demoted unconditionally — defs-first default
const EXACT_NAME_BOOST: f64 = 1.5; // exact name match
const TEST_PATH_DEMOTE: f64 = 0.8; // results from test directories
const DEPTH_PENALTY_PER_LEVEL: f64 = 0.02; // per extra '/' in path

// Context-aware boost factors (--kind, --context-path)
const KIND_HINT_MATCH: f64 = 1.4; // explicit --kind match
const PATH_OVERLAP_PER_COMPONENT: f64 = 0.08; // per shared path component
const PATH_OVERLAP_MAX: f64 = 1.4; // cap path overlap boost
const MODULE_SAME_DIR: f64 = 1.15; // result in same directory as context
const MODULE_SIBLING: f64 = 1.05; // result shares parent directory

/// One value parsed from `--kind`. Multiple selectors are unioned (any
/// match boosts the result). Canonical SymbolKind names route through
/// `Symbol`; the four meta-selectors are convenience aliases the user
/// asked for in the 11.x trust-gap audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindSelector {
    /// Specific symbol kind (function, struct, class, …).
    Symbol(SymbolKind),
    /// Any definition kind — i.e. anything except Markdown headings.
    Def,
    /// Markdown-style heading symbol.
    Comment,
    /// Result whose path is in a test directory (uses `is_test_path`).
    Test,
    /// Reserved alias for `vex usages` (refs only). On search/show this is
    /// silently accepted as a no-op so forward-compat clients don't fail.
    Ref,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "unknown --kind value: \"{0}\". Accepts canonical kind names (function, struct, class, …) \
     plus aliases: def (all definitions), comment (headings), test (test-path results), \
     ref (reserved for vex usages)"
)]
pub struct ParseKindSelectorError(String);

impl FromStr for KindSelector {
    type Err = ParseKindSelectorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "def" | "definition" => Ok(Self::Def),
            "comment" => Ok(Self::Comment),
            "test" => Ok(Self::Test),
            "ref" | "reference" => Ok(Self::Ref),
            other => SymbolKind::from_str(other)
                .map(Self::Symbol)
                .map_err(|_| ParseKindSelectorError(other.to_owned())),
        }
    }
}

impl KindSelector {
    /// Parse a list of `--kind` values from the CLI; comma-separated and
    /// repeated invocations both flatten into the returned vector.
    pub fn parse_many(raw: &[String]) -> Result<Vec<Self>, ParseKindSelectorError> {
        raw.iter()
            .flat_map(|s| s.split(',').map(str::trim).filter(|p| !p.is_empty()))
            .map(Self::from_str)
            .collect()
    }

    fn matches(&self, result_kind: &str, path: &str) -> bool {
        match self {
            Self::Symbol(k) => result_kind == k.as_str(),
            Self::Def => result_kind != "heading",
            Self::Comment => result_kind == "heading",
            Self::Test => is_test_path(path),
            Self::Ref => false,
        }
    }
}

/// Optional context for metadata-aware reranking.
#[derive(Debug, Default)]
pub struct RerankContext<'a> {
    /// Zero or more `--kind` selectors. Multiple values are unioned —
    /// the boost applies when **any** selector matches.
    pub kind_hints: Vec<KindSelector>,
    /// Path of the file the user is currently editing.
    pub context_path: Option<&'a str>,
}

/// Query shape heuristic — guess what kind of symbol the user is looking for.
#[derive(Debug, PartialEq)]
enum QueryShape {
    TypeLike,     // PascalCase → likely class/struct/trait
    FunctionLike, // snake_case or camelCase → likely function/method
    ConstantLike, // ALL_CAPS → likely constant
    Ambiguous,    // single word, lowercase
}

fn query_shape(query: &str) -> QueryShape {
    let has_underscore = query.contains('_');
    let first_upper = query.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    let all_upper = query.chars().all(|c| c.is_ascii_uppercase() || c == '_');
    let has_lower = query.chars().any(|c| c.is_ascii_lowercase());

    if has_underscore && all_upper && query.len() > 1 {
        QueryShape::ConstantLike
    } else if first_upper && has_lower {
        QueryShape::TypeLike
    } else if has_underscore
        || (!first_upper && has_lower && query.chars().any(|c| c.is_ascii_uppercase()))
    {
        QueryShape::FunctionLike
    } else {
        QueryShape::Ambiguous
    }
}

/// Rerank search results based on query context heuristics.
/// Adjusts scores via multiplicative boosts and re-sorts.
pub fn rerank(
    query: &str,
    ctx: &RerankContext<'_>,
    mut results: Vec<SearchResult>,
) -> Vec<SearchResult> {
    // Sanitise scores unconditionally (even for single-result input)
    for result in &mut results {
        if result.score.is_nan() {
            result.score = 0.0;
        } else if result.score.is_infinite() {
            result.score = f64::MAX / 2.0;
        }
    }

    if results.len() <= 1 {
        return results;
    }

    let shape = query_shape(query);
    let query_lower = query.to_lowercase();
    // Defs-first default: demote Markdown headings unless the user
    // explicitly asked for them via `--kind comment` or `--kind heading`.
    let user_wants_headings = ctx.kind_hints.iter().any(|h| {
        matches!(
            h,
            KindSelector::Comment | KindSelector::Symbol(SymbolKind::Heading)
        )
    });

    for result in &mut results {
        let mut boost = 1.0;

        // Kind affinity (heuristic from query shape)
        boost *= kind_boost(&result.kind, &shape);

        if result.kind == "heading" && !user_wants_headings {
            boost *= KIND_DEMOTE_HEADING;
        }

        // Exact name match
        if result.name.to_lowercase() == query_lower {
            boost *= EXACT_NAME_BOOST;
        }

        // Path depth penalty (shallower = more prominent), floored at 0.5
        let depth = result.path.matches('/').count();
        if depth > 1 {
            boost *= (1.0 - DEPTH_PENALTY_PER_LEVEL * (depth - 1) as f64).max(0.5);
        }

        // Test path demotion
        if is_test_path(&result.path) {
            boost *= TEST_PATH_DEMOTE;
        }

        // Explicit kind hint (--kind …). Multi-value: boost if ANY hint
        // matches. Empty hints vector → no-op.
        if !ctx.kind_hints.is_empty()
            && ctx
                .kind_hints
                .iter()
                .any(|h| h.matches(&result.kind, &result.path))
        {
            boost *= KIND_HINT_MATCH;
        }

        // Path overlap + module proximity (--context-path)
        if let Some(ctx_path) = ctx.context_path {
            boost *= path_overlap_boost(&result.path, ctx_path);
            boost *= module_proximity_boost(&result.path, ctx_path);
        }

        result.score *= boost;

        // Guard against overflow to infinity after applying boosts.
        if result.score.is_infinite() {
            result.score = f64::MAX / 2.0;
        }
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

fn kind_boost(kind: &str, shape: &QueryShape) -> f64 {
    match shape {
        QueryShape::TypeLike => match kind {
            "class" | "struct" | "interface" | "trait" | "enum" | "type_alias" => KIND_BOOST_TYPE,
            "impl" => KIND_DEMOTE_IMPL,
            _ => 1.0,
        },
        QueryShape::FunctionLike => match kind {
            "function" | "method" => KIND_BOOST_FUNC,
            "impl" => KIND_DEMOTE_IMPL,
            _ => 1.0,
        },
        QueryShape::ConstantLike => match kind {
            "constant" | "property" => KIND_BOOST_CONST,
            _ => 1.0,
        },
        QueryShape::Ambiguous => match kind {
            "impl" => KIND_DEMOTE_IMPL,
            _ => 1.0,
        },
    }
}

fn path_overlap_boost(result_path: &str, context_path: &str) -> f64 {
    // Compare directory components only (exclude filename)
    let result_dir = dir_of(result_path);
    let context_dir = dir_of(context_path);
    if result_dir.is_empty() || context_dir.is_empty() {
        return 1.0;
    }
    let shared = result_dir
        .split('/')
        .zip(context_dir.split('/'))
        .take_while(|(a, b)| a == b)
        .count();
    if shared == 0 {
        return 1.0;
    }
    (1.0 + PATH_OVERLAP_PER_COMPONENT * shared as f64).min(PATH_OVERLAP_MAX)
}

fn module_proximity_boost(result_path: &str, context_path: &str) -> f64 {
    let result_dir = dir_of(result_path);
    let context_dir = dir_of(context_path);
    if result_dir == context_dir && !result_dir.is_empty() {
        MODULE_SAME_DIR
    } else if parent_of(result_dir) == parent_of(context_dir) && !parent_of(context_dir).is_empty()
    {
        MODULE_SIBLING
    } else {
        1.0
    }
}

fn dir_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

fn parent_of(dir: &str) -> &str {
    dir.rfind('/').map_or("", |i| &dir[..i])
}

fn is_test_path(path: &str) -> bool {
    // Hot path: runs once per search result during ranking. The common
    // case (Unix) allocates once for the lowercase conversion; Windows
    // pays a second allocation to swap separators. Indexed paths
    // preserve the host separator, so without this normalization the
    // tests down-rank would silently disable itself on Windows.
    let lowered = path.to_lowercase();
    let owned_normalized;
    let p: &str = if lowered.contains('\\') {
        owned_normalized = lowered.replace('\\', "/");
        &owned_normalized
    } else {
        lowered.as_str()
    };
    p.contains("/test/")
        || p.contains("/tests/")
        || p.contains("/test_")
        || p.contains("_test.")
        || p.contains("_test_")
        || p.contains("_spec.")
        || p.starts_with("test/")
        || p.starts_with("tests/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::MatchType;

    fn make(name: &str, kind: &str, path: &str) -> SearchResult {
        SearchResult {
            name: name.to_string(),
            kind: kind.to_string(),
            path: path.to_string(),
            line: 1,
            signature: None,
            score: 1.0,
            match_type: MatchType::Structural,
        }
    }

    #[test]
    fn query_shape_pascal_case() {
        assert_eq!(query_shape("PaymentService"), QueryShape::TypeLike);
        assert_eq!(query_shape("IndexReader"), QueryShape::TypeLike);
    }

    #[test]
    fn query_shape_snake_case() {
        assert_eq!(query_shape("get_user"), QueryShape::FunctionLike);
        assert_eq!(query_shape("process_batch"), QueryShape::FunctionLike);
    }

    #[test]
    fn query_shape_camel_case() {
        assert_eq!(query_shape("processPayment"), QueryShape::FunctionLike);
    }

    #[test]
    fn query_shape_constant() {
        assert_eq!(query_shape("MAX_RETRIES"), QueryShape::ConstantLike);
        assert_eq!(query_shape("CHUNK_SIZE"), QueryShape::ConstantLike);
    }

    #[test]
    fn query_shape_single_word() {
        assert_eq!(query_shape("service"), QueryShape::Ambiguous);
        assert_eq!(query_shape("config"), QueryShape::Ambiguous);
    }

    fn ctx() -> RerankContext<'static> {
        RerankContext::default()
    }

    #[test]
    fn type_query_boosts_class_over_property() {
        let results = vec![
            make("service_url", "property", "src/config.rs"),
            make("PaymentService", "class", "src/billing.rs"),
        ];
        let ranked = rerank("PaymentService", &ctx(), results);
        assert_eq!(ranked[0].kind, "class");
    }

    #[test]
    fn function_query_boosts_function_over_class() {
        let results = vec![
            make("GetUser", "class", "src/models.rs"),
            make("get_user", "function", "src/api.rs"),
        ];
        let ranked = rerank("get_user", &ctx(), results);
        assert_eq!(ranked[0].kind, "function");
    }

    #[test]
    fn exact_name_match_ranks_first() {
        let results = vec![
            make("PaymentGateway", "class", "src/billing.rs"),
            make("Payment", "class", "src/billing.rs"),
        ];
        let ranked = rerank("Payment", &ctx(), results);
        assert_eq!(ranked[0].name, "Payment");
    }

    #[test]
    fn test_path_demoted() {
        let results = vec![
            make("Config", "class", "tests/test_config.rs"),
            make("Config", "class", "src/config.rs"),
        ];
        let ranked = rerank("Config", &ctx(), results);
        assert_eq!(ranked[0].path, "src/config.rs");
    }

    #[test]
    fn shallow_path_ranks_higher() {
        let results = vec![
            make("Foo", "struct", "src/internal/utils/helpers/foo.rs"),
            make("Foo", "struct", "src/foo.rs"),
        ];
        let ranked = rerank("Foo", &ctx(), results);
        assert_eq!(ranked[0].path, "src/foo.rs");
    }

    #[test]
    fn empty_results_unchanged() {
        let results = rerank("query", &ctx(), vec![]);
        assert!(results.is_empty());
    }

    #[test]
    fn single_result_unchanged() {
        let results = vec![make("Foo", "struct", "src/foo.rs")];
        let ranked = rerank("Foo", &ctx(), results);
        assert_eq!(ranked.len(), 1);
    }

    // --- Kind hint tests ---

    fn ctx_with_kind(raw: &[&str]) -> RerankContext<'static> {
        let owned: Vec<String> = raw.iter().map(|s| (*s).into()).collect();
        RerankContext {
            kind_hints: KindSelector::parse_many(&owned).expect("parse_many"),
            context_path: None,
        }
    }

    #[test]
    fn kind_hint_boosts_matching_kind() {
        let results = vec![
            make("process", "class", "src/a.rs"),
            make("process", "function", "src/b.rs"),
        ];
        let ranked = rerank("process", &ctx_with_kind(&["function"]), results);
        assert_eq!(ranked[0].kind, "function");
    }

    #[test]
    fn kind_hint_demotes_mismatching_kind() {
        let results = vec![
            make("Config", "function", "src/a.rs"),
            make("Config", "struct", "src/b.rs"),
        ];
        let ranked = rerank("Config", &ctx_with_kind(&["struct"]), results);
        assert_eq!(ranked[0].kind, "struct");
    }

    #[test]
    fn multi_value_kind_boosts_any_match() {
        // Both `function` and `struct` are hinted — both should outrank `class`.
        let results = vec![
            make("Foo", "class", "src/a.rs"),
            make("Foo", "function", "src/b.rs"),
            make("Foo", "struct", "src/c.rs"),
        ];
        let ranked = rerank("Foo", &ctx_with_kind(&["function", "struct"]), results);
        assert!(
            ranked[0].kind == "function" || ranked[0].kind == "struct",
            "expected fn or struct at top, got: {:?}",
            ranked[0].kind
        );
        assert_eq!(ranked[2].kind, "class", "class should be last (not hinted)");
    }

    #[test]
    fn comma_separated_kind_values_parse() {
        // `--kind fn,struct` is equivalent to `--kind fn --kind struct`.
        let hints = KindSelector::parse_many(&["fn,struct".into()]).expect("parse comma list");
        assert_eq!(hints.len(), 2);
    }

    #[test]
    fn def_alias_boosts_all_non_heading_kinds() {
        let results = vec![
            make("Section", "heading", "README.md"),
            make("payment_processor", "function", "src/api.rs"),
        ];
        let ranked = rerank("payment_processor", &ctx_with_kind(&["def"]), results);
        assert_eq!(ranked[0].kind, "function");
    }

    #[test]
    fn comment_alias_boosts_headings() {
        let results = vec![
            make("Section", "heading", "README.md"),
            make("payment_processor", "function", "src/api.rs"),
        ];
        let ranked = rerank("payment", &ctx_with_kind(&["comment"]), results);
        assert_eq!(ranked[0].kind, "heading");
    }

    #[test]
    fn test_alias_boosts_test_paths() {
        let results = vec![
            make("Foo", "function", "src/foo.rs"),
            make("Foo", "function", "tests/it.rs"),
        ];
        let ranked = rerank("Foo", &ctx_with_kind(&["test"]), results);
        assert_eq!(ranked[0].path, "tests/it.rs");
    }

    #[test]
    fn ref_alias_is_no_op_for_search() {
        // `ref` is reserved for `vex usages`; on search/show it parses but
        // never boosts. Result of seeding a function vs a class should
        // therefore depend only on the query-shape default (FunctionLike
        // wins). This guards against `ref` accidentally engaging.
        let results = vec![
            make("get_user", "class", "src/a.rs"),
            make("get_user", "function", "src/b.rs"),
        ];
        let with_ref = rerank("get_user", &ctx_with_kind(&["ref"]), results.clone());
        let without_ref = rerank("get_user", &ctx(), results);
        assert_eq!(with_ref[0].kind, without_ref[0].kind);
    }

    #[test]
    fn heading_demoted_by_default_for_defs_first_ordering() {
        // Without any --kind hint, a function should outrank a heading even
        // when they have the same exact name — the defs-first invariant.
        let results = vec![
            make("payment_processor", "heading", "docs/spec.md"),
            make("payment_processor", "function", "src/api.rs"),
        ];
        let ranked = rerank("payment_processor", &ctx(), results);
        assert_eq!(ranked[0].kind, "function");
    }

    #[test]
    fn unknown_kind_value_is_rejected() {
        let err = KindSelector::parse_many(&["banana".into()]).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("banana"),
            "expected offending value in error: {msg}"
        );
    }

    #[test]
    fn empty_or_whitespace_kind_values_yield_empty_vec() {
        // Boundary: trailing commas, only-whitespace tokens, and an empty
        // input slice all collapse to the no-op vector. Locks in the
        // contract that `--kind ',,,'` / `--kind ' '` don't fail parsing
        // and don't accidentally engage a boost branch.
        assert!(KindSelector::parse_many(&[",,, ".into()])
            .unwrap()
            .is_empty());
        assert!(KindSelector::parse_many(&[" ".into()]).unwrap().is_empty());
        assert!(KindSelector::parse_many(&[]).unwrap().is_empty());
    }

    // --- Path overlap tests ---

    #[test]
    fn path_overlap_boosts_nearby_files() {
        let results = vec![
            make("Foo", "struct", "src/auth/login.rs"),
            make("Foo", "struct", "src/billing/invoice.rs"),
        ];
        let c = RerankContext {
            context_path: Some("src/billing/gateway.rs"),
            ..Default::default()
        };
        let ranked = rerank("Foo", &c, results);
        assert_eq!(ranked[0].path, "src/billing/invoice.rs");
    }

    #[test]
    fn module_proximity_same_dir_wins() {
        let results = vec![
            make("Foo", "struct", "src/billing/other/deep.rs"),
            make("Foo", "struct", "src/billing/sibling.rs"),
        ];
        let c = RerankContext {
            context_path: Some("src/billing/gateway.rs"),
            ..Default::default()
        };
        let ranked = rerank("Foo", &c, results);
        assert_eq!(ranked[0].path, "src/billing/sibling.rs");
    }

    #[test]
    fn no_context_path_no_change() {
        let results = vec![
            make("Foo", "struct", "src/auth/login.rs"),
            make("Foo", "struct", "src/billing/invoice.rs"),
        ];
        let c = RerankContext::default();
        let ranked = rerank("Foo", &c, results);
        // Without context, both have same base score — order may vary but scores should be equal
        assert!(
            (ranked[0].score - ranked[1].score).abs() < f64::EPSILON,
            "without context, results at same depth should have equal scores"
        );
    }

    #[test]
    fn path_overlap_capped_at_max() {
        // Deep shared prefix: src/a/b/c/d/e — 5 shared components
        let boost = path_overlap_boost("src/a/b/c/d/e/foo.rs", "src/a/b/c/d/e/bar.rs");
        assert!(
            boost <= PATH_OVERLAP_MAX + f64::EPSILON,
            "boost {boost} should not exceed {PATH_OVERLAP_MAX}"
        );
    }
}
