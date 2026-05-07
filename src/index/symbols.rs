use std::fmt;
use std::str::FromStr;

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

#[derive(Debug, thiserror::Error)]
#[error("unknown symbol kind: \"{0}\". Valid: function (fn), method, struct, class, interface, trait, enum, type_alias (type), impl, constant (const), property (prop), package (pkg)")]
pub struct ParseSymbolKindError(String);

impl FromStr for SymbolKind {
    type Err = ParseSymbolKindError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "function" | "fn" => Ok(Self::Function),
            "method" => Ok(Self::Method),
            "struct" => Ok(Self::Struct),
            "class" => Ok(Self::Class),
            "interface" => Ok(Self::Interface),
            "trait" => Ok(Self::Trait),
            "enum" => Ok(Self::Enum),
            "type_alias" | "type" => Ok(Self::TypeAlias),
            "impl" => Ok(Self::Impl),
            "constant" | "const" => Ok(Self::Constant),
            "property" | "prop" => Ok(Self::Property),
            "package" | "pkg" => Ok(Self::Package),
            _ => Err(ParseSymbolKindError(s.to_owned())),
        }
    }
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_canonical_names() {
        assert_eq!(
            "function".parse::<SymbolKind>().unwrap(),
            SymbolKind::Function
        );
        assert_eq!("method".parse::<SymbolKind>().unwrap(), SymbolKind::Method);
        assert_eq!("struct".parse::<SymbolKind>().unwrap(), SymbolKind::Struct);
        assert_eq!("class".parse::<SymbolKind>().unwrap(), SymbolKind::Class);
        assert_eq!(
            "interface".parse::<SymbolKind>().unwrap(),
            SymbolKind::Interface
        );
        assert_eq!("trait".parse::<SymbolKind>().unwrap(), SymbolKind::Trait);
        assert_eq!("enum".parse::<SymbolKind>().unwrap(), SymbolKind::Enum);
        assert_eq!(
            "type_alias".parse::<SymbolKind>().unwrap(),
            SymbolKind::TypeAlias
        );
        assert_eq!("impl".parse::<SymbolKind>().unwrap(), SymbolKind::Impl);
        assert_eq!(
            "constant".parse::<SymbolKind>().unwrap(),
            SymbolKind::Constant
        );
        assert_eq!(
            "property".parse::<SymbolKind>().unwrap(),
            SymbolKind::Property
        );
        assert_eq!(
            "package".parse::<SymbolKind>().unwrap(),
            SymbolKind::Package
        );
    }

    #[test]
    fn from_str_aliases() {
        assert_eq!("fn".parse::<SymbolKind>().unwrap(), SymbolKind::Function);
        assert_eq!("type".parse::<SymbolKind>().unwrap(), SymbolKind::TypeAlias);
        assert_eq!("const".parse::<SymbolKind>().unwrap(), SymbolKind::Constant);
        assert_eq!("prop".parse::<SymbolKind>().unwrap(), SymbolKind::Property);
        assert_eq!("pkg".parse::<SymbolKind>().unwrap(), SymbolKind::Package);
    }

    #[test]
    fn from_str_unknown_returns_error() {
        let err = "foobar".parse::<SymbolKind>().unwrap_err();
        assert!(err.to_string().contains("foobar"));
        assert!(err.to_string().contains("unknown symbol kind"));
    }

    #[test]
    fn as_str_roundtrip() {
        let all = [
            SymbolKind::Function,
            SymbolKind::Method,
            SymbolKind::Struct,
            SymbolKind::Class,
            SymbolKind::Interface,
            SymbolKind::Trait,
            SymbolKind::Enum,
            SymbolKind::TypeAlias,
            SymbolKind::Impl,
            SymbolKind::Constant,
            SymbolKind::Property,
            SymbolKind::Package,
        ];
        for kind in all {
            let parsed: SymbolKind = kind.as_str().parse().unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn display_matches_as_str() {
        assert_eq!(SymbolKind::Function.to_string(), "function");
        assert_eq!(SymbolKind::TypeAlias.to_string(), "type_alias");
    }
}

/// Result of parsing a single file.
#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub path: String,
    pub symbols: Vec<ParsedSymbol>,
    #[allow(dead_code)] // TODO: wire refs into search for usages/callers
    pub refs: Vec<ParsedRef>,
}
