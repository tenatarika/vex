//! SQL grammar regression coverage.
//!
//! Uses `tree-sitter-sequel`, a PostgreSQL-flavored dialect grammar.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Sql)
        .expect("sql grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

#[test]
fn sql_grammar_loads() {
    let _ = extract_symbols_and_imports("", Language::Sql)
        .expect("sql grammar must load on empty input");
}

#[test]
fn sql_create_table() {
    let src = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "users" && *k == SymbolKind::Class),
        "expected table users, got {s:?}"
    );
}

#[test]
fn sql_create_view_and_type() {
    let src =
        "CREATE VIEW active_users AS SELECT * FROM users;\nCREATE TYPE mood AS ENUM ('ok','bad');";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "active_users" && *k == SymbolKind::Class),
        "expected view active_users, got {s:?}"
    );
    assert!(
        s.iter().any(|(n, k)| n == "mood" && *k == SymbolKind::Enum),
        "expected type mood, got {s:?}"
    );
}

#[test]
fn sql_create_function() {
    let src = "CREATE FUNCTION add(a INT, b INT) RETURNS INT AS $$ BEGIN RETURN a + b; END; $$ LANGUAGE plpgsql;";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "add" && *k == SymbolKind::Function),
        "expected fn add, got {s:?}"
    );
}

#[test]
fn sql_create_index_and_sequence() {
    let src = "CREATE INDEX idx_user_email ON users(email);\nCREATE SEQUENCE order_id_seq;";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "idx_user_email" && *k == SymbolKind::Property),
        "expected index, got {s:?}"
    );
    assert!(
        s.iter()
            .any(|(n, k)| n == "order_id_seq" && *k == SymbolKind::Property),
        "expected sequence, got {s:?}"
    );
}

#[test]
fn sql_materialized_view_and_schema() {
    let src = "CREATE MATERIALIZED VIEW recent_orders AS SELECT * FROM orders;\nCREATE SCHEMA app;";
    let s = symbols(src);
    assert!(
        s.iter()
            .any(|(n, k)| n == "recent_orders" && *k == SymbolKind::Class),
        "expected materialized view, got {s:?}"
    );
    assert!(
        s.iter().any(|(n, k)| n == "app" && *k == SymbolKind::Class),
        "expected schema, got {s:?}"
    );
}
