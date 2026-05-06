//! AST-aware pattern matcher.
//!
//! Instead of parsing the pattern as code (fragile with incomplete snippets),
//! we do text-matching against the source text of each AST node.
//!
//! Pattern syntax:
//! - Literal text is matched exactly
//! - `$NAME` matches a single identifier/expression and captures it
//! - `$_` matches anything without capturing
//! - `$$$` matches zero or more characters (ellipsis)

use anyhow::Result;
use tree_sitter::{Node, Parser};

use crate::parse::language::Language;

use super::PatternMatch;

/// Compiled pattern — a list of segments (literal or metavar).
pub struct PatternTree {
    segments: Vec<Segment>,
    pub lang: Language,
}

#[derive(Debug)]
enum Segment {
    Literal(String),
    Capture(String), // $NAME
    Wildcard,        // $_
    Ellipsis,        // $$$
}

/// Parse the pattern string into segments.
pub fn parse_pattern(pattern: &str, lang: Language) -> Result<PatternTree> {
    let mut segments = Vec::new();
    let mut chars = pattern.chars().peekable();
    let mut literal = String::new();

    while let Some(&c) = chars.peek() {
        if c == '$' {
            if !literal.is_empty() {
                segments.push(Segment::Literal(literal.clone()));
                literal.clear();
            }
            chars.next(); // consume $

            // Check for $$$
            if chars.peek() == Some(&'$') {
                chars.next();
                if chars.peek() == Some(&'$') {
                    chars.next();
                    segments.push(Segment::Ellipsis);
                    continue;
                }
                // Just $$ — treat as literal
                literal.push_str("$$");
                continue;
            }

            // Check for $_ or $NAME
            if chars.peek() == Some(&'_') {
                chars.next();
                // Ensure it's $_ not $_foo
                if !chars.peek().is_some_and(|c| c.is_alphanumeric()) {
                    segments.push(Segment::Wildcard);
                    continue;
                }
                literal.push('_');
                continue;
            }

            // $NAME — collect identifier
            let mut name = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_alphanumeric() || c == '_' {
                    name.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            if name.is_empty() {
                literal.push('$');
            } else {
                segments.push(Segment::Capture(name));
            }
        } else {
            literal.push(c);
            chars.next();
        }
    }

    if !literal.is_empty() {
        segments.push(Segment::Literal(literal));
    }

    Ok(PatternTree { segments, lang })
}

/// Find all AST nodes in `source` whose text matches the pattern.
pub fn find_matches(source: &str, pattern: &PatternTree, file_path: &str) -> Vec<PatternMatch> {
    let mut parser = Parser::new();
    let ts_lang = get_ts_language(pattern.lang);
    if parser.set_language(&ts_lang).is_err() {
        return Vec::new();
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut matches = Vec::new();
    visit_all(
        tree.root_node(),
        source,
        &pattern.segments,
        file_path,
        &mut matches,
    );

    // Dedup by (path, line) — parent and child nodes can both match
    let mut seen = std::collections::HashSet::new();
    matches.retain(|m| seen.insert((m.path.clone(), m.line)));
    matches
}

fn visit_all(
    node: Node,
    source: &str,
    segments: &[Segment],
    file_path: &str,
    matches: &mut Vec<PatternMatch>,
) {
    // Try to match this node's text against the pattern
    let node_src = safe_node_text(node, source);

    // Only try "meaningful" nodes — skip root, blocks, and very large nodes
    let skip_kinds = [
        "source_file",
        "module",
        "program",
        "translation_unit",
        "block",
        "statement_block",
        "declaration_list",
    ];
    if node.is_named() && !node_src.is_empty() && !skip_kinds.contains(&node.kind()) {
        if let Some(captures) = try_match(node_src, segments) {
            let first_line = node_src.lines().next().unwrap_or("").to_string();
            matches.push(PatternMatch {
                path: file_path.to_string(),
                line: node.start_position().row + 1,
                matched_text: first_line,
                captures,
            });
        }
    }

    // Visit children
    let cursor = &mut node.walk();
    for child in node.children(cursor) {
        visit_all(child, source, segments, file_path, matches);
    }
}

/// Try to match text against pattern segments. Returns captures on success.
fn try_match(text: &str, segments: &[Segment]) -> Option<Vec<(String, String)>> {
    let text = text.trim();
    let mut pos = 0;
    let mut captures = Vec::new();

    for (i, seg) in segments.iter().enumerate() {
        match seg {
            Segment::Literal(lit) => {
                let lit_trimmed = lit.trim();
                if lit_trimmed.is_empty() {
                    // Skip whitespace-only literals
                    while pos < text.len() && text.as_bytes()[pos].is_ascii_whitespace() {
                        pos += 1;
                    }
                    continue;
                }
                // Skip whitespace in source
                while pos < text.len() && text.as_bytes()[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                // Match literal
                if text[pos..].starts_with(lit_trimmed) {
                    pos += lit_trimmed.len();
                } else {
                    return None;
                }
            }
            Segment::Capture(name) => {
                // Skip whitespace
                while pos < text.len() && text.as_bytes()[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                // Capture a "word" — identifier or balanced expression
                let word = extract_word(&text[pos..]);
                if word.is_empty() {
                    return None;
                }
                captures.push((name.clone(), word.to_string()));
                pos += word.len();
            }
            Segment::Wildcard => {
                // Skip whitespace
                while pos < text.len() && text.as_bytes()[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                let word = extract_word(&text[pos..]);
                if word.is_empty() {
                    return None;
                }
                pos += word.len();
            }
            Segment::Ellipsis => {
                // $$$ — match everything until the next literal segment or end
                if let Some(next_lit) = find_next_literal(&segments[i + 1..]) {
                    // Find where the next literal starts
                    if let Some(idx) = text[pos..].find(next_lit.trim()) {
                        pos += idx;
                    } else {
                        return None;
                    }
                } else {
                    // No more segments — $$$ matches the rest
                    pos = text.len();
                }
            }
        }
    }

    Some(captures)
}

/// Extract a "word" from the current position — an identifier or balanced parens.
fn extract_word(s: &str) -> &str {
    if s.is_empty() {
        return "";
    }

    let bytes = s.as_bytes();

    // If starts with a bracket, find the balanced closing bracket
    if bytes[0] == b'(' || bytes[0] == b'{' || bytes[0] == b'[' {
        let open = bytes[0];
        let close = match open {
            b'(' => b')',
            b'{' => b'}',
            b'[' => b']',
            _ => unreachable!(),
        };
        let mut depth = 1;
        let mut i = 1;
        while i < bytes.len() && depth > 0 {
            if bytes[i] == open {
                depth += 1;
            } else if bytes[i] == close {
                depth -= 1;
            }
            i += 1;
        }
        return &s[..i];
    }

    // Otherwise, collect an identifier (alphanumeric + underscore + :: + <> + *)
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric()
            || b == b'_'
            || b == b'<'
            || b == b'>'
            || b == b'*'
            || b == b'&'
            || b == b'.'
            || (b == b':' && i + 1 < bytes.len() && bytes[i + 1] == b':')
            || (b == b':' && i > 0 && bytes[i - 1] == b':')
        {
            i += 1;
        } else {
            break;
        }
    }
    &s[..i]
}

fn find_next_literal(segments: &[Segment]) -> Option<&str> {
    for seg in segments {
        if let Segment::Literal(lit) = seg {
            if !lit.trim().is_empty() {
                return Some(lit);
            }
        }
    }
    None
}

fn safe_node_text<'a>(node: Node, source: &'a str) -> &'a str {
    let start = node.start_byte();
    let mut end = node.end_byte().min(source.len());
    while end > start && !source.is_char_boundary(end) {
        end -= 1;
    }
    &source[start..end]
}

fn get_ts_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        Language::Swift => tree_sitter_swift::LANGUAGE.into(),
        Language::Kotlin | Language::TypeScript => tree_sitter_rust::LANGUAGE.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
