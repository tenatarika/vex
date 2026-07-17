use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;
use regex::Regex;

pub mod trigram;

use crate::store::trigram as store_trigram;
use crate::store::trigram::TrigramRecord;
use crate::util::config;
use trigram::{Trigram, TrigramBloom};

/// A single grep match in a file.
#[derive(Debug, Clone)]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// Search file contents by regex pattern. Parallel scan.
///
/// When an `index.trigram` sidecar is present and the pattern yields a
/// required literal, files whose bloom provably can't contain that literal
/// are skipped before they're read (see [`TrigramSkip`]). Absent sidecar,
/// non-literal pattern, or a stale record → the file is read as before, so
/// the result set is identical to a full walk — the skip-index only trims
/// I/O, never matches.
///
/// `force_text` is the `--text`/`-a` escape hatch (ripgrep parity): when
/// `true`, both binary-skip layers (extension denylist in
/// [`discover_files`], content sniff in [`read_text_skip_binary`]) are
/// bypassed and every file is read whole, still subject only to the
/// UTF-8-validity check. See `docs/LIMITATIONS.md` §5b.
pub fn search(
    root: &Path,
    pattern: &str,
    filter_path: Option<&str>,
    limit: usize,
    excludes: &[String],
    force_text: bool,
) -> Result<Vec<GrepMatch>> {
    let re = Regex::new(pattern).context("invalid regex pattern")?;
    let skip = TrigramSkip::build(root, pattern);
    let files = discover_files(root, filter_path, excludes, skip.as_ref(), force_text)?;

    let matches: Vec<GrepMatch> = files
        .par_iter()
        .flat_map(|path| {
            let content = match read_text_skip_binary(path, force_text) {
                Some(c) => c,
                None => return Vec::new(), // binary or unreadable — skip silently
            };

            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            let mut file_matches = Vec::new();
            for (line_num, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    file_matches.push(GrepMatch {
                        path: rel.clone(),
                        line: line_num + 1,
                        text: line.trim().to_string(),
                    });
                }
            }
            file_matches
        })
        .collect();

    Ok(matches.into_iter().take(limit).collect())
}

/// Bytes sniffed from the head of a file to decide binary vs. text before
/// committing to a full read. Same 8 KB window as
/// `parse_files::looks_binary`, but that one is a post-hoc *str*-level check
/// on already read + valid-UTF8 content; here the sniff is byte-level and
/// happens *before* the rest of the file is pulled in, so a binary asset
/// (png, xlsx, ttf, ...) costs one 8 KB read instead of a full read. Files
/// reaching `search` are already ≤ 1 MB (capped in `discover_files`), so the
/// read this avoids tops out there — see `docs/FMINDEX-RESEARCH.md`
/// §"A0 measurement".
const SNIFF_BYTES: usize = 8192;

