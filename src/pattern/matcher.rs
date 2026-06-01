//! AST-aware pattern matcher.
//!
//! Instead of parsing the pattern as code (fragile with incomplete snippets),
//! we do text-matching against the source text of each AST node.
//!
//! Pattern syntax:
//! - Literal text is matched exactly
//! - `$NAME` matches a single identifier/expression and captures it
//! - `$_` matches anything without capturing
//! - `$$$` matches zero or more characters anonymously (ellipsis,
//!   spans across newlines and node boundaries)
//! - `$$$NAME` — node/block-spanning ellipsis with a named capture
//!   (11.4 Inc 6 — typical use: `fn $F($$ARGS) { $$$BODY }`)
//! - `$$NAME` — comma/arg-list-spanning ellipsis with a named capture
//!   (11.4 Inc 6). Functionally identical to `$$$NAME` today; the two
//!   syntaxes coexist for readability — `$$$BODY` reads naturally for
//!   block bodies, `$$ARGS` for parameter lists.
//!
//! ### Ellipsis termination — depth-aware (H4)
//!
//! `$$$NAME` / `$$NAME` / `$$$` capture by forward-scanning for the
//! next literal segment in the pattern. The scan is depth-aware: it
//! tracks `()` / `{}` / `[]` nesting and skips over double-quoted
//! `"..."` string literals (with `\` escape). A pattern ending in `}`
//! stops at the **balancing** closer of the outer `{`, not the first
//! textual `}` — so `class $T { $$$BODY }` correctly spans bodies
//! containing nested blocks like `{ get; set; }`.
//!
//! Remaining limits (documented; tracked as follow-ups):
//! - **Single-quoted strings** (`'...'`) are NOT recognised. Affects
//!   C# / TypeScript / Python single-quote strings and Rust char
//!   literals — a `'}'` char literal in source can still confuse the
//!   walker. Rust lifetimes (`'a`) make a naïve single-quote pairing
//!   unsafe without AST context, so v1 leaves this alone.
//! - **Raw / triple-quoted strings**: Rust `r#"..."#`, C++
//!   `R"(...)"`, Python `"""..."""` / `'''...'''` are not recognised
//!   as string regions — their interior `}` bytes still trigger
//!   depth tracking.
//! - **Comments containing brackets**: the walker is byte-level and
//!   doesn't know what's a comment. Block comments with mismatched
//!   brackets can still throw off depth counts.
//!
//! Full tree-sitter AST descent inside `try_match` would close these
//! gaps; filed as a v2 follow-up.
//!
//! ## Composition (Inc 7)
//!
//! Two top-level operators combine sub-patterns. They are detected
//! **before** the per-conjunct tokeniser runs, so single-pattern
//! semantics are unchanged when neither operator is present.
//!
//! - ` && ` — AND. Both sub-patterns must match in the same file;
//!   shared metavar names must capture the same text in both.
//! - ` || ` — OR. The union of either sub-pattern's matches.
//!
//! Precedence: `&&` binds tighter than `||` (standard).
//!
//! Splits only fire when the operator is **space-flanked** and at
//! bracket / quote depth 0. So `record($X, $X)` is one pattern;
//! `f($X && $Y)` is one pattern (the `&&` sits inside parens);
//! `if x && y` at top level **does** split — pattern authors who
//! intend a literal Rust/C `&&` operator should wrap it in a larger
//! structural context that puts it past depth 0.

use std::collections::HashMap;

use anyhow::Result;
use tree_sitter::{Node, Parser};

use crate::parse::language::Language;

use super::PatternMatch;

/// Compiled pattern — a list of segments (literal or metavar).
#[derive(Debug)]
pub struct PatternTree {
    segments: Vec<Segment>,
    pub lang: Language,
}

/// Composite pattern shape — OR of ANDs. Single-pattern usages produce
/// exactly one disjunct with one [`PatternTree`] inside.
///
/// Semantics:
/// - Each top-level `||`-separated branch becomes one entry in
///   `disjuncts`.
/// - Each `&&`-separated piece within a branch becomes one
///   [`PatternTree`] in that branch's vector.
/// - A file matches the composite when **any** disjunct matches, and a
///   disjunct matches when **all** of its trees match with consistent
///   captures across shared metavar names.
#[allow(dead_code)] // Debug used by `Result::unwrap_err` panics in tests
#[derive(Debug)]
pub struct CompositePattern {
    /// Outer vec = OR; inner vec = AND.
    pub disjuncts: Vec<Vec<PatternTree>>,
    /// Language the composite was parsed against. Kept on the composite
    /// so consumers don't need to reach into a `PatternTree` to recover
    /// it (e.g. tracing, future structural-rewrite preview in scope-C).
    /// Currently no in-crate reader — `#[allow(dead_code)]` until a real
    /// caller arrives.
    #[allow(dead_code)]
    pub lang: Language,
}

impl CompositePattern {
    /// `true` iff the composite has more than one disjunct (an OR).
    pub fn has_or(&self) -> bool {
        self.disjuncts.len() > 1
    }
}

