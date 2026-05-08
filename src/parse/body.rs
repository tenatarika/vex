use anyhow::{Context, Result};
use tree_sitter::Parser;

use super::language::Language;

/// Extract the body of a symbol definition using indentation heuristic.
/// Fallback for languages without tree-sitter support.
pub fn extract_symbol_body(
    content: &str,
    symbol_line: usize,
    context_lines: usize,
) -> Result<SymbolBody> {
    if symbol_line == 0 {
        anyhow::bail!("symbol_line must be >= 1");
    }
    let lines: Vec<&str> = content.lines().collect();
    let target_row = symbol_line - 1;

    if target_row >= lines.len() {
        anyhow::bail!(
            "line {symbol_line} out of range (file has {} lines)",
            lines.len()
        );
    }

    let (start, end) = find_definition_range_indent(&lines, target_row);

    let ctx_start = start.saturating_sub(context_lines);
    let ctx_end = (end + context_lines).min(lines.len());

    let body: String = lines[ctx_start..ctx_end].join("\n");

    Ok(SymbolBody {
        body,
        start_line: ctx_start + 1,
        end_line: ctx_end,
        lines: ctx_end - ctx_start,
    })
}

/// Extract symbol body using tree-sitter for precise node boundaries.
pub fn extract_symbol_body_ts(
    content: &str,
    symbol_line: usize,
    lang: Language,
    context_lines: usize,
) -> Result<SymbolBody> {
    if symbol_line == 0 {
        anyhow::bail!("symbol_line must be >= 1");
    }

    let mut parser = Parser::new();
    let ts_lang = lang.ts_language();
    parser.set_language(&ts_lang).context("set language")?;

    let tree = parser
        .parse(content, None)
        .context("tree-sitter parse failed")?;

    let target_row = symbol_line - 1;
    let root = tree.root_node();
    let mut best_node = None;

    find_definition_node(root, target_row, &mut best_node);

    let lines: Vec<&str> = content.lines().collect();

    let (start, end) = if let Some(node) = best_node {
        (node.start_position().row, node.end_position().row + 1)
    } else {
        // Fallback to indentation heuristic
        find_definition_range_indent(&lines, target_row)
    };

    let ctx_start = start.saturating_sub(context_lines);
    let ctx_end = (end + context_lines).min(lines.len());

    let body: String = lines[ctx_start..ctx_end].join("\n");

    Ok(SymbolBody {
        body,
        start_line: ctx_start + 1,
        end_line: ctx_end,
        lines: ctx_end - ctx_start,
    })
}

#[derive(Debug, Clone)]
pub struct SymbolBody {
    pub body: String,
    pub start_line: usize,
    pub end_line: usize,
    pub lines: usize,
}

/// Extract a markdown section body: from heading line to next heading of same or higher level.
pub fn extract_heading_body(
    content: &str,
    heading_line: usize,
    context_lines: usize,
) -> Result<SymbolBody> {
    if heading_line == 0 {
        anyhow::bail!("heading_line must be >= 1");
    }
    let lines: Vec<&str> = content.lines().collect();
    let target_row = heading_line - 1;

    if target_row >= lines.len() {
        anyhow::bail!(
            "line {heading_line} out of range (file has {} lines)",
            lines.len()
        );
    }

    // Determine heading level (count leading '#' characters)
    let heading_level = lines[target_row].chars().take_while(|&c| c == '#').count();
    if heading_level == 0 {
        anyhow::bail!("line {heading_line} is not a heading (no leading '#')");
    }

    // Scan forward until next heading of same or higher level (fewer or equal '#').
    // Skip '#' lines inside fenced code blocks (``` or ~~~).
    // Note: only single-level fence tracking (no nested fences).
    let mut end = target_row + 1;
    let mut in_fence = false;
    for (i, line) in lines.iter().enumerate().skip(target_row + 1) {
        let trimmed = line.trim_start();
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if is_fence {
            in_fence = !in_fence;
        }
        if !in_fence && !is_fence && trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|&c| c == '#').count();
            if level <= heading_level {
                break;
            }
        }
        end = i + 1;
    }

    // Trim trailing blank lines
    while end > target_row + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }

    let ctx_start = target_row.saturating_sub(context_lines);
    let ctx_end = (end + context_lines).min(lines.len());

    let body: String = lines[ctx_start..ctx_end].join("\n");

    Ok(SymbolBody {
        body,
        start_line: ctx_start + 1,
        end_line: ctx_end,
        lines: ctx_end - ctx_start,
    })
}

