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
    /// Detect language from file extension.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "kt" | "kts" => Some(Self::Kotlin),
            "ts" | "tsx" => Some(Self::TypeScript),
            "js" | "jsx" => Some(Self::TypeScript),
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
            Self::Rust | Self::TypeScript | Self::Python | Self::CSharp | Self::Cpp
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
