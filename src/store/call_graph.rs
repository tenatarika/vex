//! Call-graph FST construction and zero-copy readers.
//!
//! At write time we collect `CallEdgeBuilder` records from the pipeline,
//! intern callee names into the strings pool, and build two FSTs:
//!
//! - **Callers FST**: `callee_name → posting_list[edge_idx]`. Powers
//!   `vex callers <name>` — given a callee name, retrieve every edge whose
//!   `callee` matches.
//! - **Callees FST**: `caller_sym_idx (as decimal string) → posting_list[edge_idx]`.
//!   Powers `vex callees <name>` — given a caller symbol (resolved by name
//!   first), retrieve every outgoing edge.
//!
//! Both posting lists store edge indices (`u32`), letting the consumer
//! read the full [`CallEdge`] record from the edges section.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

/// Input record for [`build_callers_fst`] / [`build_callees_fst`]. The
/// writer assembles these from parsed files, then resolves callee strings
/// into the same string pool as symbol names.
#[derive(Debug, Clone)]
pub struct CallEdgeBuilder {
    pub caller_sym_idx: u32,
    pub callee_name: String,
    pub line: u32,
}

/// Build the callers FST + posting bytes.
///
/// `edges` must already be in the order they will appear in the on-disk
/// `call_edges` section — the returned posting lists hold the edge indices
/// (= positions in `edges`).
pub fn build_callers_fst(edges: &[CallEdgeBuilder]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut grouped: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (i, e) in edges.iter().enumerate() {
        grouped
            .entry(e.callee_name.to_lowercase())
            .or_default()
            .push(i as u32);
    }
    build_fst_from_groups(grouped)
}

/// Build the callees FST + posting bytes (keyed by caller symbol index).
///
/// Keys are decimal strings of the symbol index so we reuse the same FST
/// machinery as the callers index. A future format may switch to a sorted
/// array — kept as FST here for code uniformity.
pub fn build_callees_fst(edges: &[CallEdgeBuilder]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut grouped: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (i, e) in edges.iter().enumerate() {
        grouped
            .entry(encode_caller_key(e.caller_sym_idx))
            .or_default()
            .push(i as u32);
    }
    build_fst_from_groups(grouped)
}

/// Encode a caller symbol index as the lookup key for the callees FST.
/// Zero-padded to 10 digits so FST keys sort numerically; this also keeps
/// the key length constant which the FST library handles efficiently.
pub fn encode_caller_key(caller_sym_idx: u32) -> String {
    format!("{caller_sym_idx:010}")
}

fn build_fst_from_groups(mut groups: BTreeMap<String, Vec<u32>>) -> Result<(Vec<u8>, Vec<u8>)> {
    let capacity: usize = groups.values().map(|v| 4 + v.len() * 4).sum();
    let mut posting_data: Vec<u8> = Vec::with_capacity(capacity);
    let mut fst_builder = fst::MapBuilder::memory();

    for (key, edge_indices) in groups.iter_mut() {
        edge_indices.sort_unstable();
        edge_indices.dedup();
        let offset = posting_data.len() as u64;
        let count = edge_indices.len() as u32;
        posting_data.extend_from_slice(&count.to_le_bytes());
        for &idx in edge_indices.iter() {
            posting_data.extend_from_slice(&idx.to_le_bytes());
        }
        fst_builder
            .insert(key.as_bytes(), offset)
            .context("fst insert (call graph)")?;
    }

    let fst_bytes = fst_builder
        .into_inner()
        .context("finalize call-graph fst")?;
    Ok((fst_bytes, posting_data))
}

/// Zero-copy reader for a call-graph FST (callers OR callees — same shape).
pub struct CallGraphFstReader<'a> {
    fst_map: fst::Map<&'a [u8]>,
    posting_data: &'a [u8],
}

impl<'a> CallGraphFstReader<'a> {
    pub fn new(fst_bytes: &'a [u8], posting_bytes: &'a [u8]) -> Result<Self> {
        let fst_map =
            fst::Map::new(fst_bytes).map_err(|e| anyhow::anyhow!("fst load (call graph): {e}"))?;
        Ok(Self {
            fst_map,
            posting_data: posting_bytes,
        })
    }

    /// Look up edge indices for a key. Returns empty when key is unknown.
    pub fn find(&self, key: &str) -> Vec<u32> {
        match self.fst_map.get(key.as_bytes()) {
            Some(offset) => self.read_posting_list(offset),
            None => Vec::new(),
        }
    }

    fn read_posting_list(&self, offset: u64) -> Vec<u32> {
        let offset = offset as usize;
        if offset + 4 > self.posting_data.len() {
            return Vec::new();
        }
        let count = u32::from_le_bytes(
            self.posting_data[offset..offset + 4]
                .try_into()
                .unwrap_or([0; 4]),
        ) as usize;
        let mut out = Vec::with_capacity(count);
        let mut pos = offset + 4;
        for _ in 0..count {
            if pos + 4 > self.posting_data.len() {
                break;
            }
            let idx =
                u32::from_le_bytes(self.posting_data[pos..pos + 4].try_into().unwrap_or([0; 4]));
            out.push(idx);
            pos += 4;
        }
        out
    }
}