/// True if `sample` looks like binary content: a NUL byte anywhere (the
/// ripgrep convention — text files essentially never contain NUL), or
/// control bytes (excluding `\n`, `\r`, `\t`) making up more than 5% of the
/// sample. The control-ratio threshold matches `parse_files::looks_binary`;
/// this is a byte-level classifier and deliberately omits that function's
/// minified-long-line heuristic (a minified JS/CSS file is still greppable
/// text).
fn is_binary_bytes(sample: &[u8]) -> bool {
    if sample.contains(&0) {
        return true;
    }
    let control = sample
        .iter()
        .filter(|&&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
        .count();
    control * 20 > sample.len() // ≥5% control bytes
}

/// Read `path` as UTF-8 text, skipping binary files cheaply.
///
/// Reads only the first [`SNIFF_BYTES`] up front and classifies them with
/// [`is_binary_bytes`]. A file that sniffs as binary is abandoned right
/// there — the rest of it is never read, which is the whole point: the A0
/// measurement showed ~86% of a rare-token `vex grep`'s wall-clock going
/// into `read_to_string` on binary assets (png/xlsx/ttf) that only fail
/// UTF-8 validation after being read in full.
///
/// A file whose first 8 KB looks like text is read to completion and
/// validated as UTF-8 as before, so **no false negatives for TEXT files**:
/// anything that used to be matched is still fully read and regex'd. A file
/// that is valid-looking in its first 8 KB but turns out to contain
/// invalid UTF-8 or binary bytes later still returns `None` via the
/// `String::from_utf8` check, preserving today's skip-on-non-UTF8 behaviour.
///
/// One deliberate semantic change: a text file with a stray NUL byte (or a
/// high-control-byte prefix) in its first 8 KB is now treated as binary and
/// skipped, matching ripgrep's default — unless `force_text` (the
/// `--text`/`-a` escape hatch) is set, in which case the sniff is skipped
/// entirely: the whole file is read and only UTF-8 validity gates the
/// result, matching the pre-binary-skip default. vex greps line-by-line
/// over a Rust `String`, so it cannot lossy-decode — a truly invalid-UTF-8
/// file is still skipped even with `force_text`.
fn read_text_skip_binary(path: &Path, force_text: bool) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;

    if force_text {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        return String::from_utf8(buf).ok();
    }

    // Sniff the head only. `take` + `read_to_end` retries `Interrupted`
    // internally and grows the buffer to fit, so a small file never pays for
    // a full 8 KB zeroed allocation.
    let mut buf = Vec::with_capacity(SNIFF_BYTES);
    file.by_ref()
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;

    if is_binary_bytes(&buf) {
        return None;
    }

    // Head looks textual — pull the rest (appends after the sniffed prefix,
    // no re-read) and validate the whole file as UTF-8.
    file.read_to_end(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// Extensions that are definitely binary and can never be greppable text,
/// so the file is dropped in `discover_files` *before* it is opened —
/// removing the `open()`/read syscall entirely, not just the bytes. The A0
/// measurement showed a per-file `open()`-bound cost on asset-heavy repos:
/// content-sniffing (still one open per file) gave ~1.1× warm, while
/// skipping known-binary extensions before opening gave ~3×.
///
/// Text formats are deliberately absent — `svg` (XML), `json`, `csv`, `txt`,
/// `scss`, source code — so they still reach the content sniff / read. The
/// list handles only *unambiguous* cases. Generically-named extensions that
/// projects sometimes use for plain text (`.dat`, `.bin`, extensionless) are
/// deliberately NOT listed: they fall through to [`read_text_skip_binary`]'s
/// sniff, which reads them if textual and skips them only on a real NUL /
/// high-control prefix — avoiding a false-negative on text-in-`.dat`.
///
/// Bypassed entirely by the `--text`/`-a` escape hatch (`force_text` in
/// [`discover_files`]) — a denylisted extension is still opened and read
/// when the caller explicitly asks to force-read binaries.
fn is_binary_ext(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "ico"
                | "bmp"
                | "webp"
                | "tiff"
                | "pdf"
                | "xlsx"
                | "xls"
                | "docx"
                | "doc"
                | "pptx"
                | "ppt"
                | "zip"
                | "gz"
                | "bz2"
                | "xz"
                | "tar"
                | "7z"
                | "rar"
                | "ttf"
                | "otf"
                | "woff"
                | "woff2"
                | "eot"
                | "mo"
                | "avro"
                | "parquet"
                | "pyc"
                | "pyo"
                | "so"
                | "dll"
                | "dylib"
                | "class"
                | "jar"
                | "wasm"
                | "o"
                | "a"
                | "exe"
                | "mp3"
                | "mp4"
                | "wav"
                | "mov"
                | "avi"
                | "webm"
                | "flac"
                | "ogg"
        )
    )
}

