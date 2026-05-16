use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};

use super::call_graph::{build_callees_fst, build_callers_fst, CallEdgeBuilder};
use super::format::{CallEdge, CallGraphHeader, Header, SymbolRecord, MAGIC, VECTOR_DIM, VERSION};
use super::{refs_fst, symbol_fst};
use crate::index::symbols::ParsedFile;

/// Vector dimension to record in the Header when no vectors are written.
/// Stays at the legacy MiniLM-L6-v2 value so v3 readers that ignore the
/// field continue to see what they expect.
const DEFAULT_VECTOR_DIM: u32 = VECTOR_DIM;

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
    write_index_full(parsed, &[], DEFAULT_VECTOR_DIM, output)
}

/// Pre-built BM25 section bytes — `(fst, postings, stats)`. Passed in
/// already-serialised because building the BM25 index requires per-symbol
/// term bags that live in the pipeline, not the writer.
pub type Bm25Sections<'a> = (&'a [u8], &'a [u8], &'a [u8]);

/// Write parsed files + embedding vectors + refs FST into the binary index
/// format (no call graph, no BM25). For indexes with those sections use
/// [`write_index_with_call_graph`].
pub fn write_index_full(
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    vector_dim: u32,
    output: &Path,
) -> Result<()> {
    write_index_with_call_graph(parsed, vectors, vector_dim, &[], None, output)
}

/// Write parsed files + embedding vectors + refs FST + v4 sections
/// (call graph + optional BM25) into the binary index format. Uses atomic
/// write: writes to a temp file first, then renames on success.
pub fn write_index_with_call_graph(
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    vector_dim: u32,
    call_edges: &[CallEdgeBuilder],
    bm25: Option<Bm25Sections<'_>>,
    output: &Path,
) -> Result<()> {
    // Pre-validate every vector before opening the temp file. The header's
    // section offsets are computed from `vectors.len() * vector_dim`, so a
    // single bad vector slipping past would leave every downstream section
    // (strings, FST, postings, file table) pointing at the wrong bytes. We
    // must not write a single byte until all inputs are confirmed.
    for (i, vec) in vectors.iter().enumerate() {
        ensure!(
            vec.len() == vector_dim as usize,
            "vector {i} has wrong dimension: expected {vector_dim}, got {}",
            vec.len()
        );
    }

    let mut tmp_os = output.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp_path = PathBuf::from(tmp_os);

    if let Err(e) = write_index_to(&tmp_path, parsed, vectors, vector_dim, call_edges, bm25) {
        let _ = std::fs::remove_file(&tmp_path); // best-effort cleanup
        return Err(e);
    }
    std::fs::rename(&tmp_path, output)
        .with_context(|| format!("rename {} → {}", tmp_path.display(), output.display()))?;
    Ok(())
}

