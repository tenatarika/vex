//! Shared read path for the binary sidecars next to `index.vex`.
//!
//! Every sidecar codec (`store::body_tokens`, `store::trigram`,
//! `embed::cache`, …) is a magic + version header followed by a record
//! sequence, and every one of them wants the same two things on load:
//!
//! * **one read instead of a syscall per field.** Parsing straight off a
//!   `File` means `read_exact` per length prefix, per string, per vector —
//!   `EmbedCache` managed one syscall per `f32`. Reading the file once and
//!   parsing from memory removes all of it.
//! * **a refusal that costs nothing when the file is not what it claims.**
//!   Slurping the file first would mean a corrupt or truncated-and-regrown
//!   sidecar costs its full size in memory before anything checks it.
//!
//! [`SidecarReader`] gets both by reading in the order the format is written:
//! magic, then the header, then a body **bounded by what the header just
//! claimed**. A file whose header says "three records" cannot cost more than
//! three records' worth of memory, however large it is on disk.
//!
//! ```ignore
//! let mut r = SidecarReader::open(path, MAGIC)?;
//! let head = r.take_header(8)?;              // version + count
//! let count = u32::from_le_bytes(head[4..8].try_into().unwrap());
//! if count > MAX_COUNT { bail!("count absurd") }
//! let bytes = r.finish(count as u64 * MAX_RECORD_BYTES)?;
//! // `bytes` holds magic + header + body; parse the records from there.
//! ```
//!
//! Note the tradeoff this deliberately accepts: peak memory during a load is
//! the file plus the parsed structure, rather than one record at a time. For
//! the sizes these sidecars reach in practice (single-digit MiB) that is
//! irrelevant; at a format's documented absurdity ceiling it is a doubling.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Reads a sidecar in two stages: header first, then a body whose size the
/// header decides. Everything consumed is accumulated, so [`finish`] hands back
/// one buffer the caller can parse from offset zero.
///
/// [`finish`]: SidecarReader::finish
pub struct SidecarReader {
    file: std::fs::File,
    path: PathBuf,
    /// Everything read so far: magic, then each `take_header` chunk.
    buf: Vec<u8>,
    /// The file's size at open. Only an allocation hint — never a gate, since
    /// the header's own claim is the real bound.
    len_hint: u64,
}

impl SidecarReader {
    /// Open the file and verify its magic. Four bytes of I/O decide whether
    /// anything else is worth doing.
    pub fn open(path: &Path, magic: &[u8; 4]) -> Result<Self> {
        let mut file =
            std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        let len_hint = file.metadata().map(|m| m.len()).unwrap_or(0);

        let mut head = [0u8; 4];
        file.read_exact(&mut head).context("read magic")?;
        if &head != magic {
            anyhow::bail!("magic mismatch (got {:?})", head);
        }

        Ok(Self {
            file,
            path: path.to_path_buf(),
            buf: head.to_vec(),
            len_hint,
        })
    }

    /// Read `n` more header bytes and return just those, for the caller to
    /// validate. They stay in the accumulated buffer.
    pub fn take_header(&mut self, n: usize) -> Result<&[u8]> {
        let start = self.buf.len();
        self.buf.resize(start + n, 0);
        self.file
            .read_exact(&mut self.buf[start..])
            .with_context(|| format!("read header of {}", self.path.display()))?;
        Ok(&self.buf[start..])
    }

    /// Read the rest of the file, at most `max_body` bytes, and return the
    /// whole buffer — magic, header and body.
    ///
    /// `max_body` is what the header implies: record count times the largest a
    /// record may be. Bytes past it are ignored rather than rejected, matching
    /// what the streaming readers did with trailing junk.
    pub fn finish(mut self, max_body: u64) -> Result<Vec<u8>> {
        let hint = (self.len_hint as usize).min(self.buf.len() + max_body as usize);
        self.buf.reserve(hint.saturating_sub(self.buf.len()));
        self.file
            .take(max_body)
            .read_to_end(&mut self.buf)
            .with_context(|| format!("read {}", self.path.display()))?;
        Ok(self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const MAGIC: &[u8; 4] = b"TSTM";

    fn write(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn returns_magic_header_and_body_as_one_buffer() {
        let dir = TempDir::new().unwrap();
        let p = write(&dir, "ok.bin", b"TSTMhdrbody!");
        let mut r = SidecarReader::open(&p, MAGIC).unwrap();
        assert_eq!(r.take_header(3).unwrap(), b"hdr");
        assert_eq!(r.finish(64).unwrap(), b"TSTMhdrbody!");
    }

    #[test]
    fn rejects_bad_magic_before_reading_the_body() {
        let dir = TempDir::new().unwrap();
        let p = write(&dir, "bad.bin", b"XXXXpayload");
        let err = match SidecarReader::open(&p, MAGIC) {
            Ok(_) => panic!("bad magic must not open"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("magic mismatch"), "{err}");
    }

    #[test]
    fn rejects_file_too_short_for_magic() {
        let dir = TempDir::new().unwrap();
        let p = write(&dir, "short.bin", b"TS");
        assert!(SidecarReader::open(&p, MAGIC).is_err());
    }

    #[test]
    fn rejects_file_too_short_for_header() {
        let dir = TempDir::new().unwrap();
        let p = write(&dir, "nohdr.bin", b"TSTMab");
        let mut r = SidecarReader::open(&p, MAGIC).unwrap();
        assert!(r.take_header(8).is_err());
    }

    /// The point of the two-stage read: a file far larger than its header
    /// claims costs only what the claim implies, not what the file weighs.
    #[test]
    fn body_read_is_bounded_by_the_headers_claim() {
        let dir = TempDir::new().unwrap();
        let mut bytes = b"TSTMhdr".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 100_000));
        let p = write(&dir, "huge.bin", &bytes);

        let mut r = SidecarReader::open(&p, MAGIC).unwrap();
        r.take_header(3).unwrap();
        // Header claims two records of eight bytes; the other ~100 KiB on disk
        // must never reach memory.
        let got = r.finish(16).unwrap();
        assert_eq!(got.len(), 4 + 3 + 16);
        assert!(got.ends_with(b"xxxxxxxxxxxxxxxx"));
    }

    #[test]
    fn body_shorter_than_the_bound_is_fine() {
        let dir = TempDir::new().unwrap();
        let p = write(&dir, "small.bin", b"TSTMhdrxy");
        let mut r = SidecarReader::open(&p, MAGIC).unwrap();
        r.take_header(3).unwrap();
        assert_eq!(r.finish(4096).unwrap(), b"TSTMhdrxy");
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("absent.bin");
        assert!(SidecarReader::open(&p, MAGIC).is_err());
    }
}
