use super::parse::{parse_composite_pattern, parse_pattern, split_top_level};
use super::*;

/// v1.11 hotfix — `$_NAME` (invalid metavar — `$_` must stand alone)
/// must be parsed as a literal `$_NAME` so the typo doesn't silently
/// degrade to matching `_NAME` with the `$` swallowed. The wildcard
/// `$_` alone (no trailing alphanumeric) continues to parse to
/// `Segment::Wildcard`.
#[test]
fn v1_11_hotfix_invalid_underscore_metavar_preserves_dollar() {
    // Invalid form `$_Bar` — must NOT match the literal `_Bar`,
    // because the source `_Bar` is not what the user wrote.
    let source = "let _Bar = 1;\n";
    let pattern = parse_pattern("$_Bar = 1", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    assert!(
        matches.is_empty(),
        "`$_Bar` must be a literal `$_Bar` (no `$` in source ⇒ no match); got {} matches",
        matches.len()
    );

    // Valid `$_` (anonymous wildcard) still works.
    let source = "let x = 1;\n";
    let pattern = parse_pattern("let $_ = 1", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    assert!(
        !matches.is_empty(),
        "`$_` standalone must remain a working wildcard"
    );
}

#[test]
fn match_rust_function_signature() {
    let source = r#"
fn foo() -> i32 { 42 }
fn bar(x: &str) -> Result<(), Error> { Ok(()) }
fn baz() {}
"#;
    // Match functions returning Result (simple text match approach)
    let pattern = parse_pattern("fn $NAME($$$) -> Result", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");

    assert!(!matches.is_empty(), "should match 'bar' (returns Result)");
    assert_eq!(matches[0].captures[0].1, "bar");
}

#[test]
fn match_python_class() {
    let source = "class UserService:\n    pass\n\nclass PaymentService:\n    pass\n\ndef helper():\n    pass\n";
    let pattern = parse_pattern("class $NAME:", Language::Python).unwrap();
    let matches = find_matches(source, &pattern, "test.py");
    assert_eq!(matches.len(), 2, "should match both classes");

    let names: Vec<&str> = matches
        .iter()
        .flat_map(|m| m.captures.iter().map(|(_, v)| v.as_str()))
        .collect();
    assert!(names.contains(&"UserService"));
    assert!(names.contains(&"PaymentService"));
}

#[test]
fn match_rust_pub_struct() {
    let source = r#"
pub struct Foo { x: i32 }
struct Bar;
pub struct Baz { y: String }
"#;
    let pattern = parse_pattern("pub struct $NAME", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");

    assert!(
        matches.len() >= 2,
        "should match Foo and Baz, got {}",
        matches.len()
    );
}

#[test]
fn match_with_ellipsis() {
    let source = r#"
fn process(x: i32, y: &str, z: bool) -> Result<(), Error> { Ok(()) }
fn simple() {}
"#;
    let pattern = parse_pattern("fn $NAME($$$)", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    assert!(matches.len() >= 2, "should match both functions");
}

#[test]
fn no_match_returns_empty() {
    let source = "fn main() {}";
    let pattern = parse_pattern("class $NAME", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    assert!(matches.is_empty());
}

#[test]
fn match_go_func() {
    let source = r#"
package main

func NewService(db *DB) *Service {
    return &Service{db: db}
}

func main() {}
"#;
    let pattern = parse_pattern("func $NAME($$$)", Language::Go).unwrap();
    let matches = find_matches(source, &pattern, "test.go");
    assert!(matches.len() >= 2, "should match both functions");
}

#[test]
fn captures_correct_names() {
    let source = "fn alpha() {}\nfn beta() {}\nfn gamma() {}";
    let pattern = parse_pattern("fn $NAME()", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");

    let names: Vec<&str> = matches
        .iter()
        .flat_map(|m| m.captures.iter().map(|(_, v)| v.as_str()))
        .collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
    assert!(names.contains(&"gamma"));
}

#[test]
fn line_numbers_correct() {
    let source = "fn foo() {}\nfn bar() {}\nfn baz() {}";
    let pattern = parse_pattern("fn bar()", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    assert!(!matches.is_empty());
    assert_eq!(matches[0].line, 2);
}

// --- 11.4 metavar back-references ---

#[test]
fn back_reference_requires_same_value_in_both_occurrences() {
    // `$NAME($NAME)` is the canonical "function that calls itself
    // with the same name as the first argument" pattern. Should
    // match `foo(foo)` and reject `foo(bar)`.
    let source = "fn caller() { foo(foo); bar(baz); }\n";
    let pattern = parse_pattern("$NAME($NAME)", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    // Only `foo(foo)` is a self-pass; `bar(baz)` must not match
    // because the second `$NAME` would have to capture `baz` while
    // the first captured `bar`.
    let names: Vec<String> = matches
        .iter()
        .filter_map(|m| m.captures.first().map(|(_, v)| v.clone()))
        .collect();
    assert!(
        names.iter().any(|n| n == "foo"),
        "expected foo(foo) to match, got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "bar"),
        "bar(baz) must not match — back-ref mismatch: {names:?}"
    );
}

#[test]
fn back_reference_captures_both_binding_sites() {
    // Even though the value is the same, the captures list keeps
    // every binding site so output shows where each occurrence
    // landed in the matched text.
    let source = "fn use_twice() { record(state, state); }\n";
    let pattern = parse_pattern("record($NAME, $NAME)", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    let m = matches
        .iter()
        .find(|m| m.matched_text.contains("record"))
        .unwrap_or_else(|| panic!("expected back-ref match: {matches:?}"));
    let name_captures: Vec<&str> = m
        .captures
        .iter()
        .filter(|(n, _)| n == "NAME")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(name_captures, vec!["state", "state"]);
}

#[test]
fn back_reference_normalises_interior_whitespace() {
    // The bracket-balanced `extract_word` returns interior bytes
    // verbatim. Without normalisation `assertEqual($X, $X)`
    // against `assertEqual((a + b), (a+b))` would spuriously
    // mismatch on the whitespace difference. The normalised
    // back-ref equality fires so the pattern matches.
    let source = "fn t() { assertEqual((a + b), (a+b)); }\n";
    let pattern = parse_pattern("assertEqual($X, $X)", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    assert!(
        matches
            .iter()
            .any(|m| m.matched_text.contains("assertEqual")),
        "whitespace-only diff should not block back-ref: {matches:?}"
    );
}

#[test]
fn normalise_capture_strips_all_whitespace() {
    assert_eq!(normalise_capture("foo"), "foo");
    assert_eq!(normalise_capture("(x + y)"), "(x+y)");
    assert_eq!(normalise_capture("( x  +   y )"), "(x+y)");
    assert_eq!(normalise_capture("\tindented\n"), "indented");
}

#[test]
fn distinct_metavar_names_remain_independent() {
    // `$A` and `$B` are different binders; they don't constrain
    // each other. Make sure the back-ref logic doesn't accidentally
    // collapse distinct names.
    let source = "fn pair() { connect(client, server); }\n";
    let pattern = parse_pattern("connect($A, $B)", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    let m = matches
        .iter()
        .find(|m| m.matched_text.contains("connect"))
        .unwrap_or_else(|| panic!("expected $A/$B match: {matches:?}"));
    let by_name: std::collections::HashMap<&str, &str> = m
        .captures
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    assert_eq!(by_name.get("A"), Some(&"client"));
    assert_eq!(by_name.get("B"), Some(&"server"));
}

// --- 11.4 Inc 6: $$$NAME / $$NAME multi-line metavars ---

#[test]
fn parses_named_block_ellipsis() {
    let pattern = parse_pattern("fn $F($$ARGS) { $$$BODY }", Language::Rust).unwrap();
    // Literal("fn ") Capture("F") Literal("(") NamedEllipsis("ARGS")
    // Literal(") { ") NamedEllipsis("BODY") Literal(" }")
    assert!(matches!(pattern.segments[3], Segment::NamedEllipsis(ref n) if n == "ARGS"));
    assert!(matches!(pattern.segments[5], Segment::NamedEllipsis(ref n) if n == "BODY"));
}

#[test]
fn anonymous_triple_dollar_still_parses_as_ellipsis() {
    let pattern = parse_pattern("fn $NAME($$$) -> Result", Language::Rust).unwrap();
    assert!(matches!(pattern.segments[3], Segment::Ellipsis));
}

#[test]
fn matches_multiline_function_body_with_named_block_ellipsis() {
    let source = "fn process(x: i32) -> Result<i32, Error> {\n    let y = x + 1;\n    let z = y * 2;\n    Ok(z)\n}\n";
    let pattern = parse_pattern(
        "fn $NAME($$ARGS) -> Result<$T, $E> { $$$BODY }",
        Language::Rust,
    )
    .unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    assert!(!matches.is_empty(), "expected match across multi-line body");
    let by_name: std::collections::HashMap<&str, &str> = matches[0]
        .captures
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    assert_eq!(by_name.get("NAME"), Some(&"process"));
    assert_eq!(by_name.get("T"), Some(&"i32"));
    assert_eq!(by_name.get("E"), Some(&"Error"));
}

#[test]
fn capture_stops_before_next_literal_byte() {
    // Pre-Inc-6, `extract_word` greedily consumed `>` to support
    // `Result<T>`-style inline generics. Patterns with a literal
    // `>` after a capture (e.g. `Result<$T, $E>`) silently lost
    // matches because `$E` swallowed the closing `>`. The
    // `extract_word_until` boundary lookahead pins the fix.
    let bounded = extract_word_until("Error>", Some(b'>'));
    assert_eq!(bounded, "Error");
    let unbounded = extract_word("Error>");
    assert_eq!(unbounded, "Error>");
}

// --- 11.4 Inc 7: AND / OR composition ---

#[test]
fn split_top_level_respects_bracket_depth() {
    // `&&` inside parens is not a split point.
    let parts = split_top_level("f($X && $Y) && g($Z)", "&&");
    assert_eq!(parts, vec!["f($X && $Y)", "g($Z)"]);
}

#[test]
fn split_top_level_requires_space_flank() {
    // `&&` not space-flanked (no surrounding spaces) is left alone.
    let parts = split_top_level("a&&b", "&&");
    assert_eq!(parts, vec!["a&&b"]);
}

#[test]
fn parse_composite_single_pattern_has_one_branch() {
    let comp = parse_composite_pattern("fn $NAME()", Language::Rust).unwrap();
    assert_eq!(comp.disjuncts.len(), 1);
    assert_eq!(comp.disjuncts[0].len(), 1);
    assert!(!comp.has_or());
}

#[test]
fn parse_composite_and_two_conjuncts() {
    let comp = parse_composite_pattern("struct $S && impl $S", Language::Rust).unwrap();
    assert_eq!(comp.disjuncts.len(), 1, "no OR → one disjunct");
    assert_eq!(comp.disjuncts[0].len(), 2, "AND → two conjuncts");
    assert!(!comp.has_or());
}

#[test]
fn parse_composite_or_two_disjuncts() {
    let comp = parse_composite_pattern("interface $N || class $N", Language::TypeScript).unwrap();
    assert_eq!(comp.disjuncts.len(), 2);
    assert!(comp.has_or());
}

#[test]
fn parse_composite_and_or_precedence() {
    // `a && b || c && d` → (a && b) || (c && d). `&&` binds tighter.
    let comp = parse_composite_pattern("fn $A && fn $B || fn $C && fn $D", Language::Rust).unwrap();
    assert_eq!(comp.disjuncts.len(), 2, "two OR branches");
    assert_eq!(comp.disjuncts[0].len(), 2, "left branch has 2 ANDs");
    assert_eq!(comp.disjuncts[1].len(), 2, "right branch has 2 ANDs");
}

#[test]
fn captures_agree_when_shared_name_matches() {
    let c1 = vec![("S".to_string(), "Foo".to_string())];
    let c2 = vec![("S".to_string(), "Foo".to_string())];
    assert!(captures_agree(&c1, &c2));
}

#[test]
fn captures_disagree_when_shared_name_differs() {
    let c1 = vec![("S".to_string(), "Foo".to_string())];
    let c2 = vec![("S".to_string(), "Bar".to_string())];
    assert!(!captures_agree(&c1, &c2));
}

#[test]
fn captures_agree_when_names_disjoint() {
    // No shared metavar → always agree, no constraint to enforce.
    let c1 = vec![("S".to_string(), "Foo".to_string())];
    let c2 = vec![("T".to_string(), "Bar".to_string())];
    assert!(captures_agree(&c1, &c2));
}

#[test]
fn and_intersects_on_back_referenced_capture() {
    // Bar has no impl → must not appear in the result.
    let source = "struct Foo;\nstruct Bar;\nimpl Foo { fn f(&self) {} }\n";
    let comp = parse_composite_pattern("struct $S && impl $S", Language::Rust).unwrap();
    let matches = find_matches_composite(source, &comp, "test.rs");
    // Foo passes the AND; Bar is filtered out by the back-ref.
    assert!(matches
        .iter()
        .any(|m| m.captures.iter().any(|(_, v)| v == "Foo")));
    assert!(!matches
        .iter()
        .any(|m| m.captures.iter().any(|(_, v)| v == "Bar")));
}

// --- 11.4 review H findings (post-Inc-7) ---

#[test]
fn and_anchor_uses_first_conjunct_line() {
    // Reported `line` is anchored at the first conjunct's match. The
    // direction matters: swapping the conjuncts swaps the reported
    // line — this pins the documented contract.
    let source = "struct Foo;\nimpl Foo { fn f(&self) {} }\n";

    let comp = parse_composite_pattern("struct $S && impl $S", Language::Rust).unwrap();
    let matches = find_matches_composite(source, &comp, "test.rs");
    assert!(!matches.is_empty());
    assert_eq!(matches[0].line, 1, "first conjunct is `struct` on line 1");

    let comp = parse_composite_pattern("impl $S && struct $S", Language::Rust).unwrap();
    let matches = find_matches_composite(source, &comp, "test.rs");
    assert!(!matches.is_empty());
    assert_eq!(matches[0].line, 2, "first conjunct is `impl` on line 2");
}

#[test]
fn matches_single_line_result_generic() {
    // The `extract_word_until` bonus fix in Inc 6 must also hold for
    // single-line patterns — only the multi-line fixture exercised
    // it through the full pipeline before this test.
    let source = "fn f(x: i32) -> Result<i32, String> { Ok(x) }\n";
    let pattern = parse_pattern("fn $N($$$) -> Result<$T, $E>", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    assert!(!matches.is_empty());
    let caps: HashMap<&str, &str> = matches[0]
        .captures
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(caps.get("T"), Some(&"i32"));
    assert_eq!(caps.get("E"), Some(&"String"));
}

#[test]
fn and_with_disjoint_metavars_both_must_match() {
    // `fn $A() && struct $B` — independent metavar names; AND just
    // requires both shapes present in the file, with no cross-capture
    // constraint.
    let comp = parse_composite_pattern("fn $A() && struct $B", Language::Rust).unwrap();

    // Both present → one composite match.
    let both = "fn foo() {}\nstruct Bar;\n";
    let matches = find_matches_composite(both, &comp, "test.rs");
    assert_eq!(
        matches.len(),
        1,
        "both shapes present must match exactly once"
    );
    let caps: HashMap<&str, &str> = matches[0]
        .captures
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(caps.get("A"), Some(&"foo"));
    assert_eq!(caps.get("B"), Some(&"Bar"));

    // Only struct → AND fails because `fn $A()` has no matches.
    let struct_only = "struct Bar;\n";
    assert!(find_matches_composite(struct_only, &comp, "test.rs").is_empty());

    // Only fn → AND fails because `struct $B` has no matches.
    let fn_only = "fn foo() {}\n";
    assert!(find_matches_composite(fn_only, &comp, "test.rs").is_empty());
}

#[test]
fn parse_composite_empty_middle_conjunct_errors() {
    // Two consecutive space-flanked `&&` produce an empty middle
    // conjunct. `parse_pattern` on the empty leaf must bail with
    // the existing empty-pattern message — pinned so tooling that
    // matches on the error text doesn't break on a future rewording.
    let err = parse_composite_pattern("fn $A && && fn $B", Language::Rust).unwrap_err();
    assert!(
        err.to_string().contains("empty pattern"),
        "expected 'empty pattern' in error, got: {err}"
    );
}

#[test]
fn parse_composite_empty_middle_disjunct_errors() {
    // Same logic for `||`.
    let err = parse_composite_pattern("fn $A || || fn $B", Language::Rust).unwrap_err();
    assert!(
        err.to_string().contains("empty pattern"),
        "expected 'empty pattern' in error, got: {err}"
    );
}

#[test]
fn or_takes_union_of_disjuncts() {
    let source = "interface Animal {}\nclass Dog {}\nclass Cat {}\nfunction helper() {}\n";
    let comp = parse_composite_pattern("interface $N || class $N", Language::TypeScript).unwrap();
    let matches = find_matches_composite(source, &comp, "test.ts");
    let names: std::collections::HashSet<String> = matches
        .iter()
        .flat_map(|m| m.captures.iter().map(|(_, v)| v.clone()))
        .collect();
    // `helper` is a function — neither disjunct should pick it up.
    assert!(names.contains("Animal"));
    assert!(names.contains("Dog"));
    assert!(names.contains("Cat"));
    assert!(!names.contains("helper"));
}

#[test]
fn parse_segments() {
    let pattern = parse_pattern("fn $NAME($$$) -> Result", Language::Rust).unwrap();
    assert_eq!(pattern.segments.len(), 5);
    assert!(matches!(pattern.segments[0], Segment::Literal(ref s) if s == "fn "));
    assert!(matches!(pattern.segments[1], Segment::Capture(ref s) if s == "NAME"));
    assert!(matches!(pattern.segments[2], Segment::Literal(ref s) if s == "("));
    assert!(matches!(pattern.segments[3], Segment::Ellipsis));
    assert!(matches!(pattern.segments[4], Segment::Literal(ref s) if s == ") -> Result"));
}

#[test]
fn named_ellipsis_tracks_brace_nesting_post_h4() {
    // Post-H4: an `$$$BODY` that terminates on `}` matches the
    // *balancing* closer of the outer `{`, skipping nested
    // `{ ... }` blocks. Pre-H4 this asserted the opposite (BODY
    // truncated at the first inner `}`); see git history for the
    // old shape.
    let source = "class C {\n    void Run() { let inner = 1; }\n}\n";
    let pattern = parse_pattern("class $T { $$$BODY }", Language::TypeScript).unwrap();
    let matches = find_matches(source, &pattern, "test.ts");
    assert_eq!(matches.len(), 1, "one match expected");
    let body = matches[0]
        .captures
        .iter()
        .find(|(k, _)| k == "BODY")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    // The capture now spans the entire inner method including its
    // own closing `}`; only the class's balancing `}` is excluded.
    assert!(
        body.contains("void Run() { let inner = 1; }"),
        "BODY should include the nested method's closing `}}`. \
             Got: {body:?}",
    );
    // Sanity: the capture must contain exactly one `}` — the
    // inner method's closing brace. The class's balancing `}` is
    // consumed by the trailing pattern literal and must NOT be in
    // the capture.
    assert_eq!(
        body.matches('}').count(),
        1,
        "BODY must contain exactly one `}}` (the inner method's). \
             Got: {body:?}",
    );
}

// --- H4 (external-review v1.9.1): AST-aware ellipsis termination ---
//
// The next three tests RED on pre-H4 `main` and GREEN once
// `find_at_depth_zero` lands. They exercise the two failure
// modes called out by the reviewer:
//   (a) nested `{ ... }` inside `$$$BODY` truncates at the
//       inner `}`;
//   (b) a string literal containing `}` truncates at the
//       string-internal `}`.

#[test]
fn named_ellipsis_balances_nested_braces() {
    // Pre-H4: BODY truncates at the inner `}` of `Run()`.
    // Post-H4: BODY spans the whole class body including the
    // nested method's braces.
    let source = "class C {\n    void Run() { let inner = 1; }\n}\n";
    let pattern = parse_pattern("class $T { $$$BODY }", Language::TypeScript).unwrap();
    let matches = find_matches(source, &pattern, "test.ts");
    assert_eq!(matches.len(), 1, "one match expected, got {matches:?}");
    let body = matches[0]
        .captures
        .iter()
        .find(|(k, _)| k == "BODY")
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("BODY capture missing: {:?}", matches[0].captures));
    // Strict: the full method (including its `}`) must be in the
    // capture; only the class's balancing `}` is excluded.
    assert!(
        body.contains("void Run() { let inner = 1; }"),
        "BODY must include the nested method's closing `}}`. Got: {body:?}",
    );
}

#[test]
fn named_ellipsis_balances_nested_braces_with_auto_property() {
    // The exact case spec.toml's `csharp_class_body` fixture had
    // to dodge — `{ get; set; }` auto-properties. Post-H4 this
    // must capture both the field and the auto-property.
    let source = "public class UserService {\n    public int counter;\n    public string Label { get; set; }\n}\n";
    let pattern = parse_pattern("public class $NAME { $$$BODY }", Language::CSharp).unwrap();
    let matches = find_matches(source, &pattern, "test.cs");
    assert_eq!(matches.len(), 1, "one match expected, got {matches:?}");
    let body = matches[0]
        .captures
        .iter()
        .find(|(k, _)| k == "BODY")
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("BODY capture missing: {:?}", matches[0].captures));
    assert!(
        body.contains("public int counter;"),
        "BODY must include the field. Got: {body:?}",
    );
    assert!(
        body.contains("public string Label { get; set; }"),
        "BODY must include the auto-property (with its nested `{{ get; set; }}`). \
             Got: {body:?}",
    );
}

#[test]
fn named_ellipsis_skips_brace_inside_string() {
    // Pre-H4: the `}` inside the `"}"` string literal terminates
    // the ellipsis early, truncating BODY at the wrong place and
    // leaving the source's outer `}` unmatched against the
    // pattern's trailing literal. Post-H4: the in-string `}` is
    // ignored, BODY spans up to the real closing `}`.
    let source = "fn parse() {\n    let close = \"}\";\n    println!(\"{}\", close);\n}\n";
    let pattern = parse_pattern("fn $F() { $$$BODY }", Language::Rust).unwrap();
    let matches = find_matches(source, &pattern, "test.rs");
    assert_eq!(matches.len(), 1, "one match expected, got {matches:?}");
    let body = matches[0]
        .captures
        .iter()
        .find(|(k, _)| k == "BODY")
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("BODY capture missing: {:?}", matches[0].captures));
    assert!(
        body.contains("let close = \"}\";"),
        "BODY must include the string-literal-containing-brace line. Got: {body:?}",
    );
    assert!(
        body.contains("println!"),
        "BODY must reach past the string-literal line. Got: {body:?}",
    );
}

#[test]
fn anonymous_ellipsis_balances_braces() {
    // `$$$` (anonymous Ellipsis) shares the same forward-scan
    // logic as `$$$NAME` and must benefit from the same fix.
    // Pre-H4 the surrounding match fails because `$$$` truncates
    // early and the trailing `}` of the pattern can't line up
    // with the outer source `}`.
    let source = "class C {\n    void Run() { let inner = 1; }\n}\n";
    let pattern = parse_pattern("class $T { $$$ }", Language::TypeScript).unwrap();
    let matches = find_matches(source, &pattern, "test.ts");
    assert!(
        !matches.is_empty(),
        "anonymous $$$ must match the full class body — pre-H4 \
             it truncated inside Run() and broke the trailing `}}`. \
             Got: {matches:?}",
    );
}

// --- H4 unit tests for the new depth-aware finder ---

#[test]
fn find_at_depth_zero_empty_needle_returns_zero() {
    // Contract: empty needle is trivially found at offset 0.
    assert_eq!(find_at_depth_zero("anything", ""), Some(0));
}

#[test]
fn find_at_depth_zero_no_brackets_falls_back_to_first_hit() {
    // Without any brackets the walker should behave like `str::find`.
    assert_eq!(find_at_depth_zero("hello world }", "}"), Some(12));
}

#[test]
fn find_at_depth_zero_simple_close_at_depth_zero() {
    // The first `}` IS at depth 0 (nothing opened it); return its
    // offset directly. Previous literal segment is assumed to have
    // consumed the matching opener.
    assert_eq!(find_at_depth_zero("}", "}"), Some(0));
    assert_eq!(find_at_depth_zero("   }", "}"), Some(3));
}

#[test]
fn find_at_depth_zero_skips_nested_braces() {
    // The inner `}` of `{ x; }` is at depth 1 and must NOT match;
    // the outer `}` is at depth 0 and is the balancing closer.
    let s = "{ x; } }";
    assert_eq!(find_at_depth_zero(s, "}"), Some(s.len() - 1));
}

#[test]
fn find_at_depth_zero_skips_brace_inside_double_string() {
    // The `}` inside `"}"` is in a string region and must not be
    // counted. The trailing `}` (after the string closes) is the
    // real depth-0 closer.
    let s = "  \"}\"  }";
    let idx = find_at_depth_zero(s, "}").expect("must find outer `}`");
    assert_eq!(&s[idx..], "}");
}

#[test]
fn find_at_depth_zero_handles_escaped_quote_and_backslash() {
    // `"\"}\\"` — escaped quote keeps us inside the string;
    // the embedded `}` must be ignored.
    let s = r#""\"}" }"#;
    let idx = find_at_depth_zero(s, "}").expect("must find outer `}`");
    assert_eq!(&s[idx..], "}");
    // `"\\"` (two backslashes — one literal backslash) followed by
    // a `}` outside the string: the `\\` consumes both `\`s so the
    // closing `"` works, then we exit the string before reading the
    // outer `}`.
    let s2 = r#""\\" }"#;
    let idx2 = find_at_depth_zero(s2, "}").expect("must find outer `}`");
    assert_eq!(&s2[idx2..], "}");
}

#[test]
fn find_at_depth_zero_multi_char_needle() {
    // Needle of more than one byte must match as a contiguous
    // span and only at depth 0 / outside strings. The haystack
    // `{ } } else { }` has TWO `}` bytes that could (textually)
    // start a `} else` match:
    //   - byte 2: check fires while depth==1 (the `{` at byte 0
    //     bumped it; the check happens BEFORE consuming this
    //     `}`), so the depth-0 guard rejects it.
    //   - byte 4: check fires at depth==0, but `&s[4..]` is
    //     `} else { }` — wait, is it `}` or ` `? Let me recount.
    //     s = `{ } } else { }` → bytes:
    //          0 `{`, 1 ` `, 2 `}`, 3 ` `, 4 `}`, 5 ` `, 6 `e`, …
    //     At byte 4 depth is 0 and `&s[4..]` = `} else { }`,
    //     which DOES start with `"} else"`. So the match fires
    //     at byte 4.
    let s = "{ } } else { }";
    let idx = find_at_depth_zero(s, "} else").expect("must find `} else`");
    assert_eq!(idx, 4, "match must land on the depth-0 `}}` at byte 4");
    assert_eq!(&s[idx..idx + 6], "} else");
}

#[test]
fn find_at_depth_zero_returns_none_when_needle_missing() {
    assert_eq!(find_at_depth_zero("no closer here", "}"), None);
    // Even when the haystack has matching brackets, a missing
    // needle still yields None.
    assert_eq!(find_at_depth_zero("{ } { }", "X"), None);
}

#[test]
fn find_at_depth_zero_balances_parens_and_brackets() {
    // Depth tracking covers `() {} []` together. A `}` inside
    // a `(...)` parenthesised expression should still be ignored
    // when the needle is `}` at top level — pathological case but
    // pins the symmetry.
    let s = "({ }) }";
    let idx = find_at_depth_zero(s, "}").expect("must find outer `}`");
    assert_eq!(&s[idx..], "}");
}

#[test]
fn find_at_depth_zero_saturates_underflow() {
    // A haystack starting with an unbalanced closer must not
    // underflow `depth`. The closer itself is at depth 0 and the
    // walker must keep going past it without panicking.
    let s = "}} X";
    // Needle `X` lives at depth 0 after the two stray closers.
    let idx = find_at_depth_zero(s, "X").expect("must find `X` past unbalanced closers");
    assert_eq!(&s[idx..idx + 1], "X");
}
