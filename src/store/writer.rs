use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{ensure, Result};

use super::format::{Header, SymbolRecord, MAGIC, VECTOR_DIM, VERSION};
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

/// Write parsed files into the binary index format, optionally with embeddings.
#[allow(dead_code)] // used by integration tests
pub fn write_index(parsed: &[ParsedFile], output: &Path) -> Result<()> {
    write_index_with_vectors(parsed, &[], output)
}

/// Write parsed files + embedding vectors into the binary index format.
/// `vectors` is a flat list of f32[384] vectors, one per symbol (in order).
/// If empty, the vectors section is skipped.
pub fn write_index_with_vectors(
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    output: &Path,
) -> Result<()> {
    let mut strings = StringPool::new();
    let mut records = Vec::new();
    let mut symbol_idx: u32 = 0;

    for file in parsed {
        let file_offset = strings.intern(&file.path);
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

    let symbols_offset = Header::SIZE as u64;
    let symbols_size = records.len() * SymbolRecord::SIZE;

    let vectors_offset = symbols_offset + symbols_size as u64;
    let vectors_size = if vectors.is_empty() {
        0
    } else {
        vectors.len() * VECTOR_DIM as usize * std::mem::size_of::<f32>()
    };

    let strings_offset = vectors_offset + vectors_size as u64;
    let inverted_offset = strings_offset + strings.data.len() as u64;

    let header = Header {
        magic: *MAGIC,
        version: VERSION,
        symbol_count: records.len() as u64,
        vector_dim: VECTOR_DIM,
        _padding: 0,
        symbols_offset,
        vectors_offset,
        strings_offset,
        inverted_offset,
        hnsw_offset: 0,
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

    // Write vectors (dense f32 arrays, each must be exactly VECTOR_DIM elements)
    for (i, vec) in vectors.iter().enumerate() {
        ensure!(
            vec.len() == VECTOR_DIM as usize,
            "vector {i} has wrong dimension: expected {VECTOR_DIM}, got {}",
            vec.len()
        );
        // SAFETY: vec is a valid &[f32] with known length. f32 has no invalid bit patterns.
        // The resulting byte slice has the same lifetime as `vec`.
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

    w.flush()?;

    tracing::info!(
        symbols = records.len(),
        vectors = vectors.len(),
        strings = strings.lookup.len(),
        "index written to {:?}",
        output
    );

    Ok(())
}
