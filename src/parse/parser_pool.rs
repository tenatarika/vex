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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::ControlFlow;

use anyhow::{bail, Context, Result};
use tree_sitter::{ParseOptions, ParseState, Parser, Tree};

use super::language::Language;

/// Per-parse cap on tree-sitter progress-callback invocations, used by
/// [`parse_text`] to bound runaway parses.
///
/// Adversarial / malformed input can drive tree-sitter's GLR error-recovery
/// into super-linear time *and memory* — `fuzz_kotlin_binder` found 451-byte
/// Kotlin inputs that took 334 s to parse and blew past a 2 GB RSS cap. A
/// wall-clock timeout bounds time but not the memory a fast explosion
/// allocates within it, and it makes indexing non-deterministic; an
/// operation budget bounds both and stays reproducible.
///
/// Calibration: tree-sitter fires the progress callback periodically, and on
/// this grammar's error-recovery explosion memory grows ~linearly with
/// callbacks (measured on a 480-byte OOM artifact: 3 K callbacks → 67 MB,
/// 10 K → 218 MB, 50 K → 1.08 GB, 200 K → 3 GB). A healthy 250 KB parse fires
/// only ~6 K callbacks with the byte offset advancing steadily (~25 / KB) and
/// a sub-KB normal file fires a few dozen. The floor is set so the worst
/// degenerate single parse stays well under ~150 MB, while the per-byte
/// allowance (80× the healthy rate) leaves multi-MB real files untouched. A
/// parse that exceeds the budget returns `Err` and the file is skipped.
const PARSE_CALLBACK_FLOOR: u64 = 2_000;
const PARSE_CALLBACK_PER_BYTE: u64 = 2;

fn parse_callback_budget(len: usize) -> u64 {
    PARSE_CALLBACK_FLOOR.saturating_add((len as u64).saturating_mul(PARSE_CALLBACK_PER_BYTE))
}

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
///
/// # Panics
///
/// Panics if called re-entrantly from within `f` on the same thread —
/// the underlying `RefCell::borrow_mut` will detect the double borrow.
/// All current call sites are leaf parse wrappers; do not invoke another
/// `with_parser` (or anything that does, transitively) from within a
/// closure on the same thread.
pub(crate) fn with_parser<F, R>(lang: Language, f: F) -> Result<R>
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

/// Parse `content` with `lang`'s grammar under a wall-clock guard.
///
/// This is the single guarded parse entry point — every production parse
/// site routes through it so no grammar can hang or OOM the indexer on
/// adversarial input (see [`PARSE_BUDGET`]). Returns `Err` on a budget
/// timeout or a genuine parse failure; callers already treat a parse error
/// as "skip this file / fall back", so a timed-out file is simply skipped.
pub(crate) fn parse_text(lang: Language, content: &str) -> Result<Tree> {
    with_parser(lang, |parser| {
        let bytes = content.as_bytes();
        let len = bytes.len();
        let budget = parse_callback_budget(len);
        // `Cell` (shared borrow) lets the progress closure count invocations
        // and flag an over-budget cancel while we still read the flag after
        // `parse_with_options` returns — a `&mut` capture would keep the
        // borrow alive past the read.
        let calls = Cell::new(0u64);
        let over_budget = Cell::new(false);
        let mut progress = |_state: &ParseState| {
            let n = calls.get() + 1;
            calls.set(n);
            if n > budget {
                over_budget.set(true);
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = ParseOptions::new().progress_callback(&mut progress);
        let tree = parser.parse_with_options(
            &mut |i, _| {
                if i < len {
                    &bytes[i..]
                } else {
                    Default::default()
                }
            },
            None,
            Some(options),
        );
        match tree {
            Some(t) => Ok(t),
            None if over_budget.get() => bail!(
                "tree-sitter parse exceeded the {budget}-callback budget for {lang:?} \
                 ({len} bytes; likely adversarial / pathological input); file skipped"
            ),
            None => bail!("tree-sitter parse failed"),
        }
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

    #[test]
    fn parse_text_parses_normal_source() {
        let tree = parse_text(Language::Kotlin, "fun f(x: Int): Int = x + 1\n").expect("parse");
        assert!(tree.root_node().child_count() >= 1);
    }

    #[test]
    fn parse_callback_budget_scales_with_len() {
        assert_eq!(parse_callback_budget(0), PARSE_CALLBACK_FLOOR);
        assert_eq!(
            parse_callback_budget(1000),
            PARSE_CALLBACK_FLOOR + 1000 * PARSE_CALLBACK_PER_BYTE
        );
        // No overflow on absurd lengths.
        assert!(parse_callback_budget(usize::MAX) >= PARSE_CALLBACK_FLOOR);
    }

    #[test]
    fn parse_text_bails_on_pathological_input() {
        // Regression for the fuzz_kotlin_binder finding: a 451-byte malformed
        // Kotlin input drove tree-sitter-kotlin-ng's error recovery to 334 s /
        // multi-GB. The callback budget must bail it as `Err` (fast + bounded)
        // instead of hanging or OOMing. The artifact is valid UTF-8 (the fuzz
        // target only feeds UTF-8), so `from_utf8` succeeds.
        const PATHOLOGICAL: &[u8] = include_bytes!("../../fuzz/findings/kotlin-grammar-oom.bin");
        let src = std::str::from_utf8(PATHOLOGICAL).expect("fuzz finding is valid UTF-8");
        let result = parse_text(Language::Kotlin, src);
        assert!(
            result.is_err(),
            "pathological input must hit the parse budget and return Err"
        );
    }
}
