# Known limitations (v1.9-pre)

This document lists vex's known coverage gaps. Each entry has a concrete
repro, an explanation, and a workaround. Agents reading this should
treat the items below as **the kind of result vex cannot find — reach
for `vex grep` or shell tools when you suspect a hit lives outside vex's
extraction model**.

Updated 2026-05-25 after external review of v1.8.2.

---

## 1. `vex callers` coverage gaps

**What works:** every call site that lives inside a `function_definition`
/ `method_definition` / `closure` node is recorded as a caller in the
v6 persistent call graph. **Module-scope expressions are recorded as a
synthetic `<module:relpath>` caller (Phase 14.1). Function/method-level
decorators in Python and Java emit forward edges `decorated_fn →
decorator_target` (Phase 14.2); Kotlin annotations and C# method/
constructor attributes do the same (Phase 14.2.2); TypeScript method
decorators and Rust outer attributes on fns/methods do the same via
sibling-adjacency pairing (Phase 14.2.1)** — `vex callers GetMapping`
lists every Spring handler; `vex callers get` lists every FastAPI
route function; `vex callers HttpGet` lists every ASP.NET controller
action; `vex callers JvmStatic` lists every Kotlin function annotated
`@JvmStatic`; `vex callers Get` lists every Nest.js method-level
`@Get`; `vex callers test` lists every Rust `#[tokio::test]`.

**What is still invisible:**

- **TypeScript property / parameter decorators** (`@inject() svc: Svc;`,
  `constructor(@inject() svc: Svc)`) — common in Nest.js DI but
  properties / parameters are not FnDef symbols, so the decorator
  has no anchor. Future phase if real users ask.
- **Rust `#[derive(...)]` macros** — intentionally filtered out by
  attribute path-head name. Compile-time codegen, not runtime call
  edges. Other Rust attributes (`#[tokio::test]`, `#[serde(...)]`,
  `#[wasm_bindgen]`, `#[allow(...)]`, etc.) are kept.
- **String-resolved references.** `"media_server.main:create_app"`
  passed to uvicorn, `task_name="celery_task.fire"` — vex sees the
  string literal, never resolves it. **Phase 15 territory.**
- **`eval` / `exec` / reflection-style dispatch.** Out of scope by
  construction.

**Class-body call sites** (e.g. `db_url = make_dsn()` inside a class body)
are currently attributed to the **synthetic `<module:path>` symbol** as
well — the sentinel fires for any call site outside a `function`/`method`/
`closure` scope, regardless of whether it's at module scope or inside a
class body. The same applies to Kotlin class-body initializers
(`class Foo { val x = compute() }`), Kotlin `init { … }` blocks, C#
static field initializers (`static int x = Init();`), and C# property
getters/setters — all attribute to `<module:path>` rather than to the
enclosing class. The `edge.line` still points to the actual call site
so the location is accurate, but the *caller name* is the module, not
the class. A follow-up could synthesise a per-class `<class:Foo>`
caller; track as Phase 14.5 if real users ask.

**Why:** the call-graph extractor walks tree-sitter `call_expression`
nodes and attributes them to their innermost enclosing function. With
Phase 14.1 a call site outside any function is now attributed to a
synthetic `<module:path>` symbol — invisible to `vex search` / `vex
outline`, but visible as a caller in `vex callers`.

**Module-scope repro (now resolved):**

```python
# media_server/main.py
def create_app(): ...

app = create_app()   # ← now reported as <module:media_server/main.py>
```

```
$ vex callers create_app
<module:media_server/main.py>  media_server/main.py:411
```

