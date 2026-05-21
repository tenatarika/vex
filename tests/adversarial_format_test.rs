/// Adversarial binary-format tests for `IndexReader`.
///
/// Every test verifies that malformed or truncated index files are rejected
/// gracefully (returning `Err`) and that the reader never panics.
///
/// # Header layout (v3, repr(C), 144 bytes)
///
/// | Offset | Size | Field                |
/// |--------|------|----------------------|
/// |   0    |   4  | magic                |
/// |   4    |   4  | version (u32 LE)     |
/// |   8    |   8  | symbol_count (u64)   |
/// |  16    |   4  | vector_dim (u32)     |
/// |  20    |   4  | _padding             |
/// |  24    |   8  | symbols_offset       |
/// |  32    |   8  | vectors_offset       |
/// |  40    |   8  | strings_offset       |
/// |  48    |   8  | inverted_offset      |
/// |  56    |   8  | hnsw_offset          |
/// |  64    |   8  | fst_offset           |
/// |  72    |   8  | fst_len              |
/// |  80    |   8  | postings_offset      |
/// |  88    |   8  | postings_len         |
/// |  96    |   8  | file_table_offset    |
/// | 104    |   4  | file_table_count     |
/// | 108    |   4  | _padding2            |
/// | 112    |   8  | sym_fst_offset       |
/// | 120    |   8  | sym_fst_len          |
/// | 128    |   8  | sym_postings_offset  |
/// | 136    |   8  | sym_postings_len     |
use std::io::Write;

use tempfile::NamedTempFile;
use vex::store::format::{CallGraphHeader, Header, V5SectionHeader, MAGIC, VERSION};
use vex::store::reader::IndexReader;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Confirmed at compile time: `Header::SIZE` must equal our hand-computed 144.
/// If the struct ever gains or loses fields this constant check will catch it.
const EXPECTED_HEADER_SIZE: usize = 144;

fn assert_header_size() {
    assert_eq!(
        Header::SIZE,
        EXPECTED_HEADER_SIZE,
        "Header::SIZE changed — update the byte-offset table in this file"
    );
}

/// Build a correctly-structured byte buffer for the header.
///
/// All fields are zero by default; callers may overwrite specific byte ranges
/// before writing to the temp file.
fn minimal_valid_header_bytes() -> Vec<u8> {
    let mut buf = vec![0u8; Header::SIZE];

    // magic at offset 0..4
    buf[0..4].copy_from_slice(MAGIC);

    // version at offset 4..8 (little-endian u32)
    buf[4..8].copy_from_slice(&VERSION.to_le_bytes());

    // symbol_count = 0 at offset 8..16 (already zero)

    // All section offsets are 0 and all lengths are 0 — the reader treats a
    // zero-length section as present but empty, which is valid.

    buf
}

/// Write `bytes` to a fresh `NamedTempFile` and return it.
///
/// The file must outlive any call to `IndexReader::open`.
fn write_tmp(bytes: &[u8]) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("create temp file");
    f.write_all(bytes).expect("write temp file");
    f.flush().expect("flush temp file");
    f
}

/// Write a `u64` in little-endian order into `buf` at `offset`.
fn write_u64_le(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// Write a `u32` in little-endian order into `buf` at `offset`.
fn write_u32_le(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// An empty file (0 bytes) must be rejected — the reader cannot fit a header.
#[test]
fn empty_file_rejected() {
    assert_header_size();
    let f = write_tmp(&[]);
    let result = IndexReader::open(f.path());
    assert!(result.is_err(), "expected Err for empty file, got Ok");
}

/// A file shorter than `Header::SIZE` must be rejected.
#[test]
fn too_small_file_rejected() {
    assert_header_size();
    // Use half the header size — clearly insufficient.
    let bytes = vec![0xAB_u8; Header::SIZE / 2];
    let f = write_tmp(&bytes);
    let result = IndexReader::open(f.path());
    assert!(
        result.is_err(),
        "expected Err for file shorter than Header::SIZE, got Ok"
    );
}

/// A file of exactly `Header::SIZE - 1` bytes must also be rejected.
#[test]
fn one_byte_short_rejected() {
    assert_header_size();
    let bytes = vec![0u8; Header::SIZE - 1];
    let f = write_tmp(&bytes);
    let result = IndexReader::open(f.path());
    assert!(
        result.is_err(),
        "expected Err for file that is one byte short of Header::SIZE, got Ok"
    );
}

/// A header with wrong magic bytes must be rejected regardless of other fields.
#[test]
fn wrong_magic_rejected() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();
    // Overwrite magic with garbage
    buf[0..4].copy_from_slice(b"NOPE");
    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    match result {
        Ok(_) => panic!("expected Err for wrong magic bytes, got Ok"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("corrupted") || msg.contains("magic"),
                "error message should mention corruption or magic, got: {msg}"
            );
        }
    }
}

