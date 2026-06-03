//! Pattern parsing — turns the user-facing AST-grep-style pattern
//! string (`fn $NAME($$$ARGS) { $$$BODY }`) into the
//! [`super::PatternTree`] / [`super::CompositePattern`] data the
//! matcher engine consumes.
//!
//! Two entry points: `parse_pattern` for a single pattern,
//! `parse_composite_pattern` for the `&&` / `||` composition syntax.
//! Both produce inputs to the matcher half of the module — kept in a
//! sibling file so the matcher engine doesn't share screen real estate
//! with parser state. In-crate consumers reach the public seam via
//! `super::parse_composite_pattern` (re-exported from `mod.rs`); tests
//! reach private items by name through `super::parse::{...}`.

use anyhow::Result;

use crate::parse::language::Language;

use super::{CompositePattern, PatternTree, Segment};

/// Consume an alphanumeric/underscore identifier from `chars` and return it.
/// Returns an empty string when the peek is not an identifier start —
/// callers use the empty result to distinguish anonymous prefixes (e.g.
/// `$$$`) from named ones (`$$$BODY`).
fn read_ident(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut name = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || c == '_' {
            name.push(c);
            chars.next();
        } else {
            break;
        }
    }
    name
}

/// Parse the pattern string into segments.
pub(super) fn parse_pattern(pattern: &str, lang: Language) -> Result<PatternTree> {
    if pattern.trim().is_empty() {
        anyhow::bail!(
            "empty pattern — supply at least one literal token or metavar \
             (e.g. `fn $NAME($$$)`, `$X.then($X)`)"
        );
    }
    let mut segments = Vec::new();
    let mut chars = pattern.chars().peekable();
    let mut literal = String::new();

    while let Some(&c) = chars.peek() {
        if c == '$' {
            if !literal.is_empty() {
                segments.push(Segment::Literal(literal.clone()));
                literal.clear();
            }
            chars.next(); // consume $

            // Check for $$$ / $$$NAME / $$NAME — multi-`$` prefixes.
            if chars.peek() == Some(&'$') {
                chars.next();
                if chars.peek() == Some(&'$') {
                    // Three `$` — anonymous `$$$` or named `$$$NAME`.
                    chars.next();
                    let name = read_ident(&mut chars);
                    if name.is_empty() {
                        segments.push(Segment::Ellipsis);
                    } else {
                        segments.push(Segment::NamedEllipsis(name));
                    }
                    continue;
                }
                // Two `$` — either `$$NAME` (named ellipsis) or a bare
                // `$$` that we keep as a literal so existing patterns
                // mentioning `$$` in shell-style syntax don't break.
                let name = read_ident(&mut chars);
                if name.is_empty() {
                    literal.push_str("$$");
                } else {
                    segments.push(Segment::NamedEllipsis(name));
                }
                continue;
            }

            // Check for $_ or $NAME
            if chars.peek() == Some(&'_') {
                chars.next();
                // Ensure it's $_ not $_foo. v1.11 hotfix: preserve the
                // `$` so a typo like `$_Bar` (expecting a named capture)
                // doesn't silently degrade to matching the literal text
                // `_Bar` with the `$` swallowed. Pre-fix, the `$` was
                // already consumed at line 157 and only `_` was pushed
                // into `literal`, so an invalid metavar form was
                // indistinguishable from intentional literal text.
                if !chars.peek().is_some_and(|c| c.is_alphanumeric()) {
                    segments.push(Segment::Wildcard);
                    continue;
                }
                literal.push_str("$_");
                continue;
            }

            // $NAME — collect identifier
            let mut name = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_alphanumeric() || c == '_' {
                    name.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            if name.is_empty() {
                literal.push('$');
            } else {
                segments.push(Segment::Capture(name));
            }
        } else {
            literal.push(c);
            chars.next();
        }
    }

    if !literal.is_empty() {
        segments.push(Segment::Literal(literal));
    }

    Ok(PatternTree::new(segments, lang))
}

/// Parse a (possibly composite) pattern into [`CompositePattern`].
///
/// Single patterns (no top-level `&&` / `||`) produce one disjunct with
/// one tree — semantically identical to [`parse_pattern`]. Patterns
/// containing top-level composition operators are split first, then
/// each leaf is parsed via [`parse_pattern`].
pub fn parse_composite_pattern(pattern: &str, lang: Language) -> Result<CompositePattern> {
    if pattern.trim().is_empty() {
        anyhow::bail!(
            "empty pattern — supply at least one literal token or metavar \
             (e.g. `fn $NAME($$$)`, `$X.then($X)`)"
        );
    }
    let or_parts = split_top_level(pattern, "||");
    let mut disjuncts = Vec::with_capacity(or_parts.len());
    for or_part in or_parts {
        let and_parts = split_top_level(&or_part, "&&");
        let mut trees = Vec::with_capacity(and_parts.len());
        for and_part in and_parts {
            trees.push(parse_pattern(and_part.trim(), lang)?);
        }
        disjuncts.push(trees);
    }
    Ok(CompositePattern { disjuncts, lang })
}

/// Split `s` on the literal operator `op` only at positions where:
///   * bracket depth `() [] {}` is zero,
///   * the position is not inside a `"`/`'` string literal,
///   * the operator is space-flanked on both sides.
///
/// When no split fires the result is `[s.trim().to_string()]`.
pub(super) fn split_top_level(s: &str, op: &str) -> Vec<String> {
    let bytes = s.as_bytes();
    let op_bytes = op.as_bytes();
    let mut parts: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut depth: i32 = 0;
    let mut in_str: Option<u8> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        // Inside a string literal — consume verbatim, watch for escapes
        // and the matching quote.
        if let Some(q) = in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            in_str = Some(b);
            i += 1;
            continue;
        }
        if b == b'(' || b == b'[' || b == b'{' {
            depth += 1;
        } else if b == b')' || b == b']' || b == b'}' {
            depth -= 1;
        }
        // Look for ` && ` / ` || ` at depth 0.
        let op_end = i + op_bytes.len();
        if depth == 0
            && i > 0
            && bytes[i - 1] == b' '
            && op_end < bytes.len()
            && &bytes[i..op_end] == op_bytes
            && bytes[op_end] == b' '
        {
            parts.push(s[start..i].trim().to_string());
            i = op_end + 1;
            start = i;
            continue;
        }
        i += 1;
    }
    parts.push(s[start..].trim().to_string());
    parts
}
