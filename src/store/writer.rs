use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::Result;

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

/// Write parsed files into the binary index format.
pub fn write_index(parsed: &[ParsedFile], output: &Path) -> Result<()> {
    let mut strings = StringPool::new();
    let mut records = Vec::new();

    for file in parsed {
        let file_offset = strings.intern(&file.path);
        for sym in &file.symbols {
            let name_offset = strings.intern(&sym.name);
            let sig_offset = sym
                .signature
                .as_deref()
                .map(|s| strings.intern(s))
                .unwrap_or(u32::MAX);

            records.push(SymbolRecord {
                name_offset,
                kind: sym.kind as u8,
                _pad: [0; 3],
                file_offset,
                line: sym.line as u32,
                signature_offset: sig_offset,
                vector_index: u32::MAX, // no embeddings yet
            });
        }
    }

    let symbols_offset = Header::SIZE as u64;
    let symbols_size = records.len() * SymbolRecord::SIZE;
    let vectors_offset = symbols_offset + symbols_size as u64;
    let strings_offset = vectors_offset; // no vectors yet
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

    // Write header
    let header_bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(&header as *const Header as *const u8, Header::SIZE) };
    w.write_all(header_bytes)?;

    // Write symbol records
    for rec in &records {
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(rec as *const SymbolRecord as *const u8, SymbolRecord::SIZE)
        };
        w.write_all(bytes)?;
    }

    // Write strings
    w.write_all(&strings.data)?;

    w.flush()?;

    tracing::info!(
        symbols = records.len(),
        strings = strings.lookup.len(),
        "index written to {:?}",
        output
    );

    Ok(())
}
