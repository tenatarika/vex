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

    #[allow(dead_code)]
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
