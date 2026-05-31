use std::io::Read;
use std::path::Path;

use xxhash_rust::xxh3::{xxh3_64, Xxh3Default};

/// Compute a fast non-cryptographic hash of file content for change detection.
pub fn content_hash(data: &[u8]) -> u64 {
    xxh3_64(data)
}

/// Stream the file through xxh3 in 64 KiB chunks. Used by the H11
/// staleness probe so a `.md` or `.sql` migration that's tens of MB
/// doesn't spike RSS by being slurped into a single `Vec<u8>` —
/// `content_hash(&fs::read(path))` would allocate the full file size,
/// but xxh3 itself is streaming-capable. Returns the same value as
/// `content_hash(bytes)` would for the file's contents.
pub fn hash_file(path: &Path) -> std::io::Result<u64> {
    let file = std::fs::File::open(path)?;
    // 64 KiB matches the page-aligned buffer xxh3 itself processes
    // most efficiently and keeps RSS bounded regardless of file size.
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Xxh3Default::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.digest())
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

    #[test]
    fn hash_file_matches_content_hash_for_small_input() {
        // Sanity: streaming and one-shot must produce the same digest
        // for inputs that fit in one buffer. Pin the contract so a
        // future xxh3 upgrade can't silently diverge the two paths.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let data = b"fn main() { println!(\"hi\"); }\n";
        std::fs::write(tmp.path(), data).unwrap();
        let streamed = hash_file(tmp.path()).unwrap();
        let one_shot = content_hash(data);
        assert_eq!(streamed, one_shot);
    }

    #[test]
    fn hash_file_matches_content_hash_across_buffer_boundary() {
        // Force more than one 64 KiB chunk so the streaming path exercises
        // `update` more than once.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let data: Vec<u8> = (0..200_000u32).map(|i| (i & 0xff) as u8).collect();
        std::fs::write(tmp.path(), &data).unwrap();
        let streamed = hash_file(tmp.path()).unwrap();
        let one_shot = content_hash(&data);
        assert_eq!(streamed, one_shot);
    }
}
