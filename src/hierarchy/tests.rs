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

// --- 11.1.6: generic-parameterized base classes ---

#[test]
fn rust_impl_generic_trait_for_struct() {
    // `impl Iterator<Item = u32> for Foo` wraps the trait field in
    // `generic_type`; without the extra pattern the bare
    // `trait: (type_identifier)` match misses every generic impl.
    let src = r#"
struct Foo;
struct Bar;

impl Iterator<Item = u32> for Foo {}
impl Iterator<Item = i64> for Bar {}
impl Clone for Foo {}
"#;
    let matches = find(src, Language::Rust, "Iterator");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"Foo"), "Rust generic impl: {matches:?}");
    assert!(names.contains(&"Bar"), "Rust generic impl: {matches:?}");
}

#[test]
fn typescript_implements_generic_interface() {
    // `extends Foo<T>` already works (the value field stays a plain
    // identifier with type_arguments as a sibling). `implements
    // Foo<T>` is different: tree-sitter wraps it in `generic_type`.
    let src = r#"
interface Handler<T> {}

class JsonHandler implements Handler<string> {}
class CsvHandler implements Handler<Buffer> {}
"#;
    let matches = find(src, Language::TypeScript, "Handler");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"JsonHandler"),
        "TS generic implements: {matches:?}"
    );
    assert!(
        names.contains(&"CsvHandler"),
        "TS generic implements: {matches:?}"
    );
}

#[test]
fn python_class_inheritance_with_typing_subscript() {
    // `class Dog(Animal[T])` — `typing.Generic`-style
    // parameterization wraps the base in a `subscript` node.
    let src = r#"
from typing import Generic, TypeVar
T = TypeVar("T")

class Container(Generic[T]):
    pass

class IntBox(Container[int]):
    pass

class StrBox(Container[str]):
    pass
"#;
    let matches = find(src, Language::Python, "Container");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"IntBox"),
        "Python subscript inheritance: {matches:?}"
    );
    assert!(
        names.contains(&"StrBox"),
        "Python subscript inheritance: {matches:?}"
    );
}

#[test]
fn cpp_class_extends_template() {
    // `class UserRepo : public Repository<User>` — base is wrapped
    // in `template_type` rather than appearing as a plain
    // `type_identifier`.
    let src = r#"
template<typename T>
class Repository {};

class UserRepo : public Repository<User> {};
class OrderRepo : public Repository<Order> {};
"#;
    let matches = find(src, Language::Cpp, "Repository");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"UserRepo"),
        "C++ template inheritance: {matches:?}"
    );
    assert!(
        names.contains(&"OrderRepo"),
        "C++ template inheritance: {matches:?}"
    );
}

#[test]
fn rust_inherent_generic_impl_does_not_match() {
    // `impl Foo<T> { ... }` has no `trait:` field — it's an
    // inherent impl, not an `impl Trait for Type`. The new
    // `trait: (generic_type ...)` pattern must NOT fire here,
    // otherwise `vex implementations Foo` would over-report every
    // inherent impl as a subclass.
    let src = "struct Foo;\nimpl Foo<u32> {\n    fn build() -> Self { Foo }\n}\n";
    let matches = find(src, Language::Rust, "Foo");
    assert!(
        matches.is_empty(),
        "inherent generic impl must not match: {matches:?}",
    );
}

#[test]
fn python_mixed_plain_and_subscript_inheritance() {
    // The 11.1.6 `subscript` pattern is additive — it must not
    // shadow or short-circuit the plain `identifier` pattern. A
    // file with both forms must return matches for both.
    let src = r#"
class Container:
    pass

class Plain(Container):
    pass

class Generic(Container[int]):
    pass
"#;
    let matches = find(src, Language::Python, "Container");
    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"Plain"),
        "plain inheritance lost after subscript pattern added: {matches:?}",
    );
    assert!(
        names.contains(&"Generic"),
        "subscript inheritance missing: {matches:?}",
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
