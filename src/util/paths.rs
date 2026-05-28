//! Path-normalization helpers shared across the index pipeline.
//!
//! The single rule: stored relative paths use **forward slashes**, regardless
//! of the host OS. Mixing native and POSIX separators in the index produces
//! cross-platform indices that disagree on symbol identity — the v1.9.1
//! Windows eval hotfix and the v1.10.0 Phase 14.1 cli_module_symbols regression
//! were both members of this family.

use std::path::Path;

/// Strip `root` from `path` and return the relative path as a POSIX-style
/// string (forward-slash separators on every platform). Returns `None` when
/// `path` is not under `root` or the resulting bytes are not valid Unicode
/// (mirrors the prior `to_string_lossy` failure mode — non-UTF-8 paths fall
/// through, the index simply skips them).
pub fn to_rel_posix(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let lossy = rel.to_string_lossy().into_owned();
    Some(normalize_to_posix(lossy))
}

/// Convert a path string in-place: backslashes to forward slashes on
/// Windows, no-op on POSIX. The Windows branch is a single allocation
/// (`String::replace`) — cheap relative to the I/O the caller is about
/// to do, and we still avoid the work entirely on POSIX hosts.
#[inline]
pub fn normalize_to_posix(s: String) -> String {
    #[cfg(windows)]
    {
        s.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn posix_input_is_pass_through() {
        let root = PathBuf::from("/proj");
        let p = PathBuf::from("/proj/src/foo.rs");
        assert_eq!(to_rel_posix(&p, &root).unwrap(), "src/foo.rs");
    }

    #[test]
    fn returns_none_when_outside_root() {
        let root = PathBuf::from("/proj");
        let p = PathBuf::from("/elsewhere/foo.rs");
        assert!(to_rel_posix(&p, &root).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_backslashes_normalize_to_forward() {
        let root = PathBuf::from(r"C:\proj");
        let p = PathBuf::from(r"C:\proj\src\foo.rs");
        assert_eq!(to_rel_posix(&p, &root).unwrap(), "src/foo.rs");
    }

    #[test]
    fn normalize_to_posix_idempotent_on_posix_input() {
        assert_eq!(normalize_to_posix("src/foo.rs".to_string()), "src/foo.rs");
    }

    #[cfg(windows)]
    #[test]
    fn normalize_to_posix_converts_windows_separators() {
        assert_eq!(normalize_to_posix(r"src\foo.rs".to_string()), "src/foo.rs");
        assert_eq!(normalize_to_posix(r"a\b\c\d.rs".to_string()), "a/b/c/d.rs");
    }
}
