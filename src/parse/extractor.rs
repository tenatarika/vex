use std::collections::HashSet;

use anyhow::{Context, Result};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, QueryCursor};

use super::language::Language;
use super::queries;
use crate::index::symbols::{ParsedRef, ParsedSymbol, SymbolKind};

/// Extract symbols and AST-based import references in a single tree-sitter parse.
pub fn extract_symbols_and_imports(
    content: &str,
    lang: Language,
) -> Result<(Vec<ParsedSymbol>, Vec<ParsedRef>)> {
    let query = match queries::get_query(lang) {
        Some(q) => q,
        None => return Ok((Vec::new(), Vec::new())),
    };

    let mut parser = Parser::new();
    let ts_lang = lang.ts_language();
    parser.set_language(&ts_lang).context("set language")?;

    let tree = parser
        .parse(content, None)
        .context("tree-sitter parse failed")?;

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), content.as_bytes());

    let mut symbols = Vec::new();
    let mut imports = Vec::new();

    while let Some(m) = matches.next() {
        for capture in m.captures {
            let capture_name = &query.capture_names()[capture.index as usize];
            let node = capture.node;
            let name = node.utf8_text(content.as_bytes()).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let line = node.start_position().row + 1;

            if *capture_name == "import.name" {
                let context = content.lines().nth(line - 1).map(|l| l.trim().to_string());
                imports.push(ParsedRef {
                    name: name.to_string(),
                    line,
                    context,
                });
                continue;
            }

            let kind = match *capture_name {
                "fn.name" => SymbolKind::Function,
                "struct.name" => SymbolKind::Struct,
                "enum.name" => SymbolKind::Enum,
                "trait.name" => SymbolKind::Trait,
                "impl.type" => SymbolKind::Impl,
                "impl.method" => SymbolKind::Method,
                "class.name" => SymbolKind::Class,
                "interface.name" => SymbolKind::Interface,
                "type.name" => SymbolKind::TypeAlias,
                "property.name" => SymbolKind::Property,
                "const.name" => SymbolKind::Constant,
                "heading.name" => SymbolKind::Heading,
                _ => continue,
            };

            let parent = node.parent();
            let signature = parent.map(|p| {
                let start = p.start_byte();
                let mut end = (start + 200).min(content.len());
                while end > start && !content.is_char_boundary(end) {
                    end -= 1;
                }
                let slice = &content[start..end];
                slice.lines().next().unwrap_or("").to_string()
            });

            // Headings have no doc comments; extract_doc_above would misidentify parent headings
            let doc = if kind == SymbolKind::Heading {
                None
            } else {
                extract_doc_above(content, line)
            };
            let body_tokens = parent.and_then(|def| extract_body_tokens(def, content));

            symbols.push(ParsedSymbol {
                name: name.to_string(),
                kind,
                line,
                signature,
                doc,
                body_tokens,
            });
        }
    }

    Ok((symbols, imports))
}

/// Extract doc comment or docstring from lines immediately above a symbol.
/// Returns up to ~200 chars of cleaned comment text, or None.
fn extract_doc_above(content: &str, symbol_line: usize) -> Option<String> {
    if symbol_line <= 1 {
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    let mut doc_lines: Vec<&str> = Vec::new();
    let mut idx = symbol_line - 2; // 0-indexed line above symbol

    loop {
        if idx >= lines.len() {
            break;
        }
        let trimmed = lines[idx].trim();

        if trimmed.starts_with("///")
            || trimmed.starts_with("//!")
            || trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("/**")
            || trimmed.starts_with('*')
            || trimmed.starts_with("\"\"\"")
            || trimmed.starts_with("'''")
        {
            let cleaned = trimmed
                .trim_start_matches("///")
                .trim_start_matches("//!")
                .trim_start_matches("//")
                .trim_start_matches("/**")
                .trim_start_matches("*/")
                .trim_start_matches('*')
                .trim_start_matches('#')
                .trim();
            if !cleaned.is_empty() {
                doc_lines.push(cleaned);
            }
        } else if trimmed.is_empty() {
            // skip blank lines between comment and symbol
        } else {
            break;
        }

        if idx == 0 {
            break;
        }
        idx -= 1;
    }

    if doc_lines.is_empty() {
        return None;
    }

    doc_lines.reverse();
    let mut doc = doc_lines.join(" ");
    if doc.len() > 200 {
        let mut cut = 200;
        while cut > 0 && !doc.is_char_boundary(cut) {
            cut -= 1;
        }
        doc.truncate(cut);
    }
    Some(doc)
}

/// Common language keywords to exclude from body token extraction.
fn is_keyword(s: &str) -> bool {
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
    )
}