#[derive(Debug)]
enum Segment {
    Literal(String),
    Capture(String),       // $NAME — single token / balanced expression
    Wildcard,              // $_
    Ellipsis,              // $$$  — anonymous ellipsis
    NamedEllipsis(String), // $$$NAME or $$NAME — named ellipsis with capture
}

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
pub fn parse_pattern(pattern: &str, lang: Language) -> Result<PatternTree> {
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

    Ok(PatternTree { segments, lang })
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
fn split_top_level(s: &str, op: &str) -> Vec<String> {
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

/// Top-level composite matcher (Inc 7).
///
/// OR semantics: union of every disjunct's matches, deduped by
/// `(file_path, line)`. AND semantics: every tree in a disjunct must
/// match in the file and their captures must agree on shared metavar
/// names. The reported match is anchored at the **first** conjunct's
/// location with captures from all conjuncts merged in order.
pub fn find_matches_composite(
    source: &str,
    pattern: &CompositePattern,
    file_path: &str,
) -> Vec<PatternMatch> {
    let mut out: Vec<PatternMatch> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for trees in &pattern.disjuncts {
        for m in match_conjunct(source, trees, file_path) {
            if seen.insert((m.path.clone(), m.line)) {
                out.push(m);
            }
        }
    }
    out
}

fn match_conjunct(source: &str, trees: &[PatternTree], file_path: &str) -> Vec<PatternMatch> {
    if trees.is_empty() {
        return Vec::new();
    }
    if trees.len() == 1 {
        return find_matches(source, &trees[0], file_path);
    }

    // Materialise every tree's per-file matches once.
    let per_tree: Vec<Vec<PatternMatch>> = trees
        .iter()
        .map(|t| find_matches(source, t, file_path))
        .collect();
    if per_tree.iter().any(|v| v.is_empty()) {
        return Vec::new();
    }

    // For each match of tree[0], confirm an agreeing match exists in
    // every subsequent tree. Captures from all conjuncts are merged.
    //
    // Perf (review H1): the `merged → HashMap` projection is built once
    // per `(anchor, other_tree)` iteration (the input `merged` grows as
    // each conjunct is accepted, so the third+ conjunct's map includes
    // the names introduced by earlier conjuncts — that's the
    // cross-conjunct back-ref enforcement). Avoids the previous
    // O(anchors × other_trees × candidates × |captures|) HashMap work
    // by amortising the build over every candidate in the tree.
    let mut out = Vec::new();
    for anchor in &per_tree[0] {
        let mut merged = anchor.captures.clone();
        let mut all_ok = true;
        for other_matches in &per_tree[1..] {
            let merged_map = build_normalised_map(&merged);
            let next = other_matches
                .iter()
                .find(|m| captures_agree_with_map(&merged_map, &m.captures));
            match next {
                Some(m) => {
                    for (k, v) in &m.captures {
                        if !merged.iter().any(|(k2, _)| k2 == k) {
                            merged.push((k.clone(), v.clone()));
                        }
                    }
                }
                None => {
                    all_ok = false;
                    break;
                }
            }
        }
        if all_ok {
            out.push(PatternMatch {
                path: anchor.path.clone(),
                line: anchor.line,
                matched_text: anchor.matched_text.clone(),
                captures: merged,
            });
        }
    }
    out
}

/// Build the normalised lookup map used by [`captures_agree_with_map`].
/// First binding wins — matches the back-ref semantics inside a single
/// pattern.
fn build_normalised_map(captures: &[(String, String)]) -> HashMap<&str, String> {
    let mut map: HashMap<&str, String> = HashMap::new();
    for (k, v) in captures {
        map.entry(k.as_str())
            .or_insert_with(|| normalise_capture(v));
    }
    map
}

/// `true` iff every metavar name in `c2` that is also in `map_c1` has
/// the same normalised text in both. Disjoint names always agree.
/// Pre-built `map_c1` is the perf-hot path; tests use the simpler
/// [`captures_agree`] below.
fn captures_agree_with_map(map_c1: &HashMap<&str, String>, c2: &[(String, String)]) -> bool {
    for (k, v) in c2 {
        if let Some(prev) = map_c1.get(k.as_str()) {
            if *prev != normalise_capture(v) {
                return false;
            }
        }
    }
    true
}

/// Test-friendly convenience that builds the map internally. Production
/// code goes through [`captures_agree_with_map`] to amortise the build.
#[cfg(test)]
fn captures_agree(c1: &[(String, String)], c2: &[(String, String)]) -> bool {
    let map = build_normalised_map(c1);
    captures_agree_with_map(&map, c2)
}

/// Find all AST nodes in `source` whose text matches the pattern.
pub fn find_matches(source: &str, pattern: &PatternTree, file_path: &str) -> Vec<PatternMatch> {
    let mut parser = Parser::new();
    let ts_lang = pattern.lang.ts_language();
    if parser.set_language(&ts_lang).is_err() {
        return Vec::new();
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut matches = Vec::new();
    visit_all(
        tree.root_node(),
        source,
        &pattern.segments,
        file_path,
        &mut matches,
    );

    // Dedup by (path, line) — parent and child nodes can both match
    let mut seen = std::collections::HashSet::new();
    matches.retain(|m| seen.insert((m.path.clone(), m.line)));
    matches
}

fn visit_all(
    node: Node,
    source: &str,
    segments: &[Segment],
    file_path: &str,
    matches: &mut Vec<PatternMatch>,
) {
    // Try to match this node's text against the pattern
    let node_src = safe_node_text(node, source);

    // Only try "meaningful" nodes — skip root, blocks, and very large nodes
    let skip_kinds = [
        "source_file",
        "module",
        "program",
        "translation_unit",
        "block",
        "statement_block",
        "declaration_list",
    ];
    if node.is_named() && !node_src.is_empty() && !skip_kinds.contains(&node.kind()) {
        if let Some(captures) = try_match(node_src, segments) {
            let first_line = node_src.lines().next().unwrap_or("").to_string();
            matches.push(PatternMatch {
                path: file_path.to_string(),
                line: node.start_position().row + 1,
                matched_text: first_line,
                captures,
            });
        }
    }

    // Visit children
    let cursor = &mut node.walk();
    for child in node.children(cursor) {
        visit_all(child, source, segments, file_path, matches);
    }
}

/// Try to match text against pattern segments. Returns captures on success.
///
/// Metavar back-references: when the same `$NAME` appears more than once
/// in a pattern, every later occurrence must capture the *same* text as
/// the first occurrence — otherwise the match fails. The returned
/// `captures` vector preserves the original-order list of (name, value)
/// pairs so downstream output (`vex pattern --format compact`) shows
/// every binding site, not just the first.
fn try_match(text: &str, segments: &[Segment]) -> Option<Vec<(String, String)>> {
    let text = text.trim();
    let mut pos = 0;
    let mut captures: Vec<(String, String)> = Vec::new();
    let mut bound: HashMap<String, String> = HashMap::new();

    for (i, seg) in segments.iter().enumerate() {
        match seg {
            Segment::Literal(lit) => {
                let lit_trimmed = lit.trim();
                if lit_trimmed.is_empty() {
                    // Skip whitespace-only literals
                    while pos < text.len() && text.as_bytes()[pos].is_ascii_whitespace() {
                        pos += 1;
                    }
                    continue;
                }
                // Skip whitespace in source
                while pos < text.len() && text.as_bytes()[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                // Match literal
                if text[pos..].starts_with(lit_trimmed) {
                    pos += lit_trimmed.len();
                } else {
                    return None;
                }
            }
            Segment::Capture(name) => {
                // Skip whitespace
                while pos < text.len() && text.as_bytes()[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                // Capture a "word" — identifier or balanced expression.
                // Pass the first byte of the next literal so the greedy
                // identifier scan stops at the boundary instead of
                // swallowing characters that belong to the next segment.
                // Without this, `Result<$T, $E>` over `Error>` would have
                // `$E` swallow the `>` because `>` is in the identifier
                // allow-list (kept for inline generics like `$T<$U>`).
                let stop = next_literal_first_byte(&segments[i + 1..]);
                let word = extract_word_until(&text[pos..], stop);
                if word.is_empty() {
                    return None;
                }
                // 11.4: back-reference enforcement. Second-and-later
                // occurrences of `$NAME` in the same pattern must
                // capture the same text the first occurrence did. Both
                // sides are whitespace-normalised first so a balanced
                // expression like `(x + y)` vs `(x+y)` unifies — the
                // bracket-balanced `extract_word` returns the interior
                // verbatim, so without normalisation `assertEqual($X,
                // $X)` would spuriously mismatch on argument formatting.
                let norm = normalise_capture(word);
                if let Some(prev) = bound.get(name) {
                    if prev != &norm {
                        return None;
                    }
                } else {
                    bound.insert(name.clone(), norm);
                }
                captures.push((name.clone(), word.to_string()));
                pos += word.len();
            }
            Segment::Wildcard => {
                // Skip whitespace
                while pos < text.len() && text.as_bytes()[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                let stop = next_literal_first_byte(&segments[i + 1..]);
                let word = extract_word_until(&text[pos..], stop);
                if word.is_empty() {
                    return None;
                }
                pos += word.len();
            }
            Segment::Ellipsis => {
                // $$$ — match everything until the next literal segment or end.
                // Uses `find_at_depth_zero` so a needle like `}` matches the
                // *balancing* closer of an outer `{`, not the first inner `}`.
                if let Some(next_lit) = find_next_literal(&segments[i + 1..]) {
                    if let Some(idx) = find_at_depth_zero(&text[pos..], next_lit.trim()) {
                        pos += idx;
                    } else {
                        return None;
                    }
                } else {
                    // No more segments — $$$ matches the rest
                    pos = text.len();
                }
            }
            Segment::NamedEllipsis(name) => {
                // $$$NAME / $$NAME — like Ellipsis but captures the
                // consumed text under `name` and enforces back-reference
                // equality on repeat occurrences (same semantics as
                // `Capture`, just over a multi-token / multi-line span).
                let start = pos;
                if let Some(next_lit) = find_next_literal(&segments[i + 1..]) {
                    if let Some(idx) = find_at_depth_zero(&text[pos..], next_lit.trim()) {
                        pos += idx;
                    } else {
                        return None;
                    }
                } else {
                    pos = text.len();
                }
                let captured = text[start..pos].trim().to_string();
                let norm = normalise_capture(&captured);
                if let Some(prev) = bound.get(name) {
                    if prev != &norm {
                        return None;
                    }
                } else {
                    bound.insert(name.clone(), norm);
                }
                captures.push((name.clone(), captured));
            }
        }
    }

    Some(captures)
}

/// Extract a "word" from the current position — an identifier or
/// balanced parens. Stops one byte early when the byte equals `stop`
/// (used to keep a capture from swallowing characters that belong to
/// the next literal segment). Pass `None` for unconstrained extraction.
fn extract_word_until(s: &str, stop: Option<u8>) -> &str {
    if s.is_empty() {
        return "";
    }
    let bytes = s.as_bytes();

    // If starts with a bracket, find the balanced closing bracket.
    if bytes[0] == b'(' || bytes[0] == b'{' || bytes[0] == b'[' {
        let open = bytes[0];
        let close = match open {
            b'(' => b')',
            b'{' => b'}',
            b'[' => b']',
            _ => unreachable!(),
        };
        let mut depth = 1;
        let mut i = 1;
        while i < bytes.len() && depth > 0 {
            if bytes[i] == open {
                depth += 1;
            } else if bytes[i] == close {
                depth -= 1;
            }
            i += 1;
        }
        return &s[..i];
    }

    // Otherwise, collect an identifier (alphanumeric + underscore + :: + <> + *).
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if stop == Some(b) {
            break;
        }
        if b.is_ascii_alphanumeric()
            || b == b'_'
            || b == b'<'
            || b == b'>'
            || b == b'*'
            || b == b'&'
            || b == b'.'
            || (b == b':' && i + 1 < bytes.len() && bytes[i + 1] == b':')
            || (b == b':' && i > 0 && bytes[i - 1] == b':')
        {
            i += 1;
        } else {
            break;
        }
    }
    &s[..i]
}

#[cfg(test)]
fn extract_word(s: &str) -> &str {
    extract_word_until(s, None)
}

/// First non-whitespace byte of the next literal segment, if any. Used
/// as a boundary for greedy `Capture` / `Wildcard` extraction.
fn next_literal_first_byte(segments: &[Segment]) -> Option<u8> {
    for seg in segments {
        if let Segment::Literal(lit) = seg {
            let trimmed = lit.trim_start();
            if let Some(b) = trimmed.bytes().next() {
                return Some(b);
            }
        }
    }
    None
}

/// Strip all whitespace so back-reference equality on balanced
/// expressions ignores formatting differences. `(x + y)`, `(x+y)`,
/// and `( x +  y )` all normalise to `(x+y)`. Identifier captures
/// (no whitespace) round-trip unchanged. String-literal interiors
/// with significant whitespace are a known edge case — back-refs
/// over identical string literals still unify because the bytes
/// match before stripping; pattern matches that depend on
/// preserving interior whitespace of a captured string aren't a
/// supported v1 workflow.
fn normalise_capture(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn find_next_literal(segments: &[Segment]) -> Option<&str> {
    for seg in segments {
        if let Segment::Literal(lit) = seg {
            if !lit.trim().is_empty() {
                return Some(lit);
            }
        }
    }
    None
}

/// Locate `needle` in `haystack` at a position that is **not** inside a
/// `"..."` string literal **and** at bracket-depth 0 relative to the
/// haystack's starting position.
///
/// Used by the `$$$` / `$$$NAME` ellipsis arms to terminate at the
/// *balancing* closer of an outer bracket rather than the first textual
/// occurrence of the closer. Example: pattern `class $T { $$$BODY }`
/// over `class C { void M() { x; } }` — the previous literal segment
/// has consumed `class C { ` so the walker enters at depth 0; the
/// inner `{...}` increments/decrements depth around `M`'s body; the
/// final `}` is reached at depth 0 and matched as the balancer.
///
/// Limits intentionally unhandled in v1 (filed as follow-ups):
/// - `'...'` char / single-quote string literals — Rust lifetimes
///   (`'a`) and char literals (`'}'`) are ambiguous without AST
///   context; affects C#, TypeScript, Python single-quote strings,
///   Rust char literals.
/// - Raw strings: Rust `r#"..."#`, C++ `R"(...)"`.
/// - Triple-quoted strings: Python `"""..."""`, `'''...'''`.
/// - Comments containing brackets — tree-sitter would skip them
///   structurally; the byte walker treats them as code.
///
/// Within `"..."`, `\` escapes the next byte (so `\"` and `\\` are
/// handled). Depth is clamped at zero via `saturating_sub`, so a
/// haystack with unbalanced extra closers degrades gracefully
/// instead of underflowing.
fn find_at_depth_zero(haystack: &str, needle: &str) -> Option<usize> {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() {
        return Some(0);
    }

    let mut depth: u32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut i = 0;

    while i < hb.len() {
        // Match check happens BEFORE updating state for byte `i`. This
        // is what lets a needle of `}` match the balancing closer of
        // an outer `{` — at that byte depth is still 0 (the depth-1
        // span is between an inner `{` and its `}`, not at the outer
        // closer). Symmetric for `)` / `]`.
        if !in_string && depth == 0 && hb[i..].starts_with(nb) {
            return Some(i);
        }

        let b = hb[i];
        if escape {
            // Previous byte was a `\` inside a string. Consume this
            // byte verbatim regardless of what it is — handles `\"`
            // (escaped quote keeps us inside the string) and `\\`
            // (the two backslashes pair off and any following `}`
            // remains in-string but is no longer escape-protected,
            // which is the correct semantics — it's still inside
            // `"..."` so the depth counter is untouched).
            escape = false;
        } else if in_string {
            match b {
                b'\\' => escape = true,
                b'"' => in_string = false,
                _ => {}
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'(' | b'{' | b'[' => depth += 1,
                b')' | b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }

        i += 1;
    }

    None
}

fn safe_node_text<'a>(node: Node, source: &'a str) -> &'a str {
    let start = node.start_byte();
    let mut end = node.end_byte().min(source.len());
    while end > start && !source.is_char_boundary(end) {
        end -= 1;
    }
    &source[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v1.11 hotfix — `$_NAME` (invalid metavar — `$_` must stand alone)
    /// must be parsed as a literal `$_NAME` so the typo doesn't silently
    /// degrade to matching `_NAME` with the `$` swallowed. The wildcard
    /// `$_` alone (no trailing alphanumeric) continues to parse to
    /// `Segment::Wildcard`.
    #[test]
    fn v1_11_hotfix_invalid_underscore_metavar_preserves_dollar() {
        // Invalid form `$_Bar` — must NOT match the literal `_Bar`,
        // because the source `_Bar` is not what the user wrote.
        let source = "let _Bar = 1;\n";
        let pattern = parse_pattern("$_Bar = 1", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        assert!(
            matches.is_empty(),
            "`$_Bar` must be a literal `$_Bar` (no `$` in source ⇒ no match); got {} matches",
            matches.len()
        );

        // Valid `$_` (anonymous wildcard) still works.
        let source = "let x = 1;\n";
        let pattern = parse_pattern("let $_ = 1", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        assert!(
            !matches.is_empty(),
            "`$_` standalone must remain a working wildcard"
        );
    }

    #[test]
    fn match_rust_function_signature() {
        let source = r#"
fn foo() -> i32 { 42 }
fn bar(x: &str) -> Result<(), Error> { Ok(()) }
fn baz() {}
"#;
        // Match functions returning Result (simple text match approach)
        let pattern = parse_pattern("fn $NAME($$$) -> Result", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");

        assert!(!matches.is_empty(), "should match 'bar' (returns Result)");
        assert_eq!(matches[0].captures[0].1, "bar");
    }

    #[test]
    fn match_python_class() {
        let source = "class UserService:\n    pass\n\nclass PaymentService:\n    pass\n\ndef helper():\n    pass\n";
        let pattern = parse_pattern("class $NAME:", Language::Python).unwrap();
        let matches = find_matches(source, &pattern, "test.py");
        assert_eq!(matches.len(), 2, "should match both classes");

        let names: Vec<&str> = matches
            .iter()
            .flat_map(|m| m.captures.iter().map(|(_, v)| v.as_str()))
            .collect();
        assert!(names.contains(&"UserService"));
        assert!(names.contains(&"PaymentService"));
    }

    #[test]
    fn match_rust_pub_struct() {
        let source = r#"
pub struct Foo { x: i32 }
struct Bar;
pub struct Baz { y: String }
"#;
        let pattern = parse_pattern("pub struct $NAME", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");

        assert!(
            matches.len() >= 2,
            "should match Foo and Baz, got {}",
            matches.len()
        );
    }

    #[test]
    fn match_with_ellipsis() {
        let source = r#"
fn process(x: i32, y: &str, z: bool) -> Result<(), Error> { Ok(()) }
fn simple() {}
"#;
        let pattern = parse_pattern("fn $NAME($$$)", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        assert!(matches.len() >= 2, "should match both functions");
    }

    #[test]
    fn no_match_returns_empty() {
        let source = "fn main() {}";
        let pattern = parse_pattern("class $NAME", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        assert!(matches.is_empty());
    }

    #[test]
    fn match_go_func() {
        let source = r#"
package main

func NewService(db *DB) *Service {
    return &Service{db: db}
}

func main() {}
"#;
        let pattern = parse_pattern("func $NAME($$$)", Language::Go).unwrap();
        let matches = find_matches(source, &pattern, "test.go");
        assert!(matches.len() >= 2, "should match both functions");
    }

    #[test]
    fn captures_correct_names() {
        let source = "fn alpha() {}\nfn beta() {}\nfn gamma() {}";
        let pattern = parse_pattern("fn $NAME()", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");

        let names: Vec<&str> = matches
            .iter()
            .flat_map(|m| m.captures.iter().map(|(_, v)| v.as_str()))
            .collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(names.contains(&"gamma"));
    }

    #[test]
    fn line_numbers_correct() {
        let source = "fn foo() {}\nfn bar() {}\nfn baz() {}";
        let pattern = parse_pattern("fn bar()", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        assert!(!matches.is_empty());
        assert_eq!(matches[0].line, 2);
    }

    // --- 11.4 metavar back-references ---

    #[test]
    fn back_reference_requires_same_value_in_both_occurrences() {
        // `$NAME($NAME)` is the canonical "function that calls itself
        // with the same name as the first argument" pattern. Should
        // match `foo(foo)` and reject `foo(bar)`.
        let source = "fn caller() { foo(foo); bar(baz); }\n";
        let pattern = parse_pattern("$NAME($NAME)", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        // Only `foo(foo)` is a self-pass; `bar(baz)` must not match
        // because the second `$NAME` would have to capture `baz` while
        // the first captured `bar`.
        let names: Vec<String> = matches
            .iter()
            .filter_map(|m| m.captures.first().map(|(_, v)| v.clone()))
            .collect();
        assert!(
            names.iter().any(|n| n == "foo"),
            "expected foo(foo) to match, got: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "bar"),
            "bar(baz) must not match — back-ref mismatch: {names:?}"
        );
    }

    #[test]
    fn back_reference_captures_both_binding_sites() {
        // Even though the value is the same, the captures list keeps
        // every binding site so output shows where each occurrence
        // landed in the matched text.
        let source = "fn use_twice() { record(state, state); }\n";
        let pattern = parse_pattern("record($NAME, $NAME)", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        let m = matches
            .iter()
            .find(|m| m.matched_text.contains("record"))
            .unwrap_or_else(|| panic!("expected back-ref match: {matches:?}"));
        let name_captures: Vec<&str> = m
            .captures
            .iter()
            .filter(|(n, _)| n == "NAME")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(name_captures, vec!["state", "state"]);
    }

    #[test]
    fn back_reference_normalises_interior_whitespace() {
        // The bracket-balanced `extract_word` returns interior bytes
        // verbatim. Without normalisation `assertEqual($X, $X)`
        // against `assertEqual((a + b), (a+b))` would spuriously
        // mismatch on the whitespace difference. The normalised
        // back-ref equality fires so the pattern matches.
        let source = "fn t() { assertEqual((a + b), (a+b)); }\n";
        let pattern = parse_pattern("assertEqual($X, $X)", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        assert!(
            matches
                .iter()
                .any(|m| m.matched_text.contains("assertEqual")),
            "whitespace-only diff should not block back-ref: {matches:?}"
        );
    }

    #[test]
    fn normalise_capture_strips_all_whitespace() {
        assert_eq!(normalise_capture("foo"), "foo");
        assert_eq!(normalise_capture("(x + y)"), "(x+y)");
        assert_eq!(normalise_capture("( x  +   y )"), "(x+y)");
        assert_eq!(normalise_capture("\tindented\n"), "indented");
    }

    #[test]
    fn distinct_metavar_names_remain_independent() {
        // `$A` and `$B` are different binders; they don't constrain
        // each other. Make sure the back-ref logic doesn't accidentally
        // collapse distinct names.
        let source = "fn pair() { connect(client, server); }\n";
        let pattern = parse_pattern("connect($A, $B)", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        let m = matches
            .iter()
            .find(|m| m.matched_text.contains("connect"))
            .unwrap_or_else(|| panic!("expected $A/$B match: {matches:?}"));
        let by_name: std::collections::HashMap<&str, &str> = m
            .captures
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        assert_eq!(by_name.get("A"), Some(&"client"));
        assert_eq!(by_name.get("B"), Some(&"server"));
    }

    // --- 11.4 Inc 6: $$$NAME / $$NAME multi-line metavars ---

    #[test]
    fn parses_named_block_ellipsis() {
        let pattern = parse_pattern("fn $F($$ARGS) { $$$BODY }", Language::Rust).unwrap();
        // Literal("fn ") Capture("F") Literal("(") NamedEllipsis("ARGS")
        // Literal(") { ") NamedEllipsis("BODY") Literal(" }")
        assert!(matches!(pattern.segments[3], Segment::NamedEllipsis(ref n) if n == "ARGS"));
        assert!(matches!(pattern.segments[5], Segment::NamedEllipsis(ref n) if n == "BODY"));
    }

    #[test]
    fn anonymous_triple_dollar_still_parses_as_ellipsis() {
        let pattern = parse_pattern("fn $NAME($$$) -> Result", Language::Rust).unwrap();
        assert!(matches!(pattern.segments[3], Segment::Ellipsis));
    }

    #[test]
    fn matches_multiline_function_body_with_named_block_ellipsis() {
        let source = "fn process(x: i32) -> Result<i32, Error> {\n    let y = x + 1;\n    let z = y * 2;\n    Ok(z)\n}\n";
        let pattern = parse_pattern(
            "fn $NAME($$ARGS) -> Result<$T, $E> { $$$BODY }",
            Language::Rust,
        )
        .unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        assert!(!matches.is_empty(), "expected match across multi-line body");
        let by_name: std::collections::HashMap<&str, &str> = matches[0]
            .captures
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        assert_eq!(by_name.get("NAME"), Some(&"process"));
        assert_eq!(by_name.get("T"), Some(&"i32"));
        assert_eq!(by_name.get("E"), Some(&"Error"));
    }

    #[test]
    fn capture_stops_before_next_literal_byte() {
        // Pre-Inc-6, `extract_word` greedily consumed `>` to support
        // `Result<T>`-style inline generics. Patterns with a literal
        // `>` after a capture (e.g. `Result<$T, $E>`) silently lost
        // matches because `$E` swallowed the closing `>`. The
        // `extract_word_until` boundary lookahead pins the fix.
        let bounded = extract_word_until("Error>", Some(b'>'));
        assert_eq!(bounded, "Error");
        let unbounded = extract_word("Error>");
        assert_eq!(unbounded, "Error>");
    }

    // --- 11.4 Inc 7: AND / OR composition ---

    #[test]
    fn split_top_level_respects_bracket_depth() {
        // `&&` inside parens is not a split point.
        let parts = split_top_level("f($X && $Y) && g($Z)", "&&");
        assert_eq!(parts, vec!["f($X && $Y)", "g($Z)"]);
    }

    #[test]
    fn split_top_level_requires_space_flank() {
        // `&&` not space-flanked (no surrounding spaces) is left alone.
        let parts = split_top_level("a&&b", "&&");
        assert_eq!(parts, vec!["a&&b"]);
    }

    #[test]
    fn parse_composite_single_pattern_has_one_branch() {
        let comp = parse_composite_pattern("fn $NAME()", Language::Rust).unwrap();
        assert_eq!(comp.disjuncts.len(), 1);
        assert_eq!(comp.disjuncts[0].len(), 1);
        assert!(!comp.has_or());
    }

    #[test]
    fn parse_composite_and_two_conjuncts() {
        let comp = parse_composite_pattern("struct $S && impl $S", Language::Rust).unwrap();
        assert_eq!(comp.disjuncts.len(), 1, "no OR → one disjunct");
        assert_eq!(comp.disjuncts[0].len(), 2, "AND → two conjuncts");
        assert!(!comp.has_or());
    }

    #[test]
    fn parse_composite_or_two_disjuncts() {
        let comp =
            parse_composite_pattern("interface $N || class $N", Language::TypeScript).unwrap();
        assert_eq!(comp.disjuncts.len(), 2);
        assert!(comp.has_or());
    }

    #[test]
    fn parse_composite_and_or_precedence() {
        // `a && b || c && d` → (a && b) || (c && d). `&&` binds tighter.
        let comp =
            parse_composite_pattern("fn $A && fn $B || fn $C && fn $D", Language::Rust).unwrap();
        assert_eq!(comp.disjuncts.len(), 2, "two OR branches");
        assert_eq!(comp.disjuncts[0].len(), 2, "left branch has 2 ANDs");
        assert_eq!(comp.disjuncts[1].len(), 2, "right branch has 2 ANDs");
    }

    #[test]
    fn captures_agree_when_shared_name_matches() {
        let c1 = vec![("S".to_string(), "Foo".to_string())];
        let c2 = vec![("S".to_string(), "Foo".to_string())];
        assert!(captures_agree(&c1, &c2));
    }

    #[test]
    fn captures_disagree_when_shared_name_differs() {
        let c1 = vec![("S".to_string(), "Foo".to_string())];
        let c2 = vec![("S".to_string(), "Bar".to_string())];
        assert!(!captures_agree(&c1, &c2));
    }

    #[test]
    fn captures_agree_when_names_disjoint() {
        // No shared metavar → always agree, no constraint to enforce.
        let c1 = vec![("S".to_string(), "Foo".to_string())];
        let c2 = vec![("T".to_string(), "Bar".to_string())];
        assert!(captures_agree(&c1, &c2));
    }

    #[test]
    fn and_intersects_on_back_referenced_capture() {
        // Bar has no impl → must not appear in the result.
        let source = "struct Foo;\nstruct Bar;\nimpl Foo { fn f(&self) {} }\n";
        let comp = parse_composite_pattern("struct $S && impl $S", Language::Rust).unwrap();
        let matches = find_matches_composite(source, &comp, "test.rs");
        // Foo passes the AND; Bar is filtered out by the back-ref.
        assert!(matches
            .iter()
            .any(|m| m.captures.iter().any(|(_, v)| v == "Foo")));
        assert!(!matches
            .iter()
            .any(|m| m.captures.iter().any(|(_, v)| v == "Bar")));
    }

    // --- 11.4 review H findings (post-Inc-7) ---

    #[test]
    fn and_anchor_uses_first_conjunct_line() {
        // Reported `line` is anchored at the first conjunct's match. The
        // direction matters: swapping the conjuncts swaps the reported
        // line — this pins the documented contract.
        let source = "struct Foo;\nimpl Foo { fn f(&self) {} }\n";

        let comp = parse_composite_pattern("struct $S && impl $S", Language::Rust).unwrap();
        let matches = find_matches_composite(source, &comp, "test.rs");
        assert!(!matches.is_empty());
        assert_eq!(matches[0].line, 1, "first conjunct is `struct` on line 1");

        let comp = parse_composite_pattern("impl $S && struct $S", Language::Rust).unwrap();
        let matches = find_matches_composite(source, &comp, "test.rs");
        assert!(!matches.is_empty());
        assert_eq!(matches[0].line, 2, "first conjunct is `impl` on line 2");
    }

    #[test]
    fn matches_single_line_result_generic() {
        // The `extract_word_until` bonus fix in Inc 6 must also hold for
        // single-line patterns — only the multi-line fixture exercised
        // it through the full pipeline before this test.
        let source = "fn f(x: i32) -> Result<i32, String> { Ok(x) }\n";
        let pattern = parse_pattern("fn $N($$$) -> Result<$T, $E>", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        assert!(!matches.is_empty());
        let caps: HashMap<&str, &str> = matches[0]
            .captures
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(caps.get("T"), Some(&"i32"));
        assert_eq!(caps.get("E"), Some(&"String"));
    }

    #[test]
    fn and_with_disjoint_metavars_both_must_match() {
        // `fn $A() && struct $B` — independent metavar names; AND just
        // requires both shapes present in the file, with no cross-capture
        // constraint.
        let comp = parse_composite_pattern("fn $A() && struct $B", Language::Rust).unwrap();

        // Both present → one composite match.
        let both = "fn foo() {}\nstruct Bar;\n";
        let matches = find_matches_composite(both, &comp, "test.rs");
        assert_eq!(
            matches.len(),
            1,
            "both shapes present must match exactly once"
        );
        let caps: HashMap<&str, &str> = matches[0]
            .captures
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(caps.get("A"), Some(&"foo"));
        assert_eq!(caps.get("B"), Some(&"Bar"));

        // Only struct → AND fails because `fn $A()` has no matches.
        let struct_only = "struct Bar;\n";
        assert!(find_matches_composite(struct_only, &comp, "test.rs").is_empty());

        // Only fn → AND fails because `struct $B` has no matches.
        let fn_only = "fn foo() {}\n";
        assert!(find_matches_composite(fn_only, &comp, "test.rs").is_empty());
    }

    #[test]
    fn parse_composite_empty_middle_conjunct_errors() {
        // Two consecutive space-flanked `&&` produce an empty middle
        // conjunct. `parse_pattern` on the empty leaf must bail with
        // the existing empty-pattern message — pinned so tooling that
        // matches on the error text doesn't break on a future rewording.
        let err = parse_composite_pattern("fn $A && && fn $B", Language::Rust).unwrap_err();
        assert!(
            err.to_string().contains("empty pattern"),
            "expected 'empty pattern' in error, got: {err}"
        );
    }

    #[test]
    fn parse_composite_empty_middle_disjunct_errors() {
        // Same logic for `||`.
        let err = parse_composite_pattern("fn $A || || fn $B", Language::Rust).unwrap_err();
        assert!(
            err.to_string().contains("empty pattern"),
            "expected 'empty pattern' in error, got: {err}"
        );
    }

    #[test]
    fn or_takes_union_of_disjuncts() {
        let source = "interface Animal {}\nclass Dog {}\nclass Cat {}\nfunction helper() {}\n";
        let comp =
            parse_composite_pattern("interface $N || class $N", Language::TypeScript).unwrap();
        let matches = find_matches_composite(source, &comp, "test.ts");
        let names: std::collections::HashSet<String> = matches
            .iter()
            .flat_map(|m| m.captures.iter().map(|(_, v)| v.clone()))
            .collect();
        // `helper` is a function — neither disjunct should pick it up.
        assert!(names.contains("Animal"));
        assert!(names.contains("Dog"));
        assert!(names.contains("Cat"));
        assert!(!names.contains("helper"));
    }

    #[test]
    fn parse_segments() {
        let pattern = parse_pattern("fn $NAME($$$) -> Result", Language::Rust).unwrap();
        assert_eq!(pattern.segments.len(), 5);
        assert!(matches!(pattern.segments[0], Segment::Literal(ref s) if s == "fn "));
        assert!(matches!(pattern.segments[1], Segment::Capture(ref s) if s == "NAME"));
        assert!(matches!(pattern.segments[2], Segment::Literal(ref s) if s == "("));
        assert!(matches!(pattern.segments[3], Segment::Ellipsis));
        assert!(matches!(pattern.segments[4], Segment::Literal(ref s) if s == ") -> Result"));
    }

    #[test]
    fn named_ellipsis_tracks_brace_nesting_post_h4() {
        // Post-H4: an `$$$BODY` that terminates on `}` matches the
        // *balancing* closer of the outer `{`, skipping nested
        // `{ ... }` blocks. Pre-H4 this asserted the opposite (BODY
        // truncated at the first inner `}`); see git history for the
        // old shape.
        let source = "class C {\n    void Run() { let inner = 1; }\n}\n";
        let pattern = parse_pattern("class $T { $$$BODY }", Language::TypeScript).unwrap();
        let matches = find_matches(source, &pattern, "test.ts");
        assert_eq!(matches.len(), 1, "one match expected");
        let body = matches[0]
            .captures
            .iter()
            .find(|(k, _)| k == "BODY")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        // The capture now spans the entire inner method including its
        // own closing `}`; only the class's balancing `}` is excluded.
        assert!(
            body.contains("void Run() { let inner = 1; }"),
            "BODY should include the nested method's closing `}}`. \
             Got: {body:?}",
        );
        // Sanity: the capture must contain exactly one `}` — the
        // inner method's closing brace. The class's balancing `}` is
        // consumed by the trailing pattern literal and must NOT be in
        // the capture.
        assert_eq!(
            body.matches('}').count(),
            1,
            "BODY must contain exactly one `}}` (the inner method's). \
             Got: {body:?}",
        );
    }

    // --- H4 (external-review v1.9.1): AST-aware ellipsis termination ---
    //
    // The next three tests RED on pre-H4 `main` and GREEN once
    // `find_at_depth_zero` lands. They exercise the two failure
    // modes called out by the reviewer:
    //   (a) nested `{ ... }` inside `$$$BODY` truncates at the
    //       inner `}`;
    //   (b) a string literal containing `}` truncates at the
    //       string-internal `}`.

    #[test]
    fn named_ellipsis_balances_nested_braces() {
        // Pre-H4: BODY truncates at the inner `}` of `Run()`.
        // Post-H4: BODY spans the whole class body including the
        // nested method's braces.
        let source = "class C {\n    void Run() { let inner = 1; }\n}\n";
        let pattern = parse_pattern("class $T { $$$BODY }", Language::TypeScript).unwrap();
        let matches = find_matches(source, &pattern, "test.ts");
        assert_eq!(matches.len(), 1, "one match expected, got {matches:?}");
        let body = matches[0]
            .captures
            .iter()
            .find(|(k, _)| k == "BODY")
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("BODY capture missing: {:?}", matches[0].captures));
        // Strict: the full method (including its `}`) must be in the
        // capture; only the class's balancing `}` is excluded.
        assert!(
            body.contains("void Run() { let inner = 1; }"),
            "BODY must include the nested method's closing `}}`. Got: {body:?}",
        );
    }

    #[test]
    fn named_ellipsis_balances_nested_braces_with_auto_property() {
        // The exact case spec.toml's `csharp_class_body` fixture had
        // to dodge — `{ get; set; }` auto-properties. Post-H4 this
        // must capture both the field and the auto-property.
        let source = "public class UserService {\n    public int counter;\n    public string Label { get; set; }\n}\n";
        let pattern = parse_pattern("public class $NAME { $$$BODY }", Language::CSharp).unwrap();
        let matches = find_matches(source, &pattern, "test.cs");
        assert_eq!(matches.len(), 1, "one match expected, got {matches:?}");
        let body = matches[0]
            .captures
            .iter()
            .find(|(k, _)| k == "BODY")
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("BODY capture missing: {:?}", matches[0].captures));
        assert!(
            body.contains("public int counter;"),
            "BODY must include the field. Got: {body:?}",
        );
        assert!(
            body.contains("public string Label { get; set; }"),
            "BODY must include the auto-property (with its nested `{{ get; set; }}`). \
             Got: {body:?}",
        );
    }

    #[test]
    fn named_ellipsis_skips_brace_inside_string() {
        // Pre-H4: the `}` inside the `"}"` string literal terminates
        // the ellipsis early, truncating BODY at the wrong place and
        // leaving the source's outer `}` unmatched against the
        // pattern's trailing literal. Post-H4: the in-string `}` is
        // ignored, BODY spans up to the real closing `}`.
        let source = "fn parse() {\n    let close = \"}\";\n    println!(\"{}\", close);\n}\n";
        let pattern = parse_pattern("fn $F() { $$$BODY }", Language::Rust).unwrap();
        let matches = find_matches(source, &pattern, "test.rs");
        assert_eq!(matches.len(), 1, "one match expected, got {matches:?}");
        let body = matches[0]
            .captures
            .iter()
            .find(|(k, _)| k == "BODY")
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("BODY capture missing: {:?}", matches[0].captures));
        assert!(
            body.contains("let close = \"}\";"),
            "BODY must include the string-literal-containing-brace line. Got: {body:?}",
        );
        assert!(
            body.contains("println!"),
            "BODY must reach past the string-literal line. Got: {body:?}",
        );
    }

    #[test]
    fn anonymous_ellipsis_balances_braces() {
        // `$$$` (anonymous Ellipsis) shares the same forward-scan
        // logic as `$$$NAME` and must benefit from the same fix.
        // Pre-H4 the surrounding match fails because `$$$` truncates
        // early and the trailing `}` of the pattern can't line up
        // with the outer source `}`.
        let source = "class C {\n    void Run() { let inner = 1; }\n}\n";
        let pattern = parse_pattern("class $T { $$$ }", Language::TypeScript).unwrap();
        let matches = find_matches(source, &pattern, "test.ts");
        assert!(
            !matches.is_empty(),
            "anonymous $$$ must match the full class body — pre-H4 \
             it truncated inside Run() and broke the trailing `}}`. \
             Got: {matches:?}",
        );
    }

    // --- H4 unit tests for the new depth-aware finder ---

    #[test]
    fn find_at_depth_zero_empty_needle_returns_zero() {
        // Contract: empty needle is trivially found at offset 0.
        assert_eq!(find_at_depth_zero("anything", ""), Some(0));
    }

    #[test]
    fn find_at_depth_zero_no_brackets_falls_back_to_first_hit() {
        // Without any brackets the walker should behave like `str::find`.
        assert_eq!(find_at_depth_zero("hello world }", "}"), Some(12));
    }

    #[test]
    fn find_at_depth_zero_simple_close_at_depth_zero() {
        // The first `}` IS at depth 0 (nothing opened it); return its
        // offset directly. Previous literal segment is assumed to have
        // consumed the matching opener.
        assert_eq!(find_at_depth_zero("}", "}"), Some(0));
        assert_eq!(find_at_depth_zero("   }", "}"), Some(3));
    }

    #[test]
    fn find_at_depth_zero_skips_nested_braces() {
        // The inner `}` of `{ x; }` is at depth 1 and must NOT match;
        // the outer `}` is at depth 0 and is the balancing closer.
        let s = "{ x; } }";
        assert_eq!(find_at_depth_zero(s, "}"), Some(s.len() - 1));
    }

    #[test]
    fn find_at_depth_zero_skips_brace_inside_double_string() {
        // The `}` inside `"}"` is in a string region and must not be
        // counted. The trailing `}` (after the string closes) is the
        // real depth-0 closer.
        let s = "  \"}\"  }";
        let idx = find_at_depth_zero(s, "}").expect("must find outer `}`");
        assert_eq!(&s[idx..], "}");
    }

    #[test]
    fn find_at_depth_zero_handles_escaped_quote_and_backslash() {
        // `"\"}\\"` — escaped quote keeps us inside the string;
        // the embedded `}` must be ignored.
        let s = r#""\"}" }"#;
        let idx = find_at_depth_zero(s, "}").expect("must find outer `}`");
        assert_eq!(&s[idx..], "}");
        // `"\\"` (two backslashes — one literal backslash) followed by
        // a `}` outside the string: the `\\` consumes both `\`s so the
        // closing `"` works, then we exit the string before reading the
        // outer `}`.
        let s2 = r#""\\" }"#;
        let idx2 = find_at_depth_zero(s2, "}").expect("must find outer `}`");
        assert_eq!(&s2[idx2..], "}");
    }

    #[test]
    fn find_at_depth_zero_multi_char_needle() {
        // Needle of more than one byte must match as a contiguous
        // span and only at depth 0 / outside strings. The haystack
        // `{ } } else { }` has TWO `}` bytes that could (textually)
        // start a `} else` match:
        //   - byte 2: check fires while depth==1 (the `{` at byte 0
        //     bumped it; the check happens BEFORE consuming this
        //     `}`), so the depth-0 guard rejects it.
        //   - byte 4: check fires at depth==0, but `&s[4..]` is
        //     `} else { }` — wait, is it `}` or ` `? Let me recount.
        //     s = `{ } } else { }` → bytes:
        //          0 `{`, 1 ` `, 2 `}`, 3 ` `, 4 `}`, 5 ` `, 6 `e`, …
        //     At byte 4 depth is 0 and `&s[4..]` = `} else { }`,
        //     which DOES start with `"} else"`. So the match fires
        //     at byte 4.
        let s = "{ } } else { }";
        let idx = find_at_depth_zero(s, "} else").expect("must find `} else`");
        assert_eq!(idx, 4, "match must land on the depth-0 `}}` at byte 4");
        assert_eq!(&s[idx..idx + 6], "} else");
    }

    #[test]
    fn find_at_depth_zero_returns_none_when_needle_missing() {
        assert_eq!(find_at_depth_zero("no closer here", "}"), None);
        // Even when the haystack has matching brackets, a missing
        // needle still yields None.
        assert_eq!(find_at_depth_zero("{ } { }", "X"), None);
    }

    #[test]
    fn find_at_depth_zero_balances_parens_and_brackets() {
        // Depth tracking covers `() {} []` together. A `}` inside
        // a `(...)` parenthesised expression should still be ignored
        // when the needle is `}` at top level — pathological case but
        // pins the symmetry.
        let s = "({ }) }";
        let idx = find_at_depth_zero(s, "}").expect("must find outer `}`");
        assert_eq!(&s[idx..], "}");
    }

    #[test]
    fn find_at_depth_zero_saturates_underflow() {
        // A haystack starting with an unbalanced closer must not
        // underflow `depth`. The closer itself is at depth 0 and the
        // walker must keep going past it without panicking.
        let s = "}} X";
        // Needle `X` lives at depth 0 after the two stray closers.
        let idx = find_at_depth_zero(s, "X").expect("must find `X` past unbalanced closers");
        assert_eq!(&s[idx..idx + 1], "X");
    }
}