/// The `index.trigram` skip-index paired with the current pattern's
/// required trigrams. `can_skip` decides — per file, from metadata the
/// walk already fetched — whether the file provably cannot match and can
/// be left unread.
///
/// **No false negatives.** A file is skipped ONLY when it has a sidecar
/// record whose `(len, mtime)` still matches the file on disk AND whose
/// bloom lacks one of the required trigrams. Any other case (no record,
/// stale record, un-keyable path, stat failure) reads the file. See
/// `docs/GREP-TRIGRAM.md`.
struct TrigramSkip {
    /// Trigrams the pattern's literal must contain (non-empty by
    /// construction — `required_trigrams` returns `None` for < 3 bytes).
    required: Vec<Trigram>,
    index: HashMap<String, TrigramRecord>,
}

impl TrigramSkip {
    /// Build the skip-index for `pattern`, or `None` when it can't help:
    /// the pattern has no ≥3-byte required literal, or the sidecar is
    /// absent / malformed (→ full walk, matching pre-index behaviour).
    fn build(root: &Path, pattern: &str) -> Option<Self> {
        let required = trigram::required_trigrams(pattern)?;
        if required.is_empty() {
            return None;
        }
        let records = store_trigram::load(&config::trigram_path(root)).ok()?;
        let index = records
            .into_iter()
            .map(|r| (r.rel_path.clone(), r))
            .collect();
        Some(TrigramSkip { required, index })
    }

    /// True iff `path` provably cannot match and may be left unread.
    /// `meta` is the stat the walk already performed for the size cap.
    fn can_skip(&self, path: &Path, root: &Path, meta: &std::fs::Metadata) -> bool {
        // Key must be derived exactly as the sidecar wrote it (POSIX rel),
        // else the lookup silently misses on Windows and every file reads.
        let Some(rel) = crate::util::paths::to_rel_posix(path, root) else {
            return false;
        };
        let Some(rec) = self.index.get(&rel) else {
            return false; // absent → read
        };
        // Staleness guard: grep runs without a reindex, so any drift in
        // (len, mtime) means the bloom may not reflect current content →
        // read, never skip.
        if rec.len != meta.len() {
            return false;
        }
        let Ok(mtime) = meta.modified() else {
            return false;
        };
        if (rec.mtime_secs, rec.mtime_nanos) != store_trigram::mtime_parts(mtime) {
            return false;
        }
        // Fresh + matching record: skip iff the bloom proves the required
        // literal cannot be present.
        !TrigramBloom::from_raw(rec.bloom).might_contain_all(&self.required)
    }
}

