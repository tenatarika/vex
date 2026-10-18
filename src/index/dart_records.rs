use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::index::symbols::{ParsedRef, RawCallEdge};

const MAGIC: &[u8; 4] = b"VXDR";
const VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DartDeclaration {
    pub owner: String,
    pub member: String,
    pub kind: DartDeclarationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DartDeclarationKind {
    StaticMethod,
    NamedConstructor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DartRecord {
    pub source_fingerprint: u64,
    pub declarations: Vec<DartDeclaration>,
    pub raw_refs: Vec<ParsedRef>,
    pub raw_calls: Vec<RawCallEdge>,
}

pub(crate) type DartRecords = BTreeMap<String, DartRecord>;

pub(crate) fn sidecar_path(index_dir: &Path) -> PathBuf {
    index_dir.join("index.dart-records")
}

pub(crate) fn encode(records: &DartRecords) -> Result<Vec<u8>> {
    let payload = bincode::serialize(records).context("serialize Dart records")?;
    let wire_len =
        u32::try_from(payload.len()).context("Dart records payload exceeds wire length")?;
    let mut bytes = Vec::with_capacity(10 + payload.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&wire_len.to_le_bytes());
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

pub(crate) fn decode(path: &Path) -> Result<DartRecords> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    anyhow::ensure!(
        file.metadata()?.len() <= u32::MAX as u64 + 10,
        "Dart records exceed wire length"
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() >= 10 && bytes.len() as u64 <= u32::MAX as u64 + 10,
        "invalid Dart records size"
    );
    anyhow::ensure!(&bytes[..4] == MAGIC, "invalid Dart records magic");
    anyhow::ensure!(
        u16::from_le_bytes([bytes[4], bytes[5]]) == VERSION,
        "unsupported Dart records version"
    );
    let len = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]) as usize;
    anyhow::ensure!(
        len == bytes.len() - 10,
        "invalid Dart records payload length"
    );
    use bincode::Options;
    let payload = &bytes[10..];
    let records: DartRecords = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(payload.len() as u64)
        .deserialize(payload)
        .context("decode Dart records")?;
    anyhow::ensure!(
        records.keys().all(|path| {
            let path = Path::new(path);
            !path.is_absolute()
                && path
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_)))
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("dart"))
        }),
        "invalid path in Dart records"
    );
    Ok(records)
}

pub(crate) fn write_atomic(path: &Path, records: &DartRecords) -> Result<()> {
    let bytes = encode(records)?;
    let parent = path.parent().context("Dart records path has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!("index.dart-records.tmp.{}", std::process::id()));
    let mut file = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("write {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("publish {}", path.display()))
}

pub(crate) fn covers_manifest(
    index_path: &Path,
    manifest: &std::collections::HashMap<String, u64>,
) -> bool {
    let Ok(reader) = crate::store::reader::IndexReader::open(index_path) else {
        return false;
    };
    let expected: std::collections::BTreeSet<String> = reader
        .file_paths()
        .into_iter()
        .filter(|path| {
            Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("dart"))
        })
        .collect();
    if expected.is_empty() {
        return true;
    }
    let path = sidecar_path(index_path.parent().unwrap_or_else(|| Path::new(".")));
    decode(&path).is_ok_and(|records| {
        expected.iter().all(|key| {
            records
                .get(key)
                .zip(manifest.get(key))
                .is_some_and(|(record, hash)| record.source_fingerprint == *hash)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_is_deterministic_and_round_trips() {
        let records = DartRecords::new();
        let first = encode(&records).unwrap();
        assert_eq!(first, encode(&records).unwrap());
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("records");
        fs::write(&path, first).unwrap();
        assert!(decode(&path).unwrap().is_empty());
    }

    #[test]
    fn decoder_rejects_short_headers_bad_magic_version_and_length() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("records");
        for bytes in [
            vec![],
            b"VXDR".to_vec(),
            b"BAD!\x01\x00\x00\x00\x00\x00".to_vec(),
            b"VXDR\x02\x00\x00\x00\x00\x00".to_vec(),
            b"VXDR\x01\x00\x01\x00\x00\x00".to_vec(),
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(decode(&path).is_err());
        }
    }

    #[test]
    fn decoder_bounds_bincode_collection_lengths_by_payload() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("records");
        let payload = u64::MAX.to_le_bytes();
        let mut bytes = Vec::from(&b"VXDR\x01\x00"[..]);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
        fs::write(&path, bytes).unwrap();
        assert!(decode(&path).is_err());
    }

    #[test]
    fn decoder_rejects_non_relative_or_non_dart_record_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("records");
        for key in ["../lib.dart", "/tmp/lib.dart", "src/lib.rs"] {
            let mut records = DartRecords::new();
            records.insert(
                key.to_string(),
                DartRecord {
                    source_fingerprint: 0,
                    declarations: Vec::new(),
                    raw_refs: Vec::new(),
                    raw_calls: Vec::new(),
                },
            );
            fs::write(&path, encode(&records).unwrap()).unwrap();
            assert!(decode(&path).is_err(), "accepted {key}");
        }
    }
}
