//! Phase 11.4 — pattern skeleton extraction.
//!
//! At index time we walk each source file's tree-sitter AST and emit a
//! [`Skeleton`] per *pattern-targetable* node (function, struct, class,
//! method, …). The skeleton carries the node's structural shape
//! (`kind`, optional `parent_kind`), its leaf identifier when one is
//! recoverable, and span info — enough for the Phase 11.4 prefilter
//! (Inc 5) to narrow `vex pattern` candidate files without re-parsing.
//!
//! Inc 2 scope: **pure function only**. No storage wiring, no FST, no
//! pipeline integration — just the extractor + unit tests. Inc 3 will
//! pack `Skeleton`s into a side-table behind a `PatternSkeletonHeader`
//! that older readers skip.
//!
//! Per-language coverage (T1 lands now; T2/T3 in follow-up trains —
//! see `.claude/Task/PHASE11.4-first-class-pattern.md`):
//!
//! | Tier | Languages                                                              | Allowlist     |
//! |------|------------------------------------------------------------------------|---------------|
//! | T1   | Rust, TypeScript, Python                                               | populated     |
//! | T2a  | Go, C++, C#, SQL, Markdown, Java, CSS, HTML, Kotlin, Swift, PHP, Ruby  | populated     |
//! | T2   | (none — all promoted)                                                  | n/a           |
//! | T3   | YAML, TOML, Bash, Lua                                                  | empty (final) |
//!
//! JavaScript shares the TypeScript grammar (`Language::TypeScript`)
//! via `"js" | "jsx" → TypeScript` in the extension map, so the T1
//! TypeScript allowlist already covers it — no separate JS row.
//!
//! An empty allowlist short-circuits to `Vec::new()`, so unrolled-T2
//! / T3 files produce no skeletons and `vex pattern --lang <x>` falls
//! back to live-scan exactly as today.
//!
//! Go-specific note: struct / interface bodies in Go live two AST
//! levels below `type_spec` (`type_spec > struct_type >
//! field_declaration_list`), so [`has_body_block`] returns `false` for
//! `type_spec` even when there's a structural body. Patterns using
//! `$$$BODY` on Go struct/interface declarations therefore fall back
//! to live-scan — correctness preserved, perf-only impact.

use tree_sitter::Node;

use crate::parse::language::Language;

mod ident;
mod kinds;
// Internal `use` aliases for call-site brevity in `walk`/`extract_skeletons`.
// NOT re-exports: both fns are `pub(super)` and stay private to this module.
use ident::extract_ident;
use kinds::pattern_targetable_kinds;

/// One structural fingerprint per pattern-targetable AST node.
///
/// Stored in-memory only at this stage. The compact on-disk form lands
/// in Inc 3 (`SkeletonRecord` with string-pool indices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skeleton {
    /// 0-based row of the node's first token.
    pub start_row: u32,
    /// 0-based row of the node's last token. Useful for matching
    /// multi-line `$$$BODY` metavars later (Inc 6).
    pub end_row: u32,
    /// Tree-sitter node kind (e.g. `function_item`, `class_declaration`).
    /// `&'static str` because tree-sitter interns kind names.
    pub kind: &'static str,
    /// Parent node's kind when the parent is *not* the file root
    /// (`source_file` / `program` / `module` / `translation_unit`).
    /// Lets the prefilter distinguish e.g. a bare `function_definition`
    /// from one inside a `template_declaration` (C++) or
    /// `decorated_definition` (Python).
    pub parent_kind: Option<&'static str>,
    /// Leaf identifier when the grammar exposes one for this node
    /// (function name, struct name, impl type). `None` for anonymous
    /// nodes like `lambda`, `arrow_function`, `decorated_definition`.
    pub ident: Option<String>,
    /// Whether the node carries a block-shaped body child (`block`,
    /// `statement_block`, `declaration_list`, …). Inc 6 uses this to
    /// decide whether `$$$BODY` is applicable for the skeleton.
    pub has_block: bool,
}

// ── serde impls for Skeleton ─────────────────────────────────────────────────
//
// `kind` and `parent_kind` are `&'static str` (tree-sitter interned names).
// They serialize fine as strings.  Deserialization reads into `String` and
// leaks each unique string to recover a `&'static str`.  The leak is
// intentional and bounded: the set of tree-sitter node kinds is small (~50–200
// per grammar) and these strings are deduplicated by the interning below.
// There is no way to deserialize into a borrowed reference without leaking or
// carrying a lifetime through every container that holds a `Skeleton`.

impl serde::Serialize for Skeleton {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut s = serializer.serialize_struct("Skeleton", 6)?;
        s.serialize_field("start_row", &self.start_row)?;
        s.serialize_field("end_row", &self.end_row)?;
        s.serialize_field("kind", self.kind)?;
        s.serialize_field("parent_kind", &self.parent_kind)?;
        s.serialize_field("ident", &self.ident)?;
        s.serialize_field("has_block", &self.has_block)?;
        s.end()
    }
}

