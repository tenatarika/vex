//! PHP grammar regression coverage.
//!
//! Catches ABI mismatches and AST node renames against `tree-sitter-php`.
//! Targets the LANGUAGE_PHP grammar (the tag-aware one), which is what we
//! ship for `.php` / `.phtml` files.

use vex::index::symbols::SymbolKind;
use vex::parse::extractor::extract_symbols_and_imports;
use vex::parse::language::Language;

fn symbols(src: &str) -> Vec<(String, SymbolKind)> {
    extract_symbols_and_imports(src, Language::Php)
        .expect("php grammar must load")
        .0
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect()
}

fn imports(src: &str) -> Vec<String> {
    extract_symbols_and_imports(src, Language::Php)
        .expect("php grammar must load")
        .1
        .into_iter()
        .map(|r| r.name)
        .collect()
}

#[test]
fn php_grammar_loads() {
    extract_symbols_and_imports("", Language::Php).expect("php grammar must load on empty input");
}

#[test]
fn php_extracts_class_interface_trait_enum() {
    let src = r#"<?php
class PaymentService {
    public function charge(int $amount): bool { return true; }
}

interface Payable {
    public function pay(): void;
}

trait Loggable {
    public function log(string $msg): void {}
}

enum Status {
    case Active;
    case Inactive;
}
"#;
    let s = symbols(src);
    assert!(
        s.contains(&("PaymentService".into(), SymbolKind::Class)),
        "{s:?}"
    );
    assert!(
        s.contains(&("Payable".into(), SymbolKind::Interface)),
        "{s:?}"
    );
    assert!(s.contains(&("Loggable".into(), SymbolKind::Trait)), "{s:?}");
    assert!(s.contains(&("Status".into(), SymbolKind::Enum)), "{s:?}");
    assert!(
        s.contains(&("charge".into(), SymbolKind::Function)),
        "{s:?}"
    );
    assert!(s.contains(&("pay".into(), SymbolKind::Function)), "{s:?}");
}

#[test]
fn php_extracts_top_level_function() {
    let s = symbols("<?php\nfunction greet(string $name): string { return \"hello\"; }\n");
    assert!(s.contains(&("greet".into(), SymbolKind::Function)), "{s:?}");
}

#[test]
fn php_extracts_class_constant() {
    let src = r#"<?php
class Config {
    public const VERSION = '1.0';
    const MAX_RETRIES = 3;
}
"#;
    let s = symbols(src);
    assert!(
        s.contains(&("VERSION".into(), SymbolKind::Constant)),
        "{s:?}"
    );
    assert!(
        s.contains(&("MAX_RETRIES".into(), SymbolKind::Constant)),
        "{s:?}"
    );
}

#[test]
fn php_extracts_use_imports() {
    let src = r#"<?php
use App\Service\PaymentService;
use App\Util\Logger as Log;
"#;
    let imp = imports(src);
    assert!(imp.iter().any(|n| n == "PaymentService"), "{imp:?}");
    // alias is captured too — both the original tail name and the alias are useful
    assert!(imp.iter().any(|n| n == "Log"), "{imp:?}");

    // Each clause should produce its name once — guard against query
    // duplication on overlapping namespace_use_clause patterns.
    let ps_count = imp.iter().filter(|n| *n == "PaymentService").count();
    assert_eq!(ps_count, 1, "PaymentService duplicated: {imp:?}");

    // The aliased form `use App\Util\Logger as Log;` should yield both
    // `Logger` (the tail of the qualified name — what callers actually
    // reference inside the file) AND `Log` (the alias). Both are
    // legitimate search targets; we want exactly one of each.
    let logger_count = imp.iter().filter(|n| *n == "Logger").count();
    let log_count = imp.iter().filter(|n| *n == "Log").count();
    assert_eq!(logger_count, 1, "Logger missing or duplicated: {imp:?}");
    assert_eq!(log_count, 1, "Log missing or duplicated: {imp:?}");
}
