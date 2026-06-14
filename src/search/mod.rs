pub mod bloom;
pub mod bm25;
pub mod explain;
pub mod fusion;
pub mod hash_index;
pub mod metadata;
pub mod rerank;
pub mod semantic;
pub mod similar;
pub mod structural;
pub mod trace;

use std::fmt;

use serde::{Deserialize, Serialize};

/// Unified search result returned to the caller regardless of search method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: usize,
    pub signature: Option<String>,
    pub score: f64,
    pub match_type: MatchType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum MatchType {
    Structural,
    Semantic,
    Hybrid,
    Fuzzy,
    Bm25,
}

impl fmt::Display for MatchType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Stable variant names — kept in lock-step with serde's default
        // externally-tagged output for unit variants so JSON and text
        // reports agree (Phase 13.12.1: per-channel attribution).
        let s = match self {
            MatchType::Structural => "Structural",
            MatchType::Semantic => "Semantic",
            MatchType::Hybrid => "Hybrid",
            MatchType::Fuzzy => "Fuzzy",
            MatchType::Bm25 => "Bm25",
        };
        f.write_str(s)
    }
}
