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
            _ => None,
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
        }
    }
}
