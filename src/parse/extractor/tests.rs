#![cfg(test)]

use super::refs::scan_identifiers;
use super::*;
use crate::index::symbols::{ParsedSymbol, SymbolKind};

fn symbols(src: &str, lang: Language) -> Vec<ParsedSymbol> {
    extract_symbols_and_imports(src, lang).unwrap().0
}

/// Phase 8.4 — assert that at least one symbol from `lang`'s parse
/// of `src` carries a non-empty `body_tokens`, AND that the tokens
/// include each of the `expected_substrings`. Pre-8.4 these were
/// all `None` because the extractor only matched code-language
/// AST leaves.
#[track_caller]
fn assert_body_tokens_contain(src: &str, lang: Language, expected: &[&str]) {
    let syms = symbols(src, lang);
    let with_tokens: Vec<(&str, &str)> = syms
        .iter()
        .filter_map(|s| s.body_tokens.as_deref().map(|t| (s.name.as_str(), t)))
        .collect();
    assert!(
        !with_tokens.is_empty(),
        "expected at least one {:?} symbol with body_tokens populated; got: {:?}",
        lang,
        syms.iter()
            .map(|s| (&s.name, &s.body_tokens))
            .collect::<Vec<_>>()
    );
    let joined: String = with_tokens
        .iter()
        .map(|(_, t)| *t)
        .collect::<Vec<_>>()
        .join(" ");
    for needle in expected {
        assert!(
            joined.contains(needle),
            "{:?} body_tokens must include `{needle}`; got: `{joined}`",
            lang
        );
    }
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
    let names: Vec<(&str, SymbolKind)> = syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
    assert!(names.contains(&("PaymentService", SymbolKind::Class)));
    assert!(names.contains(&("process", SymbolKind::Function)));
}

#[test]
fn kotlin_extracts_interface() {
    let src = "interface Repository {\n    fun findById(id: Long): Any?\n}";
    let syms = symbols(src, Language::Kotlin);
    let names: Vec<(&str, SymbolKind)> = syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
    assert!(names.contains(&("Repository", SymbolKind::Interface)));
    assert!(names.contains(&("findById", SymbolKind::Function)));
}

#[test]
fn kotlin_extracts_object_and_property() {
    let src = "object Config {\n    val baseUrl = \"https://api.example.com\"\n}";
    let syms = symbols(src, Language::Kotlin);
    let names: Vec<(&str, SymbolKind)> = syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
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
    let names: Vec<(&str, SymbolKind)> = syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
    assert!(names.contains(&("PaymentService", SymbolKind::Class)));
}

#[test]
fn typescript_extracts_interface() {
    let src = "interface Repository {\n  findById(id: number): any;\n}";
    let syms = symbols(src, Language::TypeScript);
    let names: Vec<(&str, SymbolKind)> = syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
    assert!(names.contains(&("Repository", SymbolKind::Interface)));
}

