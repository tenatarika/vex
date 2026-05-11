use crate::index::symbols::SymbolKind;

use super::SearchResult;

// Boost factors — easy to tune, all in one place
const KIND_BOOST_TYPE: f64 = 1.3; // class, struct, interface, trait, enum
const KIND_BOOST_FUNC: f64 = 1.2; // function, method
const KIND_BOOST_CONST: f64 = 1.1; // constant
const KIND_DEMOTE_IMPL: f64 = 0.7; // impl blocks (usually noise)
const EXACT_NAME_BOOST: f64 = 1.5; // exact name match
const TEST_PATH_DEMOTE: f64 = 0.8; // results from test directories
const DEPTH_PENALTY_PER_LEVEL: f64 = 0.02; // per extra '/' in path

// Context-aware boost factors (--kind, --context-path)
const KIND_HINT_MATCH: f64 = 1.4; // explicit --kind match
const KIND_HINT_MISMATCH: f64 = 1.0; // no penalty for mismatch (hint, not filter)
const PATH_OVERLAP_PER_COMPONENT: f64 = 0.08; // per shared path component
const PATH_OVERLAP_MAX: f64 = 1.4; // cap path overlap boost
const MODULE_SAME_DIR: f64 = 1.15; // result in same directory as context
const MODULE_SIBLING: f64 = 1.05; // result shares parent directory

/// Optional context for metadata-aware reranking.
#[derive(Debug, Default)]
pub struct RerankContext<'a> {
    /// Explicit kind filter (e.g., user passed `--kind fn`).
    pub kind_hint: Option<SymbolKind>,
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

    for result in &mut results {
        let mut boost = 1.0;

        // Kind affinity (heuristic from query shape)
        boost *= kind_boost(&result.kind, &shape);

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

        // Explicit kind hint (--kind fn)
        if let Some(hint) = ctx.kind_hint {
            boost *= kind_hint_boost(&result.kind, hint);
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

fn kind_hint_boost(result_kind: &str, hint: SymbolKind) -> f64 {
    if result_kind == hint.as_str() {
        KIND_HINT_MATCH
    } else {
        KIND_HINT_MISMATCH
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
    let p = path.to_lowercase();
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

    #[test]
    fn kind_hint_boosts_matching_kind() {
        let results = vec![
            make("process", "class", "src/a.rs"),
            make("process", "function", "src/b.rs"),
        ];
        let c = RerankContext {
            kind_hint: Some(SymbolKind::Function),
            ..Default::default()
        };
        let ranked = rerank("process", &c, results);
        assert_eq!(ranked[0].kind, "function");
    }

    #[test]
    fn kind_hint_demotes_mismatching_kind() {
        let results = vec![
            make("Config", "function", "src/a.rs"),
            make("Config", "struct", "src/b.rs"),
        ];
        let c = RerankContext {
            kind_hint: Some(SymbolKind::Struct),
            ..Default::default()
        };
        let ranked = rerank("Config", &c, results);
        assert_eq!(ranked[0].kind, "struct");
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
