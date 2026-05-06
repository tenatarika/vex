//! FST-based refs storage: symbol name → posting list of (file_id, line) pairs.
//!
//! The FST maps each unique ref name to a u64 offset into the posting lists section.
//! Posting lists store variable-length sequences of (file_id: u32, line: u32) pairs,
//! prefixed by a u32 count.
//!
//! Layout in the binary index:
//! ```text
//! [FST bytes]         — fst::Map serialized data
//! [Posting lists]     — packed (count: u32, [(file_id: u32, line: u32); count])
//! ```

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::index::symbols::ParsedRef;

/// A single ref occurrence: which file and line.
#[derive(Debug, Clone, Copy)]
pub struct RefEntry {
    pub file_id: u32,
    pub line: u32,
}

/// Build FST + posting lists from parsed refs.
/// Returns (fst_bytes, posting_bytes).
pub fn build_refs_fst(
    parsed_files: &[(u32, &[ParsedRef])], // (file_id, refs)
) -> Result<(Vec<u8>, Vec<u8>)> {
    // Collect all refs grouped by name, sorted
    let mut refs_by_name: BTreeMap<String, Vec<RefEntry>> = BTreeMap::new();

    for &(file_id, refs) in parsed_files {
        for r in refs {
            refs_by_name
                .entry(r.name.clone())
                .or_default()
                .push(RefEntry {
                    file_id,
                    line: r.line as u32,
                });
        }
    }

    // Build posting lists: each name gets a contiguous block of (file_id, line) pairs
    let capacity: usize = refs_by_name.values().map(|v| 4 + v.len() * 8).sum();
    let mut posting_data: Vec<u8> = Vec::with_capacity(capacity);
    let mut fst_builder = fst::MapBuilder::memory();

    for (name, entries) in &refs_by_name {
        let offset = posting_data.len() as u64;

        // Write count
        let count = entries.len() as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());

        // Write entries
        for entry in entries {
            posting_data.extend_from_slice(&entry.file_id.to_le_bytes());
            posting_data.extend_from_slice(&entry.line.to_le_bytes());
        }

        fst_builder
            .insert(name.as_bytes(), offset)
            .context("fst insert")?;
    }

    let fst_bytes = fst_builder.into_inner().context("finalize fst")?;

    Ok((fst_bytes, posting_data))
}

/// Read refs from mmap'd data. Zero-copy FST lookup.
pub struct RefReader<'a> {
    fst_map: fst::Map<&'a [u8]>,
    posting_data: &'a [u8],
}

impl<'a> RefReader<'a> {
    /// Create a RefReader from raw byte slices (from mmap).
    pub fn new(fst_bytes: &'a [u8], posting_bytes: &'a [u8]) -> Result<Self> {
        let fst_map = fst::Map::new(fst_bytes).map_err(|e| anyhow::anyhow!("fst load: {e}"))?;
        Ok(Self {
            fst_map,
            posting_data: posting_bytes,
        })
    }

    /// Exact lookup: find all refs for a symbol name.
    pub fn find(&self, name: &str) -> Vec<RefEntry> {
        match self.fst_map.get(name.as_bytes()) {
            Some(offset) => self.read_posting_list(offset),
            None => Vec::new(),
        }
    }

    /// Prefix search: find all ref names starting with `prefix` and their entries.
    pub fn find_by_prefix(&self, prefix: &str) -> Vec<(String, Vec<RefEntry>)> {
        use fst::automaton::{Automaton, Str};
        use fst::{IntoStreamer, Streamer};

        let automaton = Str::new(prefix).starts_with();
        let mut stream = self.fst_map.search(automaton).into_stream();
        let mut results = Vec::new();

        while let Some((key, offset)) = stream.next() {
            let name = std::str::from_utf8(key).unwrap_or("").to_owned();
            let entries = self.read_posting_list(offset);
            results.push((name, entries));
        }

        results
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.fst_map.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.fst_map.is_empty()
    }