/// Find the best definition node starting at target_row.
/// Prefers the **largest** multi-line node starting exactly at target_row,
/// which corresponds to the outermost definition (function_item, class_declaration, etc.)
/// rather than an inner block or signature.
fn find_definition_node<'a>(
    node: tree_sitter::Node<'a>,
    target_row: usize,
    best: &mut Option<tree_sitter::Node<'a>>,
) {
    // Skip root node (source_file/program/module) — it spans the entire file
    let is_candidate = node.start_position().row == target_row
        && node.child_count() > 0
        && node.parent().is_some();

    if is_candidate {
        let span = node.end_position().row - node.start_position().row;
        if span > 0 {
            match best {
                Some(prev) => {
                    let prev_span = prev.end_position().row - prev.start_position().row;
                    if span > prev_span {
                        *best = Some(node);
                    }
                }
                None => *best = Some(node),
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.start_position().row <= target_row && child.end_position().row >= target_row {
            find_definition_node(child, target_row, best);
        }
    }
}

/// Indentation-based heuristic: find definition range from start_row to first dedent.
/// Works for Python and as a fallback for any language.
fn find_definition_range_indent(lines: &[&str], start_row: usize) -> (usize, usize) {
    if start_row >= lines.len() {
        return (start_row, (start_row + 1).min(lines.len()));
    }

    let base_indent = lines[start_row].len() - lines[start_row].trim_start().len();
    let mut end = start_row + 1;

    for (i, line) in lines.iter().enumerate().skip(start_row + 1) {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            end = i + 1;
            continue;
        }

        let indent = line.len() - line.trim_start().len();
        if indent <= base_indent {
            break;
        }

        end = i + 1;
    }

    // Trim trailing empty lines
    while end > start_row + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }

    (start_row, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_python_function() {
        let src =
            "def hello():\n    print('hello')\n    print('world')\n\ndef other():\n    pass\n";
        let body = extract_symbol_body(src, 1, 0).unwrap();
        assert_eq!(body.start_line, 1);
        assert_eq!(body.lines, 3);
        assert!(body.body.contains("hello"));
        assert!(body.body.contains("world"));
        assert!(!body.body.contains("other"));
    }

    #[test]
    fn extract_rust_function() {
        let src =
            "fn main() {\n    println!(\"hello\");\n}\n\nfn other() {\n    println!(\"other\");\n}\n";
        let body = extract_symbol_body_ts(src, 1, Language::Rust, 0).unwrap();
        assert_eq!(body.start_line, 1);
        assert!(body.body.contains("hello"));
        assert!(!body.body.contains("other"));
    }

    #[test]
    fn extract_with_context() {
        let src = "# comment\ndef hello():\n    pass\n\ndef other():\n    pass\n";
        let body = extract_symbol_body(src, 2, 1).unwrap();
        assert_eq!(body.start_line, 1);
        assert!(body.body.contains("comment"));
        assert!(body.body.contains("hello"));
    }

    #[test]
    fn extract_ts_class() {
        let src = "class Foo {\n  bar() {\n    return 1;\n  }\n\n  baz() {\n    return 2;\n  }\n}\n\nclass Other {}\n";
        let body = extract_symbol_body_ts(src, 1, Language::TypeScript, 0).unwrap();
        assert!(body.body.contains("bar"));
        assert!(body.body.contains("baz"));
        assert!(!body.body.contains("Other"));
    }

    #[test]
    fn extract_rust_struct_ts() {
        let src = "pub struct Foo {\n    pub x: i32,\n    pub y: i32,\n}\n\nimpl Foo {\n    fn new() -> Self { Self { x: 0, y: 0 } }\n}\n";
        let body = extract_symbol_body_ts(src, 1, Language::Rust, 0).unwrap();
        assert!(body.body.contains("pub x"));
        assert!(body.body.contains("pub y"));
        assert!(!body.body.contains("impl"));
    }

    #[test]
    fn rejects_zero_line() {
        let result = extract_symbol_body("fn main() {}", 0, 0);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_line_past_end() {
        let result = extract_symbol_body("fn main() {}", 100, 0);
        assert!(result.is_err());
    }

    #[test]
    fn rust_fn_with_braces_in_string() {
        let src = "fn example() {\n    let s = \"} not a real brace\";\n    real_code();\n}\n\nfn other() {}\n";
        let body = extract_symbol_body_ts(src, 1, Language::Rust, 0).unwrap();
        assert!(body.body.contains("real_code"));
        assert!(!body.body.contains("other"));
    }

    #[test]
    fn heading_body_basic() {
        let src = "# Title\n\nIntro.\n\n## Section A\n\nContent A.\n\n## Section B\n\nContent B.\n";
        let body = extract_heading_body(src, 5, 0).unwrap();
        assert!(body.body.contains("Content A"));
        assert!(!body.body.contains("Content B"));
    }

    #[test]
    fn heading_body_to_eof() {
        let src = "# Title\n\n## Last Section\n\nFinal content.\n";
        let body = extract_heading_body(src, 3, 0).unwrap();
        assert!(body.body.contains("Final content"));
    }

    #[test]
    fn heading_body_skips_fenced_hash() {
        let src = "## Setup\n\n```bash\n# this is a comment\necho hello\n```\n\n## Next\n";
        let body = extract_heading_body(src, 1, 0).unwrap();
        assert!(body.body.contains("# this is a comment"));
        assert!(!body.body.contains("Next"));
    }

    #[test]
    fn heading_body_non_heading_line_errors() {
        let src = "Just some text\n## Heading\n";
        let result = extract_heading_body(src, 1, 0);
        assert!(result.is_err());
    }

    #[test]
    fn heading_body_includes_subheadings() {
        let src = "## Parent\n\nText.\n\n### Child\n\nChild text.\n\n## Sibling\n";
        let body = extract_heading_body(src, 1, 0).unwrap();
        assert!(body.body.contains("Child"));
        assert!(body.body.contains("Child text"));
        assert!(!body.body.contains("Sibling"));
    }
}
