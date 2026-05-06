use xxhash_rust::xxh3::xxh3_64;

/// Compute a fast non-cryptographic hash of file content for change detection.
pub fn content_hash(data: &[u8]) -> u64 {
    xxh3_64(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_content_same_hash() {
        let a = content_hash(b"fn main() {}");
        let b = content_hash(b"fn main() {}");
        assert_eq!(a, b);
    }

    #[test]
    fn different_content_different_hash() {
        let a = content_hash(b"fn main() {}");
        let b = content_hash(b"fn main() { println!(); }");
        assert_ne!(a, b);
    }
}
