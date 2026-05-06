use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{ensure, Result};

use super::format::{Header, SymbolRecord, MAGIC, VECTOR_DIM, VERSION};
use super::refs_fst;
use crate::index::symbols::ParsedFile;

/// String pool that deduplicates strings and returns offsets.
struct StringPool {
    data: Vec<u8>,
    lookup: HashMap<String, u32>,
}

impl StringPool {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            lookup: HashMap::new(),
        }
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&offset) = self.lookup.get(s) {
            return offset;
        }
        let offset = self.data.len() as u32;
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0); // null terminator
        self.lookup.insert(s.to_string(), offset);
        offset
    }
}

/// Write parsed files into the binary index format (no vectors, no refs FST).
#[allow(dead_code)] // used by integration tests
pub fn write_index(parsed: &[ParsedFile], output: &Path) -> Result<()> {
    write_index_full(parsed, &[], output)
}

/// Write parsed files + embedding vectors + refs FST into the binary index format.
pub fn write_index_full(parsed: &[ParsedFile], vectors: &[Vec<f32>], output: &Path) -> Result<()> {
    let mut strings = StringPool::new();
    let mut records = Vec::new();
    let mut symbol_idx: u32 = 0;

    // Collect file_id mapping for refs: file path → sequential id
    let mut file_ids: HashMap<String, u32> = HashMap::new();
    let mut next_file_id: u32 = 0;

    for file in parsed {
        let file_offset = strings.intern(&file.path);
        let _file_id = *file_ids.entry(file.path.clone()).or_insert_with(|| {
            let id = next_file_id;
            next_file_id += 1;
            id
        });

        for sym in &file.symbols {
            let name_offset = strings.intern(&sym.name);
            let sig_offset = sym
                .signature
                .as_deref()
                .map(|s| strings.intern(s))
                .unwrap_or(u32::MAX);

            let vec_idx = if !vectors.is_empty() && (symbol_idx as usize) < vectors.len() {
                symbol_idx
            } else {
                u32::MAX
            };

            records.push(SymbolRecord {
                name_offset,
                kind: sym.kind as u8,
                _pad: [0; 3],
                file_offset,
                line: sym.line as u32,
                signature_offset: sig_offset,
                vector_index: vec_idx,
            });
            symbol_idx += 1;
        }
    }

    // Build FST + posting lists from refs
    let refs_input: Vec<(u32, &[crate::index::symbols::ParsedRef])> = parsed
        .iter()
        .map(|file| {
            let file_id = file_ids.get(&file.path).copied().unwrap_or(0);
            (file_id, file.refs.as_slice())
        })
        .collect();

    let (fst_bytes, posting_bytes) = refs_fst::build_refs_fst(&refs_input)?;

    // Calculate section offsets
    let symbols_offset = Header::SIZE as u64;
    let symbols_size = records.len() * SymbolRecord::SIZE;

    let vectors_offset = symbols_offset + symbols_size as u64;
    let vectors_size = if vectors.is_empty() {
        0
    } else {
        vectors.len() * VECTOR_DIM as usize * std::mem::size_of::<f32>()
    };

    let strings_offset = vectors_offset + vectors_size as u64;
    let fst_offset = strings_offset + strings.data.len() as u64;
    let postings_offset = fst_offset + fst_bytes.len() as u64;

    let header = Header {
        magic: *MAGIC,
        version: VERSION,
        symbol_count: records.len() as u64,
        vector_dim: VECTOR_DIM,
        _padding: 0,
        symbols_offset,
        vectors_offset,
        strings_offset,
        inverted_offset: 0,
        hnsw_offset: 0,
        fst_offset,
        fst_len: fst_bytes.len() as u64,
        postings_offset,
        postings_len: posting_bytes.len() as u64,
    };

    let file = std::fs::File::create(output)?;
    let mut w = BufWriter::new(file);

    // SAFETY: Header is #[repr(C)] with fixed layout, no padding issues on same arch
    let header_bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(&header as *const Header as *const u8, Header::SIZE) };
    w.write_all(header_bytes)?;

    // Write symbol records
    for rec in &records {
        // SAFETY: SymbolRecord is #[repr(C)] with fixed layout
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(rec as *const SymbolRecord as *const u8, SymbolRecord::SIZE)
        };
        w.write_all(bytes)?;
    }

    // Write vectors
    for (i, vec) in vectors.iter().enumerate() {
        ensure!(
            vec.len() == VECTOR_DIM as usize,
            "vector {i} has wrong dimension: expected {VECTOR_DIM}, got {}",
            vec.len()
        );
        // SAFETY: vec is a valid &[f32] with known length
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                vec.as_ptr() as *const u8,
                vec.len() * std::mem::size_of::<f32>(),
            )
        };
        w.write_all(bytes)?;
    }

    // Write strings
    w.write_all(&strings.data)?;

    // Write FST + postings
    w.write_all(&fst_bytes)?;
    w.write_all(&posting_bytes)?;

    w.flush()?;

    let ref_count: usize = parsed.iter().map(|f| f.refs.len()).sum();
    tracing::info!(
        symbols = records.len(),
        refs = ref_count,
        vectors = vectors.len(),
        fst_bytes = fst_bytes.len(),
        "index written to {:?}",
        output
    );

    Ok(())
}