fn write_index_to(
    output: &Path,
    parsed: &[ParsedFile],
    vectors: &[Vec<f32>],
    vector_dim: u32,
    call_edges: &[CallEdgeBuilder],
    bm25: Option<Bm25Sections<'_>>,
) -> Result<()> {
    let mut strings = StringPool::new();
    let mut records = Vec::new();
    let mut symbol_idx: u32 = 0;

    // Assign file_id sequentially per unique path. Collect ordered file table.
    let mut file_ids: HashMap<String, u32> = HashMap::new();
    let mut file_table: Vec<u32> = Vec::new(); // string offsets ordered by file_id

    for file in parsed {
        let str_offset = strings.intern(&file.path);
        file_ids.entry(file.path.clone()).or_insert_with(|| {
            let id = file_table.len() as u32;
            file_table.push(str_offset);
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
                file_offset: str_offset,
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
            let file_id = file_ids
                .get(&file.path)
                .copied()
                .expect("file_id must exist after prior loop");
            (file_id, file.refs.as_slice())
        })
        .collect();

    let (fst_bytes, posting_bytes) = refs_fst::build_refs_fst(&refs_input)?;

    // Build symbol FST: name + CamelCase sub-tokens → symbol indices
    let sym_entries: Vec<(String, u32)> = {
        let mut entries = Vec::new();
        let mut idx: u32 = 0;
        for file in parsed {
            for sym in &file.symbols {
                entries.push((sym.name.clone(), idx));
                idx += 1;
            }
        }
        entries
    };
    let (sym_fst_bytes, sym_posting_bytes) = symbol_fst::build_symbol_fst(&sym_entries)?;

    // Build call graph: intern callee names into the string pool, then
    // construct CallEdge records (resolved to string offsets) and the two
    // FSTs over those edges.
    let edge_records: Vec<CallEdge> = call_edges
        .iter()
        .map(|e| CallEdge {
            caller_sym_idx: e.caller_sym_idx,
            callee_name_offset: strings.intern(&e.callee_name),
            line: e.line,
            _pad: 0,
        })
        .collect();
    let (callers_fst_bytes, callers_post_bytes) = build_callers_fst(call_edges)?;
    let (callees_fst_bytes, callees_post_bytes) = build_callees_fst(call_edges)?;

    // Calculate section offsets — v4 places the CallGraphHeader immediately
    // after the base Header, so Symbols starts at Header::SIZE + CallGraphHeader::SIZE.
    let cg_header_offset = Header::SIZE as u64;
    let symbols_offset = cg_header_offset + CallGraphHeader::SIZE as u64;
    let symbols_size = records.len() * SymbolRecord::SIZE;

    let vectors_offset = symbols_offset + symbols_size as u64;
    let vectors_size = if vectors.is_empty() {
        0
    } else {
        vectors.len() * vector_dim as usize * std::mem::size_of::<f32>()
    };

    let strings_offset = vectors_offset + vectors_size as u64;
    let fst_offset = strings_offset + strings.data.len() as u64;
    let postings_offset = fst_offset + fst_bytes.len() as u64;
    let file_table_offset = postings_offset + posting_bytes.len() as u64;
    let file_table_size = file_table.len() * 4;
    let sym_fst_offset = file_table_offset + file_table_size as u64;
    let sym_postings_offset = sym_fst_offset + sym_fst_bytes.len() as u64;

    // Call graph sections come after the v3 sections. Align to 4 bytes so
    // that CallEdge (align_of == 4) can be cast directly from the mmap bytes.
    let call_edges_unaligned = sym_postings_offset + sym_posting_bytes.len() as u64;
    let call_edges_offset = (call_edges_unaligned + 3) & !3u64; // round up to 4-byte boundary
    let _call_edges_pad = (call_edges_offset - call_edges_unaligned) as usize;
    let call_edges_len = (edge_records.len() * CallEdge::SIZE) as u64;
    let callers_fst_offset = call_edges_offset + call_edges_len;
    let callers_postings_offset = callers_fst_offset + callers_fst_bytes.len() as u64;
    let callees_fst_offset = callers_postings_offset + callers_post_bytes.len() as u64;
    let callees_postings_offset = callees_fst_offset + callees_fst_bytes.len() as u64;

    // BM25 sections come after callees postings. No alignment requirement —
    // they're variable-length byte blobs (FST + posting + stats).
    let (bm25_fst, bm25_posts, bm25_stats): (&[u8], &[u8], &[u8]) = bm25.unwrap_or((&[], &[], &[]));
    let bm25_fst_offset = callees_postings_offset + callees_post_bytes.len() as u64;
    let bm25_postings_offset = bm25_fst_offset + bm25_fst.len() as u64;
    let bm25_stats_offset = bm25_postings_offset + bm25_posts.len() as u64;

    let call_graph_header = CallGraphHeader {
        call_edges_offset,
        call_edges_len,
        callers_fst_offset,
        callers_fst_len: callers_fst_bytes.len() as u64,
        callers_postings_offset,
        callers_postings_len: callers_post_bytes.len() as u64,
        callees_fst_offset,
        callees_fst_len: callees_fst_bytes.len() as u64,
        callees_postings_offset,
        callees_postings_len: callees_post_bytes.len() as u64,
        bm25_fst_offset,
        bm25_fst_len: bm25_fst.len() as u64,
        bm25_postings_offset,
        bm25_postings_len: bm25_posts.len() as u64,
        bm25_stats_offset,
        bm25_stats_len: bm25_stats.len() as u64,
    };

    let header = Header {
        magic: *MAGIC,
        version: VERSION,
        symbol_count: records.len() as u64,
        vector_dim,
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
        file_table_offset,
        file_table_count: file_table.len() as u32,
        _padding2: 0,
        sym_fst_offset,
        sym_fst_len: sym_fst_bytes.len() as u64,
        sym_postings_offset,
        sym_postings_len: sym_posting_bytes.len() as u64,
    };

    let file = std::fs::File::create(output)?;
    let mut w = BufWriter::new(file);

    // SAFETY: Header is #[repr(C)] with fixed layout, no padding issues on same arch
    let header_bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(&header as *const Header as *const u8, Header::SIZE) };
    w.write_all(header_bytes)?;

    // v4: CallGraphHeader immediately after the base header.
    // SAFETY: CallGraphHeader is #[repr(C)] with fixed layout.
    let cg_header_bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(
            &call_graph_header as *const CallGraphHeader as *const u8,
            CallGraphHeader::SIZE,
        )
    };
    w.write_all(cg_header_bytes)?;

    for rec in &records {
        // SAFETY: SymbolRecord is #[repr(C)] with fixed layout
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(rec as *const SymbolRecord as *const u8, SymbolRecord::SIZE)
        };
        w.write_all(bytes)?;
    }

    for vec in vectors.iter() {
        // Length was pre-validated in `write_index_full` before this fn ran.
        debug_assert_eq!(vec.len(), vector_dim as usize);
        // SAFETY: vec is a valid &[f32] with known length
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                vec.as_ptr() as *const u8,
                vec.len() * std::mem::size_of::<f32>(),
            )
        };
        w.write_all(bytes)?;
    }

    w.write_all(&strings.data)?;
    w.write_all(&fst_bytes)?;
    w.write_all(&posting_bytes)?;

    // Write file table
    for &str_offset in &file_table {
        w.write_all(&str_offset.to_le_bytes())?;
    }

    // Write symbol FST + postings
    w.write_all(&sym_fst_bytes)?;
    w.write_all(&sym_posting_bytes)?;

    // Pad to 4-byte alignment before call-graph sections so that CallEdge
    // records (align_of == 4) can be safely cast from the mmap pointer.
    if _call_edges_pad > 0 {
        w.write_all(&[0u8; 3][.._call_edges_pad])?;
    }

    // v4 call-graph sections: edge records + 2 FSTs + 2 posting lists.
    for rec in &edge_records {
        // SAFETY: CallEdge is #[repr(C)] with fixed layout.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(rec as *const CallEdge as *const u8, CallEdge::SIZE)
        };
        w.write_all(bytes)?;
    }
    w.write_all(&callers_fst_bytes)?;
    w.write_all(&callers_post_bytes)?;
    w.write_all(&callees_fst_bytes)?;
    w.write_all(&callees_post_bytes)?;

    // v4 BM25 sections (may be empty slices, which is the right behaviour
    // when bm25 == None — writes nothing, header records 0-length).
    w.write_all(bm25_fst)?;
    w.write_all(bm25_posts)?;
    w.write_all(bm25_stats)?;

    w.flush()?;

    let ref_count: usize = parsed.iter().map(|f| f.refs.len()).sum();
    tracing::info!(
        symbols = records.len(),
        refs = ref_count,
        files = file_table.len(),
        fst_bytes = fst_bytes.len(),
        edges = edge_records.len(),
        "index written to {:?}",
        output
    );

    Ok(())
}
