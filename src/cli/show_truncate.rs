//! Phase 13.3 — Smart `vex show` truncation helpers.
//!
//! Pure string-slicing utilities (no tree-sitter, no AST) that trim a
//! symbol body into one of four shapes so agents can `show` a symbol
//! without burning the full body in their context window:
//!
//! - [`signature_only`] — keep only the declaration line(s) up to the
//!   `{` / `:` / `=>` / `=` that marks "end of declaration".
//! - [`head_n`]          — keep the first `N` lines, append a
//!   `... (M more lines)` ellipsis when truncated.
//! - [`no_body`]         — keep the signature plus any leading
//!   docstring / comments / attributes / decorators.
//! - [`collapsed`]       — placeholder for future language-aware
//!   nested-body replacement. Ships as a NO-OP in v1.9 that simply
//!   returns the body unchanged; the caller emits a `tracing::warn!`.
//!
//! All four helpers are pure functions on `&str` and return owned
//! `String`s, never mutating the input. They are kept deliberately
//! conservative: they treat the body as plain text and accept that
//! exotic syntax (e.g. `{` inside a string literal on the signature
//! line) may produce slightly off results. The trade-off is zero
//! re-parse cost.

/// Truncation mode (matches the surface CLI flag set). Serializes into
/// the JSON envelope's `truncation.mode` field as `kebab-case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TruncationMode {
    SignatureOnly,
    Head,
    NoBody,
    Collapsed,
}

impl TruncationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TruncationMode::SignatureOnly => "signature-only",
            TruncationMode::Head => "head",
            TruncationMode::NoBody => "no-body",
            TruncationMode::Collapsed => "collapsed",
        }
    }
}

/// Result of any truncation transform — carries enough information for
/// the JSON envelope to surface a `truncation { mode, original_lines,
/// kept_lines }` metadata block.
#[derive(Debug, Clone)]
pub struct TruncatedBody {
    pub body: String,
    pub mode: TruncationMode,
    pub original_lines: usize,
    pub kept_lines: usize,
}

/// Count physical lines in a body string. We treat an empty input as
/// zero lines (matches `str::lines()` behaviour).
fn line_count(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.lines().count()
    }
}

