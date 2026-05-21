//! Rust scope binder — stub for 11.1.2a.
//!
//! Returns an empty `Vec<BoundRef>` so the rest of the pipeline can
//! plumb `ParsedFile::bound_refs` through without depending on the
//! actual scope walker. 11.1.2b will replace this stub with a real
//! tree-walking binder that handles `let` shadowing, `fn` params,
//! `match` arm bindings, and mod-level types.

use anyhow::Result;

use super::{BoundRef, ScopeBinder};
use crate::index::symbols::ParsedSymbol;

pub struct RustBinder;

impl ScopeBinder for RustBinder {
    fn bind(&self, _content: &str, _file_symbols: &[ParsedSymbol]) -> Result<Vec<BoundRef>> {
        Ok(Vec::new())
    }
}
