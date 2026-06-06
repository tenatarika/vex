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

---

## 2. `vex usages` coverage is uneven across languages

The legacy refs FST is populated in two ways depending on the
language:

| Tier | Languages | Extraction |
| --- | --- | --- |
| T1 (AST identifier walk) | Rust, TypeScript, Python, C#, C++ | Walk every `identifier` node in the AST, skipping comments and string literals. Captures inheritance refs (`class Foo(Bar):` → `Bar`), call targets, type annotations, all real usages. |
| T2 (line-scan regex) | the other 14 languages | Regex over each line: any identifier-shaped token becomes a ref. Higher false-positive rate (matches text inside strings / comments depending on whitespace), but covers grammars without an AST filter yet. |

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

---

## 4. `vex usages` non-strict mode quality varies by language

When the index has no `reference_edges` section (built with
`--no-call-graph`, or T2 language outside the binder set), `vex usages`
falls back to the legacy refs FST. Quality notes:

- **T1 languages with `has_ast_ref_filter`** (Rust, TypeScript, Python,
  C#, C++): refs come from an AST walk that skips comments and plain
  string literals. False-positive rate is low; identifier collisions
  across scopes still produce noise.
- **T2 languages** (everything else — Go, Java, Kotlin, Swift, PHP,
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

**What still does NOT resolve cross-file in C++:**

- **Class member methods** — `gw.Charge()` where `Charge` is a
  method of `class Gateway`. The scope binder emits a
  `field_identifier` ref for `Charge` but the symbol extractor
  treats class members as nested-scope, not top-level. v1.14 only
  fixes the `BindTarget::Unresolved → free symbol` path. Pending
  scope-binder work.
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

`no` here means the next semantic update will be a full rebuild. The
status field is also surfaced in `vex status --format json` as
`body_tokens_persisted: bool`.

**Cold-start applies per-index, not globally.** Each project's index
needs the `vex index --semantic` priming separately. The `vex update`
that follows will be incremental.

**Non-semantic `vex update` is unaffected.** B1.2 only changes the
HNSW path. The structural (FST), BM25, and call-graph sections were
already rebuilt incrementally and continue to work as before — the
body_tokens persistence side-effect closes the legacy "BM25 recall
drops for unchanged symbols after `vex update`" warning.

---

## 5. `vex grep` is the right fallback

Whenever vex's indexed surface misses something the user can see in the
source, `vex grep <pattern>` is the textual-content escape hatch. It's
slower (~50 ms per query vs ~4 ms FST lookup) but exhaustive. The
guidance for agents:

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