fn discover_files(
    root: &Path,
    filter_path: Option<&str>,
    excludes: &[String],
    skip: Option<&TrigramSkip>,
    force_text: bool,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for entry in crate::util::walk::walk_builder(root, excludes)?.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();

        // Optional path filter
        if let Some(fp) = filter_path {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if !rel.to_string_lossy().contains(fp) {
                continue;
            }
        }

        // Drop definitely-binary files by extension before we even stat or
        // open them — the cheapest possible skip (see `is_binary_ext`).
        // `force_text` (the `--text`/`-a` escape hatch) bypasses this.
        if !force_text && is_binary_ext(&path) {
            continue;
        }

        // Single stat, reused for both the 1 MB cap and the trigram skip.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() > 1_048_576 {
            continue;
        }

        // Trigram skip-index: drop files that provably can't match before
        // they're ever read.
        if let Some(skip) = skip {
            if skip.can_skip(&path, root, &meta) {
                continue;
            }
        }

        files.push(path);
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("main.py"),
            "def hello():\n    print('world')\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("config.py"),
            "TIMEOUT = '40 MINUTE'\nDEBUG = True\n",
        )
        .unwrap();
        fs::create_dir(dir.path().join("api")).unwrap();
        fs::write(
            dir.path().join("api/routes.py"),
            "def get_user():\n    return user\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn grep_finds_string_in_content() {
        let dir = setup_test_dir();
        let matches = search(dir.path(), "40 MINUTE", None, 50, &[], false).unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].path.contains("config.py"));
        assert_eq!(matches[0].line, 1);
    }

    #[test]
    fn grep_regex_pattern() {
        let dir = setup_test_dir();
        let matches = search(dir.path(), r"def \w+\(\)", None, 50, &[], false).unwrap();
        assert_eq!(matches.len(), 2); // hello() and get_user()
    }

    #[test]
    fn grep_with_path_filter() {
        let dir = setup_test_dir();
        let matches = search(dir.path(), "def", Some("api"), 50, &[], false).unwrap();
        assert_eq!(matches.len(), 1);
        // Path separators differ between Unix (`api/routes.py`) and
        // Windows (`api\routes.py`); assert on the directory name only.
        assert!(
            matches[0].path.contains("api"),
            "expected match path to contain `api`, got {:?}",
            matches[0].path
        );
    }

    #[test]
    fn grep_respects_limit() {
        let dir = setup_test_dir();
        let matches = search(dir.path(), ".", None, 2, &[], false).unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn grep_invalid_regex_returns_error() {
        let dir = setup_test_dir();
        assert!(search(dir.path(), "[invalid", None, 50, &[], false).is_err());
    }

    #[test]
    fn grep_skips_file_with_nul_in_first_8kb() {
        let dir = setup_test_dir();
        // Pattern bytes appear AFTER the NUL — must still be skipped, not
        // matched, since is_binary_bytes classifies from the 8 KB prefix.
        let mut content = vec![0u8; 100];
        content.extend_from_slice(b"\nneedle_after_nul\n");
        fs::write(dir.path().join("binary.dat"), &content).unwrap();

        let matches = search(dir.path(), "needle_after_nul", None, 50, &[], false).unwrap();
        assert!(
            matches.is_empty(),
            "expected NUL-containing file to be skipped, got {matches:?}"
        );
    }

    #[test]
    fn grep_matches_normal_utf8_source_file() {
        // Regression: the real hit still found for plain text files.
        let dir = setup_test_dir();
        let matches = search(dir.path(), "40 MINUTE", None, 50, &[], false).unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].path.contains("config.py"));
    }

    #[test]
    fn grep_skips_file_valid_utf8_prefix_then_invalid_later() {
        let dir = setup_test_dir();
        // First 8KB+ is clean ASCII text (passes is_binary_bytes), but the
        // file ends with an invalid UTF-8 byte sequence, so
        // String::from_utf8 fails and the file is skipped.
        let mut content = vec![b'a'; SNIFF_BYTES + 10];
        content.extend_from_slice(b"needle_marker");
        content.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8
        fs::write(dir.path().join("late_invalid.dat"), &content).unwrap();

        let matches = search(dir.path(), "needle_marker", None, 50, &[], false).unwrap();
        assert!(
            matches.is_empty(),
            "expected late-invalid-UTF-8 file to be skipped, got {matches:?}"
        );
    }

    #[test]
    fn grep_skips_file_with_high_control_byte_ratio() {
        let dir = setup_test_dir();
        // >5% control bytes (excluding \n \r \t) in the prefix, no NUL.
        let mut content = Vec::new();
        for _ in 0..100 {
            content.extend_from_slice(b"ab\x01\x02\x03cd"); // 3/8 control = 37.5%
        }
        content.extend_from_slice(b"needle_control\n");
        fs::write(dir.path().join("control.dat"), &content).unwrap();

        let matches = search(dir.path(), "needle_control", None, 50, &[], false).unwrap();
        assert!(
            matches.is_empty(),
            "expected high-control-ratio file to be skipped, got {matches:?}"
        );
    }

    #[test]
    fn is_binary_bytes_empty_slice_is_not_binary() {
        assert!(!is_binary_bytes(&[]));
    }

    #[test]
    fn is_binary_bytes_nul_is_binary() {
        assert!(is_binary_bytes(&[b'a', b'b', 0, b'c']));
    }

    #[test]
    fn is_binary_bytes_clean_ascii_is_not_binary() {
        assert!(!is_binary_bytes(
            b"def hello():\n    print('world')\r\n\t ok"
        ));
    }

    #[test]
    fn is_binary_bytes_six_percent_control_is_binary() {
        // 100 bytes total, 6 control bytes -> 6% > 5% threshold.
        let mut sample = vec![b'a'; 94];
        sample.extend_from_slice(&[0x01; 6]);
        assert_eq!(sample.len(), 100);
        assert!(is_binary_bytes(&sample));
    }

    #[test]
    fn grep_matches_text_file_with_hit_beyond_8kb() {
        // No-false-negative for TEXT: a clean-ASCII file larger than the
        // sniff window whose ONLY matching line sits past byte 8192 must
        // still be read to completion and found.
        let dir = setup_test_dir();
        let mut content = String::new();
        while content.len() <= SNIFF_BYTES + 4096 {
            content.push_str("// filler line of ordinary ascii source text\n");
        }
        content.push_str("let needle_past_sniff = 1;\n");
        fs::write(dir.path().join("big.rs"), &content).unwrap();

        let matches = search(dir.path(), "needle_past_sniff", None, 50, &[], false).unwrap();
        assert_eq!(
            matches.len(),
            1,
            "hit past the 8 KB sniff must still be found"
        );
        assert!(matches[0].path.contains("big.rs"));
    }

    #[test]
    fn grep_handles_file_exactly_sniff_bytes_long() {
        // Boundary: file length == SNIFF_BYTES. The sniff read consumes the
        // whole file; read_to_end then appends nothing. Must still match.
        let dir = setup_test_dir();
        let marker = "xyzzy_boundary";
        let mut content = vec![b'a'; SNIFF_BYTES - marker.len() - 1];
        content.extend_from_slice(marker.as_bytes());
        content.push(b'\n');
        assert_eq!(content.len(), SNIFF_BYTES);
        fs::write(dir.path().join("exact.rs"), &content).unwrap();

        let matches = search(dir.path(), marker, None, 50, &[], false).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn grep_skips_binary_extension_even_when_content_is_text() {
        // A .png whose bytes are actually plain ASCII must still be skipped
        // by the extension denylist — dropped in discover_files before open.
        let dir = setup_test_dir();
        fs::write(dir.path().join("image.png"), b"needle_in_png\n").unwrap();
        let matches = search(dir.path(), "needle_in_png", None, 50, &[], false).unwrap();
        assert!(
            matches.is_empty(),
            "expected .png to be skipped by extension, got {matches:?}"
        );
    }

    #[test]
    fn is_binary_ext_classifies_common_types() {
        use std::path::Path;
        for p in ["a.png", "b.XLSX", "c.ttf", "d.pyc", "e.zip", "f.mp4"] {
            assert!(is_binary_ext(Path::new(p)), "{p} should be binary");
        }
        // Text formats and source must NOT be denylisted. `.dat`/`.bin` are
        // deliberately excluded (ambiguous → left to the content sniff).
        for p in [
            "a.svg", "b.json", "c.csv", "d.txt", "e.rs", "f.py", "g", "h.scss", "i.dat", "j.bin",
        ] {
            assert!(!is_binary_ext(Path::new(p)), "{p} should NOT be binary");
        }
    }

    #[test]
    fn is_binary_ext_never_shadows_a_supported_source_extension() {
        use std::path::Path;
        // Guard against future drift: no extension vex parses as source may
        // appear in the binary denylist, or grep would silently skip real
        // code. Mirrors `Language::from_extension`.
        for ext in [
            "rs", "kt", "kts", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "cs",
            "rb", "swift", "sql", "md", "markdown", "cpp", "cc", "cxx", "hpp", "hxx", "h", "php",
            "phtml", "sh", "bash", "lua", "css", "html", "htm", "yaml", "yml", "toml",
        ] {
            assert!(
                crate::parse::language::Language::from_extension(ext).is_some(),
                "test list drifted: {ext} no longer a supported source ext"
            );
            assert!(
                !is_binary_ext(Path::new(&format!("f.{ext}"))),
                "supported source ext {ext} must not be in the binary denylist"
            );
        }
    }

    #[test]
    fn grep_skips_uppercase_binary_extension_end_to_end() {
        let dir = setup_test_dir();
        fs::write(dir.path().join("PHOTO.PNG"), b"needle_in_caps_png\n").unwrap();
        let matches = search(dir.path(), "needle_in_caps_png", None, 50, &[], false).unwrap();
        assert!(
            matches.is_empty(),
            "uppercase .PNG must be skipped, got {matches:?}"
        );
    }

    #[test]
    fn grep_handles_empty_file() {
        // Empty file: sniff reads 0 bytes, is_binary_bytes(&[]) is false,
        // read_to_end appends nothing → Some("") → zero matches, no panic.
        let dir = setup_test_dir();
        fs::write(dir.path().join("empty.rs"), b"").unwrap();
        let matches = search(dir.path(), "anything", None, 50, &[], false).unwrap();
        assert!(matches.iter().all(|m| !m.path.contains("empty.rs")));
    }

    #[test]
    fn grep_force_text_bypasses_binary_extension_denylist() {
        // A .png whose bytes are actually plain ASCII: denylisted by
        // extension, so the default (false) skips it, but force_text=true
        // must bypass discover_files's extension filter and find the match.
        let dir = setup_test_dir();
        fs::write(dir.path().join("image.png"), b"needle_in_png\n").unwrap();

        let matches = search(dir.path(), "needle_in_png", None, 50, &[], true).unwrap();
        assert_eq!(
            matches.len(),
            1,
            "force_text=true must read a .png with textual bytes, got {matches:?}"
        );
        assert!(matches[0].path.contains("image.png"));

        let matches = search(dir.path(), "needle_in_png", None, 50, &[], false).unwrap();
        assert!(
            matches.is_empty(),
            "default (force_text=false) must still skip .png by extension"
        );
    }

    #[test]
    fn grep_force_text_bypasses_nul_content_sniff() {
        // A NUL byte in the first 8 KB is valid UTF-8 (NUL is a legal code
        // point), so String::from_utf8 succeeds once the sniff is skipped;
        // the needle sits on a line after the NUL and must still be found.
        let dir = setup_test_dir();
        let mut content = vec![0u8; 100];
        content.extend_from_slice(b"\nneedle_after_nul_force\n");
        fs::write(dir.path().join("binary.dat"), &content).unwrap();

        let matches = search(dir.path(), "needle_after_nul_force", None, 50, &[], true).unwrap();
        assert_eq!(
            matches.len(),
            1,
            "force_text=true must read past a NUL prefix, got {matches:?}"
        );
        assert!(matches[0].path.contains("binary.dat"));

        let matches = search(dir.path(), "needle_after_nul_force", None, 50, &[], false).unwrap();
        assert!(
            matches.is_empty(),
            "default (force_text=false) must still skip the NUL-prefixed file"
        );
    }

    #[test]
    fn grep_force_text_still_skips_truly_invalid_utf8() {
        // force_text only removes the ext-denylist + content-sniff skips; a
        // file whose bytes are not valid UTF-8 at all still yields None from
        // String::from_utf8, matching the pre-binary-skip default (vex greps
        // line-by-line over a Rust String and cannot lossy-decode).
        let dir = setup_test_dir();
        let mut content = b"needle_invalid_utf8\n".to_vec();
        content.extend_from_slice(&[0xFF, 0xFE, 0xFA]); // invalid UTF-8
        fs::write(dir.path().join("truly_invalid.bin"), &content).unwrap();

        let matches = search(dir.path(), "needle_invalid_utf8", None, 50, &[], true).unwrap();
        assert!(
            matches.is_empty(),
            "force_text=true must still skip genuinely invalid UTF-8, got {matches:?}"
        );
    }
}