/// Extract meaningful identifiers from a symbol's AST definition node.
/// Walks the subtree, collects identifier and string literal text,
/// filters keywords and short names. Returns space-separated tokens.
fn extract_body_tokens(def_node: tree_sitter::Node, content: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let mut tokens = Vec::new(); // preserves first-occurrence order
    let mut stack = vec![def_node];
    let mut nodes_visited = 0usize;
    const MAX_NODES: usize = 2000;

    while let Some(node) = stack.pop() {
        nodes_visited += 1;
        if nodes_visited > MAX_NODES {
            break;
        }

        match node.kind() {
            "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "attribute"
            | "shorthand_field_identifier" => {
                // content is guaranteed UTF-8 by read_to_string
                let text = node.utf8_text(content.as_bytes()).unwrap_or_default();
                if text.len() > 1 && !is_keyword(text) && seen.insert(text.to_string()) {
                    tokens.push(text.to_string());
                }
            }
            "string_content" | "string_fragment" => {
                let text = node.utf8_text(content.as_bytes()).unwrap_or_default();
                for word in text.split_whitespace() {
                    let clean: String = word
                        .chars()
                        .filter(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if clean.len() > 2 && seen.insert(clean.to_lowercase()) {
                        tokens.push(clean.to_lowercase());
                    }
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }

    if tokens.is_empty() {
        return None;
    }

    let mut joined = tokens.join(" ");
    if joined.len() > 400 {
        // Find a char boundary at or before 400 bytes
        let mut cut = 400;
        while cut > 0 && !joined.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(pos) = joined[..cut].rfind(' ') {
            joined.truncate(pos);
        } else {
            joined.truncate(cut);
        }
    }
    Some(joined)
}

/// Extract references (symbol usages) via simple identifier scanning.
pub fn extract_references(content: &str) -> Vec<ParsedRef> {
    let mut refs = Vec::new();
    for (line_num, line) in content.lines().enumerate() {
        for cap in regex_lite_camel_case(line) {
            refs.push(ParsedRef {
                name: cap.to_string(),
                line: line_num + 1,
                context: Some(line.trim().to_string()),
            });
        }
    }
    refs
}

/// Simple CamelCase identifier extractor (no regex crate dependency).
fn regex_lite_camel_case(line: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_uppercase() {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            // Must have at least one lowercase letter (not ALL_CAPS constant)
            if word.len() > 1 && word.bytes().any(|b| b.is_ascii_lowercase()) {
                results.push(word);
            }
        } else {
            i += 1;
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbols(src: &str, lang: Language) -> Vec<ParsedSymbol> {
        extract_symbols_and_imports(src, lang).unwrap().0
    }

    fn import_names(src: &str, lang: Language) -> Vec<String> {
        extract_symbols_and_imports(src, lang)
            .unwrap()
            .1
            .into_iter()
            .map(|r| r.name)
            .collect()
    }

    // --- Kotlin symbol tests ---

    #[test]
    fn kotlin_extracts_class() {
        let src = "class PaymentService {\n    fun process() {}\n}";
        let syms = symbols(src, Language::Kotlin);
        let names: Vec<(&str, SymbolKind)> =
            syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert!(names.contains(&("PaymentService", SymbolKind::Class)));
        assert!(names.contains(&("process", SymbolKind::Function)));
    }

    #[test]
    fn kotlin_extracts_interface() {
        let src = "interface Repository {\n    fun findById(id: Long): Any?\n}";
        let syms = symbols(src, Language::Kotlin);
        let names: Vec<(&str, SymbolKind)> =
            syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert!(names.contains(&("Repository", SymbolKind::Interface)));
        assert!(names.contains(&("findById", SymbolKind::Function)));
    }

    #[test]
    fn kotlin_extracts_object_and_property() {
        let src = "object Config {\n    val baseUrl = \"https://api.example.com\"\n}";
        let syms = symbols(src, Language::Kotlin);
        let names: Vec<(&str, SymbolKind)> =
            syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert!(names.contains(&("Config", SymbolKind::Class)));
        assert!(names.contains(&("baseUrl", SymbolKind::Property)));
    }

    #[test]
    fn kotlin_extracts_top_level_function() {
        let src = "fun topLevelFunction(): String = \"hello\"";
        let syms = symbols(src, Language::Kotlin);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "topLevelFunction");
        assert_eq!(syms[0].kind, SymbolKind::Function);
    }

    #[test]
    fn kotlin_extracts_data_class_and_enum() {
        let src = "data class User(val name: String)\n\nenum class Status { ACTIVE, INACTIVE }";
        let syms = symbols(src, Language::Kotlin);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"User"));
        assert!(names.contains(&"Status"));
    }

    // --- TypeScript symbol tests ---

    #[test]
    fn typescript_extracts_class_and_function() {
        let src =
            "class PaymentService {\n  processPayment(amount: number): boolean { return true; }\n}";
        let syms = symbols(src, Language::TypeScript);
        let names: Vec<(&str, SymbolKind)> =
            syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert!(names.contains(&("PaymentService", SymbolKind::Class)));
    }

    #[test]
    fn typescript_extracts_interface() {
        let src = "interface Repository {\n  findById(id: number): any;\n}";
        let syms = symbols(src, Language::TypeScript);
        let names: Vec<(&str, SymbolKind)> =
            syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert!(names.contains(&("Repository", SymbolKind::Interface)));
    }

    #[test]
    fn typescript_extracts_enum_and_type_alias() {
        let src = "enum Status { ACTIVE, INACTIVE }\n\ntype UserId = string;";
        let syms = symbols(src, Language::TypeScript);
        let names: Vec<(&str, SymbolKind)> =
            syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert!(names.contains(&("Status", SymbolKind::Enum)));
        assert!(names.contains(&("UserId", SymbolKind::TypeAlias)));
    }

    #[test]
    fn typescript_extracts_arrow_function() {
        let src = "const arrowFn = (x: number): number => x * 2;";
        let syms = symbols(src, Language::TypeScript);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "arrowFn");
        assert_eq!(syms[0].kind, SymbolKind::Function);
    }

    #[test]
    fn typescript_extracts_top_level_function() {
        let src = "function topLevelFunction(): string { return \"hello\"; }";
        let syms = symbols(src, Language::TypeScript);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "topLevelFunction");
        assert_eq!(syms[0].kind, SymbolKind::Function);
    }

    #[test]
    fn typescript_extracts_exported_function() {
        let src = "export function fetchData(): void {}";
        let syms = symbols(src, Language::TypeScript);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "fetchData");
        assert_eq!(syms[0].kind, SymbolKind::Function);
    }

    // --- Import extraction tests ---

    #[test]
    fn rust_imports_use_path() {
        let names = import_names("use std::collections::HashMap;", Language::Rust);
        assert!(names.contains(&"HashMap".to_string()));
    }

    #[test]
    fn rust_imports_use_list() {
        let names = import_names("use anyhow::{Context, Result};", Language::Rust);
        assert!(names.contains(&"Context".to_string()));
        assert!(names.contains(&"Result".to_string()));
    }

    #[test]
    fn rust_imports_simple() {
        let names = import_names("use std::io;", Language::Rust);
        assert!(names.contains(&"io".to_string()));
    }

    #[test]
    fn python_import_module() {
        let names = import_names("import os\nimport sys", Language::Python);
        assert!(names.contains(&"os".to_string()));
        assert!(names.contains(&"sys".to_string()));
    }

    #[test]
    fn python_from_import() {
        let names = import_names("from collections import OrderedDict", Language::Python);
        assert!(names.contains(&"OrderedDict".to_string()));
    }

    #[test]
    fn go_imports() {
        let names = import_names(
            "package main\n\nimport \"fmt\"\nimport (\n    \"os\"\n    \"strings\"\n)",
            Language::Go,
        );
        assert!(names.contains(&"fmt".to_string()));
        assert!(names.contains(&"os".to_string()));
        assert!(names.contains(&"strings".to_string()));
    }

    #[test]
    fn java_imports() {
        let names = import_names("import java.util.HashMap;", Language::Java);
        assert!(names.contains(&"HashMap".to_string()));
    }

    #[test]
    fn typescript_named_imports() {
        let names = import_names(
            "import { useState, useEffect } from 'react';",
            Language::TypeScript,
        );
        assert!(names.contains(&"useState".to_string()));
        assert!(names.contains(&"useEffect".to_string()));
    }

    #[test]
    fn typescript_default_import() {
        let names = import_names("import React from 'react';", Language::TypeScript);
        assert!(names.contains(&"React".to_string()));
    }

    #[test]
    fn typescript_namespace_import() {
        let names = import_names("import * as path from 'path';", Language::TypeScript);
        assert!(names.contains(&"path".to_string()));
    }

    #[test]
    fn kotlin_imports() {
        let names = import_names(
            "import java.util.List\nimport kotlinx.coroutines.flow.Flow",
            Language::Kotlin,
        );
        assert!(names.contains(&"List".to_string()));
        assert!(names.contains(&"Flow".to_string()));
    }

    // --- SQL symbol tests ---

    #[test]
    fn sql_extracts_create_table() {
        let src = "CREATE TABLE users (\n  id SERIAL PRIMARY KEY,\n  name VARCHAR(255)\n);";
        let syms = symbols(src, Language::Sql);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "users");
        assert_eq!(syms[0].kind, SymbolKind::Class);
    }

    #[test]
    fn sql_extracts_create_view() {
        let src = "CREATE VIEW active_users AS SELECT * FROM users WHERE active = true;";
        let syms = symbols(src, Language::Sql);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "active_users");
        assert_eq!(syms[0].kind, SymbolKind::Class);
    }

    #[test]
    fn sql_extracts_create_function() {
        let src = "CREATE FUNCTION get_user(user_id INT) RETURNS INT AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql;";
        let syms = symbols(src, Language::Sql);
        let names: Vec<(&str, SymbolKind)> =
            syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert!(names.contains(&("get_user", SymbolKind::Function)));
    }

    #[test]
    fn sql_extracts_create_index() {
        let src = "CREATE INDEX idx_users_email ON users (email);";
        let syms = symbols(src, Language::Sql);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "idx_users_email");
        assert_eq!(syms[0].kind, SymbolKind::Property);
    }

    #[test]
    fn sql_extracts_multiple_statements() {
        let src = "CREATE TABLE orders (id INT);\nCREATE TABLE products (id INT);\nCREATE VIEW order_summary AS SELECT * FROM orders;";
        let syms = symbols(src, Language::Sql);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"orders"));
        assert!(names.contains(&"products"));
        assert!(names.contains(&"order_summary"));
    }

    #[test]
    fn sql_extracts_create_type() {
        let src = "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');";
        let syms = symbols(src, Language::Sql);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "mood");
        assert_eq!(syms[0].kind, SymbolKind::Enum);
    }

    #[test]
    fn sql_extracts_create_schema() {
        let src = "CREATE SCHEMA analytics;";
        let syms = symbols(src, Language::Sql);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "analytics");
        assert_eq!(syms[0].kind, SymbolKind::Class);
    }

    #[test]
    fn sql_extracts_materialized_view() {
        let src = "CREATE MATERIALIZED VIEW monthly_stats AS SELECT * FROM stats;";
        let syms = symbols(src, Language::Sql);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "monthly_stats");
        assert_eq!(syms[0].kind, SymbolKind::Class);
    }

    #[test]
    fn sql_extracts_sequence_and_extension() {
        let src = "CREATE SEQUENCE user_id_seq START 1;\nCREATE EXTENSION IF NOT EXISTS pgcrypto;";
        let syms = symbols(src, Language::Sql);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"user_id_seq"));
        assert!(names.contains(&"pgcrypto"));
    }

    #[test]
    fn sql_alter_table_as_ref() {
        let imports = import_names("ALTER TABLE users ADD COLUMN age INT;", Language::Sql);
        assert!(imports.contains(&"users".to_string()));
    }

    // --- Doc extraction tests ---

    #[test]
    fn rust_doc_comment_extracted() {
        let src = "/// Process a batch of sensor readings.\n/// Returns the average value.\nfn process_batch() {}";
        let syms = symbols(src, Language::Rust);
        assert_eq!(syms.len(), 1);
        let doc = syms[0].doc.as_deref().unwrap();
        assert!(doc.contains("Process a batch of sensor readings"));
        assert!(doc.contains("Returns the average value"));
    }

    #[test]
    fn python_comment_extracted() {
        let src = "# Calculate the total price including tax\ndef calculate_total():\n    pass";
        let syms = symbols(src, Language::Python);
        assert_eq!(syms.len(), 1);
        let doc = syms[0].doc.as_deref().unwrap();
        assert!(doc.contains("Calculate the total price"));
    }

    #[test]
    fn no_doc_when_no_comment() {
        let src = "fn main() {}";
        let syms = symbols(src, Language::Rust);
        assert_eq!(syms.len(), 1);
        assert!(syms[0].doc.is_none());
    }
}