#[test]
fn typescript_extracts_enum_and_type_alias() {
    let src = "enum Status { ACTIVE, INACTIVE }\n\ntype UserId = string;";
    let syms = symbols(src, Language::TypeScript);
    let names: Vec<(&str, SymbolKind)> = syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
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
    let names: Vec<(&str, SymbolKind)> = syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
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

// --- Identifier scanner case-style coverage ---

fn scan(line: &str) -> Vec<String> {
    scan_identifiers(line)
        .into_iter()
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn scan_pascal_case() {
    assert_eq!(
        scan("let g = PaymentGateway::new();"),
        vec!["PaymentGateway"]
    );
}

#[test]
fn scan_camel_case() {
    assert_eq!(scan("await processOrder(total);"), vec!["processOrder"]);
}

#[test]
fn scan_snake_case() {
    assert_eq!(
        scan("return process_order(gateway, total)"),
        vec!["process_order"]
    );
}

#[test]
fn scan_screaming_snake_case() {
    assert_eq!(scan("retries < MAX_RETRY_COUNT"), vec!["MAX_RETRY_COUNT"]);
}

#[test]
fn scan_skips_plain_lowercase_words() {
    // Prose nouns and trivial locals would drown the refs FST in noise.
    let captured = scan("the total amount equals the charge value");
    assert!(captured.is_empty(), "captured noise: {captured:?}");
}

#[test]
fn scan_skips_short_identifiers() {
    // Two-char identifiers (i, x, fn-locals like `id`, `ok`) are
    // almost always noise; the threshold is len >= 3.
    assert!(scan("if (i < n) { id = ok; }").is_empty());
}

#[test]
fn scan_skips_underscore_only() {
    // `_`, `__`, `___` are placeholder/ignore tokens.
    assert!(scan("let _ = foo();").is_empty());
    assert!(scan("let __ = foo();").is_empty());
}

#[test]
fn scan_skips_keywords() {
    // `return`, `class`, `function` are in is_keyword.
    let captured = scan("return class function override");
    assert!(captured.is_empty(), "leaked keyword: {captured:?}");
}

#[test]
fn scan_mixed_line() {
    let line = "PaymentGateway gateway = new StripeGateway(api_key);";
    let captured = scan(line);
    // Should pick up the structurally-shaped identifiers and drop
    // bare lowercase ones (`gateway`) and the 7-char keyword `new`.
    assert!(captured.contains(&"PaymentGateway".to_string()));
    assert!(captured.contains(&"StripeGateway".to_string()));
    assert!(captured.contains(&"api_key".to_string()));
    assert!(!captured.iter().any(|n| n == "gateway"));
}

#[test]
fn scan_python_snake_call() {
    let captured = scan("result = calculate_total_price(items, tax_rate)");
    assert!(captured.contains(&"calculate_total_price".to_string()));
    assert!(captured.contains(&"tax_rate".to_string()));
    // `result` and `items` are bare lowercase and should be skipped.
    assert!(!captured.iter().any(|n| n == "result"));
    assert!(!captured.iter().any(|n| n == "items"));
}

// --- Phase 8.4: body tokens for config languages ---

#[test]
fn phase_8_4_toml_body_tokens_include_keys_and_string_values() {
    // `[server]` table groups two keys: `endpoint` (string URL) and
    // `port` (number). Semantic search should be able to find this
    // section by querying for "production endpoint" — pre-8.4 the
    // body_tokens were None so the semantic channel was blind.
    let src = "[server]\nendpoint = \"https://prod.example.com/api\"\nport = 8080\n";
    assert_body_tokens_contain(
        src,
        Language::Toml,
        &["endpoint", "https", "prod", "example"],
    );
}

#[test]
fn phase_8_4_yaml_body_tokens_include_top_level_key() {
    // YAML's SCM captures only top-level mapping keys (see
    // `queries/yaml.scm`), and the captured node's `parent()` is
    // `plain_scalar` — which holds only the key text, not the
    // mapping value. So Phase 8.4's win for YAML is "the key is
    // searchable at all" rather than "key + value." Pre-8.4 even
    // the key was lost because `plain_scalar` produced no tokens.
    let src = "server:\n  endpoint: https://prod.example.com/api\n  port: 8080\n";
    assert_body_tokens_contain(src, Language::Yaml, &["server"]);
}

#[test]
fn phase_8_4_html_body_tokens_include_tag_attribute_and_value() {
    // Custom element with an `id` attribute — both the element name
    // and the id value should be searchable.
    let src = "<my-component id=\"user-profile-card\"></my-component>";
    assert_body_tokens_contain(
        src,
        Language::Html,
        &["my", "component", "user", "profile", "card"],
    );
}

#[test]
fn phase_8_4_css_body_tokens_include_class_and_property_names() {
    // @keyframes block — both the keyframe name and the declarations
    // inside (`property_name`, `plain_value`) should surface.
    let src = "@keyframes slide-in {\n  from { transform: translateX(-100%); }\n  to { transform: translateX(0); }\n}\n";
    assert_body_tokens_contain(src, Language::Css, &["slide"]);
}

/// v1.11 hotfix — the bare `"string"` node-kind arm must only fire
/// for TOML. Pre-fix, every grammar that exposes a `"string"` parent
/// node (Rust, Python, TypeScript, …) re-tokenised the entire raw
/// string region — quotes included, escapes unprocessed — alongside
/// the language-correct `string_content` / `string_fragment` walk.
/// Dedup masked the bug today, but it's a footgun for any future
/// non-config language that doesn't emit a `string_content` child.
/// This test proves a Python docstring is tokenised exclusively via
/// `string_content` (the proper leaf), so removing the bare arm for
/// non-TOML languages doesn't lose tokens.
#[test]
fn v1_11_hotfix_python_string_tokens_come_from_string_content_not_bare_string() {
    let src = "def greet():\n    \"\"\"hello searchable world\"\"\"\n    return 1\n";
    // Tokens are still emitted (via `string_content`), so `greet`'s
    // body_tokens MUST contain the docstring words.
    assert_body_tokens_contain(src, Language::Python, &["hello", "searchable", "world"]);
}
