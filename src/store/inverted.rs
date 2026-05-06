use std::collections::HashMap;

/// In-memory inverted index: maps lowercased name tokens to symbol indices.
#[derive(Default)]
pub struct InvertedIndex {
    entries: HashMap<String, Vec<u32>>,
}

impl InvertedIndex {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Build an inverted index from an mmap'd IndexReader.
    pub fn from_reader(reader: &super::reader::IndexReader) -> Self {
        let mut idx = Self::new();
        for i in 0..reader.symbol_count() {
            if let Some(rec) = reader.symbol(i) {
                let name = reader.read_string(rec.name_offset);
                if !name.is_empty() {
                    idx.insert(name, i as u32);
                }
            }
        }
        idx
    }

    /// Add a symbol name to the index.
    pub fn insert(&mut self, name: &str, symbol_idx: u32) {
        let key = name.to_lowercase();
        self.entries.entry(key).or_default().push(symbol_idx);

        // Also index sub-tokens for CamelCase: "FooBarBaz" -> ["foo", "bar", "baz"]
        for token in split_camel_case(name) {
            let token_lower = token.to_lowercase();
            if token_lower != name.to_lowercase() {
                self.entries.entry(token_lower).or_default().push(symbol_idx);
            }
        }
    }

    /// Find symbol indices matching a query (exact or prefix).
    pub fn search(&self, query: &str, limit: usize) -> Vec<u32> {
        let q = query.to_lowercase();

        // Exact match first
        if let Some(indices) = self.entries.get(&q) {
            return indices.iter().take(limit).copied().collect();
        }

        // Prefix match
        let mut results = Vec::new();
        for (key, indices) in &self.entries {
            if key.starts_with(&q) {
                results.extend(indices.iter().take(limit));
                if results.len() >= limit {
                    break;
                }
            }
        }
        results.truncate(limit);
        results
    }
}

fn split_camel_case(s: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0;

    for i in 1..bytes.len() {
        if bytes[i].is_ascii_uppercase() && bytes[i - 1].is_ascii_lowercase() {
            tokens.push(&s[start..i]);
            start = i;
        }
    }
    if start < s.len() {
        tokens.push(&s[start..]);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_split() {
        assert_eq!(split_camel_case("FooBarBaz"), vec!["Foo", "Bar", "Baz"]);
        assert_eq!(split_camel_case("simple"), vec!["simple"]);
        assert_eq!(split_camel_case("XMLParser"), vec!["XMLParser"]);
    }

    #[test]
    fn exact_and_prefix_search() {
        let mut idx = InvertedIndex::new();
        idx.insert("PaymentService", 0);
        idx.insert("PaymentGateway", 1);
        idx.insert("UserService", 2);

        assert_eq!(idx.search("paymentservice", 10), vec![0]);
        let prefix = idx.search("payment", 10);
        assert!(prefix.contains(&0) || prefix.contains(&1));
    }
}