/// Locate the byte offset of the signature-end marker across the
/// *entire* body (not per-line). This is required because a multi-line
/// signature like `fn foo(\n    x: i32,\n) -> i32 {` has its `:` inside
/// parens on line 2 — a per-line scan would erroneously fire there.
/// Returns the byte position of the marker character (`{` or `:`).
fn find_signature_end_global(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut i = 0;
    let mut in_string: Option<u8> = None;
    let mut paren_depth: i32 = 0;
    let mut bracket_depth: i32 = 0;
    let mut brace_depth: i32 = 0;
    let mut angle_depth: i32 = 0;
    let mut line_is_comment = false;
    let mut at_line_start = true;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            line_is_comment = false;
            at_line_start = true;
            i += 1;
            continue;
        }
        if line_is_comment {
            i += 1;
            continue;
        }
        if at_line_start && (b == b' ' || b == b'\t') {
            i += 1;
            continue;
        }
        // Detect a full-line comment (`//`, `#`, `--`, `///`). Block
        // comments aren't perfectly handled but agents rarely place
        // `/* ... { ... */` on a signature line.
        if at_line_start {
            at_line_start = false;
            let rest = &bytes[i..];
            if rest.starts_with(b"//") || rest.starts_with(b"#") || rest.starts_with(b"--") {
                line_is_comment = true;
                i += 1;
                continue;
            }
        }
        if let Some(q) = in_string {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == q {
                in_string = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => {
                in_string = Some(b);
                i += 1;
            }
            b'(' => {
                paren_depth += 1;
                i += 1;
            }
            b')' => {
                paren_depth -= 1;
                i += 1;
            }
            b'[' => {
                bracket_depth += 1;
                i += 1;
            }
            b']' => {
                bracket_depth -= 1;
                i += 1;
            }
            b'<' => {
                let prev_meaningful = (0..i)
                    .rev()
                    .map(|j| bytes[j])
                    .find(|c| *c != b' ' && *c != b'\t' && *c != b'\n');
                if matches!(prev_meaningful, Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'>')
                {
                    angle_depth += 1;
                }
                i += 1;
            }
            b'>' => {
                if angle_depth > 0 {
                    angle_depth -= 1;
                }
                i += 1;
            }
            b'{' => {
                if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 {
                    return Some(i);
                }
                brace_depth += 1;
                i += 1;
            }
            b'}' => {
                brace_depth -= 1;
                i += 1;
            }
            b':' => {
                let next_is_colon = bytes.get(i + 1) == Some(&b':');
                let prev_is_colon = i > 0 && bytes[i - 1] == b':';
                if !next_is_colon
                    && !prev_is_colon
                    && paren_depth == 0
                    && bracket_depth == 0
                    && brace_depth == 0
                    && angle_depth == 0
                {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// `--signature-only` transform: scan the body for the first
/// signature-end marker (`{` / Python `:`) at top-level grouping depth
/// and return everything *up to* that marker. Trailing whitespace and
/// a trailing newline are trimmed so a brace on its own line doesn't
/// leave a dangling blank line in the output. If no marker is found,
/// the body is returned untouched (one-line declarations with no body).
pub fn signature_only(body: &str) -> TruncatedBody {
    let original_lines = line_count(body);
    let Some(marker) = find_signature_end_global(body) else {
        return TruncatedBody {
            body: body.to_string(),
            mode: TruncationMode::SignatureOnly,
            original_lines,
            kept_lines: original_lines,
        };
    };
    let prefix = &body[..marker];
    // Trim trailing whitespace AND a trailing blank line so we don't
    // emit `") -> i32\n"` as `") -> i32\n"` when the brace lives on
    // its own line — collapsing it down to the previous content line
    // is the principle-of-least-surprise output.
    let trimmed = prefix.trim_end_matches(|c: char| c.is_whitespace());
    let kept_lines = line_count(trimmed);
    TruncatedBody {
        body: trimmed.to_string(),
        mode: TruncationMode::SignatureOnly,
        original_lines,
        kept_lines,
    }
}

/// `--head N` transform: keep the first `N` lines verbatim; if the
/// body has more than `N` lines, append a `... (M more lines)`
/// indicator. When the body already fits, return it untouched (no
/// ellipsis, no metadata noise).
pub fn head_n(body: &str, n: usize) -> TruncatedBody {
    let original_lines = line_count(body);
    if n == 0 {
        // Defensive — clap should reject `--head 0` but if a future
        // call site bypasses that, emit an empty body rather than
        // panic on `take(0)` semantics.
        return TruncatedBody {
            body: String::new(),
            mode: TruncationMode::Head,
            original_lines,
            kept_lines: 0,
        };
    }
    if original_lines <= n {
        return TruncatedBody {
            body: body.to_string(),
            mode: TruncationMode::Head,
            original_lines,
            kept_lines: original_lines,
        };
    }
    let mut kept: Vec<&str> = body.lines().take(n).collect();
    let more = original_lines - n;
    let trailer = format!("... ({more} more lines)");
    let mut joined = kept.join("\n");
    // We need an owned String for the trailer line; push it directly.
    joined.push('\n');
    joined.push_str(&trailer);
    kept.clear(); // drop borrows before returning
    TruncatedBody {
        body: joined,
        mode: TruncationMode::Head,
        original_lines,
        kept_lines: n,
    }
}

/// Is this line a "leading" line that should stick with the signature?
/// We accept:
/// - blank lines (preserve spacing)
/// - line comments (`//`, `#`, `///`, `//!`, `--`)
/// - block-comment markers (`/*`, `*`, `*/`)
/// - Python triple-quoted docstrings (a line starting with `"""` or
///   `'''` after trimming)
/// - decorators / attributes (`@foo`, `#[derive(...)]`)
fn is_leading_line(line: &str) -> bool {
    let t = line.trim_start();
    if t.is_empty() {
        return true;
    }
    let starts_with = |p: &str| t.starts_with(p);
    starts_with("//")
        || starts_with("#")
        || starts_with("/*")
        || starts_with("*/")
        || starts_with("*")
        || starts_with("\"\"\"")
        || starts_with("'''")
        || starts_with("@")
        || starts_with("--")
}

/// Compute the 0-indexed line number of the byte offset `pos` in
/// `body`. Lines are 1-indexed in CLI output but we return 0-indexed
/// here because the caller does the +1 conversion.
fn line_of_offset(body: &str, pos: usize) -> usize {
    body.as_bytes()
        .iter()
        .take(pos)
        .filter(|&&b| b == b'\n')
        .count()
}

/// `--no-body` transform: keep the signature line(s) plus any
/// docstring / comment lines that immediately follow. Drop the rest.
///
/// The heuristic is:
/// 1. Locate the global signature-end marker.
/// 2. Keep all lines up to and including the marker line.
/// 3. Scan forward from the next line picking up docstring / comment /
///    blank / attribute lines, stopping at the first "real" line.
///
/// For Python-style `def foo():\n    """docstring"""\n    return ...`
/// we keep the signature + docstring; the `return` line is dropped.
pub fn no_body(body: &str) -> TruncatedBody {
    let original_lines = line_count(body);
    let lines: Vec<&str> = body.lines().collect();

    let sig_line_idx = match find_signature_end_global(body) {
        Some(marker) => line_of_offset(body, marker),
        None => 0,
    };

    let mut kept: Vec<String> = Vec::new();
    // Lines *before* the signature line (attributes, leading docs)
    // ride along verbatim.
    for line in &lines[..sig_line_idx] {
        kept.push((*line).to_string());
    }
    // Signature line itself — strip whatever follows the marker so a
    // one-line body (`fn foo() -> i32 { 1 }`) doesn't leak the body.
    if let Some(sig_line) = lines.get(sig_line_idx) {
        // Find the marker column on this specific line by re-scanning
        // the byte offset. Cheap because the line is short.
        let marker_col = find_signature_end_global(sig_line).unwrap_or(sig_line.len());
        let mut sig = sig_line[..marker_col].to_string();
        let trimmed_len = sig.trim_end().len();
        sig.truncate(trimmed_len);
        kept.push(sig);
    }
    // Trailing docstring/comment block.
    let mut docstring_state: DocstringState = DocstringState::None;
    for line in &lines[sig_line_idx + 1..] {
        if docstring_state.in_block() {
            kept.push((*line).to_string());
            let t = line.trim();
            if t.ends_with("\"\"\"") || t.ends_with("'''") {
                docstring_state = DocstringState::None;
            }
            continue;
        }
        if !is_leading_line(line) {
            break;
        }
        kept.push((*line).to_string());
        let t = line.trim();
        if (t.starts_with("\"\"\"") || t.starts_with("'''"))
            && !(t.len() > 3 && (t.ends_with("\"\"\"") || t.ends_with("'''")))
        {
            docstring_state = if t.starts_with("\"\"\"") {
                DocstringState::DoubleQuoted
            } else {
                DocstringState::SingleQuoted
            };
        }
    }
    let kept_lines = kept.len();
    let body = kept.join("\n");
    TruncatedBody {
        body,
        mode: TruncationMode::NoBody,
        original_lines,
        kept_lines,
    }
}

#[derive(Debug, Clone, Copy)]
enum DocstringState {
    None,
    DoubleQuoted,
    SingleQuoted,
}

impl DocstringState {
    fn in_block(self) -> bool {
        !matches!(self, DocstringState::None)
    }
}

/// `--collapsed` transform — **v1.9 NO-OP**.
///
/// The roadmap entry (`--collapsed` in 13.3) calls for replacing
/// nested method bodies inside class/impl/module bodies with `// ...`
/// placeholders. Doing this robustly needs language-aware brace
/// tracking (Python dedent, Rust brace depth, etc.), which is too
/// hairy for the v1.9 window. We ship the flag-shape now (so MCP
/// schemas and downstream agent prompts can reference it) but emit
/// the body unchanged. Callers emit a `tracing::warn!` to surface the
/// stub status.
///
/// Tracking note: see `.claude/Task/ROADMAP-improvements.md` Phase
/// 13.3 — flesh this out once we have a per-language body extractor
/// that returns nested-symbol ranges from the tree-sitter pass.
pub fn collapsed(body: &str) -> TruncatedBody {
    let original_lines = line_count(body);
    TruncatedBody {
        body: body.to_string(),
        mode: TruncationMode::Collapsed,
        original_lines,
        kept_lines: original_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_only_strips_after_opening_brace_rust() {
        let body = "fn foo(x: i32) -> i32 {\n    let y = x + 1;\n    y\n}";
        let out = signature_only(body);
        assert_eq!(out.body, "fn foo(x: i32) -> i32");
        assert_eq!(out.mode, TruncationMode::SignatureOnly);
        assert_eq!(out.original_lines, 4);
        assert_eq!(out.kept_lines, 1);
    }

    #[test]
    fn signature_only_handles_python_def_colon() {
        let body = "def foo(x):\n    return x + 1\n";
        let out = signature_only(body);
        assert_eq!(out.body, "def foo(x)");
    }

    #[test]
    fn signature_only_brace_on_own_line() {
        // Brace dropped to next line — keep the multi-line signature
        // intact up to (but excluding) the opening brace. Trailing
        // whitespace and the empty line that previously held the `{`
        // are trimmed so the output ends cleanly at `") -> i32"`.
        let body = "fn foo(\n    x: i32,\n) -> i32\n{\n    x\n}";
        let out = signature_only(body);
        assert!(out.body.starts_with("fn foo("));
        assert!(out.body.contains(") -> i32"));
        // 3 lines kept: `fn foo(`, `    x: i32,`, `) -> i32`. The `{`
        // line is trimmed off.
        assert_eq!(out.kept_lines, 3);
        // Sanity: the trailing `{` and body never make it into the
        // output.
        assert!(!out.body.contains('{'));
        assert!(!out.body.contains("    x\n}"));
    }

    #[test]
    fn signature_only_no_brace_returns_full_body() {
        // Constant with no marker — should return body unchanged.
        let body = "const FOO = 42";
        let out = signature_only(body);
        assert_eq!(out.body, "const FOO = 42");
    }

    #[test]
    fn signature_only_empty_body() {
        let out = signature_only("");
        assert_eq!(out.body, "");
        assert_eq!(out.original_lines, 0);
    }

    #[test]
    fn signature_only_does_not_split_rust_path_colon() {
        // `std::collections::HashMap` should NOT cause us to truncate
        // at the first `::`.
        let body = "fn foo() -> std::collections::HashMap<String, i32> {\n    HashMap::new()\n}";
        let out = signature_only(body);
        assert_eq!(
            out.body,
            "fn foo() -> std::collections::HashMap<String, i32>"
        );
    }

    #[test]
    fn head_n_truncates_to_first_n_lines() {
        let body = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = head_n(&body, 3);
        let lines: Vec<&str> = out.body.lines().collect();
        assert_eq!(lines.len(), 4); // 3 + trailer
        assert_eq!(lines[0], "line 1");
        assert_eq!(lines[1], "line 2");
        assert_eq!(lines[2], "line 3");
        assert_eq!(lines[3], "... (7 more lines)");
        assert_eq!(out.original_lines, 10);
        assert_eq!(out.kept_lines, 3);
    }

    #[test]
    fn head_n_does_not_truncate_short_body() {
        let body = "a\nb\nc";
        let out = head_n(body, 10);
        assert_eq!(out.body, "a\nb\nc");
        assert!(!out.body.contains("more lines"));
        assert_eq!(out.kept_lines, 3);
    }

    #[test]
    fn head_n_exact_match_no_trailer() {
        let body = "a\nb\nc";
        let out = head_n(body, 3);
        assert_eq!(out.body, "a\nb\nc");
        assert!(!out.body.contains("more lines"));
    }

    #[test]
    fn head_n_zero_is_empty() {
        let out = head_n("a\nb\nc", 0);
        assert_eq!(out.body, "");
        assert_eq!(out.kept_lines, 0);
    }

    #[test]
    fn no_body_keeps_signature_and_doc_comment() {
        let body = "/// Increments x by one.\nfn foo(x: i32) -> i32 {\n    x + 1\n}";
        let out = no_body(body);
        // Should include the doc comment AND the trimmed signature.
        assert!(out.body.contains("/// Increments x by one."));
        assert!(out.body.contains("fn foo(x: i32) -> i32"));
        assert!(!out.body.contains("x + 1"));
    }

    #[test]
    fn no_body_keeps_python_docstring() {
        let body = "def foo(x):\n    \"\"\"Increments x by one.\"\"\"\n    return x + 1\n";
        let out = no_body(body);
        assert!(out.body.contains("def foo(x)"));
        assert!(out.body.contains("Increments x by one"));
        assert!(!out.body.contains("return x + 1"));
    }

    #[test]
    fn no_body_keeps_multiline_python_docstring() {
        let body = "def foo():\n    \"\"\"\n    Long docstring.\n    Multiple lines.\n    \"\"\"\n    return 1\n";
        let out = no_body(body);
        assert!(out.body.contains("Long docstring"));
        assert!(out.body.contains("Multiple lines"));
        assert!(!out.body.contains("return 1"));
    }

    #[test]
    fn no_body_handles_attribute_above_signature() {
        let body = "#[inline]\nfn foo() -> i32 {\n    1\n}";
        let out = no_body(body);
        assert!(out.body.contains("#[inline]"));
        assert!(out.body.contains("fn foo() -> i32"));
        assert!(!out.body.contains("1\n}"));
    }

    #[test]
    fn collapsed_currently_returns_body_unchanged() {
        let body = "class Foo:\n    def bar(self):\n        return 1\n    def baz(self):\n        return 2\n";
        let out = collapsed(body);
        assert_eq!(out.body, body);
        assert_eq!(out.mode, TruncationMode::Collapsed);
        assert_eq!(out.original_lines, out.kept_lines);
    }

    #[test]
    fn truncation_mode_kebab_case_strings() {
        assert_eq!(TruncationMode::SignatureOnly.as_str(), "signature-only");
        assert_eq!(TruncationMode::Head.as_str(), "head");
        assert_eq!(TruncationMode::NoBody.as_str(), "no-body");
        assert_eq!(TruncationMode::Collapsed.as_str(), "collapsed");
    }

    #[test]
    fn truncation_mode_serializes_kebab() {
        // Verify the `serde_json` output of the mode is kebab-case so
        // the JSON envelope's `truncation.mode` field matches the CLI
        // flag.
        let s = serde_json::to_string(&TruncationMode::SignatureOnly).unwrap();
        assert_eq!(s, "\"signature-only\"");
        let s = serde_json::to_string(&TruncationMode::NoBody).unwrap();
        assert_eq!(s, "\"no-body\"");
    }
}
