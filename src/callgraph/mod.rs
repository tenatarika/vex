use std::path::Path;

use anyhow::{Context, Result};
use rayon::prelude::*;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::parse::language::Language;

/// A caller→callee relationship found in source code.
#[derive(Debug, Clone)]
pub struct CallMatch {
    /// Function that contains the call (caller) or is being called (callee)
    pub name: String,
    pub path: String,
    pub line: usize,
}

/// Find all functions that call `target_name`.
pub fn find_callers(
    root: &Path,
    target_name: &str,
    limit: usize,
    excludes: &[String],
) -> Result<Vec<CallMatch>> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files: Vec<_> = crate::util::walk::discover_source_files(&root, excludes)?
        .into_iter()
        .filter(|(_, lang)| callgraph_query(*lang).is_some())
        .collect();

    let matches: Vec<CallMatch> = files
        .par_iter()
        .flat_map(|(path, lang)| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            callers_in_source(&content, *lang, &rel, target_name)
        })
        .collect();

    Ok(matches.into_iter().take(limit).collect())
}

/// Find all functions called by `target_name`.
pub fn find_callees(
    root: &Path,
    target_name: &str,
    limit: usize,
    excludes: &[String],
) -> Result<Vec<CallMatch>> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files: Vec<_> = crate::util::walk::discover_source_files(&root, excludes)?
        .into_iter()
        .filter(|(_, lang)| callgraph_query(*lang).is_some())
        .collect();

    let matches: Vec<CallMatch> = files
        .par_iter()
        .flat_map(|(path, lang)| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            callees_in_source(&content, *lang, &rel, target_name)
        })
        .collect();

    Ok(matches.into_iter().take(limit).collect())
}

struct FnDef {
    name: String,
    line: usize,
    start_byte: usize,
    end_byte: usize,
}

struct Call {
    callee: String,
    line: usize,
    byte_offset: usize,
}

/// Find callers of `target` in a single source file.
fn callers_in_source(content: &str, lang: Language, path: &str, target: &str) -> Vec<CallMatch> {
    let (fns, calls) = match extract_callgraph(content, lang) {
        Some(r) => r,
        None => return Vec::new(),
    };

    // Find all calls to target
    let target_calls: Vec<&Call> = calls.iter().filter(|c| c.callee == target).collect();

    if target_calls.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for call in target_calls {
        // Find the innermost containing function
        if let Some(f) = fns
            .iter()
            .filter(|f| call.byte_offset >= f.start_byte && call.byte_offset < f.end_byte)
            .min_by_key(|f| f.end_byte - f.start_byte)
        {
            if seen.insert((f.name.as_str(), f.line)) {
                results.push(CallMatch {
                    name: f.name.clone(),
                    path: path.to_string(),
                    line: f.line,
                });
            }
        }
    }

    results
}

/// Find callees of `target` in a single source file.
fn callees_in_source(content: &str, lang: Language, path: &str, target: &str) -> Vec<CallMatch> {
    let (fns, calls) = match extract_callgraph(content, lang) {
        Some(r) => r,
        None => return Vec::new(),
    };

    // Find the target function definition
    let target_fn = match fns.iter().find(|f| f.name == target) {
        Some(f) => f,
        None => return Vec::new(),
    };

    // Find all calls within the target function's body
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for call in &calls {
        if call.byte_offset >= target_fn.start_byte
            && call.byte_offset < target_fn.end_byte
            && call.callee != target
            && seen.insert(call.callee.as_str())
        {
            results.push(CallMatch {
                name: call.callee.clone(),
                path: path.to_string(),
                line: call.line,
            });
        }
    }

    results
}

/// Extract function definitions and call expressions from source.
fn extract_callgraph(content: &str, lang: Language) -> Option<(Vec<FnDef>, Vec<Call>)> {
    let query_src = callgraph_query(lang)?;
    let ts_lang = lang.ts_language();

    let query = Query::new(&ts_lang, query_src).ok()?;

    let mut parser = Parser::new();
    parser.set_language(&ts_lang).ok()?;
    let tree = parser.parse(content, None)?;

    let fn_name_idx = query.capture_index_for_name("fn.name")?;
    let fn_body_idx = query.capture_index_for_name("fn.decl")?;
    let call_name_idx = query.capture_index_for_name("call.name")?;

    let mut cursor = QueryCursor::new();
    let mut query_matches = cursor.matches(&query, tree.root_node(), content.as_bytes());

    let mut fns = Vec::new();
    let mut calls = Vec::new();

    while let Some(m) = query_matches.next() {
        let mut fn_name = None;
        let mut fn_body_range = None;
        let mut fn_line = 0;
        let mut call_name = None;
        let mut call_line = 0;
        let mut call_offset = 0;

        for capture in m.captures {
            let text = &content[capture.node.byte_range()];
            if capture.index == fn_name_idx {
                fn_name = Some(text);
                fn_line = capture.node.start_position().row + 1;
            } else if capture.index == fn_body_idx {
                fn_body_range = Some((capture.node.start_byte(), capture.node.end_byte()));
            } else if capture.index == call_name_idx {
                call_name = Some(text);
                call_line = capture.node.start_position().row + 1;
                call_offset = capture.node.start_byte();
            }
        }

        if let (Some(name), Some((start, end))) = (fn_name, fn_body_range) {
            fns.push(FnDef {
                name: name.to_string(),
                line: fn_line,
                start_byte: start,
                end_byte: end,
            });
        }

        if let Some(callee) = call_name {
            calls.push(Call {
                callee: callee.to_string(),
                line: call_line,
                byte_offset: call_offset,
            });
        }
    }

    Some((fns, calls))
}

