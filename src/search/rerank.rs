use super::SearchResult;

// Boost factors — easy to tune, all in one place
const KIND_BOOST_TYPE: f64 = 1.3; // class, struct, interface, trait, enum
const KIND_BOOST_FUNC: f64 = 1.2; // function, method
const KIND_BOOST_CONST: f64 = 1.1; // constant
const KIND_DEMOTE_IMPL: f64 = 0.7; // impl blocks (usually noise)
const EXACT_NAME_BOOST: f64 = 1.5; // exact name match
const TEST_PATH_DEMOTE: f64 = 0.8; // results from test directories
const DEPTH_PENALTY_PER_LEVEL: f64 = 0.02; // per extra '/' in path

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
pub fn rerank(query: &str, mut results: Vec<SearchResult>) -> Vec<SearchResult> {
    if results.len() <= 1 {
        return results;
    }

    let shape = query_shape(query);
    let query_lower = query.to_lowercase();

    for result in &mut results {
        let mut boost = 1.0;

        // Kind affinity
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

        result.score *= boost;
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

    #[test]
    fn type_query_boosts_class_over_property() {
        let results = vec![
            make("service_url", "property", "src/config.rs"),
            make("PaymentService", "class", "src/billing.rs"),
        ];
        let ranked = rerank("PaymentService", results);
        assert_eq!(ranked[0].kind, "class");
    }

    #[test]
    fn function_query_boosts_function_over_class() {
        let results = vec![
            make("GetUser", "class", "src/models.rs"),
            make("get_user", "function", "src/api.rs"),
        ];
        let ranked = rerank("get_user", results);
        assert_eq!(ranked[0].kind, "function");
    }

    #[test]
    fn exact_name_match_ranks_first() {
        let results = vec![
            make("PaymentGateway", "class", "src/billing.rs"),
            make("Payment", "class", "src/billing.rs"),
        ];
        let ranked = rerank("Payment", results);
        assert_eq!(ranked[0].name, "Payment");
    }

    #[test]
    fn test_path_demoted() {
        let results = vec![
            make("Config", "class", "tests/test_config.rs"),
            make("Config", "class", "src/config.rs"),
        ];
        let ranked = rerank("Config", results);
        assert_eq!(ranked[0].path, "src/config.rs");
    }

    #[test]
    fn shallow_path_ranks_higher() {
        let results = vec![
            make("Foo", "struct", "src/internal/utils/helpers/foo.rs"),
            make("Foo", "struct", "src/foo.rs"),
        ];
        let ranked = rerank("Foo", results);
        assert_eq!(ranked[0].path, "src/foo.rs");
    }

    #[test]
    fn empty_results_unchanged() {
        let results = rerank("query", vec![]);
        assert!(results.is_empty());
    }

    #[test]
    fn single_result_unchanged() {
        let results = vec![make("Foo", "struct", "src/foo.rs")];
        let ranked = rerank("Foo", results);
        assert_eq!(ranked.len(), 1);
    }
}
