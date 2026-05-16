pub mod bloom;
pub mod bm25;
pub mod fusion;
pub mod rerank;
pub mod semantic;
pub mod similar;
pub mod structural;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MatchType {
    Structural,
    Semantic,
    Hybrid,
    Fuzzy,
    Bm25,
}
