/// Supported programming languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Kotlin,
    TypeScript,
    Python,
    Go,
    Java,
    CSharp,
    Ruby,
    Swift,
    Sql,
    Markdown,
    Cpp,
    Php,
    Bash,
    Lua,
    Css,
    Html,
    Yaml,
    Toml,
}

impl Language {
    /// Every supported [`Language`] variant in declaration order.
    ///
    /// The order MUST match the enum variant declaration so callers that
    /// rely on positional indexing (none today; tracked here so a future
    /// reordering can't silently break it) stay correct. Adding a new
    /// language requires appending it to both the enum AND this slice;
    /// removing one leaves a gap in `lang_id` but is removed from this
    /// slice. Compile-time `assert_eq!(Language::ALL.len(), …)` in a
    /// test pins the count so a missing entry surfaces as a test
    /// failure rather than a silent gap in language-iterating consumers.
    ///
    /// ```
    /// use vex::parse::language::Language;
    ///
    /// // Iterate every supported language — used by callgraph's
    /// // `COMPILED_QUERIES` and pattern-skeleton fingerprinting so a
    /// // new language additions don't need per-consumer registration.
    /// for &lang in Language::ALL {
    ///     assert!(lang.lang_id() >= 1 && lang.lang_id() <= 19);
    /// }
    /// ```
    pub const ALL: &'static [Language] = &[
        Self::Rust,
        Self::Kotlin,
        Self::TypeScript,
        Self::Python,
        Self::Go,
        Self::Java,
        Self::CSharp,
        Self::Ruby,
        Self::Swift,
        Self::Sql,
        Self::Markdown,
        Self::Cpp,
        Self::Php,
        Self::Bash,
        Self::Lua,
        Self::Css,
        Self::Html,
        Self::Yaml,
        Self::Toml,
    ];

    /// Detect language from file extension.
    ///
    /// The extension argument is the bare extension without the leading
    /// dot. Returns `None` for unsupported or unknown extensions.
    ///
    /// ```
    /// use vex::parse::language::Language;
    ///
    /// assert_eq!(Language::from_extension("rs"), Some(Language::Rust));
    /// assert_eq!(Language::from_extension("py"), Some(Language::Python));
    /// // JS variants (incl. ESM .mjs / CommonJS .cjs) share the TypeScript grammar.
    /// assert_eq!(Language::from_extension("jsx"), Some(Language::TypeScript));
    /// assert_eq!(Language::from_extension("mjs"), Some(Language::TypeScript));
    /// assert_eq!(Language::from_extension("zig"), None);
    /// ```
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "kt" | "kts" => Some(Self::Kotlin),
            "ts" | "tsx" => Some(Self::TypeScript),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::TypeScript),
            "py" => Some(Self::Python),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "cs" => Some(Self::CSharp),
            "rb" => Some(Self::Ruby),
            "swift" => Some(Self::Swift),
            "sql" => Some(Self::Sql),
            "md" | "markdown" => Some(Self::Markdown),
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "h" => Some(Self::Cpp),
            "php" | "phtml" => Some(Self::Php),
            "sh" | "bash" => Some(Self::Bash),
            "lua" => Some(Self::Lua),
            "css" => Some(Self::Css),
            "html" | "htm" => Some(Self::Html),
            "yaml" | "yml" => Some(Self::Yaml),
            "toml" => Some(Self::Toml),
            _ => None,
        }
    }

    /// Get the tree-sitter Language for this language variant.
    pub fn ts_language(&self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Swift => tree_sitter_swift::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            // TSX grammar is a superset of TypeScript — handles .ts, .tsx, .js, .jsx
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Sql => tree_sitter_sequel::LANGUAGE.into(),
            Self::Markdown => tree_sitter_md::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            // PHP exposes two grammars: LANGUAGE_PHP (with <?php tags) and
            // LANGUAGE_PHP_ONLY (raw PHP without tags). We accept either via
            // the .php / .phtml extensions, so the tag-aware grammar wins.
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Lua => tree_sitter_lua::LANGUAGE.into(),
            Self::Css => tree_sitter_css::LANGUAGE.into(),
            Self::Html => tree_sitter_html::LANGUAGE.into(),
            Self::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Self::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
        }
    }

    /// Languages that participate in AST-aware reference extraction
    /// (11.1.1). For these languages, `parse_file` walks the tree-sitter
    /// AST to collect identifier refs, skipping subtrees rooted at
    /// comment / string nodes so that names mentioned only in prose or
    /// string literals don't pollute `vex usages`.
    ///
    /// All other languages keep using the line-based scanner from
    /// [`crate::parse::extractor::extract_references`], which still has
    /// a higher false-positive rate but covers grammars without a
    /// scope-binder yet.
    pub fn has_ast_ref_filter(&self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::TypeScript
                | Self::Python
                | Self::CSharp
                | Self::Cpp
                | Self::Go
                | Self::Java
                | Self::Kotlin
        )
    }

    /// Stable numeric identifier for persisting per-language grammar
    /// fingerprints in [`crate::store::format::PatternSkeletonHeader`].
    ///
    /// IDs are **explicitly assigned** and must never change — adding a
    /// new language variant gets the next available integer, and removing
    /// one leaves a gap (the slot stays reserved). Slot 0 is reserved for
    /// "not fingerprinted". Maximum capacity in the header: 32 slots (IDs
    /// 1..=31).
    pub fn lang_id(self) -> u8 {
        match self {
            Self::Rust => 1,
            Self::Kotlin => 2,
            Self::TypeScript => 3,
            Self::Python => 4,
            Self::Go => 5,
            Self::Java => 6,
            Self::CSharp => 7,
            Self::Ruby => 8,
            Self::Swift => 9,
            Self::Sql => 10,
            Self::Markdown => 11,
            Self::Cpp => 12,
            Self::Php => 13,
            Self::Bash => 14,
            Self::Lua => 15,
            Self::Css => 16,
            Self::Html => 17,
            Self::Yaml => 18,
            Self::Toml => 19,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Kotlin => "kotlin",
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
            Self::CSharp => "csharp",
            Self::Ruby => "ruby",
            Self::Swift => "swift",
            Self::Sql => "sql",
            Self::Markdown => "markdown",
            Self::Cpp => "cpp",
            Self::Php => "php",
            Self::Bash => "bash",
            Self::Lua => "lua",
            Self::Css => "css",
            Self::Html => "html",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the `Language::ALL` cardinality so a future variant addition
    /// that forgets to update the slice surfaces here instead of silently
    /// shrinking language-iterating consumers (callgraph `COMPILED_QUERIES`,
    /// pattern-skeleton grammar fingerprints, …).
    #[test]
    fn all_slice_covers_every_variant() {
        // Every `lang_id` is in `1..=19` (slot 0 reserved for "not
        // fingerprinted"), so the slice must have the same count.
        assert_eq!(Language::ALL.len(), 19);
        // `lang_id` is also our canonical ordinal; the slice must list
        // each one exactly once.
        let mut ids: Vec<u8> = Language::ALL.iter().map(|l| l.lang_id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Language::ALL.len(), "duplicate variant in ALL");
        assert_eq!(*ids.first().unwrap(), 1, "lang_id starts at 1");
        assert_eq!(*ids.last().unwrap(), 19, "lang_id ends at 19");
    }

    /// All JavaScript flavours — classic, JSX, ESM (`.mjs`), CommonJS
    /// (`.cjs`) — route through the TypeScript grammar.
    #[test]
    fn js_variants_map_to_typescript() {
        for ext in ["js", "jsx", "mjs", "cjs", "ts", "tsx"] {
            assert_eq!(
                Language::from_extension(ext),
                Some(Language::TypeScript),
                "extension {ext:?} should map to TypeScript"
            );
        }
    }
}
