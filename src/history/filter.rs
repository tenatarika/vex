//! Phase 14.9 Tier A.2-4 — `HistoryFilter`: post-FST/post-walker
//! filtering by date, author, and symbol kind.
//!
//! Filter runs uniformly against [`HistoricalSymbol`] slices regardless
//! of which path (walker or indexed) produced them. All four fields of
//! the underlying `HistoricalSymbol` that the filter touches —
//! `commit_date` (`YYYY-MM-DD`), `author`, `kind` — are already
//! `String` on both paths, so lex-comparison on the date and
//! lowercased substring matching on the author work without any
//! representation conversion.
//!
//! `--author` is walker-only. The CLI dispatcher rejects it on the
//! indexed path before the filter is ever constructed (Phase 14.8
//! sidecar drops author info; Phase 14.10 will fix that).

use crate::history::HistoricalSymbol;

/// Composable filter for [`HistoricalSymbol`] result lists. Each
/// field defaults to "no filter"; build by setting fields directly,
/// then call [`Self::apply`].
#[derive(Debug, Default, Clone)]
pub struct HistoryFilter {
    /// Inclusive lower bound, `YYYY-MM-DD`. Compared lexicographically
    /// against [`HistoricalSymbol::commit_date`] (fixed-width
    /// zero-padded ISO is lex-equivalent to chronological order).
    pub since_iso: Option<String>,
    /// Inclusive upper bound, `YYYY-MM-DD`. Same lex-compare contract.
    pub until_iso: Option<String>,
    /// Case-insensitive substring against
    /// [`HistoricalSymbol::author`]. Walker-only — the indexed path
    /// must reject `--author` before constructing the filter.
    pub author: Option<String>,
    /// Exact (lowercased) match against [`HistoricalSymbol::kind`].
    /// Kinds on both paths are already stored lowercase.
    pub kind: Option<String>,
}

impl HistoryFilter {
    /// True iff at least one field is set. Useful for short-circuiting
    /// allocation when no filter is requested.
    pub fn is_active(&self) -> bool {
        self.since_iso.is_some()
            || self.until_iso.is_some()
            || self.author.is_some()
            || self.kind.is_some()
    }

    /// Apply the filter to `rows`, returning an iterator over the
    /// matching subset. Borrows `rows` for `'a`; captures owned
    /// copies of the filter fields into the closure (no `&self`
    /// lifetime tie — the returned iterator does not depend on the
    /// filter struct outliving the call).
    pub fn apply<'a>(
        &self,
        rows: &'a [HistoricalSymbol],
    ) -> impl Iterator<Item = &'a HistoricalSymbol> + 'a {
        let since = self.since_iso.clone();
        let until = self.until_iso.clone();
        let author = self.author.as_ref().map(|a| a.to_ascii_lowercase());
        let kind = self.kind.clone();

        rows.iter().filter(move |r| {
            if let Some(ref s) = since {
                if r.commit_date.as_str() < s.as_str() {
                    return false;
                }
            }
            if let Some(ref u) = until {
                if r.commit_date.as_str() > u.as_str() {
                    return false;
                }
            }
            if let Some(ref a) = author {
                if !r.author.to_ascii_lowercase().contains(a) {
                    return false;
                }
            }
            if let Some(ref k) = kind {
                if &r.kind != k {
                    return false;
                }
            }
            true
        })
    }
}