/// Fast-path resolution of `vex callers <target>` over a v4 index.
///
/// Returns `(caller_name, caller_path, line)` triples — one per call site.
/// Looks up `target` in the callers FST (case-insensitive) then dereferences
/// each edge index to its caller symbol record. Returns an empty vec when
/// the index has no call graph or when no edges target the requested name.
pub fn find_callers_fast(
    reader: &super::reader::IndexReader,
    target: &str,
    limit: usize,
) -> Vec<crate::callgraph::CallMatch> {
    // Public API guard: callers must not have to know that an empty
    // call_edges section still ships non-empty FST bytes — gate on the
    // same invariant `has_call_graph()` uses elsewhere.
    if !reader.has_call_graph() {
        return Vec::new();
    }
    let fst_bytes = reader.callers_fst_bytes();
    let post_bytes = reader.callers_posting_bytes();
    let Ok(fst) = CallGraphFstReader::new(fst_bytes, post_bytes) else {
        return Vec::new();
    };
    let edge_indices = fst.find(&target.to_lowercase());
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for idx in edge_indices {
        let Some(edge) = reader.call_edge(idx as usize) else {
            continue;
        };
        let Some(caller_rec) = reader.symbol(edge.caller_sym_idx as usize) else {
            continue;
        };
        let name = reader.read_string(caller_rec.name_offset).to_string();
        let path = reader.read_string(caller_rec.file_offset).to_string();
        // Deduplicate by the caller's DEFINITION identity, not the call
        // site. Two call sites in the same caller share `caller_rec.line`
        // (the function's def line), so this collapses them to one
        // CallMatch even though the output `CallMatch.line` below is the
        // call-site line for the first hit.
        if !seen.insert((name.clone(), path.clone(), caller_rec.line)) {
            continue;
        }
        out.push(crate::callgraph::CallMatch {
            name,
            path,
            line: edge.line as usize,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Fast-path resolution of `vex callees <target>` over a v4 index.
///
/// Resolves `target` by name (via symbol FST) to one or more caller symbol
/// indices, then enumerates outgoing edges per index. Returns each callee
/// name with its call site (`path:line`) attributed to the caller's file.
/// When `target` resolves to multiple symbols (same name in different
/// files) the results are merged.
pub fn find_callees_fast(
    reader: &super::reader::IndexReader,
    target: &str,
    limit: usize,
) -> Vec<crate::callgraph::CallMatch> {
    if !reader.has_call_graph() {
        return Vec::new();
    }
    let fst_bytes = reader.callees_fst_bytes();
    let post_bytes = reader.callees_posting_bytes();
    let Ok(fst) = CallGraphFstReader::new(fst_bytes, post_bytes) else {
        return Vec::new();
    };

    let Some(sym_fst) = reader.symbol_fst_reader() else {
        return Vec::new();
    };
    let target_lower = target.to_lowercase();
    // Resolve target by name → all symbol indices that match.
    let candidates: Vec<u32> = sym_fst
        .find(&target_lower)
        .into_iter()
        .filter(|&idx| {
            reader
                .symbol(idx as usize)
                .is_some_and(|r| reader.read_string(r.name_offset).to_lowercase() == target_lower)
        })
        .collect();

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for caller_sym_idx in candidates {
        let key = encode_caller_key(caller_sym_idx);
        for edge_idx in fst.find(&key) {
            let Some(edge) = reader.call_edge(edge_idx as usize) else {
                continue;
            };
            let Some(caller_rec) = reader.symbol(caller_sym_idx as usize) else {
                continue;
            };
            let callee_name = reader.read_string(edge.callee_name_offset).to_string();
            if callee_name.is_empty() {
                continue;
            }
            let path = reader.read_string(caller_rec.file_offset).to_string();
            // Deduplicate by callee identity within this resolution.
            if !seen.insert((callee_name.clone(), path.clone(), edge.line)) {
                continue;
            }
            out.push(crate::callgraph::CallMatch {
                name: callee_name,
                path,
                line: edge.line as usize,
            });
            if out.len() >= limit {
                return out;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(caller: u32, callee: &str, line: u32) -> CallEdgeBuilder {
        CallEdgeBuilder {
            caller_sym_idx: caller,
            callee_name: callee.to_string(),
            line,
        }
    }

    #[test]
    fn empty_input_produces_empty_outputs() {
        let (fst, posts) = build_callers_fst(&[]).unwrap();
        // Empty FST has a non-zero header but the map is empty.
        let reader = CallGraphFstReader::new(&fst, &posts).unwrap();
        assert!(reader.find("Foo").is_empty());
    }

    #[test]
    fn callers_lookup_returns_edge_indices() {
        let edges = vec![edge(0, "Foo", 10), edge(1, "Bar", 20), edge(2, "Foo", 30)];
        let (fst, posts) = build_callers_fst(&edges).unwrap();
        let reader = CallGraphFstReader::new(&fst, &posts).unwrap();
        let foo_edges = reader.find("foo");
        assert_eq!(foo_edges, vec![0, 2]);
        let bar_edges = reader.find("bar");
        assert_eq!(bar_edges, vec![1]);
    }

    #[test]
    fn callees_lookup_by_caller_sym_idx() {
        let edges = vec![edge(5, "alpha", 1), edge(5, "beta", 2), edge(7, "alpha", 3)];
        let (fst, posts) = build_callees_fst(&edges).unwrap();
        let reader = CallGraphFstReader::new(&fst, &posts).unwrap();
        let edges_from_5 = reader.find(&encode_caller_key(5));
        assert_eq!(edges_from_5, vec![0, 1]);
        let edges_from_7 = reader.find(&encode_caller_key(7));
        assert_eq!(edges_from_7, vec![2]);
        assert!(reader.find(&encode_caller_key(99)).is_empty());
    }

    #[test]
    fn callers_is_case_insensitive() {
        let edges = vec![edge(0, "MyFunc", 1)];
        let (fst, posts) = build_callers_fst(&edges).unwrap();
        let reader = CallGraphFstReader::new(&fst, &posts).unwrap();
        assert_eq!(reader.find("myfunc"), vec![0]);
        // Caller must lowercase the query before lookup.
        assert!(reader.find("MyFunc").is_empty());
    }
}
