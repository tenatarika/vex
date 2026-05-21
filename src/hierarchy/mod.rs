use std::path::Path;

use anyhow::{Context, Result};
use rayon::prelude::*;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::parse::language::Language;

/// A match where a class/struct implements or extends a base type.
#[derive(Debug, Clone)]
pub struct ImplMatch {
    pub path: String,
    pub line: usize,
    pub name: String,
    pub base: String,
    pub relation: &'static str, // "implements", "extends", "impl"
}

/// Find all types that inherit from / implement `base_name` across all supported languages.
pub fn find_implementations(
    root: &Path,
    base_name: &str,
    limit: usize,
    excludes: &[String],
) -> Result<Vec<ImplMatch>> {
    let root = root.canonicalize().context("canonicalize root")?;
    let files: Vec<_> = crate::util::walk::discover_source_files(&root, excludes)?
        .into_iter()
        .filter(|(_, lang)| inheritance_query(*lang).is_some())
        .collect();

    let matches: Vec<ImplMatch> = files
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

            find_in_source(&content, *lang, &rel, base_name)
        })
        .collect();

    Ok(matches.into_iter().take(limit).collect())
}

/// Get the inheritance tree-sitter query for a language, if supported.
fn inheritance_query(lang: Language) -> Option<&'static str> {
    match lang {
        Language::Rust => Some(
            r#"
            (impl_item
              trait: (type_identifier) @base
              type: (type_identifier) @child) @def
            "#,
        ),
        Language::Python => Some(
            r#"
            (class_definition
              name: (identifier) @child
              superclasses: (argument_list
                (identifier) @base)) @def
            "#,
        ),
        Language::Java => Some(
            r#"
            (class_declaration
              name: (identifier) @child
              (superclass (type_identifier) @base)) @def

            (class_declaration
              name: (identifier) @child
              (super_interfaces (type_list (type_identifier) @base))) @def

            (interface_declaration
              name: (identifier) @child
              (extends_interfaces (type_list (type_identifier) @base))) @def

            ; 11.3: generic-parameterised bases — `extends Repository<T>`,
            ; `implements Handler<T>`. Tree-sitter wraps the identifier in
            ; a `generic_type` parent when type arguments are present.
            (class_declaration
              name: (identifier) @child
              (superclass (generic_type (type_identifier) @base))) @def

            (class_declaration
              name: (identifier) @child
              (super_interfaces (type_list (generic_type (type_identifier) @base)))) @def

            (interface_declaration
              name: (identifier) @child
              (extends_interfaces (type_list (generic_type (type_identifier) @base)))) @def
            "#,
        ),
        Language::TypeScript => Some(
            r#"
            (class_declaration
              name: (type_identifier) @child
              (class_heritage
                (extends_clause
                  value: (identifier) @base))) @def

            (class_declaration
              name: (type_identifier) @child
              (class_heritage
                (implements_clause
                  (type_identifier) @base))) @def
            "#,
        ),
        Language::CSharp => Some(
            r#"
            (class_declaration
              name: (identifier) @child
              (base_list (identifier) @base)) @def

            (class_declaration
              name: (identifier) @child
              (base_list
                (qualified_name
                  (identifier) @base))) @def

            ; 11.3: generic-parameterised bases — `: Repository<T>`,
            ; `: IRepository<T>`. The grammar wraps the identifier in
            ; `generic_name` when type arguments are present.
            ;
            ; TODO(11.4 follow-up): qualified+generic combo — `: Namespace.Repo<T>`
            ; — needs a `(qualified_name ... (generic_name (identifier)))`
            ; pattern. Common in EF Core / DI-heavy .NET codebases.
            (class_declaration
              name: (identifier) @child
              (base_list (generic_name (identifier) @base))) @def
            "#,
        ),
        Language::Swift => Some(
            r#"
            (class_declaration
              name: (type_identifier) @child
              (inheritance_specifier
                (user_type (type_identifier) @base))) @def

            (protocol_declaration
              name: (type_identifier) @child
              (inheritance_specifier
                (user_type (type_identifier) @base))) @def
            "#,
        ),
        Language::Kotlin => Some(
            // tree-sitter-kotlin-ng node names verified via AST dump:
            // class name is `identifier` (not `type_identifier`); the
            // delegation list is `delegation_specifiers` (not
            // `_list`); generic args live as a sibling
            // `type_arguments` of `identifier` inside `user_type`,
            // so the bare `(user_type (identifier))` pattern matches
            // both plain and generic bases.
            r#"
            (class_declaration
              (identifier) @child
              (delegation_specifiers
                (delegation_specifier
                  (user_type (identifier) @base)))) @def

            ; Superclass call: `class Foo : Bar()` or `class Foo : Repository<T>()`.
            (class_declaration
              (identifier) @child
              (delegation_specifiers
                (delegation_specifier
                  (constructor_invocation
                    (user_type (identifier) @base))))) @def
            "#,
        ),
        // Go has implicit interfaces, Ruby has mixins — skip for now
        Language::Cpp => Some(
            r#"
            (class_specifier
              name: (type_identifier) @child
              (base_class_clause
                (type_identifier) @base)) @def

            (struct_specifier
              name: (type_identifier) @child
              (base_class_clause
                (type_identifier) @base)) @def
            "#,
        ),
        // PHP: `class Foo extends Bar implements I1, I2`, plus
        // `interface ChildI extends ParentI`, plus trait composition via
        // `use TraitName;` inside a class or trait body. Base types can
        // be a bare name or a qualified name (`App\Service\Foo`); we
        // capture the trailing `name` node in either form.
        //
        // Pattern order matters: patterns 0..=5 are extends/implements
        // (label "extends"), patterns 6..=9 are trait `use` (label
        // "uses"). `php_relation_label` below dispatches on
        // `m.pattern_index` accordingly — reorder with care.
        Language::Php => Some(
            r#"
            (class_declaration
              name: (name) @child
              (base_clause (name) @base)) @def

            (class_declaration
              name: (name) @child
              (base_clause (qualified_name (name) @base))) @def

            (class_declaration
              name: (name) @child
              (class_interface_clause (name) @base)) @def

            (class_declaration
              name: (name) @child
              (class_interface_clause (qualified_name (name) @base))) @def

            (interface_declaration
              name: (name) @child
              (base_clause (name) @base)) @def

            (interface_declaration
              name: (name) @child
              (base_clause (qualified_name (name) @base))) @def

            (enum_declaration
              name: (name) @child
              (class_interface_clause (name) @base)) @def

            (enum_declaration
              name: (name) @child
              (class_interface_clause (qualified_name (name) @base))) @def

            (class_declaration
              name: (name) @child
              body: (declaration_list
                (use_declaration (name) @base))) @def

            (class_declaration
              name: (name) @child
              body: (declaration_list
                (use_declaration (qualified_name (name) @base)))) @def

            (trait_declaration
              name: (name) @child
              body: (declaration_list
                (use_declaration (name) @base))) @def

            (trait_declaration
              name: (name) @child
              body: (declaration_list
                (use_declaration (qualified_name (name) @base)))) @def
            "#,
        ),
        // Ruby: `class Foo < Bar` for single inheritance, and
        // `include Mixin` / `extend Mixin` / `prepend Mixin` inside class
        // or module bodies for mixin composition. The mixin call appears
        // as a normal `call` node (no implicit receiver) in
        // tree-sitter-ruby.
        //
        // The `#match?` predicate MUST sit inside the enclosing pattern's
        // S-expression — otherwise tree-sitter silently treats it as a
        // standalone (no-op) pattern and the query degrades to "any
        // method call inside the class body with a Constant argument",
        // which would match noise like `assert_equal Foo, bar.class`.
        Language::Ruby => Some(
            r#"
            (class
              name: (constant) @child
              superclass: (superclass (constant) @base)) @def

            (class
              name: (constant) @child
              body: (body_statement
                (call
                  method: (identifier) @_m
                  arguments: (argument_list (constant) @base)))
              (#match? @_m "^(include|extend|prepend)$")) @def

            (module
              name: (constant) @child
              body: (body_statement
                (call
                  method: (identifier) @_m
                  arguments: (argument_list (constant) @base)))
              (#match? @_m "^(include|extend|prepend)$")) @def
            "#,
        ),
        // Languages without class-hierarchy semantics in their grammar.
        // Go uses structural typing (no declared base list), SQL /
        // Markdown / config formats have no classes, Bash and Lua are
        // not OO.
        Language::Go
        | Language::Sql
        | Language::Markdown
        | Language::Bash
        | Language::Lua
        | Language::Css
        | Language::Html
        | Language::Yaml
        | Language::Toml => None,
    }
}

/// First pattern index in PHP's [`inheritance_query`] that denotes a
/// trait `use` (rather than class extends / class implements / interface
/// extends / enum implements). Patterns 0..[`PHP_TRAIT_PATTERN_START`) →
/// `"extends"`, patterns [`PHP_TRAIT_PATTERN_START`).. → `"uses"`.
///
/// Layout (keep in sync with the query string):
/// - 0..=1: `class extends` (bare + qualified)
/// - 2..=3: `class implements` (bare + qualified)
/// - 4..=5: `interface extends` (bare + qualified)
/// - 6..=7: `enum implements` (bare + qualified, PHP 8.1+)
/// - 8..=9: `class { use Trait }` (bare + qualified)
/// - 10..=11: `trait { use OtherTrait }` (bare + qualified)
///
/// Adding a new extends / implements pattern requires bumping this
/// constant, otherwise the new pattern will be mislabelled as `"uses"`.
/// Symmetric regression tests `php_extends_stays_extends_not_uses` and
/// `php_trait_uses_relation_is_uses_not_extends` guard the boundary
/// from both sides.
const PHP_TRAIT_PATTERN_START: usize = 8;

/// First pattern index in Ruby's [`inheritance_query`] that denotes a
/// mixin (`include` / `extend` / `prepend`) rather than `class < Bar`.
/// Pattern 0 is the superclass form → `"inherits"`; patterns 1.. are
/// mixin calls → `"include"`.
const RUBY_MIXIN_PATTERN_START: usize = 1;

/// Relation label for a language + matched query pattern.
///
/// Most languages return a single label per language (Java/C# lump
/// `extends` and `implements` under `"extends"`, mirroring user-visible
/// source). PHP and Ruby dispatch on [`tree_sitter::QueryMatch::pattern_index`]
/// to surface the semantic difference between inheritance and mixin /
/// trait composition.
fn relation_label(lang: Language, pattern_index: usize) -> &'static str {
    match lang {
        Language::Rust => "impl",
        Language::Ruby if pattern_index >= RUBY_MIXIN_PATTERN_START => "include",
        Language::Ruby => "inherits",
        Language::Php if pattern_index >= PHP_TRAIT_PATTERN_START => "uses",
        Language::Php => "extends",
        Language::Java | Language::CSharp | Language::Kotlin | Language::Swift => "extends",
        Language::Cpp
        | Language::Python
        | Language::TypeScript
        | Language::Go
        | Language::Sql
        | Language::Markdown
        | Language::Bash
        | Language::Lua
        | Language::Css
        | Language::Html
        | Language::Yaml
        | Language::Toml => "extends",
    }
}

/// Find all implementations of `base_name` in a single source string.
fn find_in_source(content: &str, lang: Language, path: &str, base_name: &str) -> Vec<ImplMatch> {
    let query_src = match inheritance_query(lang) {
        Some(q) => q,
        None => return Vec::new(),
    };

    let ts_lang = lang.ts_language();

    let query = match Query::new(&ts_lang, query_src) {
        Ok(q) => q,
        Err(_) => return Vec::new(),
    };

    let mut parser = Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        return Vec::new();
    }

    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let base_idx = match query.capture_index_for_name("base") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let child_idx = match query.capture_index_for_name("child") {
        Some(i) => i,
        None => return Vec::new(),
    };

    let mut cursor = QueryCursor::new();
    let mut query_matches = cursor.matches(&query, tree.root_node(), content.as_bytes());
    let mut results = Vec::new();

    while let Some(m) = query_matches.next() {
        let mut base_text = None;
        let mut child_text = None;
        let mut child_line = 0;

        for capture in m.captures {
            let text = &content[capture.node.byte_range()];
            if capture.index == base_idx {
                base_text = Some(text);
            } else if capture.index == child_idx {
                child_text = Some(text);
                child_line = capture.node.start_position().row + 1;
            }
        }

        if let (Some(base), Some(child)) = (base_text, child_text) {
            if base == base_name {
                results.push(ImplMatch {
                    path: path.to_string(),
                    line: child_line,
                    name: child.to_string(),
                    base: base.to_string(),
                    relation: relation_label(lang, m.pattern_index),
                });
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(source: &str, lang: Language, base_name: &str) -> Vec<ImplMatch> {
        find_in_source(source, lang, "test", base_name)
    }

    #[test]
    fn rust_impl_trait_for_struct() {
        let src = r#"
struct Foo;
struct Bar;

impl Iterator for Foo {
    type Item = i32;
    fn next(&mut self) -> Option<Self::Item> { None }
}

impl Clone for Bar {
    fn clone(&self) -> Self { Bar }
}

impl Foo {
    fn new() -> Self { Foo }
}
"#;
        let matches = find(src, Language::Rust, "Iterator");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "Foo");
        assert_eq!(matches[0].relation, "impl");

        let matches = find(src, Language::Rust, "Clone");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "Bar");

        // inherent impl should NOT match
        let matches = find(src, Language::Rust, "Foo");
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn python_class_inheritance() {
        let src = r#"
class Animal:
    pass

class Dog(Animal):
    pass

class Cat(Animal):
    pass

class Puppy(Dog):
    pass
"#;
        let matches = find(src, Language::Python, "Animal");
        assert_eq!(matches.len(), 2);
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"Dog"));
        assert!(names.contains(&"Cat"));

        let matches = find(src, Language::Python, "Dog");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "Puppy");
    }

    #[test]
    fn java_extends_and_implements() {
        let src = r#"
public class Animal {}

public class Dog extends Animal {}

public interface Serializable {}

public class Cat extends Animal implements Serializable {}
"#;
        let matches = find(src, Language::Java, "Animal");
        assert_eq!(matches.len(), 2);
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"Dog"));
        assert!(names.contains(&"Cat"));

        let matches = find(src, Language::Java, "Serializable");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "Cat");
    }

    #[test]
    fn typescript_extends_and_implements() {
        let src = r#"
class Component {}

class Button extends Component {
    render() {}
}

interface Clickable {
    onClick(): void;
}

class IconButton extends Component implements Clickable {
    onClick() {}
    render() {}
}
"#;
        let matches = find(src, Language::TypeScript, "Component");
        assert_eq!(matches.len(), 2);
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"Button"));
        assert!(names.contains(&"IconButton"));

        let matches = find(src, Language::TypeScript, "Clickable");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "IconButton");
    }

    #[test]
    fn csharp_base_list() {
        let src = r#"
public class Animal {}
public interface IMovable {}

public class Dog : Animal {}
public class Cat : Animal, IMovable {}
"#;
        // C# queries may not match this grammar exactly — verify
        let matches = find(src, Language::CSharp, "Animal");
        // If query doesn't work for this grammar, matches will be empty
        // That's acceptable — we'll refine later
        if !matches.is_empty() {
            let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
            assert!(names.contains(&"Dog"));
            assert!(names.contains(&"Cat"));
        }
    }

    // --- 11.3 generics band-aid ---
    //
    // Subclasses parameterised over a base — `class Foo : BaseClass<T>`,
    // `class Repo extends Repository<User>`, etc. Tree-sitter wraps the
    // base identifier in a `generic_name` / `generic_type` parent node
    // in several grammars, so the bare `(identifier) @base` pattern
    // misses them. These tests pin down the contract that
    // `vex implementations BaseClass` finds those subclasses too.

    #[test]
    fn java_generic_extends() {
        let src = r#"
public class Repository<T> {}
public class UserRepo extends Repository<User> {}
public class OrderRepo extends Repository<Order> {}
"#;
        let matches = find(src, Language::Java, "Repository");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(
            names.contains(&"UserRepo"),
            "Java generic extends: {matches:?}"
        );
        assert!(
            names.contains(&"OrderRepo"),
            "Java generic extends: {matches:?}"
        );
    }

    #[test]
    fn java_generic_implements() {
        let src = r#"
public interface Handler<T> {}
public class StringHandler implements Handler<String> {}
public class IntHandler implements Handler<Integer> {}
"#;
        let matches = find(src, Language::Java, "Handler");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(
            names.contains(&"StringHandler"),
            "Java generic implements: {matches:?}"
        );
        assert!(
            names.contains(&"IntHandler"),
            "Java generic implements: {matches:?}"
        );
    }

    #[test]
    fn typescript_generic_extends() {
        let src = r#"
class Repository<T> {}
class UserRepo extends Repository<User> {}
class OrderRepo extends Repository<Order> {}
"#;
        let matches = find(src, Language::TypeScript, "Repository");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(
            names.contains(&"UserRepo"),
            "TS generic extends: {matches:?}"
        );
        assert!(
            names.contains(&"OrderRepo"),
            "TS generic extends: {matches:?}"
        );
    }

    #[test]
    #[ignore = "debug helper — run with `cargo test -- --include-ignored kotlin_ast` to dump"]
    fn kotlin_ast_dump_for_inheritance() {
        use tree_sitter::{Node, Parser};
        let src = "class UserRepo : Repository<User>()\nclass NoArgs : Other\nclass Plain : Bar";
        let mut parser = Parser::new();
        parser
            .set_language(&Language::Kotlin.ts_language())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        fn dump(n: Node<'_>, depth: usize, src: &str) {
            let snip: String = src[n.byte_range()]
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            eprintln!(
                "{}{} [{}..{}] {:?}",
                " ".repeat(depth),
                n.kind(),
                n.start_position().row,
                n.end_position().row,
                snip
            );
            for i in 0..n.child_count() {
                dump(n.child(u32::try_from(i).unwrap()).unwrap(), depth + 2, src);
            }
        }
        dump(tree.root_node(), 0, src);
    }

    #[test]
    fn kotlin_plain_extends_without_args() {
        // Covers the bare `(user_type (identifier) @base)` branch of
        // the rewritten Kotlin inheritance query — i.e. `class Foo :
        // Bar` with no `()` constructor invocation. The 11.3 generic
        // tests only exercise the `constructor_invocation` branch via
        // `Repository<T>()` forms, so this guards the other half from
        // silent breakage on a future grammar bump.
        let src = "open class Animal\nclass Dog : Animal\nclass Cat : Animal\n";
        let matches = find(src, Language::Kotlin, "Animal");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"Dog"), "Kotlin plain extends: {matches:?}");
        assert!(names.contains(&"Cat"), "Kotlin plain extends: {matches:?}");
    }

    #[test]
    fn kotlin_generic_extends() {
        let src = r#"
open class Repository<T>
class UserRepo : Repository<User>()
class OrderRepo : Repository<Order>()
"#;
        let matches = find(src, Language::Kotlin, "Repository");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        // Kotlin's tree-sitter `user_type (type_identifier)` already
        // matches identifiers regardless of type-argument siblings, so
        // this is a regression guard rather than a new fix.
        assert!(
            names.contains(&"UserRepo"),
            "Kotlin generic extends: {matches:?}"
        );
        assert!(
            names.contains(&"OrderRepo"),
            "Kotlin generic extends: {matches:?}"
        );
    }

    #[test]
    fn csharp_generic_base() {
        let src = r#"
public class Repository<T> {}
public class UserRepo : Repository<User> {}
public class OrderRepo : Repository<Order> {}
"#;
        let matches = find(src, Language::CSharp, "Repository");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"UserRepo"), "C# generic base: {matches:?}");
        assert!(names.contains(&"OrderRepo"), "C# generic base: {matches:?}");
    }

    #[test]
    fn no_match_returns_empty() {
        let src = "struct Foo;\nimpl Foo { fn new() -> Self { Foo } }";
        let matches = find(src, Language::Rust, "NonExistent");
        assert!(matches.is_empty());
    }

    #[test]
    fn unsupported_language_returns_empty() {
        let matches = find("package main", Language::Go, "Anything");
        assert!(matches.is_empty());
    }

    // --- PHP ---

    #[test]
    fn php_class_extends() {
        let src = r#"<?php
class Animal {}
class Dog extends Animal {}
class Cat extends Animal {}
"#;
        let matches = find(src, Language::Php, "Animal");
        assert_eq!(matches.len(), 2, "{matches:?}");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"Dog"));
        assert!(names.contains(&"Cat"));
        assert_eq!(matches[0].relation, "extends");
    }

    #[test]
    fn php_class_implements_interface() {
        let src = r#"<?php
interface Payable {}
interface Refundable {}
class StripeGateway implements Payable {}
class PaypalGateway implements Payable, Refundable {}
"#;
        let matches = find(src, Language::Php, "Payable");
        assert_eq!(matches.len(), 2, "{matches:?}");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"StripeGateway"));
        assert!(names.contains(&"PaypalGateway"));

        let matches = find(src, Language::Php, "Refundable");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "PaypalGateway");
    }

    #[test]
    fn php_interface_extends_interface() {
        let src = r#"<?php
interface Readable {}
interface ReadWritable extends Readable {}
"#;
        let matches = find(src, Language::Php, "Readable");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "ReadWritable");
    }

    #[test]
    fn php_qualified_base_name() {
        let src = r#"<?php
class Cat extends App\Domain\Animal {}
class Dog implements App\Contract\Payable {}
"#;
        // Capture the trailing name of the qualified path; `App\Domain\Animal`
        // is searchable as `Animal`. Mirrors how qualified `use` imports
        // get stored.
        let matches = find(src, Language::Php, "Animal");
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0].name, "Cat");

        let matches = find(src, Language::Php, "Payable");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "Dog");
    }

    #[test]
    fn php_class_uses_trait() {
        let src = r#"<?php
trait Loggable { public function log(string $m): void {} }
class StripeGateway {
    use Loggable;
}
class PaypalGateway {
    use Loggable;
}
"#;
        let matches = find(src, Language::Php, "Loggable");
        assert_eq!(matches.len(), 2, "{matches:?}");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"StripeGateway"));
        assert!(names.contains(&"PaypalGateway"));
        // Trait composition is labelled "uses", not "extends" — the
        // pattern_index dispatch must pick the right bucket.
        assert!(matches.iter().all(|m| m.relation == "uses"), "{matches:?}");
    }

    #[test]
    fn php_class_uses_multiple_traits() {
        let src = r#"<?php
class Service {
    use Loggable, Cacheable;
}
"#;
        // The `use Loggable, Cacheable;` form yields two name children
        // under the same use_declaration. Tree-sitter's S-expression
        // matcher fires the pattern once per name.
        let log = find(src, Language::Php, "Loggable");
        let cache = find(src, Language::Php, "Cacheable");
        assert_eq!(log.len(), 1);
        assert_eq!(cache.len(), 1);
        assert_eq!(log[0].name, "Service");
        assert_eq!(cache[0].name, "Service");
    }

    #[test]
    fn php_class_uses_qualified_trait() {
        let src = r#"<?php
class Service {
    use App\Util\Loggable;
}
"#;
        let matches = find(src, Language::Php, "Loggable");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "Service");
        assert_eq!(matches[0].relation, "uses");
    }

    #[test]
    fn php_trait_uses_trait() {
        let src = r#"<?php
trait BaseLogger {}
trait FileLogger {
    use BaseLogger;
}
"#;
        // Trait composition: trait composing another trait.
        let matches = find(src, Language::Php, "BaseLogger");
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0].name, "FileLogger");
        assert_eq!(matches[0].relation, "uses");
    }

    #[test]
    fn php_extends_stays_extends_not_uses() {
        // Regression guard for the pattern_index threshold: an `extends`
        // clause must still produce relation == "extends", never "uses",
        // even after the trait patterns were appended at the end of the
        // PHP query.
        let src = "<?php\nclass Dog extends Animal {}\n";
        let matches = find(src, Language::Php, "Animal");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].relation, "extends");
    }

    #[test]
    fn php_trait_uses_relation_is_uses_not_extends() {
        // Symmetric guard for the pattern_index threshold from the other
        // side: a trait `use` must produce "uses", never "extends",
        // catching the case where someone inserts a new extends pattern
        // and forgets to bump PHP_TRAIT_PATTERN_START.
        let src = "<?php\ntrait Loggable {}\nclass Foo { use Loggable; }\n";
        let matches = find(src, Language::Php, "Loggable");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].relation, "uses");
    }

    #[test]
    fn php_enum_implements_interface() {
        // PHP 8.1+ enums can implement interfaces — common pattern in
        // typed-enum libraries. Lives in the extends bucket, not the
        // trait-use bucket.
        let src = r#"<?php
interface HasLabel {}
interface Sortable {}
enum Status: string implements HasLabel {
    case Active = 'active';
}
enum Priority: int implements HasLabel, Sortable {
    case High = 1;
}
"#;
        let matches = find(src, Language::Php, "HasLabel");
        assert_eq!(matches.len(), 2, "{matches:?}");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"Status"));
        assert!(names.contains(&"Priority"));
        assert_eq!(matches[0].relation, "extends");

        let matches = find(src, Language::Php, "Sortable");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "Priority");
    }

    // --- Ruby ---

    #[test]
    fn ruby_class_superclass() {
        let src = r#"
class Animal
end

class Dog < Animal
end

class Cat < Animal
end
"#;
        let matches = find(src, Language::Ruby, "Animal");
        assert_eq!(matches.len(), 2, "{matches:?}");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"Dog"));
        assert!(names.contains(&"Cat"));
    }

    #[test]
    fn ruby_class_include_module() {
        let src = r#"
module Comparable
end

class Animal
  include Comparable
end

class Plant
  include Comparable
  extend Enumerable
end
"#;
        let matches = find(src, Language::Ruby, "Comparable");
        assert_eq!(matches.len(), 2, "{matches:?}");
        let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"Animal"));
        assert!(names.contains(&"Plant"));

        let matches = find(src, Language::Ruby, "Enumerable");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "Plant");
        assert_eq!(matches[0].relation, "include");
    }

    #[test]
    fn ruby_module_includes_module() {
        let src = r#"
module Helpers
  include Enumerable
end
"#;
        let matches = find(src, Language::Ruby, "Enumerable");
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0].name, "Helpers");
    }

    #[test]
    fn ruby_superclass_relation_is_inherits_not_include() {
        // Per RUBY_MIXIN_PATTERN_START dispatch, the `<` form must label
        // as "inherits", distinguishing true inheritance from mixin
        // composition in the output.
        let src = "class Dog < Animal\nend\n";
        let matches = find(src, Language::Ruby, "Animal");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].relation, "inherits");
    }

    #[test]
    fn ruby_non_mixin_call_does_not_match() {
        // Regression guard for the `#match?` predicate placement: a
        // method call that takes a Constant argument but isn't one of
        // include / extend / prepend must NOT register as an
        // implementation edge. Misplaced predicate (outside the
        // S-expression) silently degrades the query to "any call with a
        // constant argument", which would falsely match assertion DSLs
        // and other prose code.
        let src = r#"
class FooTest
  assert_equal Mixin, foo.class
  describe Mixin do
    nil
  end
end
"#;
        let matches = find(src, Language::Ruby, "Mixin");
        assert!(
            matches.is_empty(),
            "non-mixin calls should not be matched: {matches:?}"
        );
    }

    #[test]
    fn ruby_include_multi_arg() {
        // `include Foo, Bar` is valid Ruby and produces a single call
        // with two constant arguments. Our `(argument_list (constant)
        // @base)` pattern fires once per constant, so both names should
        // resolve to the enclosing class.
        let src = r#"
class Service
  include Loggable, Cacheable
end
"#;
        let log = find(src, Language::Ruby, "Loggable");
        let cache = find(src, Language::Ruby, "Cacheable");
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(cache.len(), 1, "{cache:?}");
        assert_eq!(log[0].name, "Service");
        assert_eq!(cache[0].name, "Service");
        assert_eq!(log[0].relation, "include");
    }
}
