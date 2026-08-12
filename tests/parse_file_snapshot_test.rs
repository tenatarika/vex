//! Byte-identity oracle for the shared-tree refactor
//! (`.claude/Task/PERF-parse-once-shared-tree.md`, commit 0).
//!
//! `parse_file` currently runs a full tree-sitter parse of the same content
//! once per extractor. The refactor parses once and threads a single `&Tree`
//! through all of them. Every extractor keeps a language/allowlist gate that
//! fires *before* its parse today — most notably
//! `!Language::has_ast_ref_filter()`, which routes 11 languages through the
//! line-based `extract_references` scanner instead of the AST walker. A core
//! that "walks the tree it was handed" would silently re-route those languages
//! and change the refs FST.
//!
//! These snapshots are the regression oracle for exactly that class of bug.
//! The per-extractor equivalence tests added alongside each migration commit
//! compare a `*_with_tree` core against its own thin wrapper, so they go
//! tautological the moment they land — they catch "forgot to wire it up",
//! not "gate dropped inside the core". This file is what catches the latter,
//! which is why it is captured on `main` BEFORE any migration commit.
//!
//! Regenerate deliberately (and review the diff!) with:
//!
//! ```text
//! VEX_UPDATE_SNAPSHOTS=1 cargo test --test parse_file_snapshot_test
//! ```

use std::path::{Path, PathBuf};

use serde_json::json;
use vex::index::symbols::ParsedFile;
use vex::parse::language::Language;
use vex::parse::parse_file;

/// Fixture extension per language. Exhaustive over `Language::ALL` by
/// construction — `fixture_ext` panics on an unmapped language, so adding a
/// 20th language fails this test until a fixture exists for it.
fn fixture_ext(lang: Language) -> &'static str {
    match lang {
        Language::Rust => "rs",
        Language::Kotlin => "kt",
        Language::TypeScript => "ts",
        Language::Python => "py",
        Language::Go => "go",
        Language::Java => "java",
        Language::CSharp => "cs",
        Language::Ruby => "rb",
        Language::Swift => "swift",
        Language::Sql => "sql",
        Language::Markdown => "md",
        Language::Cpp => "cpp",
        Language::Php => "php",
        Language::Bash => "sh",
        Language::Lua => "lua",
        Language::Css => "css",
        Language::Html => "html",
        Language::Yaml => "yaml",
        Language::Toml => "toml",
    }
}

fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parse_file_snapshots")
}

/// Serialize a `ParsedFile` for snapshotting.
///
/// `ParsedSymbol::doc` is `#[serde(skip)]` (it only ever feeds embedding
/// context, never the index), so the derive alone would leave a blind spot on
/// `extract_doc_above` — which lives inside the site-1 extractor this refactor
/// touches. Capture it explicitly alongside the derived shape.
/// `trigram_bloom` is genuinely `None` at this layer (built by `parse_files`
/// from raw bytes, not here), so nothing is lost there.
fn snapshot_value(parsed: &ParsedFile) -> serde_json::Value {
    let docs: Vec<_> = parsed
        .symbols
        .iter()
        .map(|s| json!({ "name": s.name, "line": s.line, "doc": s.doc }))
        .collect();
    json!({ "parsed": parsed, "docs": docs })
}

/// Compare against the committed snapshot, or rewrite it under
/// `VEX_UPDATE_SNAPSHOTS=1`.
fn assert_snapshot(name: &str, value: &serde_json::Value) {
    let path = snapshot_dir().join(format!("{name}.json"));
    let actual = serde_json::to_string_pretty(value).expect("serialize snapshot");

    if std::env::var_os("VEX_UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(snapshot_dir()).expect("create snapshot dir");
        std::fs::write(&path, format!("{actual}\n")).expect("write snapshot");
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing snapshot {}: {e}\n\
             capture it with VEX_UPDATE_SNAPSHOTS=1 cargo test --test parse_file_snapshot_test",
            path.display()
        )
    });

    // Structural comparison, not a hash — a mismatch must be readable. Compare
    // as `Value` so trailing-newline / key-order churn never fails the test,
    // then show the first differing line for a usable diff.
    let expected_value: serde_json::Value =
        serde_json::from_str(&expected).expect("committed snapshot is valid JSON");
    if &expected_value == value {
        return;
    }

    let expected_pretty =
        serde_json::to_string_pretty(&expected_value).expect("re-render expected");
    let first_diff = expected_pretty
        .lines()
        .zip(actual.lines())
        .enumerate()
        .find(|(_, (e, a))| e != a)
        .map(|(i, (e, a))| format!("line {}:\n  expected: {e}\n  actual:   {a}", i + 1))
        .unwrap_or_else(|| "(one side has extra trailing lines)".to_string());

    panic!(
        "parse_file snapshot mismatch for `{name}` ({})\n\n{first_diff}\n\n\
         This is the shared-tree byte-identity oracle. A mismatch means an \
         extractor's pre-parse gate changed behaviour — do NOT regenerate the \
         snapshot to make it pass unless the change is understood and intended.",
        path.display()
    );
}