/// Validate a `YYYY-MM-DD` date string. Returns the canonical
/// representation on success (currently a passthrough, but normalising
/// here gives the CLI surface a single chokepoint to reject `2024-1-1`
/// or other malformed shapes before they reach the filter).
pub fn parse_iso_date(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(format!("expected date in YYYY-MM-DD form, got {s:?}"));
    }
    for (i, b) in bytes.iter().enumerate() {
        if i == 4 || i == 7 {
            continue;
        }
        if !b.is_ascii_digit() {
            return Err(format!("expected date in YYYY-MM-DD form, got {s:?}"));
        }
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(date: &str, author: &str, kind: &str) -> HistoricalSymbol {
        HistoricalSymbol {
            commit_sha: "deadbeef".into(),
            commit_date: date.into(),
            author: author.into(),
            file_path: "lib.rs".into(),
            blob_sha: "cafebabe".into(),
            line: 1,
            signature: "fn f()".into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn empty_filter_passes_everything() {
        let rows = vec![sym("2026-01-01", "alice", "function")];
        let f = HistoryFilter::default();
        assert!(!f.is_active());
        let kept: Vec<_> = f.apply(&rows).collect();
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn since_includes_boundary() {
        let rows = vec![
            sym("2025-12-31", "alice", "function"),
            sym("2026-01-01", "bob", "function"),
            sym("2026-02-15", "carol", "function"),
        ];
        let f = HistoryFilter {
            since_iso: Some("2026-01-01".into()),
            ..Default::default()
        };
        let kept: Vec<_> = f.apply(&rows).map(|r| r.author.as_str()).collect();
        assert_eq!(kept, vec!["bob", "carol"]);
    }

    #[test]
    fn until_includes_boundary() {
        let rows = vec![
            sym("2026-01-01", "alice", "function"),
            sym("2026-06-09", "bob", "function"),
            sym("2026-06-10", "carol", "function"),
        ];
        let f = HistoryFilter {
            until_iso: Some("2026-06-09".into()),
            ..Default::default()
        };
        let kept: Vec<_> = f.apply(&rows).map(|r| r.author.as_str()).collect();
        assert_eq!(kept, vec!["alice", "bob"]);
    }

    #[test]
    fn since_and_until_window() {
        let rows = vec![
            sym("2025-12-31", "a", "function"),
            sym("2026-01-15", "b", "function"),
            sym("2026-06-09", "c", "function"),
            sym("2026-06-10", "d", "function"),
        ];
        let f = HistoryFilter {
            since_iso: Some("2026-01-01".into()),
            until_iso: Some("2026-06-09".into()),
            ..Default::default()
        };
        let kept: Vec<_> = f.apply(&rows).map(|r| r.author.as_str()).collect();
        assert_eq!(kept, vec!["b", "c"]);
    }

    #[test]
    fn author_substring_case_insensitive() {
        let rows = vec![
            sym("2026-01-01", "Alice Liddell", "function"),
            sym("2026-01-02", "Bob Smith", "function"),
            sym("2026-01-03", "alice@example.com", "function"),
        ];
        let f = HistoryFilter {
            author: Some("ALICE".into()),
            ..Default::default()
        };
        let kept: Vec<_> = f.apply(&rows).map(|r| r.author.as_str()).collect();
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|a| a.to_lowercase().contains("alice")));
    }

    #[test]
    fn kind_exact_match() {
        let rows = vec![
            sym("2026-01-01", "a", "struct"),
            sym("2026-01-02", "b", "impl"),
            sym("2026-01-03", "c", "struct"),
            sym("2026-01-04", "d", "function"),
        ];
        let f = HistoryFilter {
            kind: Some("struct".into()),
            ..Default::default()
        };
        let kept: Vec<_> = f.apply(&rows).map(|r| r.author.as_str()).collect();
        assert_eq!(kept, vec!["a", "c"]);
    }

    #[test]
    fn all_four_compose() {
        let rows = vec![
            sym("2026-01-15", "alice", "struct"),
            sym("2026-01-15", "alice", "impl"),
            sym("2026-07-01", "alice", "struct"),
            sym("2026-01-15", "bob", "struct"),
        ];
        let f = HistoryFilter {
            since_iso: Some("2026-01-01".into()),
            until_iso: Some("2026-06-30".into()),
            author: Some("alice".into()),
            kind: Some("struct".into()),
        };
        let kept: Vec<_> = f.apply(&rows).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].author, "alice");
        assert_eq!(kept[0].kind, "struct");
        assert_eq!(kept[0].commit_date, "2026-01-15");
    }

    #[test]
    fn parse_iso_date_accepts_well_formed() {
        assert_eq!(parse_iso_date("2026-06-09").unwrap(), "2026-06-09");
        assert_eq!(parse_iso_date("1970-01-01").unwrap(), "1970-01-01");
    }

    #[test]
    fn parse_iso_date_rejects_malformed() {
        for bad in [
            "2026-1-1",
            "2026/06/09",
            "2026-06-9",
            "yesterday",
            "",
            "2026-06-9 ",
        ] {
            assert!(parse_iso_date(bad).is_err(), "should reject {bad:?}");
        }
    }
}