impl<'de> serde::Deserialize<'de> for Skeleton {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, SeqAccess, Visitor};
        use std::fmt;

        #[derive(serde::Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            StartRow,
            EndRow,
            Kind,
            ParentKind,
            Ident,
            HasBlock,
        }

        struct SkeletonVisitor;

        impl<'de> Visitor<'de> for SkeletonVisitor {
            type Value = Skeleton;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("struct Skeleton")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Skeleton, A::Error> {
                use serde::de::Error as _;
                let start_row = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::missing_field("start_row"))?;
                let end_row = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::missing_field("end_row"))?;
                let kind_s: String = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::missing_field("kind"))?;
                let parent_kind_s: Option<String> = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::missing_field("parent_kind"))?;
                let ident = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::missing_field("ident"))?;
                let has_block = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::missing_field("has_block"))?;
                Ok(Skeleton {
                    start_row,
                    end_row,
                    kind: intern_static(kind_s),
                    parent_kind: parent_kind_s.map(intern_static),
                    ident,
                    has_block,
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Skeleton, A::Error> {
                use serde::de::Error as _;
                let mut start_row = None;
                let mut end_row = None;
                let mut kind_s: Option<String> = None;
                let mut parent_kind_s: Option<Option<String>> = None;
                let mut ident = None;
                let mut has_block = None;

                while let Some(key) = map.next_key()? {
                    match key {
                        Field::StartRow => {
                            start_row = Some(map.next_value()?);
                        }
                        Field::EndRow => {
                            end_row = Some(map.next_value()?);
                        }
                        Field::Kind => {
                            kind_s = Some(map.next_value()?);
                        }
                        Field::ParentKind => {
                            parent_kind_s = Some(map.next_value()?);
                        }
                        Field::Ident => {
                            ident = Some(map.next_value()?);
                        }
                        Field::HasBlock => {
                            has_block = Some(map.next_value()?);
                        }
                    }
                }

                Ok(Skeleton {
                    start_row: start_row.ok_or_else(|| A::Error::missing_field("start_row"))?,
                    end_row: end_row.ok_or_else(|| A::Error::missing_field("end_row"))?,
                    kind: intern_static(kind_s.ok_or_else(|| A::Error::missing_field("kind"))?),
                    parent_kind: parent_kind_s
                        .ok_or_else(|| A::Error::missing_field("parent_kind"))?
                        .map(intern_static),
                    ident: ident.ok_or_else(|| A::Error::missing_field("ident"))?,
                    has_block: has_block.ok_or_else(|| A::Error::missing_field("has_block"))?,
                })
            }
        }

        const FIELDS: &[&str] = &[
            "start_row",
            "end_row",
            "kind",
            "parent_kind",
            "ident",
            "has_block",
        ];
        deserializer.deserialize_struct("Skeleton", FIELDS, SkeletonVisitor)
    }
}

/// Convert an owned `String` into a `&'static str` by leaking it into a
/// process-global pool, deduplicating against previously seen values.
///
/// The set of distinct tree-sitter node kinds across the full language matrix
/// is small (≤ ~1 000 strings), so the steady-state pool is bounded. The
/// deduplication is what makes that bound real: without it, every deserialized
/// `Skeleton` on every cache hit would leak a fresh allocation, which in a
/// long-running process (LSP / MCP server / watch mode) would accumulate.
fn intern_static(s: String) -> &'static str {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static POOL: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let pool = POOL.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = pool.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(&existing) = guard.get(s.as_str()) {
        return existing;
    }
    let leaked: &'static str = Box::leak(s.into_boxed_str());
    guard.insert(leaked);
    leaked
}

/// Walk `source` under `lang`'s grammar and emit one [`Skeleton`] per
/// allowlisted node. Returns an empty `Vec` when the language has no
/// allowlist (T2/T3 today) or when tree-sitter fails to parse.
pub fn extract_skeletons(source: &str, lang: Language) -> Vec<Skeleton> {
    let allowlist = pattern_targetable_kinds(lang);
    if allowlist.is_empty() {
        return Vec::new();
    }
    // v1.12.0 P3 — pooled per-thread parser. with_parser may return Err
    // on grammar-set-language failure for an unsupported language, but the
    // allowlist short-circuit above already filters those; collapse any
    // residual error path to an empty result (callers tolerate empty).
    let Ok(tree) = crate::parse::parser_pool::with_parser(lang, |parser| {
        parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("tree-sitter parse failed in skeleton extractor"))
    }) else {
        return Vec::new();
    };
    let mut skeletons = Vec::new();
    walk(tree.root_node(), source, lang, allowlist, &mut skeletons);
    skeletons
}

