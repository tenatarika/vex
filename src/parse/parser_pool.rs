//! Per-thread `tree_sitter::Parser` cache, v1.12.0 P3.
//!
//! Before this module every call site of `Parser::new()` + `set_language()`
//! paid the grammar-table allocation and `parser.set_language()` cost
//! per file. After Phase 14.7 cut cold-start parse time with the blob
//! cache, the remaining parse work was bottlenecked on this per-file
//! overhead — visible on `vex index` of a project with several thousand
//! files, especially when only a fraction hit the blob cache.
//!
//! The pool keeps one `Parser` per (thread, language) pair via
//! `thread_local!`. rayon worker threads each maintain their own pool,
//! so there is no mutex contention: callers borrow a `&mut Parser` for
//! the duration of a closure. The pool entries live for the lifetime
//! of the thread — at most `Language::count() × num_threads` Parsers
//! are held simultaneously (≈15 × 16 = 240 on a typical machine),
//! which is bounded and trivially small.
//!
//! ## Why per-thread instead of a shared `Mutex<Vec<Parser>>`
//!
//! Tree-sitter `Parser` is `Send + !Sync`. A shared pool would force
//! callers to lock-and-pop for every file, serialising what is otherwise
//! parallel-by-rayon. The thread_local pattern matches the existing
//! parse-loop structure (rayon spawns N workers, each chews through a
//! slice of files independently) without any extra coordination.

use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{Context, Result};
use tree_sitter::Parser;

use super::language::Language;

thread_local! {
    /// One `Parser` per language for the current thread. Lazily initialized
    /// on first use; never evicted (the working set is bounded by the number
    /// of supported languages).
    static PARSERS: RefCell<HashMap<Language, Parser>> = RefCell::new(HashMap::new());
}

/// Borrow a `tree_sitter::Parser` already configured for `lang` and invoke
/// `f` with it. The parser is owned by the calling thread; the same
/// thread always sees the same `Parser` instance for a given `Language`.
///
/// `f` is given `&mut Parser` because `Parser::parse` requires mutable
/// access. Callers must finish reading any `Tree` they produced before
/// returning from `f` — re-using the parser to re-parse another file is
/// fine because tree-sitter `Tree`s are independently owned.
pub fn with_parser<F, R>(lang: Language, f: F) -> Result<R>
where
    F: FnOnce(&mut Parser) -> Result<R>,
{
    PARSERS.with(|cell| {
        let mut map = cell.borrow_mut();
        let parser = match map.entry(lang) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                let mut parser = Parser::new();
                let ts_lang = lang.ts_language();
                parser
                    .set_language(&ts_lang)
                    .context("set language for parser pool")?;
                slot.insert(parser)
            }
        };
        f(parser)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_initializes_the_parser_and_returns_the_payload() {
        let count = with_parser(Language::Rust, |parser| {
            let src = "fn alpha() {}\nfn beta() {}\n";
            let tree = parser.parse(src, None).expect("parse");
            Ok(tree.root_node().child_count())
        })
        .expect("with_parser");
        assert!(
            count >= 2,
            "expected at least two top-level nodes for two fns, got {count}"
        );
    }

    #[test]
    fn second_call_reuses_the_same_parser_instance() {
        // We can't directly observe identity since the parser is borrowed by
        // closure, but we can prove no panic / no re-init failure on a second
        // call — and that the configured grammar still works.
        with_parser(Language::Python, |parser| {
            let _ = parser.parse("def f(): pass\n", None).expect("parse");
            Ok(())
        })
        .unwrap();
        with_parser(Language::Python, |parser| {
            let tree = parser.parse("class C:\n    pass\n", None).expect("parse");
            assert!(tree.root_node().child_count() >= 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn different_languages_get_independent_parsers() {
        // Rust and Python on the same thread: each populates its own slot.
        with_parser(Language::Rust, |parser| {
            let tree = parser.parse("fn x() {}\n", None).expect("parse");
            assert!(tree.root_node().child_count() >= 1);
            Ok(())
        })
        .unwrap();
        with_parser(Language::Python, |parser| {
            let tree = parser.parse("def y(): pass\n", None).expect("parse");
            assert!(tree.root_node().child_count() >= 1);
            Ok(())
        })
        .unwrap();
    }
}
