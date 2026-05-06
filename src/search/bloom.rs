use bloomfilter::Bloom;

/// Bloom filter for fast "definitely not in index" checks.
pub struct SymbolBloom {
    filter: Bloom<str>,
}

impl SymbolBloom {
    /// Create a new bloom filter sized for `expected_items` with target false-positive rate.
    pub fn new(expected_items: usize) -> Self {
        let filter = Bloom::new_for_fp_rate(expected_items, 0.01);
        Self { filter }
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