fn callgraph_query(lang: Language) -> Option<&'static str> {
    match lang {
        Language::Rust => Some(
            r#"
            (function_item name: (identifier) @fn.name) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (scoped_identifier
                name: (identifier) @call.name))

            (call_expression
              function: (field_expression
                field: (field_identifier) @call.name))
            "#,
        ),
        Language::Python => Some(
            r#"
            (function_definition name: (identifier) @fn.name) @fn.decl

            (call function: (identifier) @call.name)

            (call function: (attribute
              attribute: (identifier) @call.name))
            "#,
        ),
        Language::Java => Some(
            r#"
            (method_declaration name: (identifier) @fn.name) @fn.decl
            (constructor_declaration name: (identifier) @fn.name) @fn.decl

            (method_invocation name: (identifier) @call.name)
            "#,
        ),
        Language::TypeScript => Some(
            r#"
            (function_declaration name: (identifier) @fn.name) @fn.decl
            (method_definition name: (property_identifier) @fn.name) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (member_expression
                property: (property_identifier) @call.name))
            "#,
        ),
        Language::Go => Some(
            r#"
            (function_declaration name: (identifier) @fn.name) @fn.decl
            (method_declaration name: (field_identifier) @fn.name) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (selector_expression
                field: (field_identifier) @call.name))
            "#,
        ),
        Language::Cpp => Some(
            r#"
            (function_definition
              declarator: (function_declarator
                declarator: (identifier) @fn.name)) @fn.decl

            (function_definition
              declarator: (function_declarator
                declarator: (qualified_identifier
                  name: (identifier) @fn.name))) @fn.decl

            (call_expression
              function: (identifier) @call.name)

            (call_expression
              function: (qualified_identifier
                name: (identifier) @call.name))

            (call_expression
              function: (field_expression
                field: (field_identifier) @call.name))
            "#,
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callers(src: &str, lang: Language, target: &str) -> Vec<CallMatch> {
        callers_in_source(src, lang, "test", target)
    }

    fn callees(src: &str, lang: Language, target: &str) -> Vec<CallMatch> {
        callees_in_source(src, lang, "test", target)
    }

    #[test]
    fn rust_callers() {
        let src = r#"
fn process() {
    validate();
    transform();
}

fn run() {
    process();
    cleanup();
}

fn validate() {}
fn transform() {}
fn cleanup() {}
"#;
        let matches = callers(src, Language::Rust, "process");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "run");
    }

    #[test]
    fn rust_callees() {
        let src = r#"
fn process() {
    validate();
    transform();
    log_result();
}

fn validate() {}
fn transform() {}
fn log_result() {}
"#;
        let matches = callees(src, Language::Rust, "process");
        assert_eq!(matches.len(), 3);
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"validate"));
        assert!(names.contains(&"transform"));
        assert!(names.contains(&"log_result"));
    }

    #[test]
    fn rust_method_calls() {
        let src = r#"
fn process(data: &Data) {
    data.validate();
    data.transform();
}
"#;
        let matches = callees(src, Language::Rust, "process");
        assert_eq!(matches.len(), 2);
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"validate"));
        assert!(names.contains(&"transform"));
    }

    #[test]
    fn python_callers_and_callees() {
        let src = r#"
def process():
    validate()
    transform()

def run():
    process()
    cleanup()

def validate():
    pass

def transform():
    pass

def cleanup():
    pass
"#;
        let caller_matches = callers(src, Language::Python, "process");
        assert_eq!(caller_matches.len(), 1);
        assert_eq!(caller_matches[0].name, "run");

        let callee_matches = callees(src, Language::Python, "process");
        assert_eq!(callee_matches.len(), 2);
        let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"validate"));
        assert!(names.contains(&"transform"));
    }

    #[test]
    fn go_callers() {
        let src = r#"
package main

func Process() {
    Validate()
}

func Run() {
    Process()
}

func Validate() {}
"#;
        let matches = callers(src, Language::Go, "Process");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "Run");
    }

    #[test]
    fn no_callers_returns_empty() {
        let src = "fn main() {}\nfn unused() {}";
        let matches = callers(src, Language::Rust, "unused");
        assert!(matches.is_empty());
    }

    #[test]
    fn callees_excludes_self_recursion() {
        let src = r#"
fn factorial(n: u32) -> u32 {
    if n == 0 { 1 } else { n * factorial(n - 1) }
}
"#;
        let matches = callees(src, Language::Rust, "factorial");
        // factorial calls itself — excluded by the callee != target filter
        assert!(matches.is_empty());
    }

    #[test]
    fn typescript_class_methods() {
        let src = r#"
class Service {
    process() {
        this.validate();
        this.transform();
    }

    validate() {}
    transform() {}
}

function main() {
    const svc = new Service();
    svc.process();
}
"#;
        let caller_matches = callers(src, Language::TypeScript, "process");
        assert_eq!(caller_matches.len(), 1);
        assert_eq!(caller_matches[0].name, "main");

        let callee_matches = callees(src, Language::TypeScript, "process");
        assert_eq!(callee_matches.len(), 2);
        let names: Vec<&str> = callee_matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"validate"));
        assert!(names.contains(&"transform"));
    }

    #[test]
    fn unsupported_language_returns_empty() {
        let matches = callers("CREATE TABLE foo (id INT);", Language::Sql, "foo");
        assert!(matches.is_empty());
    }
}
