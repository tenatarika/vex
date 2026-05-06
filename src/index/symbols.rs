use serde::{Deserialize, Serialize};

/// Kind of code symbol extracted from AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Class,
    Interface,
    Trait,
    Enum,
    TypeAlias,
    Impl,
    Constant,
    Property,
    Package,
}

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Trait => "trait",
            Self::Enum => "enum",
            Self::TypeAlias => "type_alias",
            Self::Impl => "impl",
            Self::Constant => "constant",
            Self::Property => "property",
            Self::Package => "package",
        }
    }
}

/// A symbol extracted from parsing a source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub line: usize,
    pub signature: Option<String>,
}

/// A reference (usage) of a symbol found in source code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedRef {
    pub name: String,
    pub line: usize,
    pub context: Option<String>,
}

/// Result of parsing a single file.
#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub path: String,
    pub symbols: Vec<ParsedSymbol>,
    #[allow(dead_code)] // TODO: wire refs into search for usages/callers
    pub refs: Vec<ParsedRef>,
}
