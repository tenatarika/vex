use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Kind of code symbol extracted from AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum SymbolKind {
    Function = 0,
    Method = 1,
    Struct = 2,
    Class = 3,
    Interface = 4,
    Trait = 5,
    Enum = 6,
    TypeAlias = 7,
    Impl = 8,
    Constant = 9,
    Property = 10,
    Package = 11,
    Heading = 12,
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
            Self::Heading => "heading",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown symbol kind: \"{0}\". Valid: function (fn), method, struct, class, interface, trait, enum, type_alias (type), impl, constant (const), property (prop), package (pkg), heading (h)")]
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
            "heading" | "h" => Ok(Self::Heading),
            _ => Err(ParseSymbolKindError(s.to_owned())),
        }
    }
}

impl TryFrom<u8> for SymbolKind {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Function),
            1 => Ok(Self::Method),
            2 => Ok(Self::Struct),
            3 => Ok(Self::Class),
            4 => Ok(Self::Interface),
            5 => Ok(Self::Trait),
            6 => Ok(Self::Enum),
            7 => Ok(Self::TypeAlias),
            8 => Ok(Self::Impl),
            9 => Ok(Self::Constant),
            10 => Ok(Self::Property),
            11 => Ok(Self::Package),
            12 => Ok(Self::Heading),
            other => Err(other),
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
    /// Docstring/comment extracted from lines above the symbol (used for embedding context only).
    #[serde(skip)]
    pub doc: Option<String>,
    /// Meaningful identifiers extracted from the symbol body (used for embedding context only).
    #[serde(skip)]
    pub body_tokens: Option<String>,
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
        assert_eq!("h".parse::<SymbolKind>().unwrap(), SymbolKind::Heading);
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
            SymbolKind::Heading,
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

    #[test]
    fn try_from_u8_roundtrip() {
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
            SymbolKind::Heading,
        ];
        for kind in all {
            let val = kind as u8;
            let back = SymbolKind::try_from(val).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn try_from_u8_invalid_returns_err() {
        assert_eq!(SymbolKind::try_from(99), Err(99));
        assert_eq!(SymbolKind::try_from(13), Err(13));
    }
}

/// Raw call edge extracted from source, prior to caller-name resolution.
///
/// `caller_fn_name` + `caller_fn_line` identify the enclosing function
/// definition — both fields are needed because a file may contain two
/// functions with the same name (e.g. overloaded methods, duplicate impl
/// blocks). The pair `(path, name, line)` uniquely identifies a symbol.
/// Resolution to `caller_sym_idx` happens during writer assembly.
#[derive(Debug, Clone)]
pub struct RawCallEdge {
    pub caller_fn_name: String,
    pub caller_fn_line: usize,
    pub callee_name: String,
    pub line: usize,
}

/// Result of parsing a single file.
#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub path: String,
    pub symbols: Vec<ParsedSymbol>,
    #[allow(dead_code)] // TODO: wire refs into search for usages/callers
    pub refs: Vec<ParsedRef>,
    /// Caller → callee edges extracted at parse time. Empty for languages
    /// without a call-graph tree-sitter query, and for incremental
    /// reconstruction paths that skip the call-edge extraction step.
    pub call_edges: Vec<RawCallEdge>,
    /// Scope-resolved references produced by the per-language binder
    /// (11.1.2). Empty for languages without a binder and for the
    /// incremental reconstruction paths in `pipeline.rs` that never
    /// re-parse from source. Becomes the input to the persistent
    /// `reference_edges` section in 11.1.3.
    #[allow(dead_code)]
    pub bound_refs: Vec<crate::parse::scope::BoundRef>,
    /// Pattern skeletons emitted by the Phase 11.4 extractor. Empty for
    /// T2/T3 languages (no allowlist yet) and — like `bound_refs` and
    /// `call_edges` — empty for files reconstructed from the existing
    /// index during `vex update`. Populated only for files actually
    /// re-parsed from source. Inc 5 reads from the persistent section.
    pub skeletons: Vec<crate::pattern::skeleton::Skeleton>,
}
