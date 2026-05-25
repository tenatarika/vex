# Known limitations (v1.9-pre)

This document lists vex's known coverage gaps. Each entry has a concrete
repro, an explanation, and a workaround. Agents reading this should
treat the items below as **the kind of result vex cannot find — reach
for `vex grep` or shell tools when you suspect a hit lives outside vex's
extraction model**.

Updated 2026-05-25 after external review of v1.8.2.

---

## 1. `vex callers` is function-scoped

**What works:** every call site that lives inside a `function_definition`
/ `method_definition` / `closure` node is recorded as a caller in the
v6 persistent call graph.

**What is invisible:**

- **Module-level expressions.** `app = create_app()` at Python module
  scope, top-level `let server = build_server()` in Rust, top-level
  `const router = setup()` in TypeScript.
- **Decorator-based dispatch.** `@app.get("/foo")` does not register
  the decorated handler as a caller of `app.get`. The decorator
  invocation lives outside any function body.
- **Class-body statements.** Field initializers that call something
  (`x: Foo = make_foo()` in a class body) miss the call graph.
- **String-resolved references.** `"media_server.main:create_app"`
  passed to uvicorn, `task_name="celery_task.fire"` — vex sees the
  string literal, never resolves it.
- **`eval` / `exec` / reflection-style dispatch.** Out of scope by
  construction.

**Why:** the call-graph extractor walks tree-sitter `call_expression`
nodes only when they appear under a function-shaped ancestor. This
matches the data model — a caller is a *symbol*, and module-level
expressions are not symbols.

**Workaround:** `vex grep '\bcreate_app\b'` returns every textual
mention including module-level call sites. For the inheritance case
specifically (`class Foo(Bar):`), `vex implementations Bar` is the
right tool — it captures the supertype reference.

**Repro:**

```python
# media_server/main.py
def create_app(): ...

app = create_app()   # ← invisible to vex callers create_app

@app.get("/items")
def list_items(): ...   # ← decorator dispatch invisible
```

```
$ vex callers create_app
(empty)

$ vex grep '\bcreate_app\b'
media_server/main.py:411:app = create_app()    # ← here it is
```

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

**`mode: "text_scan"` in `--why` output is a historical label.** The
underlying data source is the FST, not a live tree-sitter scan. For
T1 languages the FST itself was populated from an AST walk; for T2 it
came from a regex. The label name predates the AST walk — kept for
back-compat with agents that learned the contract in v1.8.0.

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
| Decorator dispatch | `@app.get("/")` | The decorator expression is captured as a ref to `app.get`, but the decorated function is NOT linked as a caller. |
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
> concluding the symbol is unused. The most common false negative
> sources are module-level call sites and decorator dispatch.

---

## Coverage matrix (one-line summary)

| Query | T1 strict | T1 default | T2 (line-scan) | Module-level | Decorator | String-resolved |
| --- | --- | --- | --- | --- | --- | --- |
| `vex search` | ✅ | ✅ | ✅ | n/a (it finds names) | n/a | n/a |
| `vex usages` | ✅ binder | ✅ AST idents | ⚠️ regex (FPs) | ✅ if symbol used by name | ❌ | ❌ |
| `vex callers` | ✅ | ✅ | ✅ | ❌ | ❌ | ❌ |
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
