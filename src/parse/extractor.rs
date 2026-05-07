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
                _ => continue,
            };

            let signature = node.parent().map(|p| {
                let start = p.start_byte();
                let mut end = (start + 200).min(content.len());
                while end > start && !content.is_char_boundary(end) {
                    end -= 1;
                }
                let slice = &content[start..end];
                slice.lines().next().unwrap_or("").to_string()
            });

            symbols.push(ParsedSymbol {
                name: name.to_string(),
                kind,
                line,
                signature,
            });
        }
    }

    Ok((symbols, imports))
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
}
