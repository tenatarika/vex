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

use tree_sitter::Node;

use crate::parse::language::Language;

use super::PatternMatch;

mod parse;
// `parse_composite_pattern` is the only in-crate consumer (called from
// `pattern::mod`). `parse_pattern` is intentionally NOT re-exported —
// its only callers are this directory's tests, which reach it via
// `super::parse::parse_pattern` directly.
pub use parse::parse_composite_pattern;

/// Compiled pattern — a list of segments (literal or metavar).
#[derive(Debug)]
pub struct PatternTree {
    segments: Vec<Segment>,
    pub lang: Language,
}

impl PatternTree {
    /// Sibling-module constructor. `pub(super)` so `parse::parse_pattern`
    /// can build a `PatternTree` without making the `segments` field
    /// itself visible outside this directory module.
    pub(super) fn new(segments: Vec<Segment>, lang: Language) -> Self {
        Self { segments, lang }
    }
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
pub(super) enum Segment {
    Literal(String),
    Capture(String),       // $NAME — single token / balanced expression
    Wildcard,              // $_
    Ellipsis,              // $$$  — anonymous ellipsis
    NamedEllipsis(String), // $$$NAME or $$NAME — named ellipsis with capture
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
    // Guarded parse entry point — see the note in `hierarchy::find_in_source`.
    // `vex pattern` live-scans arbitrary repository files, so an unguarded
    // parser here is exposed to the same runaway-grammar input the v1.23.0
    // callback budget exists to bound.
    let Ok(tree) = crate::parse::parser_pool::parse_text(pattern.lang, source) else {
        return Vec::new();
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
                    pos += find_at_depth_zero(&text[pos..], next_lit.trim())?;
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
                    pos += find_at_depth_zero(&text[pos..], next_lit.trim())?;
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
mod tests;