**Decorator repro (now resolved for Python + Java + Kotlin + C# + TypeScript + Rust):**

```python
@app.get("/items")
def list_items(): ...   # ← Phase 14.2: edge list_items → get
```

```
$ vex callers get
list_items  media_server/main.py:411
```

```java
class Controller {
    @GetMapping("/users")
    public Response listUsers() { ... }   // ← edge listUsers → GetMapping
}
```

```kotlin
@JvmStatic
fun helper() { ... }   // ← Phase 14.2.2: edge helper → JvmStatic
```

```csharp
[HttpGet("/users")]
public Response GetUsers() { ... }   // ← edge GetUsers → HttpGet
```

```typescript
class C {
    @Get("/x")
    handler() { ... }   // ← Phase 14.2.1: edge handler → Get
}
```

```rust
#[tokio::test]
fn it_works() { ... }   // ← Phase 14.2.1: edge it_works → test
```

For class-level decorators, TS property/parameter decorators, and Rust
`#[derive(...)]` the gap remains — see "What is still invisible" above
for the deferred phase numbers and the intentional exclusions.

**Rightmost-identifier convention has a collision surface.** Because
`@app.get("/x")` → `get` and a literal `dict.get(key)` call also →
`get`, `vex callers get` returns both decorator-edge handlers AND any
function that does a regular `.get(...)` call. Narrow with `--include
'src/routes/**'` or `--exclude 'src/utils/**'` if the corpus mixes the
two populations. Same convention applies to method calls already
(`obj.method() → method`); decorator edges just expand the pool.

**Self-edge artifact when fn name matches decorator-rightmost id.** A
fn whose name happens to equal the rightmost identifier of its own
decorator/attribute produces a self-edge. The most common real case
is Rust's `#[tokio::main] fn main()` → edge `main → main`; Python's
`def get(): @app.get(...)` would similarly emit `get → get`. The
self-edge is technically correct under the rightmost-id convention
and would be wrong to suppress generically (a fn named `get` that
genuinely calls `something.get(...)` in its body MUST have a `get`
callee), so we accept the artifact. `vex callees main` on a tokio
binary will show one synthetic `main` entry alongside the function's
real body calls; readers should expect this. Same pattern applies to
`#[test] fn test()`, `@bound fn bound()`, etc.

**Double-invocation decorators silently dropped.** TypeScript's
`@factory()(arg)` form — a decorator factory immediately invoked
with a second argument — has the outer `call_expression`'s
`function:` slot as another `call_expression` (not an `identifier`
or `member_expression`). Our SCM patterns require the function slot
to be a name node, so this pattern produces no edge. Rare in
practice; track separately if real reports surface.

**Decorator factories** like `@functools.lru_cache(maxsize=128)`,
`@click.command()`, `@retry(max_attempts=3)` emit an edge to the
factory name (`lru_cache` / `command` / `retry`), since the factory
IS the call expression that wraps the decorated function. Querying
`vex callers lru_cache` correctly returns every memoised function. In
`vex callees`, the factory name appears alongside regular body calls.

**Workaround for the remaining gaps:** `vex grep '\bcreate_app\b'`
returns every textual mention. For the inheritance case specifically
(`class Foo(Bar):`), `vex implementations Bar` is the right tool — it
captures the supertype reference.

### Inheritance by `vex paths` and `vex reachable` (Phase 11.5)

The multi-hop `vex paths <From> <To>` and `vex reachable <Target>`
commands traverse the same persistent `CallEdge` section that backs
`vex callers`. **Every limit in this section propagates to them**:
- Module-level expressions reach the graph only as synthetic
  `<module:path>` callers (Phase 14.1 sentinel). A `paths main foo`
  query that should go through a top-level `foo()` call surfaces with
  the synthetic caller, not the conceptual "module main".
- Class-level decorators (Phase 14.6) are visible via the same module
  sentinel; per-class scoping is not.
- Dynamic dispatch, string-resolved factories, task-queue `.delay()`
  bindings, `getattr` reflection, and macro expansions (see §3 below)
  produce no edges, so they are also invisible to `paths` / `reachable`.
  A "this caller chain should exist but doesn't" report on `paths`
  almost always traces back to one of these missing edges.

`vex tests-for <Sym>` (Phase 13.10) is a post-filter on
`vex reachable`, so it inherits the entire list above on top of its
own path-pattern + name-heuristic limits.

---

## 2. `vex usages` coverage is uneven across languages

The legacy refs FST is populated in two ways depending on the
language:

| Tier | Languages | Extraction |
| --- | --- | --- |
| T1 (AST identifier walk) | Rust, TypeScript, Python, C#, C++, Go, Java | Walk every `identifier` node in the AST, skipping comments and string literals. Captures inheritance refs (`class Foo(Bar):` → `Bar`), call targets, type annotations, all real usages. |
| T2 (line-scan regex) | the other 12 languages | Regex over each line: any identifier-shaped token becomes a ref. Higher false-positive rate (matches text inside strings / comments depending on whitespace), but covers grammars without an AST filter yet. |

**`--strict` is the precision upgrade** for T1 languages (Phase 11.1,
shipped in v1.8.0). It reads the v5 `reference_edges` section produced
by the scope binder: every ref is type-aware, cross-file imports are
resolved, no false positives from same-named identifiers in unrelated
scopes. Use it for refactoring on Rust / TypeScript / Python / C# /
C++. **C++ cross-file via `#include "..."`** is v1.14+; see
[§4a](#4a-c-include-driven-cross-file-resolution-v114) for the v1.14
contract and remaining gaps (class members, system headers).

**What is invisible in both modes:**

- Decorator-based dispatch (`@app.route(...)`)
- String-resolved targets (`"module.path:function"`)
- Reflection / `getattr` / dynamic imports
- Macro-expanded references (Rust `macro_rules!`, C++ `#define`)

**`mode: "fst_lookup"` in `--why` output** (Phase 14.4 rename, was
`text_scan` in v1.8 – v1.9). The underlying data source is the refs
FST, not a live tree-sitter scan. For T1 languages the FST itself was
populated from an AST walk; for T2 it came from a regex. The legacy
label is still emitted as `mode_legacy` for v1.9.x consumers — slated
for removal in v1.12.

**Repro:**

```python
class MediaController: ...

class VideoController(MediaController): ...   # T1 AST walk captures `MediaController`
class AudioController(MediaController): ...   # same
```

```
$ vex usages MediaController
media_server/video.py:3
media_server/audio.py:3
...
```

If `vex usages MediaController` returns `[]` on a T1 language, the
likely cause is a stale index from before v1.8.0. Re-run `vex index`
to rebuild with the AST-walk refs.

---

## 3. Dynamic / runtime-resolved dispatch is invisible

vex is a **static-analysis** tool. It indexes what tree-sitter can
parse. The following patterns produce no edges in any of `usages` /
`callers` / `callees` / `implementations`:

| Pattern | Example | vex visibility |
| --- | --- | --- |
| Decorator dispatch (Python, Java, Kotlin, C#, TS, Rust) | `@app.get("/")`, `@GetMapping("/x")`, `@JvmStatic`, `[HttpGet("/x")]`, `@Get("/x")`, `#[tokio::test]` | Phase 14.2 (Python+Java) + Phase 14.2.2 (Kotlin+C#) + Phase 14.2.1 (TS+Rust sibling-adjacency): edge `decorated_fn → decorator_target` (rightmost identifier of path wins; args ignored). |
| Class-level decorator | `@dataclass class Foo:`, `@Component class Bar`, `[ApiController] class Baz` | Phase 14.6 (v1.12.0): edge attributed to module scope (synthetic `<module:path>` caller via Phase 14.1 sentinel). Covers Python, Java, TypeScript, Kotlin, C#. Rust `#[derive(...)]` intentionally excluded. |
| String-resolved factory | `uvicorn.run("main:app")` | Literal string only; no edge from `uvicorn.run` to `main.app`. |
| Task queues | `celery_task.delay()` | The `.delay()` call site is captured, but the bound task body is not linked. |
| `getattr` / reflection | `getattr(obj, name)()` | The bound target depends on a runtime value. |
| Dynamic imports | `importlib.import_module(name)` | Same. |
| Macro-expanded refs | Rust `macro_rules!` body, C `#define` | Tree-sitter sees the macro token, not the expansion. |

**Workaround:** combine `vex grep`, `vex pattern`, and your understanding
of the framework's conventions. For example, FastAPI route handlers
can be enumerated by:

```
$ vex pattern '@$ROUTER.get($_)' --lang python
$ vex pattern '@$ROUTER.post($_)' --lang python
```

For Celery tasks:

```
$ vex pattern '@$APP.task' --lang python
$ vex pattern '@celery.shared_task' --lang python
```

**`vex tests-for` inherits this.** Phase 13.10's `tests-for` walks the
same call-graph edges as `vex reachable`, so any test whose call to
the unit under test goes through macro-expanded code (Rust
`rstest::rstest` parameter cases, Python `@pytest.mark.parametrize`
generated variants), a JS `it()` / `describe()` block descriptor, or
a string-resolved factory is invisible to `tests-for`. Fall back to
`vex grep '<symbol-name>' tests/` when the heuristic returns less than
expected.

---

## 4. `vex usages` non-strict mode quality varies by language

When the index has no `reference_edges` section (built with
`--no-call-graph`, or T2 language outside the binder set), `vex usages`
falls back to the legacy refs FST. Quality notes:

- **T1 languages with `has_ast_ref_filter`** (Rust, TypeScript, Python,
  C#, C++, Go, Java): refs come from an AST walk that skips comments and
  plain string literals. False-positive rate is low; identifier
  collisions across scopes still produce noise.
- **T2 languages** (everything else — Kotlin, Swift, PHP,
  Ruby, etc.): refs come from a regex line-scan. Strings are not
  skipped. False positives where the symbol name appears in a doc
  comment, log message, or template literal.

**Recommendation:** for refactor-grade accuracy on T1 languages, always
use `--strict`. For everything else, treat `vex usages` results as a
starting set and filter manually.

---

## 4a. C++ `#include`-driven cross-file resolution (v1.14)

Before v1.14, every C++ ref to a symbol declared in a separate header
landed as `Unresolved` and produced **no `--strict` edge**. The one
working pattern was `using app::Gateway;` (which goes through
`BindTarget::Imported`). v1.14 adds a Pass-2 BFS over the quoted
`#include "..."` graph in `src/store/include_resolver.rs`: for each
`Unresolved` ref in a `.cpp` / `.h` / `.cxx` / etc. file, BFS walks
the transitively-included headers and resolves against the first
file that defines a symbol with the matching name.

**What works (v1.14+):**

- Free functions declared in a `.h` and called from a `.cpp` that
  `#include`s that header (direct or transitive).
- Classes / structs / namespaces referenced after `#include "foo.h"`
  by bare name (`Gateway gw;`, `app::Gateway gw;`).
- Mutual includes (`A.h ⇄ B.h`) — BFS uses a `HashSet<file_id>`
  visited set, terminates on cycles.
- Basename fallback when the exact relative path doesn't match:
  e.g. `#include "util.h"` resolves to `src/util.h` if that's the
  only `util.h` in the project; ambiguous matches break ties as
  same-dir > shortest-path-from-root > alphabetical (deterministic
  rather than always-correct).
- **Class member methods (v1.14.1+)** — `gw.Charge()` and
  `app::Gateway::static_method()` resolve cross-file: the SCM query
  now indexes method declarations and inline definitions as
  `SymbolKind::Method`, so Pass-2 finds them by name. Covered by
  `tests/cpp_strict_refs_test.rs`.

**What still does NOT resolve cross-file in C++:**

- **Ambiguous same-name picks** — basename fallback and the
  `name_to_global` lookup resolve to the *first* / tie-broken match
  when two headers or two symbols share a name across directories. The
  pick is deterministic, not always correct; vex does not model the
  `-I` search order that would disambiguate.
- **Operator overloads** — `operator==`, `operator()`, etc. The
  operator token is not an `identifier` node, so no ref is emitted and
  the call appears to have zero strict usages.
- **Multiple declarators** — in `int a, b, c;` only the first
  declarator is bound; refs to `b` / `c` may stay unresolved.
- **System headers** — `#include <vector>`, `#include <string>`.
  Tree-sitter classifies these as `system_lib_string`; the parser
  filters them out so `std::vector` stays `Unresolved`. The vex
  index doesn't contain libstdc++.
- **Macro includes** — `#include MY_HEADER`. The path is an
  `identifier` node, not a literal. Resolving these would require
  preprocessor state vex doesn't track.
- **`-I` compiler search paths** — vex only sees the project tree;
  it does not consume `compile_commands.json` or `-I` flags. Headers
  in `third_party/` or `vendor/` that the build feeds via `-I` are
  visible to vex only via basename fallback.
- **`using namespace std;`** — wildcard imports stay unresolved.
- **Conditional includes via `#ifdef`** — both branches of an
  `#ifdef WIN32 / #else` block contribute to the include graph
  (parser doesn't evaluate the macro), so resolution is
  optimistic across platforms.

Use `vex status` to confirm an index was built with v1.14+ resolution:
the line `C++ includes: yes` (text) / `"cpp_includes_processed": true`
(JSON) marks indexes that ran Pass-2. Pre-v1.14 indexes show
`C++ includes: no (run \`vex index\` to enable cross-file C++ refs)`
— rebuild to pick up the resolver.

---

## 4a.1 C# cross-file resolution

C# has no `#include` graph (it uses assembly / project references that
live outside the source tree), so there is no C++-style Pass-2 BFS.
Cross-file resolution for C# goes through two paths:

- **`using` alias / `using static`** — `using G = App.Lib.Gateway;` and
  `using static System.Math;` bind a name via `BindTarget::Imported`,
  resolved against the last path segment.
- **Single-candidate fallback** — a bare reference (e.g. `new Gateway()`
  after a plain `using App.Lib;` namespace import) is `Unresolved`, then
  resolves **only if exactly one** symbol of that name exists in the
  whole corpus. This is the dominant real-world path and is covered by
  `tests/csharp_strict_refs_test.rs`.

**What does NOT resolve cross-file in C#:**

- **Ambiguous names** — when two classes/methods share a name (e.g. a
  `Widget` in namespace `A` and another in `B`), the single-candidate
  fallback declines rather than guess, so the reference produces **no
  strict edge**. `vex usages --strict Widget` simply omits the call
  site. (If *every* ref in the project is ambiguous the
  `reference_edges` section is empty and `--strict` reports it needs a
  rebuild — re-running `vex index` won't change an inherently ambiguous
  corpus.)
- **Namespace wildcard imports** — `using App.Lib;` does not itself bind
  member names; bare references only resolve via the unique-candidate
  fallback above, never by namespace membership.
- **Partial classes** — each `partial class Foo { … }` is a separate
  symbol; members are not merged across files, so a cross-file ref to a
  member declared in another `partial` part may not resolve.
- **Extension methods** — `x.M()` where `M` is an extension method on a
  foreign type is not resolved to the extension's defining class.
- **Generic type parameters / destructuring** — deferred, same as the
  other binders.

---

## 4a.2 Go cross-file resolution

Go gained a scope binder so `vex usages --strict`, `vex impact`, and the
update cascade work on Go repos (before, Go had no binder and strict refs
were always unavailable). Like C#, there is no include graph; resolution
goes through:

- **Within-package, cross-file** — Go files in a directory sharing a
  `package` see each other's symbols by bare name. A bare `Helper()` call
  referencing a function in a sibling file is `Unresolved` in the binder
  and linked by Pass-2's single-candidate fallback (resolves only if the
  name is unique corpus-wide).
- **Cross-package `pkg.Symbol`** — in `util.DoThing()` the operand `util`
  is filtered (see below), but the trailing `DoThing` is a by-name ref
  that resolves to the unique `DoThing` symbol across packages. vex does
  not match the import path to the target package — it resolves by symbol
  name, so an unrelated `DoThing` in a third package makes it ambiguous.

**What does NOT resolve cross-file in Go:**

- **Unexported lowercase calls** — `is_meaningful_identifier` drops
  pure-lowercase identifiers without an underscore (`spin()`, `parse()`,
  package aliases like `mr`) before resolution, to keep the ref table
  free of prose nouns. Exported (`Spin`, `Println`) and snake_case names
  resolve; unexported single-word lowercase calls are invisible to
  `--strict`.
- **Ambiguous names** — when the same exported name is defined in two
  packages, the single-candidate fallback declines (no edge), same as C#.
- **`var` / `const` / `range` / type-switch bindings** — best-effort:
  these names are walked as refs rather than bound as locals, so a
  capitalized package-level `var Config = …` referenced elsewhere may
  resolve oddly. Function/method *input* params, receivers, and `:=`
  short vars ARE bound correctly. **Named return values** (`func F()
  (Out int)`) are walked for their type but not bound as locals — like
  the lowercase-call gap this only matters for the rare capitalized
  named return. Variadic params (`elems ...T`) ARE bound.
- **Generic type parameters** (`func F[K comparable]()`, `type Set[T
  any]`) — not bound; the `type_parameters` clause is not walked. Single-
  and two-letter names (`T`, `K`) are filtered out before resolution, so
  a phantom ref only arises for a 3+ char mixed-case constraint name
  referenced in the body.
- **Dot imports (`. "strings"`)** — names imported unqualified are not
  bound; bare references to them rely on the unique-candidate fallback.

---

## 4a.3 Java cross-file resolution

Java gained a scope binder so `vex usages --strict`, `vex impact`, and the
update cascade work on Java repos (before, Java had no binder and strict
refs were always unavailable). Like C#, there is no include graph;
resolution goes through:

- **Same-package, cross-file** — Java classes in a package see each
  other by simple name. A `Helper.doWork()` call referencing a method in
  a sibling file is `Unresolved` in the binder and linked by Pass-2's
  single-candidate fallback (resolves only if `doWork` is unique
  corpus-wide).
- **Single-type imports** — `import a.b.C;` binds the tail `C`; a later
  `C.member()` resolves `C` to the import and `member` to the unique
  symbol by name. `import static a.b.C.method;` binds `method`. vex
  resolves by symbol name, not by matching the import path to a package,
  so an unrelated same-named symbol elsewhere makes it ambiguous.
- **Lowercase-package noise is free-filtered** — `is_meaningful_identifier`
  drops the `java`/`util` segments of a qualified `java.util.List`, so
  qualified names can be walked generically without leaking package refs.

**What does NOT resolve cross-file in Java:**

- **Unexported lowercase calls** — pure-lowercase-without-underscore
  idents (`run()`, `parse()`) are dropped before resolution.
  Capitalized, camelCase (`doWork`), and snake_case names resolve.
- **Wildcard imports (`import a.b.*;`)** — like C# `using a.b;`, the
  unqualified members stay `Unresolved` unless uniquely named; the
  import itself binds nothing.
- **Ambiguous names** — when the same name is defined in two classes, the
  single-candidate fallback declines (no edge), same as C#.
- **Generic type parameters** (`<T extends Comparable<T>>`) — not bound;
  single-/two-letter names are filtered anyway.
- **Lambda params, enhanced-`for` loop vars, `catch` params,
  try-with-resources vars** — best-effort (walked as refs, not bound),
  harmless for idiomatic lowercase names; a capitalized one becomes a
  phantom `Unresolved` ref. Method/constructor params (incl. varargs
  `T...`), record components, and local vars ARE bound.
- **Anonymous classes / `enum_constant` bodies** — an anonymous class
  body (`new Runnable() { … }`) is contained in its own scope so members
  don't leak outward, but they resolve only locally (never promoted to
  `ModuleSymbol`, and not visible cross-file). Per-constant enum bodies
  are walked in the enum's class scope rather than a dedicated child.

---

## 4b. B1.2 incremental HNSW — first-update cold start (v1.15.0)

**What works:** from v1.15.0, `vex update --semantic` performs an
*incremental* HNSW update — usearch `load` → `remove` orphan hashes →
`add` new hashes → `save`, against the existing `index.hashes` sidecar.
Tombstone threshold of 25% triggers a transparent fall-back to full
rebuild for high-churn updates. The diff is enabled by the new
`index.bodytokens` sidecar (`VEXT` magic v1), which persists per-symbol
body_tokens so reconstructed symbols produce the same `context_hash` as
fresh-parsed ones.

**First update after upgrade is full rebuild.** Pre-v1.15 indexes have
no `index.bodytokens` sidecar. The first `vex update --semantic`
after upgrading reads `body_tokens: None` for unchanged symbols,
recomputes body-less hashes, and the diff against the v1.14.1
`index.hashes` sidecar treats every symbol as a remove+add → full
rebuild. `vex update --semantic` works correctly but doesn't benefit
from incremental until you run `vex index --semantic` ONCE to write
the sidecar. Confirm via `vex status`:

```
$ vex status
...
Body tokens: yes (incremental HNSW update enabled)
```

When the sidecar is absent the status line is:

```
Body tokens: no (run `vex index` to enable incremental HNSW update)
```

That means the next semantic update will be a full rebuild. The status
field is also surfaced in `vex status --format json` as
`body_tokens_persisted: bool`.

**Cold-start applies per-index, not globally.** Each project's index
needs the `vex index --semantic` priming separately. The `vex update`
that follows will be incremental.

**Non-semantic `vex update` is unaffected.** B1.2 only changes the
HNSW path. The structural (FST), BM25, and call-graph sections were
already rebuilt incrementally and continue to work as before — the
body_tokens persistence side-effect closes the legacy "BM25 recall
drops for unchanged symbols after `vex update`" warning.

**See also:** `docs/SEMANTIC.md` for the full pipeline spec (file
layout, hash-keyed HNSW, incremental contract, performance, disk-state
recovery matrix).

---

## 4c. `vex history` index — known limits (v1.15.0, Phase 14.8)

**What works:** opt in via `vex index --history` and `vex history
<Symbol>` returns ~10ms FST lookups (~675-1640× faster than the
v1.15.0 query-time walker that ships in the same release). Includes symbols whose name has been removed from
HEAD (the walker can't find these). `vex update` keeps the section
fresh: incremental walker on linear history, force-push detect with
warning, fast-path skip on no-new-commits, sticky-via-manifest. See
`docs/HISTORY-INDEX.md` for the full spec.

The list below is what the indexed path **does not** cover.

### Single ref only — indexed reflects HEAD at index time

The section is built from `HEAD` at the time of `vex index --history`.

**Phase 14.11 (v1.19):** `vex history <Symbol> --branch <non-HEAD>`
now transparently routes to the walker even when the sidecar is
present — no `--no-index` required, no warning. `--branch HEAD`
(literal) normalizes to absent and keeps the indexed fast path.

**Surviving walker limitation:** the walker only finds symbols whose
name still appears at the requested tip (it shells out to
`git grep <name> <rev>`). A symbol that existed on `feature` and
was deleted before its current tip is invisible. This is the same
constraint that already applied to `--no-index --branch X` queries
pre-14.11; it now applies to every `--branch X` query.

A future phase could index multiple refs (would also close §4c #3
per-commit time-travel); not in v1.

### Symbol-rename tracking — qualified (Phase 14.10)

**Pre-Phase-14.10 contract** (still applies when `index.rename_chains`
is absent or stale): a function renamed `foo` → `bar` surfaces as two
separate symbols. `vex history foo` cuts off at the rename; `vex
history bar` starts from the rename.

**v1.17 Phase 14.10:** chain detection runs unconditionally during
`vex index --history` (no opt-in flag). The chain builder computes
240-slot MinHash signatures over body_tokens, prunes candidates via
20×12 LSH bands, gates each candidate pair on `kind` match +
length-ratio ≥ 0.60 + body-Jaccard ≥ 0.70, composite-scores at
0.78·body + 0.22·sig (no-cosine path; MiniLM tiebreaker plumbing
deferred), greedy 1:1 assignment per commit pair, then union-find
merges across boundaries. Detected chains land in
`<index_dir>/index.rename_chains` (VEXR v1 magic, 48 B header
guarded by `body_tokens_hash` + `history_tip_sha_prefix`); `vex
history <name>` opens the sidecar via `RenameChainsReader::open_for_query`
and expands every FST hit through `follow_chain` so a query for
either side of the rename returns the full pre + post-rename
timeline.

Validation: CodeShovel oracle smoke run on commons-io (10 methods)
hits macro F1 0.947 / P 0.917 / R 1.000 — chain detection catches
all ground-truth files for every method including ones that survived
two consecutive class renames (`FilesystemObserver` →
`FileObserver` → `FileAlterationObserver`). See
`tests/oracle_codeshovel_test.rs`.

**Caveats (open):**

- **N:M (merge / split) is NOT detected.** Two methods merged into
  one or one method split into two are 1:N / N:1 transitions; the
  builder enforces greedy 1:1 per commit boundary and drops the
  losing edges. Every reviewed tool (CodeTracker, RefactoringMiner
  3.0, HistoryFinder) punts here too — N:M is the rename-detection
  hard problem.
- **Same-name overload chains in the same file are over-eager.**
  Vex doesn't disambiguate by method signature; two `read()`
  overloads with similar bodies in the same class file can chain
  together. The CodeShovel commons-io smoke produced 2/10 cases
  with F1 < 1.0 from this artifact (still P ≥ 0.50, R = 1.0).
- **Extract-method false positives** are gated by the length-ratio
  filter (`min/max ≥ 0.60`) per RefactoringMiner 3.0's empirical
  fix — but a refactor that extracts roughly half a method into a
  same-name helper can still chain if body-Jaccard stays high.
- **Cross-merge-boundary chains** during `vex update --history`
  are not detected: the merge path pads the prior side's body
  tokens with `None` (the previous history sidecar didn't persist
  them). Full rebuild restores coverage.
- **MiniLM tiebreaker is wired but dark.** `entry_context_hash` is
  fed `None` for every entry today. Activating it requires plumbing
  the in-memory `vectors` + `hashes` slice through to the chain
  builder.

### No per-commit time-travel

`vex callers <Symbol> @<sha>` and similar historical structural
queries are NOT supported. The history index is symbol-only — it
records when each symbol existed, not what called it at that commit.
A "historical call graph" would multiply storage by `commit_count`;
deferred until evidence demonstrates need.

### Convex-hull commit spans (architect H1 — accepted lossy)

If blob X appears at commits A → C and a DIFFERENT blob lives at B
in between (revert / cherry-pick), the entry's
`[first_commit_idx=A, last_commit_idx=C]` overstates continuity.
The "X existed at some point in [first, last]" sense is preserved.

**v1.16.0 Phase 14.9 Tier B.7:** pass `--exact-presence` to enumerate
the exact set of commits where the entry's blob lived in its file.
Implementation walks `git log` from HEAD (capped by
`--exact-presence-max-commits N`, default 500) and resolves blob SHAs
via batched `git cat-file --batch-check`; in-process result cache per
`(file_path, blob_sha)`. JSON output adds
`presence: { commits, walked, truncated }`; text mode adds a
`present: K / N commits in walked range` line per entry. Above the
cap, the entry falls back to the convex-hull span with
`presence_truncated: true` in JSON.

**Caveat — file-blob, not symbol-body equality.** Presence is
file-blob equality (`entry.blob_sha == git_cat_file(commit:path)`).
A commit where the symbol body is unchanged but a sibling symbol in
the same file moved will produce a different file blob → presence
narrows. True symbol-body presence would require per-commit
re-parsing of every blob (expensive); deferred. The current contract
matches the sidecar's `(symbol, blob)` row identity.

### Section size scales with history depth, not current symbols

Realistic ratios from the v1.15.0 perf bench:

```
                  index.vex   index.git_history   ratio
vex self-repo     1.8 MB      1.5 MB              84%
tokio             5.6 MB      19.5 MB             346%
```

Long-lived repos (tokio: 4346 commits, 586k history entries) produce
sections 3.5× the size of the main index. The Step 2 design napkin
"≤10% of index.vex" target was wrong. This is correct behaviour given
the per-(symbol, blob) row layout, not a bug. Use `--history-depth N`
to cap the walk if storage is constrained; `vex status` warns when a
cap was hit.

**v1.16.0 Phase 14.9 Tier B.6:** `vex status` now also emits an
informational line when `index.git_history > 2× index.vex`, naming
the ratio in absolute KB and suggesting `--history-depth N`. JSON
adds `git_history_size_bytes` so consumers can compute the ratio
themselves without re-statting.

### Submodule history is silently skipped

Mirrors the Phase 14.7 blob cache behaviour: submodule blobs aren't
in the main repo's git database, so `git cat-file --batch` reports
them missing and the builder drops them silently.

**v1.16.0 Phase 14.9 Tier B.6:** the silent-skip behaviour is
unchanged (architecturally required — submodule blobs cannot be
parsed without a separate `vex index` against the submodule's own
checkout), but `vex status` now warns when history is indexed AND
the project root has a `.gitmodules` file. JSON output adds
`has_submodules: bool`. Use `vex history` inside each submodule's
own checkout for per-submodule history.

### No back-fill from the walker's `git grep` probe

The walker can find symbols that `git grep --word-regexp` matches at
the chosen tip. The indexed path matches FST exactly (lowercased
symbol name); case differences are normalised, word-boundary
differences are not.

**v1.16.0 Phase 14.9 Tier B.8 (partial close):** when exact FST
lookup misses AND the query is identifier-shaped AND length ≥ 3,
the indexed path now walks the FST for keys starting with the
lowercased query and unions their posting lists (capped at 50
distinct names — `vex history inde` will surface `index`, `IndexReader`,
`index_path`, etc.). **Order is lexicographic, not relevance** — for
discovery-style queries on common prefixes, fall back to the walker
via `--no-index` which honours the full `git grep --word-regexp`
match set. The walker remains the authoritative escape hatch for
sub-3-char or non-identifier queries.

**See also:** `docs/HISTORY-INDEX.md` for the full pipeline spec.

---

## 4d. Stale `vex usages --strict` results after renaming a symbol (Phase 11.1.9 + 11.1.10)

**Closed in Phase 11.1.10 (Q4-B)** for the depth-1 case via the
`imported_by` cascade. The pre-11.1.10 description below is kept as
historical context.

### Current behavior (11.1.10+)

`vex update` consults the manifest's `imported_by` reverse-import map.
When file A is in `changed/deleted`, every importer recorded in
`imported_by["A"]` is added to the changed set and re-parsed. Their
fresh `bound_refs` resolve against the new name table, so a
rename in A correctly produces edges to the new target name from the
re-parsed importers. `vex usages --strict NewName` returns the
importer sites without a full `vex index`.

### Pre-11.1.10 (Phase 11.1.9, Q4-A only)

The 11.1.9 description below describes the old gap:

> **What works (post-11.1.9):** `vex update` preserves cross-file
> ref_edges from unchanged files via reconstruction from the previous
> index. Multi-candidate ambiguities are resolved by a path-tiebreak on
> the target's defining file — refs targeting `a::Helper` when both
> `a::Helper` and `b::Helper` exist resolve correctly.

**The gap.** When a *changed* file renames or deletes a symbol that
unchanged files referenced by name, the reconstructed ref's
`target_name` no longer resolves through the new index's
`name_to_global` map. Such edges are silently dropped from the new
`ref_edges` section. Aggregate dropped-count is surfaced via
`tracing::info!` when `RUST_LOG=vex=info`.

```rust
// Before update:
//   src/a.rs:   pub struct Old;
//   src/b.rs:   use crate::a::Old;  fn f() -> Old { … }   ← unchanged
// User edits a.rs:
//   src/a.rs:   pub struct New;     ← Old renamed to New
// vex update: b.rs's ref to Old → silently dropped from ref_edges.
//             vex usages --strict Old returns nothing (correct).
//             vex usages --strict New does NOT show b.rs's site
//             (this is the gap — Q4-B will fix).
```

**Workaround.** Run `vex index` (full rebuild) after any rename of an
exported symbol; the binder will re-walk b.rs and produce a fresh edge.
This matches the user's intuition: a refactor that touches APIs is the
right moment to fully reconcile.

**Closed in 11.1.11 (Q4-C).** The cascade now follows the `imported_by`
reverse graph **transitively** via BFS, bounded by
`CASCADE_MAX_DEPTH = 16`. A `c → b → a` rename where only `a` changes
re-parses both `b` (depth 1) and `c` (depth 2); deeper chains are
covered up to the cap. Cycles (`a ↔ b`) terminate via the visited-set
+ "already in changed_set" guard.

**Remaining Q4-C limits:**

1. **Re-export chains deeper than `CASCADE_MAX_DEPTH=16`.** Pathologically
   deep Python/TypeScript façade trees beyond 16 hops are not followed;
   the cascade logs a `tracing::warn!` when this happens so the operator
   knows a `vex index` may still be needed. The cap is generous — every
   idiomatic codebase observed bottoms out well below it.
2. **First-update bootstrap.** Pre-11.1.10 manifests have no
   `imported_by`; the first `vex update` after upgrading vex degrades
   to Q4-A behavior (no cascade) but populates the map for the second
   and subsequent updates.
3. **Coarse granularity.** Cascade fires whenever a changed file is in
   the reverse map, even when the edit didn't shift any exports.
   Over-invalidation costs a re-parse per importer; correctness is
   preserved. A future phase may tighten to per-symbol granularity if
   profile evidence justifies it.

---

## 5. `vex grep` is the right fallback

Whenever vex's indexed surface misses something the user can see in the
source, `vex grep <pattern>` is the textual-content escape hatch. It's
slower (~50 ms per query vs ~4 ms FST lookup) but exhaustive. The
guidance for agents:

> **Tool-selection pitfall: `vex search Foo` for an undefined symbol.**
> When `Foo` is imported from a dependency (no local definition), `vex
> search` returns NEIGHBOURS — callers + import sites — not "the
> definition of Foo". Reason: structural FST gets 0 hits, BM25 +
> semantic both rank up files that mention the token. This is the
> ranked-relevance surface working as designed; the gap is in tool
> choice. For exact-symbol lookup use **`vex check`** (existence
> probe), **`vex show`** (definition body), or **`vex usages --strict`**
> (every reference). v1.15.0+ `vex search` emits a stderr hint
> suggesting these when it detects an identifier-shaped query with
> zero structural hits. See [COOKBOOK FAQ](COOKBOOK.md#faq--vex-search-foo-returned-the-wrong-things)
> for the full decision rule.


> If `vex callers` returns an empty list AND you have reason to believe
> the symbol is called somewhere, run `vex grep '\b<name>\b'` before
> concluding the symbol is unused. Module-level call sites are reported
> via synthetic `<module:path>` callers (Phase 14.1). Python, Java,
> Kotlin, C#, TypeScript, and Rust function/method decorators emit
> forward edges (Phase 14.2 + 14.2.2 + 14.2.1); class-level decorators
> emit module-scope edges (Phase 14.6, v1.12.0). Rust `#[derive(...)]`
> macros and TypeScript property / parameter decorators remain
> invisible — `vex grep` is the workaround there.

---

## 6. `vex impact` — recommended delete-safety workflow (v1.20.0, F1)

Each of the four reference channels listed above misses something
different. To answer "is it safe to delete this?" the historical
recommendation in `pets/CLAUDE.md` was a manual dance:

1. `vex usages X --strict` — binder-resolved real refs.
2. `vex grep '\bX\b'` — catch string-literal mentions, configs, comments.
3. `vex callers X` — confirm via call-graph that something actually calls it.
4. Cross-check the disagreement.

v1.20.0 (F1) collapses that into one call:

```
vex impact <Symbol>
```

The command runs all four channels in parallel and joins their counts
into one verdict:

| Verdict | Rule | Action |
|---|---|---|
| `safe` | every channel reports 0 hits | delete is highly likely safe |
| `unsafe` | strict_refs > 0 OR call_graph_callers > 0 | binder/graph confirmed real usage; do not delete without rewriting call sites |
| `uncertain` | strict + callers = 0, but FST or grep > 0 | text-only mention (string literal, comment, dynamic dispatch); manual inspection required |

The JSON envelope (`vex impact X --format json`) carries the verdict,
a one-line `verdict_explanation` quoting the load-bearing channel
counts, and a per-channel `{ available, count, sample[], truncated }`
block so an agent can see *where* each channel's hits landed without
re-running the four sub-commands. Strict refs and the call graph
report `available: false` (with a `unavailable_reason` string) when
the index lacks the requisite section (pre-v1.8 / pre-Phase 10.2).
An unavailable channel's `count: 0` does NOT drag the verdict toward
`safe` — only `available: true && count == 0` counts as a confirmation.

**Channel-by-channel caveats** (so the verdict can be read accurately):

- **strict_refs** misses everything `vex usages --strict` misses —
  see §2 (uneven cross-language coverage) and §4 (decorator /
  property-binding edges).
- **fst_refs** false-positives on identifier matches in comments and
  string literals — these inflate `uncertain` verdicts without
  being real usage.
- **grep_word_boundary** is the only channel that sees inside string
  literals and config files. It also catches the def-site itself —
  vex pre-filters that row out (see D2 in v1.20.0 CHANGELOG).
- **call_graph_callers** misses dynamic dispatch, reflection, and
  string-resolved calls — same set as §3.

**Use `vex impact` as the entry point, then drill into the specific
channel whose count you want to inspect** — each channel sample
points at file:line so a follow-up `vex show` / `vex usages` /
`vex grep` lands in seconds.

---

## 7. `--workspace` (multi-repo) caveats

`vex index / update / search / check / grep / usages / impact / callers /
callees / reachable --workspace` fan a command across every repo declared
in the nearest `.vex-workspace.toml`.
Each member keeps its own per-repo index; results are grouped by repo. See
`docs/MULTIREPO.md` for the design. Known limits of the shipped MVP:

- **Cross-repo resolution is limited to `usages --strict` (Phase 6).**
  `vex usages <name> --strict --workspace` performs a gtags-style ordered
  fallback: a binder-confirmed reference in repo B to a symbol defined in
  repo A IS surfaced, attributed to the owning repo and tagged as a
  distinct **name-resolved** sub-tier (`cross-repo → repoA (name-resolved)`
  in text; `cross_repo_usages` + `resolves_to` + `confidence: "name"` in
  JSON). It fires only when some member defines the name (first-hit-wins
  owner in declared order), so truly-undefined names / typos stay silent
  and single-repo `--strict` binder precision is not diluted. Caveats: (a)
  it is *name-resolved*, weaker than in-repo binder-confirmed refs; (b)
  import aliases (`use a::Foo as Bar`) key on the alias used at the call
  site; (c) the `diff` / `--base` filter is NOT applied to cross-repo hits
  (per-member changed-path sets don't compose); (d) requires v7 member
  indexes — pre-v7 members are skipped (re-run `vex index`). **Everything
  else stays per-repo:** `impact --workspace`, the call graph, and `usages`
  non-strict scope each member's result to that member (non-strict already
  finds names in every repo via FST fanout; `callers` already crosses repos
  because it is keyed by callee name). Merging corpora is still avoided —
  it would *reduce* binder precision via more ambiguous-name collisions.
  With `--strict`, a member whose index predates v5 is reported as
  `unavailable` for that repo rather than aborting the whole run.
- **`--limit` is per-member, not a total.** A 3-member workspace with
  `--limit 20` can return up to 60 results. There is no unified cross-repo
  ranking — results are grouped per repo, each ranked within its own index.
  With `usages --strict --workspace`, the cross-repo sub-tier adds its own
  per-member `--limit` budget on top of the regular per-member hits, so the
  effective ceiling is higher still (regular + cross-repo per non-owner).
- **`vex search --why` is single-repo only** — it is a hard clap conflict
  with `--workspace`. Per-result JSON `signals` are likewise omitted from
  workspace output.
- **Staleness is reported per-member.** Each member's stale-index reason
  is captured independently and surfaced as a per-repo `stale_reason` in
  JSON (and a stderr advisory in text); the top-level envelope meta is not
  used for per-member staleness. (`grep` has no index, so no staleness.)
- **Sequential, all-or-nothing.** Members are processed one at a time
  (rayon parallelism is *within* each member's build/scan); a hard failure
  on one member aborts the run.
- **Per-member `cache_dir` / `local_cache` (Phase 2).** A member may keep
  its own `cache_dir`/`local_cache` in `.vex.toml` — it is resolved into a
  per-member cache layout (own `local_cache` → in-tree `<member>/.vex_cache/`
  with a `*` `.gitignore`; own `cache_dir` → hashed). Only a hash-less cache
  at the *workspace root* (root `.vex.toml` `local_cache`) across >1 member
  is rejected — it would alias every member into one dir. Embed/blob caches
  (model weights, blob SHA cache) anchor to the workspace root (shared),
  NOT per-member, to avoid N× duplication — so a member's index travels with
  it under `local_cache` but its shared model weights do not. TOCTOU note:
  the resolver is built from one `.vex-workspace.toml` read at startup and
  the command re-reads it; a member added between the two reads routes to the
  shared default for that run (re-run to pick it up). `VEX_CACHE_DIR` /
  `--cache-dir` still override every member (env/CLI beat a member's config).
- **No orphaned-index reconciliation.** `vex update --workspace` refreshes
  every declared member, but removing a member from `.vex-workspace.toml`
  does not clean its old index dir. `index_dir` is keyed by canonical path
  on a cache shared with standalone `vex index`, so an "orphaned" dir may
  still be a live standalone index — auto-deleting it would be unsafe. A
  declared member whose path no longer exists is rejected at load.

---

## Coverage matrix (one-line summary)

| Query | T1 strict | T1 default | T2 (line-scan) | Module-level | Decorator | String-resolved |
| --- | --- | --- | --- | --- | --- | --- |
| `vex search` | ✅ | ✅ | ✅ | n/a (it finds names) | n/a | n/a |
| `vex usages` | ✅ binder | ✅ AST idents | ⚠️ regex (FPs) | ✅ if symbol used by name | ❌ | ❌ |
| `vex callers` | ✅ | ✅ | ✅ | ✅ via `<module:>` (14.1) | ⚠️ Python+Java (14.2), Kotlin+C# (14.2.2), TS+Rust (14.2.1); class-level → 14.6 | ❌ (15) |
| `vex implementations` | ✅ | ✅ | ⚠️ depends on grammar query | n/a | n/a | n/a |
| `vex grep` | ✅ all | ✅ all | ✅ all | ✅ | ✅ | ✅ (literal) |

Legend: ✅ covered · ⚠️ partial · ❌ invisible

---

## Roadmap items that close some of these

- **Phase 13.10 `vex tests-for`** — reverse callgraph walk gated on
  test-classifier; covers a subset of the "is this code reachable from
  tests" question that decorator-based test discovery currently misses.
- **Phase 14.x (planned)** — extend callgraph extractor to capture
  module-level call expressions. Would close the `app = create_app()`
  gap but not decorator dispatch.
- **No current plan** for decorator-aware or string-literal-resolved
  references. These are framework-specific and a fundamental limit
  of static analysis without per-framework heuristics.

Open new issues on the roadmap if a specific pattern hurts your
workflow.