/// One snapshot per supported language, over the shared `tests/fixtures`
/// corpus. Covers all 19 languages so the gated sites are all represented:
/// the 8 binder/ref-filter languages (6 parses each), C++ (the only 7-parse
/// language — it also runs `extract_cpp_includes`), the 3
/// skeleton+hierarchy languages (Swift/PHP/Ruby, 3 parses) and the 8 that have
/// a skeleton allowlist but no inheritance query (SQL/Markdown/CSS/HTML/
/// Bash/Lua/YAML/TOML, 2 parses). The exact per-language counts are pinned by
/// `parse_count_tests` in `src/parse/mod.rs`.
#[test]
fn parse_file_snapshots_cover_every_language() {
    for &lang in Language::ALL {
        let ext = fixture_ext(lang);
        let rel = format!("tests/fixtures/sample.{ext}");
        let abs = Path::new(env!("CARGO_MANIFEST_DIR")).join(&rel);
        let content = std::fs::read_to_string(&abs)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", abs.display()));

        let parsed = parse_file(&rel, &content, lang)
            .unwrap_or_else(|e| panic!("parse_file failed for {lang:?} fixture {rel}: {e}"));

        let name = format!("{lang:?}").to_lowercase();
        assert_snapshot(&name, &snapshot_value(&parsed));
    }
}

/// Degenerate inputs, snapshotted separately from the language corpus: these
/// exercise the empty / BOM / truncated paths where an extractor's early
/// return is easy to get wrong when its parse moves out.
#[test]
fn parse_file_snapshots_cover_degenerate_inputs() {
    let cases: &[(&str, &str, &str, Language)] = &[
        ("edge_empty_rust", "empty.rs", "", Language::Rust),
        (
            "edge_bom_python",
            "bom.py",
            "\u{FEFF}def hello(): pass\n",
            Language::Python,
        ),
        (
            "edge_truncated_rust",
            "truncated.rs",
            "pub fn charge(amount: u64) -> Result<(), Error> {\n    gateway.charge(",
            Language::Rust,
        ),
        (
            "edge_crlf_rust",
            "crlf.rs",
            "fn first() {}\r\nfn mixed_fn() {}\n",
            Language::Rust,
        ),
    ];

    for (name, path, content, lang) in cases {
        let parsed = parse_file(path, content, *lang)
            .unwrap_or_else(|e| panic!("parse_file failed for degenerate case {name}: {e}"));
        assert_snapshot(name, &snapshot_value(&parsed));
    }
}

/// The `fuzz_kotlin_binder` artifact that drove tree-sitter-kotlin-ng's GLR
/// error recovery to 334 s / multi-GB. `parse_text`'s progress-callback budget
/// must keep rejecting it — and after the refactor there is exactly ONE parse
/// to reject it at, so pin the `Err` outcome rather than a snapshot.
#[test]
fn pathological_input_still_fails_the_parse_budget() {
    const PATHOLOGICAL: &[u8] = include_bytes!("../fuzz/findings/kotlin-grammar-oom.bin");
    let src = std::str::from_utf8(PATHOLOGICAL).expect("fuzz finding is valid UTF-8");

    let result = parse_file("kotlin-grammar-oom.kt", src, Language::Kotlin);

    assert!(
        result.is_err(),
        "pathological input must be rejected by the parse budget, not indexed"
    );
}