    fn read_posting_list(&self, offset: u64) -> Vec<RefEntry> {
        let offset = offset as usize;
        if offset + 4 > self.posting_data.len() {
            tracing::warn!(offset, "posting list offset out of bounds");
            return Vec::new();
        }

        let count = u32::from_le_bytes(
            self.posting_data[offset..offset + 4]
                .try_into()
                .unwrap_or([0; 4]),
        ) as usize;

        let entry_size = 8; // file_id(4) + line(4)
        let data_start = offset + 4;

        // Overflow-safe bounds check
        let data_end = match count
            .checked_mul(entry_size)
            .and_then(|n| data_start.checked_add(n))
        {
            Some(end) => end,
            None => {
                tracing::warn!(count, "posting list count overflow");
                return Vec::new();
            }
        };

        if data_end > self.posting_data.len() {
            tracing::warn!(
                count,
                data_end,
                len = self.posting_data.len(),
                "posting list truncated"
            );
            return Vec::new();
        }

        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let base = data_start + i * entry_size;
            let file_id = u32::from_le_bytes(
                self.posting_data[base..base + 4]
                    .try_into()
                    .unwrap_or([0; 4]),
            );
            let line = u32::from_le_bytes(
                self.posting_data[base + 4..base + 8]
                    .try_into()
                    .unwrap_or([0; 4]),
            );
            entries.push(RefEntry { file_id, line });
        }

        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_refs() -> Vec<ParsedRef> {
        vec![
            ParsedRef {
                name: "PaymentService".into(),
                line: 10,
                context: None,
            },
            ParsedRef {
                name: "PaymentService".into(),
                line: 25,
                context: None,
            },
            ParsedRef {
                name: "UserRepo".into(),
                line: 5,
                context: None,
            },
            ParsedRef {
                name: "PaymentGateway".into(),
                line: 15,
                context: None,
            },
        ]
    }

    #[test]
    fn roundtrip_build_and_find() {
        let refs = make_refs();
        let files = vec![(0u32, refs.as_slice())];

        let (fst_bytes, posting_bytes) = build_refs_fst(&files).unwrap();
        let reader = RefReader::new(&fst_bytes, &posting_bytes).unwrap();

        let results = reader.find("PaymentService");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].line, 10);
        assert_eq!(results[1].line, 25);

        let results = reader.find("UserRepo");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line, 5);
    }

    #[test]
    fn prefix_search() {
        let refs = make_refs();
        let files = vec![(0u32, refs.as_slice())];

        let (fst_bytes, posting_bytes) = build_refs_fst(&files).unwrap();
        let reader = RefReader::new(&fst_bytes, &posting_bytes).unwrap();

        let results = reader.find_by_prefix("Payment");
        assert_eq!(results.len(), 2); // PaymentGateway, PaymentService
        let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"PaymentService"));
        assert!(names.contains(&"PaymentGateway"));
    }

    #[test]
    fn missing_name_returns_empty() {
        let refs = make_refs();
        let files = vec![(0u32, refs.as_slice())];

        let (fst_bytes, posting_bytes) = build_refs_fst(&files).unwrap();
        let reader = RefReader::new(&fst_bytes, &posting_bytes).unwrap();

        assert!(reader.find("NonExistent").is_empty());
    }

    #[test]
    fn multiple_files() {
        let refs1 = vec![ParsedRef {
            name: "Config".into(),
            line: 1,
            context: None,
        }];
        let refs2 = vec![ParsedRef {
            name: "Config".into(),
            line: 42,
            context: None,
        }];

        let files = vec![(0u32, refs1.as_slice()), (1u32, refs2.as_slice())];
        let (fst_bytes, posting_bytes) = build_refs_fst(&files).unwrap();
        let reader = RefReader::new(&fst_bytes, &posting_bytes).unwrap();

        let results = reader.find("Config");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].file_id, 0);
        assert_eq!(results[1].file_id, 1);
    }

    #[test]
    fn len_counts_unique_names() {
        let refs = make_refs();
        let files = vec![(0u32, refs.as_slice())];

        let (fst_bytes, posting_bytes) = build_refs_fst(&files).unwrap();
        let reader = RefReader::new(&fst_bytes, &posting_bytes).unwrap();

        assert_eq!(reader.len(), 3); // PaymentGateway, PaymentService, UserRepo
    }
}
