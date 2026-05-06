use bloomfilter::Bloom;

/// Bloom filter for fast "definitely not in index" checks.
/// TODO(phase4): wire into MCP server for fast pre-filtering
#[allow(dead_code)]
pub struct SymbolBloom {
    filter: Bloom<str>,
}

#[allow(dead_code)]
impl SymbolBloom {
    /// Create a new bloom filter sized for `expected_items` with target false-positive rate.
    pub fn new(expected_items: usize) -> Self {
        let items = expected_items.max(1);
        let filter = Bloom::new_for_fp_rate(items, 0.01);
        Self { filter }
    }

    /// Build a bloom filter from an mmap'd IndexReader.
    pub fn from_reader(reader: &crate::store::reader::IndexReader) -> Self {
        let count = reader.symbol_count();
        // Size for 2x: both original name and lowercased variant may be inserted
        let mut bloom = Self::new(count * 2);
        for i in 0..count {
            if let Some(rec) = reader.symbol(i) {
                let name = reader.read_string(rec.name_offset);
                if !name.is_empty() {
                    bloom.insert(name);
                    // Also insert lowercased for case-insensitive checks
                    let lower = name.to_lowercase();
                    if lower != name {
                        bloom.insert(&lower);
                    }
                }
            }
        }
        bloom
    }

    pub fn insert(&mut self, name: &str) {
        self.filter.set(name);
    }

    /// Returns false if the name is definitely NOT in the index.
    /// Returns true if the name MIGHT be in the index (check the real index).
    pub fn may_contain(&self, name: &str) -> bool {
        self.filter.check(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserted_items_found() {
        let mut bloom = SymbolBloom::new(1000);
        bloom.insert("PaymentService");
        bloom.insert("UserRepository");

        assert!(bloom.may_contain("PaymentService"));
        assert!(bloom.may_contain("UserRepository"));
    }

    #[test]
    fn missing_items_usually_rejected() {
        let mut bloom = SymbolBloom::new(1000);
        for i in 0..100 {
            bloom.insert(&format!("Symbol{i}"));
        }
        // Items never inserted should mostly return false
        let mut false_positives = 0;
        for i in 1000..2000 {
            if bloom.may_contain(&format!("Other{i}")) {
                false_positives += 1;
            }
        }
        // With 1% FP rate and 1000 tests, expect ~10 false positives
        assert!(false_positives < 50, "too many false positives: {false_positives}");
    }
}