/// All-zeros magic (plausible accident) must be rejected.
#[test]
fn zero_magic_rejected() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();
    buf[0..4].copy_from_slice(&[0u8; 4]);
    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    assert!(result.is_err(), "expected Err for all-zero magic, got Ok");
}

/// A header with an unsupported version number must be rejected.
#[test]
fn wrong_version_rejected() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();
    // Write version 99 — neither the current VERSION nor the legacy v2
    write_u32_le(&mut buf, 4, 99);
    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    match result {
        Ok(_) => panic!("expected Err for version 99, got Ok"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("version") || msg.contains("mismatch"),
                "error message should mention version mismatch, got: {msg}"
            );
        }
    }
}

/// Version 0 (clearly invalid) must be rejected.
#[test]
fn version_zero_rejected() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();
    write_u32_le(&mut buf, 4, 0);
    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    assert!(result.is_err(), "expected Err for version 0, got Ok");
}

/// A header where `symbols_offset` points past the end of the file must be
/// rejected as truncated.
#[test]
fn symbols_offset_past_eof() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();

    // Claim one symbol exists at an offset well beyond the file boundary.
    // symbol_count = 1 at offset 8
    write_u64_le(&mut buf, 8, 1);
    // symbols_offset = 10 MiB, far past our Header::SIZE-byte file
    write_u64_le(&mut buf, 24, 10 * 1024 * 1024);

    // Write only the header — no actual symbol data follows.
    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    match result {
        Ok(_) => panic!("expected Err for symbols_offset past EOF, got Ok"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("truncated") || msg.contains("corrupted"),
                "error message should mention truncation or corruption, got: {msg}"
            );
        }
    }
}

/// A header where `fst_offset + fst_len` exceeds the file size must be
/// rejected as corrupted.
#[test]
fn fst_section_past_eof() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();

    // fst_offset at offset 64, fst_len at offset 72
    write_u64_le(&mut buf, 64, Header::SIZE as u64); // starts right after header
    write_u64_le(&mut buf, 72, 1_000_000); // claims 1 MB of FST data

    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    assert!(
        result.is_err(),
        "expected Err when fst section exceeds file size, got Ok"
    );
}

/// A header where `postings_offset + postings_len` exceeds file size must be
/// rejected.
#[test]
fn postings_section_past_eof() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();

    // postings_offset at offset 80, postings_len at offset 88
    write_u64_le(&mut buf, 80, Header::SIZE as u64);
    write_u64_le(&mut buf, 88, 999_999);

    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    assert!(
        result.is_err(),
        "expected Err when postings section exceeds file size, got Ok"
    );
}

/// A header where `file_table_offset + file_table_count * 4` exceeds the file
/// size must be rejected.
#[test]
fn file_table_past_eof() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();

    // file_table_offset at offset 96, file_table_count at offset 104
    write_u64_le(&mut buf, 96, Header::SIZE as u64);
    write_u32_le(&mut buf, 104, 100_000); // 100_000 * 4 = 400 KB

    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    assert!(
        result.is_err(),
        "expected Err when file table exceeds file size, got Ok"
    );
}

/// A header where `sym_fst_offset + sym_fst_len` exceeds the file size must
/// be rejected.
#[test]
fn sym_fst_section_past_eof() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();

    // sym_fst_offset at offset 112, sym_fst_len at offset 120
    write_u64_le(&mut buf, 112, Header::SIZE as u64);
    write_u64_le(&mut buf, 120, 500_000);

    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    assert!(
        result.is_err(),
        "expected Err when sym_fst section exceeds file size, got Ok"
    );
}

