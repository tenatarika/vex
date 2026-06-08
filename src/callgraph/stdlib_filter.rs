//! v1.15.1 MEDIUM — stdlib + macro filter for `vex callees`.
//!
//! Field-test reported (`.claude/Task/v1.15.1-amics-field-test-fixes.md`):
//! `vex callees WriteFrameFile` on a real C++ codebase returned `std::move`×4,
//! `c_str`×3, `_T` (MFC macro), `ToString`, `ok` — macros, stdlib calls, and
//! method-chain artifacts swamped the actual graph edges the user cared about.
//!
//! This module provides [`is_likely_stdlib_or_macro`], a coarse-but-fast
//! predicate that the CLI applies as a default-on post-filter. Users who
//! want the unfiltered output pass `--include-stdlib` to bypass it.
//!
//! The predicate is intentionally conservative:
//! - Anything qualified with `std::` is dropped (covers `std::move`,
//!   `std::forward`, `std::make_shared`, etc.).
//! - Names starting with `__` are dropped (compiler/library internals
//!   like `__builtin_*`, `__atomic_*`).
//! - All-uppercase identifiers ≤ 6 chars with optional `_` separators
//!   match the C / C++ macro convention (`_T`, `ASSERT`, `MIN`, `MAX`,
//!   `Q_OBJECT`-style short macros). Longer all-uppercase names are
//!   left alone because they are commonly used for enum variants and
//!   real symbols (e.g. `VK_NULL_HANDLE`).
//! - A small fixed list of C++ stdlib container/string methods that
//!   are almost never the "edge of interest" in callgraph queries:
//!   `c_str`, `size`, `length`, `empty`, `begin`, `end`, `cbegin`,
//!   `cend`, `front`, `back`, `push_back`, `pop_back`, `emplace_back`.
//!   Generic names like `get` / `data` / `clear` / `reset` were
//!   excluded from the list because they are also extremely common in
//!   user-defined domain code (`config.get(key)`, `response.data()`)
//!   and filtering them would silently drop real edges. Users who want
//!   the broader filter pass `--include-stdlib=false` … there is no
//!   such flag — the opt-in for an aggressive filter would be the
//!   inverse flag (out of scope for v1.15.1).
//!
//! The list is intentionally NOT configurable in v1.15.1 — the heuristic
//! is meant to be a quick win on real C++ corpora. Tunability can come
//! later if users report false positives on non-stdlib names.

const STDLIB_METHOD_NAMES: &[&str] = &[
    "c_str",
    "size",
    "length",
    "empty",
    "begin",
    "end",
    "cbegin",
    "cend",
    "front",
    "back",
    "push_back",
    "pop_back",
    "emplace_back",
];

/// Return `true` when `name` matches the "noisy stdlib or macro" heuristic
/// the v1.15.1 `vex callees` post-filter uses to suppress edges that
/// dilute the real callgraph signal. See module docs for the full rule
/// list. Designed to be cheap (no allocations, single pass over bytes).
pub fn is_likely_stdlib_or_macro(name: &str) -> bool {
    if name.starts_with("std::") {
        return true;
    }
    if name.starts_with("__") {
        return true;
    }
    if STDLIB_METHOD_NAMES.contains(&name) {
        return true;
    }
    // Short all-uppercase identifier with optional `_` separators →
    // almost certainly a macro on a C / C++ codebase.
    if name.len() <= 6
        && !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        && name.chars().any(|c| c.is_ascii_uppercase())
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_likely_stdlib_or_macro as f;

    #[test]
    fn drops_std_qualified() {
        assert!(f("std::move"));
        assert!(f("std::forward"));
        assert!(f("std::make_shared"));
    }

    #[test]
    fn drops_compiler_internals() {
        assert!(f("__builtin_expect"));
        assert!(f("__atomic_load_n"));
    }

    #[test]
    fn drops_stdlib_method_names() {
        assert!(f("c_str"));
        assert!(f("push_back"));
        assert!(f("empty"));
    }

    #[test]
    fn keeps_ambiguous_generic_method_names() {
        // `get` / `data` / `clear` / `reset` are common in user-defined
        // domain code (`config.get(key)`, `response.data()`) — the
        // filter must not drop them by default. C++ users who want
        // them filtered can post-process the output.
        assert!(!f("get"));
        assert!(!f("data"));
        assert!(!f("clear"));
        assert!(!f("reset"));
    }

    #[test]
    fn drops_short_macro_style_identifiers() {
        assert!(f("_T"));
        assert!(f("MAX"));
        assert!(f("MIN"));
        assert!(f("ASSERT"));
        assert!(f("Q_FOO")); // 5 chars all-upper
    }

    #[test]
    fn keeps_real_function_names() {
        assert!(!f("WriteFrameFile"));
        assert!(!f("HardscapeProvider"));
        assert!(!f("toString")); // lowercase first char → not a macro
        assert!(!f("compute_total"));
        assert!(!f("ok")); // 2-char lowercase — not a macro, NOT filtered
    }

    #[test]
    fn keeps_long_screaming_snake() {
        // Long all-uppercase names are commonly enum variants / real
        // symbols and must NOT be filtered.
        assert!(!f("VK_NULL_HANDLE"));
        assert!(!f("GL_TEXTURE_2D_ARRAY"));
    }

    #[test]
    fn empty_input_is_not_filtered() {
        // Defensive: extractor shouldn't pass empty names but the
        // upstream code does `if callee_name.is_empty() { continue; }`
        // anyway, so the filter need only avoid panicking.
        assert!(!f(""));
    }
}