fn walk(
    node: Node<'_>,
    source: &str,
    lang: Language,
    allowlist: &[&'static str],
    out: &mut Vec<Skeleton>,
) {
    let kind = node.kind();
    // Gate emission on named-only nodes. Anonymous keyword tokens
    // share their kind STRING with the named grammar rule in some
    // languages — Ruby is the first to actually collide: `class`,
    // `module`, and `alias` are both named rules AND anonymous
    // keyword tokens (`class C`, the `class` keyword inside the
    // `class` rule has `kind() == "class"` with `is_named() == false`).
    // Without this gate, every Ruby `class`/`module`/`alias` site
    // would emit TWO skeletons — one for the rule, one for the
    // keyword token. Every allowlisted kind across T1/T2a is a
    // named grammar rule, so this gate never drops a legitimate
    // emission.
    if allowlist.contains(&kind) && node.is_named() {
        let parent_kind = node
            .parent()
            .map(|p| p.kind())
            .filter(|k| !is_root_kind(k, lang));
        out.push(Skeleton {
            start_row: node.start_position().row as u32,
            end_row: node.end_position().row as u32,
            kind,
            parent_kind,
            ident: extract_ident(node, source, lang, kind),
            has_block: has_body_block(node),
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, source, lang, allowlist, out);
    }
}

/// Per-language root-node kind suppression. The shared base set
/// (`source_file` / `program` / `module` / `translation_unit` /
/// `compilation_unit`) covers Rust, Go, TS, Python, C++, C# — none of
/// those grammars use any of those names elsewhere. `stylesheet` is
/// CSS's root; the kind name is unused by every other grammar in the
/// matrix so it stays in the global base set.
///
/// `document` is gated to Markdown AND HTML because both grammars use
/// it as the file root, but YAML uses the same kind name for a
/// *non-root* subtree under `stream`. A global suppression would
/// silently break the parent-kind contract the moment YAML moves out
/// of the empty T3 allowlist.
fn is_root_kind(kind: &str, lang: Language) -> bool {
    if matches!(
        kind,
        "source_file"
            | "program"
            | "module"
            | "translation_unit"
            | "compilation_unit"
            | "stylesheet"
    ) {
        return true;
    }
    matches!(
        (kind, lang),
        ("document", Language::Markdown) | ("document", Language::Html)
    )
}

/// Block-shaped body markers, language-agnostic. Tree-sitter uses these
/// kinds for the statement-list child of declarations across grammars.
///
/// Maintenance note: the `"block"` arm is a body marker for Rust/Python
/// functions AND an allowlisted Ruby kind in [`kinds`]. Both occurrences
/// must stay — Rust/Python need the body detection, Ruby needs the
/// skeleton emission. Removing it from either side silently regresses
/// one language.
fn has_body_block(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            // Universal markers across T1 grammars, plus per-
            // language body kinds accumulated through the T2a
            // train. All T2 languages have been promoted; further
            // additions here only happen if a T3 language flips
            // (intentionally never, today). The `function_body`
            // arm below is shared across SQL `CREATE FUNCTION`,
            // Kotlin (`function_declaration` + `anonymous_function`),
            // and Swift (`function_declaration` + `init_declaration`
            // + `deinit_declaration`) — same kind name, different
            // grammars.
            "block"
                | "statement_block"
                | "declaration_list"
                | "field_declaration_list"
                | "enum_body"
                | "enum_variant_list"
                | "enumerator_list"           // C++ enum body
                | "class_body"
                | "interface_body"
                | "compound_statement"           // C++ fn / lambda body
                | "enum_member_declaration_list" // C# enum body
                | "accessor_list" // C# property body
                | "column_definitions" // SQL CREATE TABLE body
                | "function_body" // SQL CREATE FUNCTION body
                | "code_fence_content" // Markdown fenced block content
                | "annotation_type_body" // Java @interface body
                | "constructor_body" // Java constructor body
                | "keyframe_block_list" // CSS @keyframes body
                | "enum_class_body" // Kotlin/Swift `enum class` body
                | "protocol_body" // Swift `protocol` body
                | "enum_declaration_list" // PHP `enum` body
                | "body_statement" // Ruby class/module/method/do_block body
                | "block_body" // Ruby brace-block + lambda body
                | "do_block" // Ruby `->(x) do ... end` lambda body kind
                             // (do_block is itself an allowlisted kind;
                             // listing it here lets the OUTER `lambda`
                             // skeleton report has_block=true when its
                             // body is a do/end form rather than `{}`)
                             // Note: the string "block" above is shared with Ruby —
                             // it's a Rust/Python function-body marker AND a Ruby
                             // brace-block allowlist kind. `pattern_targetable_kinds`
                             // is lang-gated, so the allowlist path can't collide;
                             // here in `has_body_block` we only care that the kind
                             // counts as a block-shaped body in any language.
        )
    });
    found
}

#[cfg(test)]
mod tests;