/// `symbol_count = u64::MAX` combined with a non-zero `symbols_offset` must not
/// overflow — `saturating_mul` in the reader prevents it — and must be rejected
/// rather than cause a panic or wrap-around acceptance.
#[test]
fn symbol_count_overflow_rejected() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();

    // symbol_count = u64::MAX at offset 8
    write_u64_le(&mut buf, 8, u64::MAX);
    // symbols_offset = Header::SIZE (points just past the header)
    write_u64_le(&mut buf, 24, Header::SIZE as u64);

    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    // saturating_mul(SymbolRecord::SIZE) saturates to u64::MAX, which exceeds
    // any real file length — so the reader must return Err.
    assert!(
        result.is_err(),
        "expected Err for symbol_count = u64::MAX, got Ok"
    );
}

/// `symbol_count = u64::MAX` with `symbols_offset = 0` must also not panic.
/// The saturated product (u64::MAX) still exceeds file length.
#[test]
fn symbol_count_overflow_offset_zero_rejected() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();

    write_u64_le(&mut buf, 8, u64::MAX);
    // symbols_offset stays 0 — saturating_add(saturating_mul(MAX, record_size))
    // saturates before comparing with mmap_len.

    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    assert!(
        result.is_err(),
        "expected Err for symbol_count = u64::MAX with offset 0, got Ok"
    );
}

/// A complete, valid magic + version header with all other fields zeroed must
/// either be accepted (0 symbols, empty sections) or rejected cleanly — it
/// must never crash or panic.
#[test]
fn all_zeros_after_magic_version_no_crash() {
    assert_header_size();
    let buf = minimal_valid_header_bytes();
    // All section offsets and lengths are 0; symbol_count is 0.
    // The reader validates sym_end = 0 + 0*record_size = 0 <= file_len (144).
    // Similarly all other section ends are 0 <= 144. This should be accepted.
    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    // Either outcome is acceptable — we only require no panic.
    match result {
        Ok(_) => {
            // Accepted as a valid empty index — fine.
        }
        Err(e) => {
            // Rejected for some structural reason — also fine, must not panic.
            let _ = e.to_string(); // ensure Display is reachable
        }
    }
}

/// A header with `sym_postings_offset + sym_postings_len` past EOF must be
/// rejected.
#[test]
fn sym_postings_section_past_eof() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();

    // sym_postings_offset at offset 128, sym_postings_len at offset 136
    write_u64_le(&mut buf, 128, Header::SIZE as u64);
    write_u64_le(&mut buf, 136, 1_234_567);

    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    assert!(
        result.is_err(),
        "expected Err when sym_postings section exceeds file size, got Ok"
    );
}

/// A header with `symbols_offset = u64::MAX` (without overflow in sym_end
/// because symbol_count = 0) but file_table past EOF — exercises the combined
/// section bounds check.
#[test]
fn max_symbols_offset_zero_count_with_bad_filetable() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();

    // symbols_offset = u64::MAX but symbol_count = 0 → sym_end = u64::MAX + 0 = u64::MAX.
    // u64::MAX > file_len so this should be caught by the sym_end check.
    write_u64_le(&mut buf, 24, u64::MAX);

    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    assert!(
        result.is_err(),
        "expected Err for symbols_offset = u64::MAX with zero count, got Ok"
    );
}

/// Completely random bytes of length `Header::SIZE` must not panic — the
/// reader either accepts (astronomically unlikely) or rejects cleanly.
#[test]
fn random_header_sized_bytes_no_crash() {
    assert_header_size();
    // Pseudo-random but deterministic bytes — avoid depending on `rand` crate.
    let buf: Vec<u8> = (0..Header::SIZE)
        .map(|i| ((i * 0xDEAD_BEEF + 0x1337) & 0xFF) as u8)
        .collect();

    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    // No panic is the only hard requirement.
    let _ = result.map_err(|e| e.to_string());
}

/// A file that is exactly `Header::SIZE` bytes with valid magic and version
/// is accepted when all section lengths are zero.
#[test]
fn exact_header_size_valid_no_crash() {
    assert_header_size();
    let buf = minimal_valid_header_bytes();
    assert_eq!(buf.len(), Header::SIZE);

    let f = write_tmp(&buf);
    // Must not panic regardless of outcome.
    let result = IndexReader::open(f.path());
    let _ = result.map_err(|e| e.to_string());
}

