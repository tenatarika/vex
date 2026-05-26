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
decorator_target` (Phase 14.2)** — `vex callers GetMapping` lists every
Spring handler; `vex callers get` lists every FastAPI route function.

**What is still invisible:**

- **Decorators in TypeScript and Rust** — the `decorator` / `attribute_item`
  AST node lives as a *sibling* of the method/function, not inside its
  byte range. The existing inner-fn attribution can't handle sibling
  adjacency without a separate extractor pass. **Phase 14.2.1 territory.**
- **Decorators in Kotlin and C#** — these languages have no base
  callgraph today, so adding decorator-only coverage would emit
  decorator edges but leave regular method calls invisible. Bundled
  with full callgraph support in **Phase 14.2.2.**
- **Class-level decorators / annotations** (`@dataclass class Foo:`,
  `@RestController class C {}`) — the callgraph model only indexes
  function/method symbols today. **Phase 14.6 if real users ask.**
- **String-resolved references.** `"media_server.main:create_app"`
  passed to uvicorn, `task_name="celery_task.fire"` — vex sees the
  string literal, never resolves it. **Phase 15 territory.**
- **`eval` / `exec` / reflection-style dispatch.** Out of scope by
  construction.

**Class-body call sites** (e.g. `db_url = make_dsn()` inside a class body)
are currently attributed to the **synthetic `<module:path>` symbol** as
well — the sentinel fires for any call site outside a `function`/`method`/
`closure` scope, regardless of whether it's at module scope or inside a
class body. The `edge.line` still points to the actual call site so the
location is accurate, but the *caller name* is the module, not the class.
A follow-up could synthesise a per-class `<class:Foo>` caller; track as
Phase 14.5 if real users ask.

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

**Decorator repro (now resolved for Python + Java):**

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

For TypeScript / Rust decorators, Kotlin / C# annotations, and any
class-level decorator, the gap remains — see "What is still invisible"
above for the deferred phase numbers.

**Rightmost-identifier convention has a collision surface.** Because
`@app.get("/x")` → `get` and a literal `dict.get(key)` call also →
`get`, `vex callers get` returns both decorator-edge handlers AND any
function that does a regular `.get(...)` call. Narrow with `--include
'src/routes/**'` or `--exclude 'src/utils/**'` if the corpus mixes the
two populations. Same convention applies to method calls already
(`obj.method() → method`); decorator edges just expand the pool.

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
C++.

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
| Decorator dispatch (Python, Java) | `@app.get("/")`, `@GetMapping("/x")` | Phase 14.2: edge `decorated_fn → decorator_target` (rightmost identifier wins). |
| Decorator dispatch (TS, Rust) | `@Controller()`, `#[tokio::test]` | Deferred to Phase 14.2.1 — sibling-adjacency in grammar AST. |
| Decorator dispatch (Kotlin, C#) | `@RestController`, `[HttpGet]` | Deferred to Phase 14.2.2 — no base callgraph today. |
| Class-level decorator | `@dataclass class Foo:` | Phase 14.6 — callgraph indexes only functions/methods today. |
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

## 5. `vex grep` is the right fallback

Whenever vex's indexed surface misses something the user can see in the
source, `vex grep <pattern>` is the textual-content escape hatch. It's
slower (~50 ms per query vs ~4 ms FST lookup) but exhaustive. The
guidance for agents:

> If `vex callers` returns an empty list AND you have reason to believe
> the symbol is called somewhere, run `vex grep '\b<name>\b'` before
> concluding the symbol is unused. Module-level call sites are reported
> via synthetic `<module:path>` callers (Phase 14.1). Python and Java
> function/method decorators emit forward edges (Phase 14.2). Decorators
> in TypeScript / Rust / Kotlin / C# and class-level decorators in any
> language remain invisible — `vex grep` is the workaround there.

---

## Coverage matrix (one-line summary)

| Query | T1 strict | T1 default | T2 (line-scan) | Module-level | Decorator | String-resolved |
| --- | --- | --- | --- | --- | --- | --- |
| `vex search` | ✅ | ✅ | ✅ | n/a (it finds names) | n/a | n/a |
| `vex usages` | ✅ binder | ✅ AST idents | ⚠️ regex (FPs) | ✅ if symbol used by name | ❌ | ❌ |
| `vex callers` | ✅ | ✅ | ✅ | ✅ via `<module:>` (14.1) | ⚠️ Python+Java (14.2); TS/Rust → 14.2.1; Kotlin/C# → 14.2.2 | ❌ (15) |
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
