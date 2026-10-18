use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};

const MAGIC: &[u8; 4] = b"VEXR";
// Bump when parser carry/reconstruction behavior changes the output of an
// existing index; a missing or old marker forces one migration update.
// Serialized `ParsedFile` shape changes instead bump `CACHE_FORMAT_VERSION`.
pub(super) const BASELINE: u16 = 1;
pub(super) const DART: u16 = 5;
pub(super) const CURRENT: u16 = DART;
pub(super) const FILE_NAME: &str = "index.parse-revision";

fn marker_bytes(revision: u16) -> [u8; 6] {
    let version = revision.to_le_bytes();
    [
        MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], version[0], version[1],
    ]
}

/// The marker versions parser carry semantics independently from cache payloads.
pub(super) fn required_for_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> u16 {
    paths.into_iter().fold(BASELINE, |required, path| {
        let language = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(crate::parse::language::Language::from_extension);
        if language == Some(crate::parse::language::Language::Dart) {
            DART
        } else {
            required
        }
    })
}

fn read_revision(index_dir: &Path) -> Option<u16> {
    let Ok(file) = File::open(index_dir.join(FILE_NAME)) else {
        return None;
    };
    let read_limit = 7;
    let mut bytes = Vec::with_capacity(read_limit);
    if file
        .take(read_limit as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return None;
    }
    if bytes.len() != 6 || &bytes[..4] != MAGIC {
        return None;
    }
    let revision = u16::from_le_bytes([bytes[4], bytes[5]]);
    (BASELINE..=CURRENT).contains(&revision).then_some(revision)
}

pub(super) fn is_current(index_dir: &Path, required: u16) -> bool {
    read_revision(index_dir).is_some_and(|revision| revision >= required)
}

pub(super) fn preserve_known_revision(index_dir: &Path, required: u16) -> u16 {
    read_revision(index_dir).unwrap_or(required).max(required)
}

/// Durably make the index stale before the first required output mutation.
pub(super) fn invalidate(index_dir: &Path) -> Result<()> {
    let path = index_dir.join(FILE_NAME);
    let marker = match OpenOptions::new().write(true).truncate(true).open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("invalidate {}", path.display())),
    };
    marker
        .sync_all()
        .with_context(|| format!("sync invalidated marker {}", path.display()))
}

/// Publish the current parser carry revision after all outputs succeed.
pub(super) fn save(index_dir: &Path, required: u16) -> Result<()> {
    fs::create_dir_all(index_dir)
        .with_context(|| format!("create index directory {}", index_dir.display()))?;
    let path = index_dir.join(FILE_NAME);
    let tmp = index_dir.join("index.parse-revision.tmp");
    let mut file = File::create(&tmp)
        .with_context(|| format!("create revision marker tmp {}", tmp.display()))?;
    let revision = read_revision(index_dir).unwrap_or(required).max(required);
    file.write_all(&marker_bytes(revision))
        .with_context(|| format!("write revision marker tmp {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("sync revision marker tmp {}", tmp.display()))?;
    // The same-directory rename atomically replaces the invalidated marker.
    fs::rename(&tmp, &path).with_context(|| format!("publish revision marker {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn marker_is_exact_and_invalidated_durably() {
        let tmp = TempDir::new().unwrap();
        save(tmp.path(), BASELINE).unwrap();
        assert_eq!(
            fs::read(tmp.path().join(FILE_NAME)).unwrap(),
            b"VEXR\x01\x00"
        );
        assert!(is_current(tmp.path(), BASELINE));
        invalidate(tmp.path()).unwrap();
        assert!(!is_current(tmp.path(), BASELINE));
        save(tmp.path(), BASELINE).unwrap();
        assert!(is_current(tmp.path(), BASELINE));
    }

    #[test]
    fn malformed_marker_lengths_and_bytes_are_not_current() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(FILE_NAME);
        assert!(!is_current(tmp.path(), BASELINE), "missing marker is stale");

        let expected = marker_bytes(BASELINE);
        let mut trailing = expected.to_vec();
        trailing.push(0);
        let cases: [&[u8]; 3] = [&expected[..5], b"BADR\x01\x00", &trailing];
        for bytes in cases {
            fs::write(&path, bytes).unwrap();
            assert!(
                !is_current(tmp.path(), BASELINE),
                "invalid marker: {bytes:?}"
            );
        }
    }

    #[test]
    fn oversized_marker_is_rejected_after_a_bounded_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(FILE_NAME);
        let mut file = File::create(path).unwrap();
        file.write_all(&marker_bytes(BASELINE)).unwrap();
        file.set_len(1024 * 1024).unwrap();
        assert!(!is_current(tmp.path(), BASELINE));
    }

    #[test]
    fn failed_marker_rename_keeps_stale_marker_and_temp_for_retry() {
        let tmp = TempDir::new().unwrap();
        save(tmp.path(), BASELINE).unwrap();
        invalidate(tmp.path()).unwrap();

        let marker = tmp.path().join(FILE_NAME);
        fs::remove_file(&marker).unwrap();
        fs::create_dir(&marker).unwrap();
        let tmp_marker = tmp.path().join("index.parse-revision.tmp");

        assert!(save(tmp.path(), BASELINE).is_err());
        assert!(!is_current(tmp.path(), BASELINE));
        assert!(tmp_marker.is_file());

        fs::remove_dir(&marker).unwrap();
        save(tmp.path(), BASELINE).unwrap();
        assert!(is_current(tmp.path(), BASELINE));
        assert!(!tmp_marker.exists());
    }

    #[test]
    fn dart_revision_is_path_scoped_and_marker_versions_are_monotonic() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(required_for_paths(["src/lib.rs"].iter().copied()), BASELINE);
        assert_eq!(required_for_paths(["lib.dart"].iter().copied()), DART);
        assert_eq!(
            required_for_paths(["lib.rs", "lib.dart"].iter().copied()),
            DART
        );

        save(tmp.path(), BASELINE).unwrap();
        assert!(is_current(tmp.path(), BASELINE));
        assert!(!is_current(tmp.path(), DART));

        save(tmp.path(), DART).unwrap();
        assert_eq!(
            fs::read(tmp.path().join(FILE_NAME)).unwrap(),
            b"VEXR\x05\x00"
        );
        assert!(is_current(tmp.path(), DART));
        assert!(is_current(tmp.path(), BASELINE));

        save(tmp.path(), BASELINE).unwrap();
        assert!(
            is_current(tmp.path(), DART),
            "known newer marker stays current"
        );
    }

    #[test]
    fn marker_revision_above_supported_range_is_stale() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(FILE_NAME), marker_bytes(CURRENT + 1)).unwrap();
        assert!(!is_current(tmp.path(), BASELINE));
    }

    #[test]
    fn previous_dart_revision_requires_refresh_but_stays_known_for_native_paths() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(FILE_NAME), b"VEXR\x02\x00").unwrap();
        assert!(is_current(tmp.path(), BASELINE));
        assert!(!is_current(tmp.path(), DART));
        save(tmp.path(), BASELINE).unwrap();
        assert_eq!(
            fs::read(tmp.path().join(FILE_NAME)).unwrap(),
            b"VEXR\x02\x00"
        );
        save(tmp.path(), DART).unwrap();
        assert_eq!(
            fs::read(tmp.path().join(FILE_NAME)).unwrap(),
            b"VEXR\x05\x00"
        );
    }
}