/// Legacy version 2 (accepted by the reader per the compatibility comment)
/// with otherwise valid zero-filled fields must not panic.
#[test]
fn legacy_version_2_no_crash() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();
    // Override version to 2 (the legacy accepted value)
    write_u32_le(&mut buf, 4, 2);

    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    // v2 is explicitly supported — must not return a version-mismatch error.
    match result {
        Ok(_) => {}
        Err(e) => {
            let msg = e.to_string();
            assert!(
                !msg.contains("version") && !msg.contains("mismatch"),
                "v2 index should not fail with a version error, got: {msg}"
            );
        }
    }
}

/// 10.6 regression: every IndexReader::open failure must surface the index
/// file path so the user can `rm` it without grepping stderr for the cache
/// location.
#[test]
fn open_error_includes_index_path() {
    assert_header_size();
    let mut buf = minimal_valid_header_bytes();
    buf[0..4].copy_from_slice(b"NOPE"); // bad magic — picks one of the bail! paths
    let f = write_tmp(&buf);
    let err = match IndexReader::open(f.path()) {
        Ok(_) => panic!("expected Err for bad magic"),
        Err(e) => e,
    };
    let msg = format!("{err:#}"); // anyhow chain
    let path_str = f.path().to_string_lossy();
    assert!(
        msg.contains(path_str.as_ref()),
        "open error should include the index file path `{path_str}`, got: {msg}"
    );
}

/// 10.6 regression: opening a non-existent file should also surface the
/// requested path (different code path — `File::open` failure rather than a
/// `bail!` after mmap).
#[test]
fn open_missing_file_error_includes_path() {
    let path = std::path::PathBuf::from("/nonexistent-vex-test-path/index.vex");
    let err = match IndexReader::open(&path) {
        Ok(_) => panic!("expected Err for nonexistent file"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("/nonexistent-vex-test-path/index.vex"),
        "open error should include the requested path, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// 11.1.3a: v5 format bump — V5SectionHeader appended after CallGraphHeader.
// All offsets stay zero in 11.1.3a; the section payload itself lands in 11.1.3b.
// ---------------------------------------------------------------------------

/// `VERSION` is the version this build writes. After 11.1.3a it must be 5
/// and the v5 layout must place a `V5SectionHeader` immediately after the
/// existing `CallGraphHeader`.
#[test]
fn version_is_five_after_format_bump() {
    assert_eq!(VERSION, 5, "11.1.3a bumps the writer to v5");
}

/// A v5 file truncated *exactly* at the end of the CallGraphHeader (i.e.,
/// missing the V5SectionHeader bytes) must be rejected with a clear
/// error rather than silently treated as an empty section.
#[test]
fn v5_truncated_at_v5_section_header_rejected() {
    let mut buf = minimal_valid_header_bytes();
    // Append a zeroed CallGraphHeader but *not* the V5SectionHeader.
    buf.resize(Header::SIZE + CallGraphHeader::SIZE, 0);
    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    assert!(
        result.is_err(),
        "expected Err for v5 file missing V5SectionHeader bytes, got Ok",
    );
}

/// A v5 file with `V5SectionHeader.ref_edges_offset + ref_edges_len`
/// past EOF must be rejected. The V5SectionHeader bytes themselves fit;
/// only the section payload exceeds the file size.
#[test]
fn v5_ref_edges_section_past_eof_rejected() {
    let mut buf = minimal_valid_header_bytes();
    buf.resize(
        Header::SIZE + CallGraphHeader::SIZE + V5SectionHeader::SIZE,
        0,
    );
    // V5SectionHeader sits at Header::SIZE + CallGraphHeader::SIZE.
    // Its first field is `ref_edges_offset` (u64 at offset 0), and the
    // second is `ref_edges_len` (u64 at offset 8). Point past the file
    // end.
    let v5_off = Header::SIZE + CallGraphHeader::SIZE;
    write_u64_le(&mut buf, v5_off, Header::SIZE as u64);
    write_u64_le(&mut buf, v5_off + 8, 1_234_567);
    let f = write_tmp(&buf);
    let result = IndexReader::open(f.path());
    assert!(
        result.is_err(),
        "expected Err when ref_edges section exceeds file size, got Ok",
    );
}

/// Sanity check: the V5SectionHeader is non-zero size so empty-section
/// validation is meaningful.
/// Compile-time guard: `V5SectionHeader` must define at least one
/// field. A zero-sized header would let a corrupt index skip the
/// V5SectionHeader bytes entirely without the reader noticing.
#[test]
fn v5_section_header_has_nonzero_size() {
    const _: () = assert!(V5SectionHeader::SIZE > 0);
}
