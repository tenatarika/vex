//! Pattern skeleton section: construction and zero-copy reader (11.4 Inc 3).
//!
//! # On-disk layout (within the v6 index)
//!
//! Four sub-sections are stored contiguously after the last v5 section:
//!
//! ```text
//! [Skeleton Records]  N × SkeletonRecord (24 bytes each), sorted by file_id
//! [Kind Path Arena]   variable; each entry: [u16 depth][u32 arena_off; depth]
//!                     where `arena_off` points to a null-terminated string
//!                     stored inline in the same arena blob.
//! [Ident Pool]        variable; each entry: [u32 byte_len][UTF-8; byte_len]
//! [File Index]        [u32 count][{u32 file_id, u32 first_skel_idx}; count]
//! ```
//!
//! Kind-name strings are stored **inline** in the kind_path_arena as
//! null-terminated UTF-8 (not as offsets into the global Strings section).
//! This keeps `PatternSkeletonReader` self-contained — it needs no
//! back-reference to `IndexReader::mmap`, which would add a lifetime
//! parameter that leaks into every Inc 5 call site.
//!
//! Grammar fingerprints live in the `grammar_fingerprints` array inside
//! `PatternSkeletonHeader` (not a separate sub-section). They are
//! capacity-32 `u32` slots indexed by `Language::lang_id()`.

use anyhow::{bail, Result};

use super::format::{SkeletonRecord, SkeletonRecord as SR};
use crate::parse::language::Language;
use crate::pattern::skeleton::Skeleton;

// ── Writer-side types ────────────────────────────────────────────────────────

/// Opaque byte vectors produced by [`build_pattern_skeleton_section`].
/// Each field corresponds to one on-disk sub-section; all are empty for
/// an empty input.
#[derive(Debug, Default)]
pub struct PatternSkeletonSectionBytes {
    pub skeleton_records: Vec<u8>,
    pub kind_path_arena: Vec<u8>,
    pub ident_pool: Vec<u8>,
    pub file_index: Vec<u8>,
}

/// Deduplicating arena for kind-name strings (inline null-terminated UTF-8).
struct KindArena {
    data: Vec<u8>,
    lookup: std::collections::HashMap<&'static str, u32>,
}

impl KindArena {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            lookup: std::collections::HashMap::new(),
        }
    }

    /// Intern a `&'static str` kind name and return its byte offset in
    /// the arena. Deduplication is by pointer identity first (fast path
    /// because tree-sitter interns kind names), then by value.
    fn intern(&mut self, s: &'static str) -> u32 {
        if let Some(&off) = self.lookup.get(s) {
            return off;
        }
        let off = self.data.len() as u32;
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0); // null terminator
        self.lookup.insert(s, off);
        off
    }
}

/// Append-only pool for identifier strings (length-prefixed UTF-8).
struct IdentPool {
    data: Vec<u8>,
}

impl IdentPool {
    fn new() -> Self {
        Self { data: Vec::new() }
    }

    /// Append `s` as `[u32 len][UTF-8 bytes]` and return the byte offset
    /// of the entry start (`u32::MAX` sentinel is reserved for "no ident").
    fn push(&mut self, s: &str) -> u32 {
        let offset = self.data.len() as u32;
        let bytes = s.as_bytes();
        self.data
            .extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        self.data.extend_from_slice(bytes);
        offset
    }
}

/// Build the four pattern-skeleton sub-sections from a slice of
/// `(file_id, Skeleton)` pairs.
///
/// `lang_fingerprints` is a list of `(lang_id: u8, fingerprint: u32)` pairs
/// where `lang_id` is [`Language::lang_id()`]; duplicate entries are
/// last-write-wins. Slot 0 and slots ≥ 32 are rejected with `Err`.
///
/// Returns `(sub-section bytes, [u32; 32] fingerprint array)`. The
/// fingerprint array is stored verbatim in `PatternSkeletonHeader.grammar_fingerprints`.
///
/// The `intern` parameter is reserved for future use (sharing the global
/// Strings string pool for ident deduplication). It is accepted but not
/// called today — kind names are stored inline in the kind_path arena.
#[allow(unused_variables)] // `intern` is reserved for future use
pub fn build_pattern_skeleton_section(
    entries: &[(u32, Skeleton)],
    intern: &mut dyn FnMut(&str) -> u32,
    lang_fingerprints: &[(u8, u32)],
) -> Result<(PatternSkeletonSectionBytes, [u32; 32])> {
    // Validate and fill fingerprint array.
    let mut fingerprints = [0u32; 32];
    for &(lang_id, fp) in lang_fingerprints {
        if lang_id == 0 || lang_id as usize >= fingerprints.len() {
            bail!(
                "lang_id {} is out of range for grammar fingerprint array (valid: 1..31)",
                lang_id
            );
        }
        fingerprints[lang_id as usize] = fp;
    }

    if entries.is_empty() {
        return Ok((PatternSkeletonSectionBytes::default(), fingerprints));
    }

    // Sort by file_id; stable preserves intra-file declaration order.
    let mut sorted: Vec<(u32, &Skeleton)> = entries.iter().map(|(id, sk)| (*id, sk)).collect();
    sorted.sort_by_key(|(id, _)| *id);

    // We build the kind_path_arena as a two-part blob:
    //   [kind_name_strings...][path_entries...]
    //
    // `kind_arena` accumulates inline null-terminated kind-name strings.
    // `kind_path_buf` accumulates per-skeleton path entries that reference
    // byte offsets within `kind_arena.data`.
    //
    // After the loops we concatenate them:
    //   final kind_path_arena = kind_arena.data + kind_path_buf
    //
    // `SkeletonRecord.kind_path_offset` is stored as
    //   kind_arena.data.len() + kind_path_buf_entry_offset
    // so it is a valid index into the final combined blob.
    // The u32 arena offsets inside each path entry point into
    // `kind_arena.data` which occupies the prefix of the combined blob —
    // no adjustment needed for those.

    let mut records_bytes: Vec<u8> = Vec::with_capacity(sorted.len() * SR::SIZE);
    let mut kind_arena = KindArena::new();
    let mut kind_path_buf: Vec<u8> = Vec::new();
    let mut ident_pool = IdentPool::new();
    let mut file_index_entries: Vec<(u32, u32)> = Vec::new();
    let mut last_file_id: Option<u32> = None;

    for (skel_idx, (file_id, sk)) in sorted.iter().enumerate() {
        // Track file-id transitions for the file-index sub-section.
        if last_file_id != Some(*file_id) {
            file_index_entries.push((*file_id, skel_idx as u32));
            last_file_id = Some(*file_id);
        }

        // Record the current position in kind_path_buf so we can compute
        // the final kp_offset after we know kind_arena.data.len().
        // We store it temporarily as an offset-within-kind_path_buf.
        let kp_buf_offset = kind_path_buf.len() as u32;
        let depth: u16 = if sk.parent_kind.is_some() { 2 } else { 1 };
        kind_path_buf.extend_from_slice(&depth.to_le_bytes());
        let kind_arena_off = kind_arena.intern(sk.kind);
        kind_path_buf.extend_from_slice(&kind_arena_off.to_le_bytes());
        if let Some(pk) = sk.parent_kind {
            let pk_off = kind_arena.intern(pk);
            kind_path_buf.extend_from_slice(&pk_off.to_le_bytes());
        }

        // Ident pool.
        let ident_off = if let Some(name) = &sk.ident {
            ident_pool.push(name)
        } else {
            u32::MAX
        };

        let flags = if sk.has_block { SR::FLAG_HAS_BLOCK } else { 0 };

        // kp_offset is a placeholder (kind_arena.data.len() not yet known).
        // We patch it below after the loop using the known strings-blob size.
        let rec = SkeletonRecord {
            file_id: *file_id,
            start_row: sk.start_row,
            end_row: sk.end_row,
            // Temporary: offset-within-kind_path_buf. Patched after loop.
            kind_path_offset: kp_buf_offset,
            ident_offset: ident_off,
            kind_path_len: depth,
            flags,
            _pad: 0,
        };
        // SAFETY: SkeletonRecord is #[repr(C)] with a fixed 24-byte layout;
        // no padding issues on any supported target.
        let rec_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(&rec as *const SkeletonRecord as *const u8, SR::SIZE)
        };
        records_bytes.extend_from_slice(rec_bytes);
    }

    // Patch kind_path_offset in every SkeletonRecord now that we know
    // kind_arena.data.len() (the strings-blob prefix size in the combined
    // kind_path_arena).
    let strings_prefix = kind_arena.data.len() as u32;
    if strings_prefix > 0 {
        for i in 0..sorted.len() {
            let rec_off = i * SR::SIZE;
            // kind_path_offset is at byte 12 within the SkeletonRecord
            // (fields: file_id[4], start_row[4], end_row[4], kind_path_offset[4]).
            const KP_FIELD_OFFSET: usize = 12;
            let field_start = rec_off + KP_FIELD_OFFSET;
            let field_end = field_start + 4;
            let old = u32::from_le_bytes(records_bytes[field_start..field_end].try_into().unwrap());
            let new_off = old + strings_prefix;
            records_bytes[field_start..field_end].copy_from_slice(&new_off.to_le_bytes());
        }
    }

    // Combine: kind_path_arena = kind_name_strings + path_entries.
    let mut combined_kind_path: Vec<u8> =
        Vec::with_capacity(kind_arena.data.len() + kind_path_buf.len());
    combined_kind_path.extend_from_slice(&kind_arena.data);
    combined_kind_path.extend_from_slice(&kind_path_buf);

    // File-index blob: [u32 count][{u32 file_id, u32 first_idx}; count]
    let mut file_index_bytes: Vec<u8> = Vec::with_capacity(4 + file_index_entries.len() * 8);
    file_index_bytes.extend_from_slice(&(file_index_entries.len() as u32).to_le_bytes());
    for (fid, first) in &file_index_entries {
        file_index_bytes.extend_from_slice(&fid.to_le_bytes());
        file_index_bytes.extend_from_slice(&first.to_le_bytes());
    }

    Ok((
        PatternSkeletonSectionBytes {
            skeleton_records: records_bytes,
            kind_path_arena: combined_kind_path,
            ident_pool: ident_pool.data,
            file_index: file_index_bytes,
        },
        fingerprints,
    ))
}

/// Compute the grammar fingerprint for `lang` by hashing all node-kind
/// names in id order with `xxh3_64`, then truncating to `u32`.
pub fn grammar_fingerprint_for_lang(lang: Language) -> u32 {
    use xxhash_rust::xxh3::xxh3_64;
    let ts_lang = lang.ts_language();
    let node_count = ts_lang.node_kind_count();
    let mut buf: Vec<u8> = Vec::new();
    for i in 0..node_count {
        buf.extend_from_slice(ts_lang.node_kind_for_id(i as u16).unwrap_or("").as_bytes());
        buf.push(0);
    }
    xxh3_64(&buf) as u32
}

// ── Reader-side types ────────────────────────────────────────────────────────

/// A resolved skeleton returned by [`PatternSkeletonReader::skeletons_for_file`].
///
/// All string data is owned so the result outlives the mmap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedSkeleton {
    pub file_id: u32,
    pub start_row: u32,
    pub end_row: u32,
    /// Primary node kind (e.g. `"function_item"`).
    pub kind: String,
    /// Parent node kind, when the node is not a direct child of the file root.
    pub parent_kind: Option<String>,
    /// Leaf identifier, if present.
    pub ident: Option<String>,
    /// Whether the node carries a block-shaped body child.
    pub has_block: bool,
}

/// Zero-copy reader over the four pattern-skeleton sub-sections.
///
/// All byte slices must originate from the same mmap (or test buffer)
/// and must outlive this reader. [`PatternSkeletonReader::new`] validates
/// structural invariants; individual record reads degrade gracefully on
/// corrupt data rather than panicking.
pub struct PatternSkeletonReader<'a> {
    skeleton_data: &'a [u8],
    kind_path_data: &'a [u8],
    ident_pool_data: &'a [u8],
    file_index_data: &'a [u8],
    fingerprints: [u32; 32],
    record_count: usize,
    file_index_count: usize,
}

impl<'a> PatternSkeletonReader<'a> {
    /// Construct a reader and validate structural invariants.
    ///
    /// Fails when:
    /// - `skeleton_data.len()` is not a multiple of `SkeletonRecord::SIZE`.
    /// - `file_index_data` claims more entries than the blob can hold.
    pub fn new(
        skeleton_data: &'a [u8],
        kind_path_data: &'a [u8],
        ident_pool_data: &'a [u8],
        file_index_data: &'a [u8],
        fingerprints: [u32; 32],
    ) -> Result<Self> {
        if skeleton_data.len() % SR::SIZE != 0 {
            bail!(
                "skeleton_data length {} is not a multiple of SkeletonRecord::SIZE ({})",
                skeleton_data.len(),
                SR::SIZE
            );
        }
        let record_count = skeleton_data.len() / SR::SIZE;

        let file_index_count = if file_index_data.is_empty() {
            0
        } else {
            if file_index_data.len() < 4 {
                bail!(
                    "file_index_data is {} bytes, need at least 4 for the count word",
                    file_index_data.len()
                );
            }
            let count = u32::from_le_bytes(file_index_data[..4].try_into().unwrap()) as usize;
            let expected = 4usize.saturating_add(count.saturating_mul(8));
            if file_index_data.len() < expected {
                bail!(
                    "file_index_data claims {} entries but is only {} bytes (need {})",
                    count,
                    file_index_data.len(),
                    expected
                );
            }
            count
        };

        Ok(Self {
            skeleton_data,
            kind_path_data,
            ident_pool_data,
            file_index_data,
            fingerprints,
            record_count,
            file_index_count,
        })
    }

    /// True when the section holds no skeleton records.
    pub fn is_empty(&self) -> bool {
        self.record_count == 0
    }

    /// Iterate over every `file_id` that has at least one skeleton record.
    /// Reads directly from the file-index sub-section; O(n) in the number
    /// of distinct files.
    pub fn file_ids(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.file_index_count).filter_map(|i| self.file_index_file_id(i))
    }

    /// Return all persisted skeletons for `file_id`. O(log n) binary search
    /// on the file-index plus a linear scan over the file's contiguous run.
    pub fn skeletons_for_file(&self, file_id: u32) -> Vec<PersistedSkeleton> {
        let Some((first, end)) = self.file_range(file_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for idx in first..end {
            if let Some(sk) = self.read_record(idx) {
                out.push(sk);
            }
        }
        out
    }

    /// Return the stored grammar fingerprint for `lang_id`, or `None` when
    /// the slot is zero (not fingerprinted) or out of range.
    pub fn grammar_fingerprint(&self, lang_id: u8) -> Option<u32> {
        let slot = lang_id as usize;
        if slot == 0 || slot >= self.fingerprints.len() {
            return None;
        }
        let fp = self.fingerprints[slot];
        if fp == 0 {
            None
        } else {
            Some(fp)
        }
    }

    // ── Internal helpers ─────────────────────────────────────────────────

    fn file_range(&self, file_id: u32) -> Option<(usize, usize)> {
        if self.file_index_count == 0 {
            return None;
        }
        let mut lo = 0usize;
        let mut hi = self.file_index_count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let fid = self.file_index_file_id(mid)?;
            match fid.cmp(&file_id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let first = self.file_index_first(mid)? as usize;
                    let end = if mid + 1 < self.file_index_count {
                        self.file_index_first(mid + 1)? as usize
                    } else {
                        self.record_count
                    };
                    return Some((first, end));
                }
            }
        }
        None
    }

    fn file_index_file_id(&self, entry: usize) -> Option<u32> {
        // Layout: [u32 count][{u32 file_id, u32 first_idx}; count]
        // Entry at index `entry` starts at 4 + entry*8.
        let off = 4usize.checked_add(entry.checked_mul(8)?)?;
        let end = off.checked_add(4)?;
        if end > self.file_index_data.len() {
            return None;
        }
        Some(u32::from_le_bytes(
            self.file_index_data[off..end].try_into().ok()?,
        ))
    }

    fn file_index_first(&self, entry: usize) -> Option<u32> {
        // `first_idx` is at offset 4 + entry*8 + 4
        let off = 4usize.checked_add(entry.checked_mul(8)?)?.checked_add(4)?;
        let end = off.checked_add(4)?;
        if end > self.file_index_data.len() {
            return None;
        }
        Some(u32::from_le_bytes(
            self.file_index_data[off..end].try_into().ok()?,
        ))
    }

    fn read_record(&self, idx: usize) -> Option<PersistedSkeleton> {
        if idx >= self.record_count {
            return None;
        }
        let off = idx * SR::SIZE;
        if off + SR::SIZE > self.skeleton_data.len() {
            return None;
        }
        // SAFETY: bounds checked above; copy into stack-aligned storage to
        // avoid unaligned reads from the mmap slice.
        let mut rec = std::mem::MaybeUninit::<SkeletonRecord>::uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.skeleton_data[off..].as_ptr(),
                rec.as_mut_ptr() as *mut u8,
                SR::SIZE,
            );
        }
        // SAFETY: all fields are integer types; every bit pattern is valid.
        let rec = unsafe { rec.assume_init() };

        let kind = self.read_kind_primary(rec.kind_path_offset, rec.kind_path_len)?;
        let parent_kind = if rec.kind_path_len >= 2 {
            self.read_kind_secondary(rec.kind_path_offset)
        } else {
            None
        };
        let ident = if rec.ident_offset == u32::MAX {
            None
        } else {
            self.read_ident(rec.ident_offset)
        };
        let has_block = (rec.flags & SR::FLAG_HAS_BLOCK) != 0;

        Some(PersistedSkeleton {
            file_id: rec.file_id,
            start_row: rec.start_row,
            end_row: rec.end_row,
            kind,
            parent_kind,
            ident,
            has_block,
        })
    }

    /// Layout of a kind-path entry:
    /// `[u16 depth][u32 arena_off_0][u32 arena_off_1; if depth>=2]`
    /// Each `arena_off` is an offset into `kind_path_data` where the
    /// null-terminated kind name begins.
    fn read_kind_primary(&self, kp_off: u32, depth: u16) -> Option<String> {
        if depth == 0 {
            return None;
        }
        let base = kp_off as usize;
        // Skip 2-byte depth word, read first u32 arena offset.
        let off_start = base.checked_add(2)?;
        let off_end = off_start.checked_add(4)?;
        if off_end > self.kind_path_data.len() {
            return None;
        }
        let arena_off =
            u32::from_le_bytes(self.kind_path_data[off_start..off_end].try_into().ok()?) as usize;
        self.read_null_term(arena_off)
    }

    fn read_kind_secondary(&self, kp_off: u32) -> Option<String> {
        // Skip depth (2 bytes) + primary offset (4 bytes) = 6 bytes.
        let off_start = (kp_off as usize).checked_add(2)?.checked_add(4)?;
        let off_end = off_start.checked_add(4)?;
        if off_end > self.kind_path_data.len() {
            return None;
        }
        let arena_off =
            u32::from_le_bytes(self.kind_path_data[off_start..off_end].try_into().ok()?) as usize;
        self.read_null_term(arena_off)
    }

    fn read_null_term(&self, offset: usize) -> Option<String> {
        if offset >= self.kind_path_data.len() {
            return None;
        }
        let slice = &self.kind_path_data[offset..];
        let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
        std::str::from_utf8(&slice[..end]).ok().map(String::from)
    }

    fn read_ident(&self, offset: u32) -> Option<String> {
        let off = offset as usize;
        let len_end = off.checked_add(4)?;
        if len_end > self.ident_pool_data.len() {
            return None;
        }
        let byte_len =
            u32::from_le_bytes(self.ident_pool_data[off..len_end].try_into().ok()?) as usize;
        let str_start = len_end;
        let str_end = str_start.checked_add(byte_len)?;
        if str_end > self.ident_pool_data.len() {
            return None;
        }
        std::str::from_utf8(&self.ident_pool_data[str_start..str_end])
            .ok()
            .map(String::from)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::language::Language;

    fn sk(
        kind: &'static str,
        parent_kind: Option<&'static str>,
        ident: Option<&str>,
        has_block: bool,
        start: u32,
        end: u32,
    ) -> Skeleton {
        Skeleton {
            start_row: start,
            end_row: end,
            kind,
            parent_kind,
            ident: ident.map(String::from),
            has_block,
        }
    }

    fn no_intern(_s: &str) -> u32 {
        0
    }

    fn build_and_read(
        entries: &[(u32, Skeleton)],
        fps: &[(u8, u32)],
    ) -> (PatternSkeletonSectionBytes, [u32; 32]) {
        build_pattern_skeleton_section(entries, &mut no_intern, fps).expect("build")
    }

    fn reader<'a>(
        bytes: &'a PatternSkeletonSectionBytes,
        fingerprints: [u32; 32],
    ) -> PatternSkeletonReader<'a> {
        PatternSkeletonReader::new(
            &bytes.skeleton_records,
            &bytes.kind_path_arena,
            &bytes.ident_pool,
            &bytes.file_index,
            fingerprints,
        )
        .expect("reader construction")
    }

    // ── Round-trip ───────────────────────────────────────────────────────

    #[test]
    fn round_trip_three_skeletons_across_two_files() {
        let entries = vec![
            (0u32, sk("function_item", None, Some("foo"), true, 0, 5)),
            (
                0u32,
                sk(
                    "struct_item",
                    Some("declaration_list"),
                    Some("Bar"),
                    false,
                    7,
                    9,
                ),
            ),
            (
                1u32,
                sk("class_declaration", None, Some("MyClass"), true, 1, 20),
            ),
        ];

        let (bytes, fps) = build_and_read(&entries, &[]);
        let r = reader(&bytes, fps);

        assert!(!r.is_empty());

        let f0 = r.skeletons_for_file(0);
        assert_eq!(f0.len(), 2);
        assert_eq!(f0[0].kind, "function_item");
        assert_eq!(f0[0].ident.as_deref(), Some("foo"));
        assert!(f0[0].has_block);
        assert_eq!(f0[0].parent_kind, None);
        assert_eq!(f0[0].start_row, 0);
        assert_eq!(f0[0].end_row, 5);

        assert_eq!(f0[1].kind, "struct_item");
        assert_eq!(f0[1].parent_kind.as_deref(), Some("declaration_list"));
        assert_eq!(f0[1].ident.as_deref(), Some("Bar"));
        assert!(!f0[1].has_block);

        let f1 = r.skeletons_for_file(1);
        assert_eq!(f1.len(), 1);
        assert_eq!(f1[0].kind, "class_declaration");
        assert_eq!(f1[0].ident.as_deref(), Some("MyClass"));
        assert!(f1[0].has_block);

        // Non-existent file returns empty.
        assert!(r.skeletons_for_file(99).is_empty());
    }

    // ── Empty section ────────────────────────────────────────────────────

    #[test]
    fn empty_section_is_empty_and_returns_no_records() {
        let (bytes, fps) = build_and_read(&[], &[]);
        let r = reader(&bytes, fps);
        assert!(r.is_empty());
        assert!(r.skeletons_for_file(0).is_empty());
        assert!(r.skeletons_for_file(99).is_empty());
    }

    // ── Grammar fingerprint round-trip ────────────────────────────────────

    #[test]
    fn grammar_fingerprint_roundtrip() {
        let rust_id = Language::Rust.lang_id(); // 1
        let py_id = Language::Python.lang_id(); // 4

        let (bytes, fps) = build_and_read(&[], &[(rust_id, 0xDEAD_BEEF)]);
        let r = reader(&bytes, fps);

        assert_eq!(r.grammar_fingerprint(rust_id), Some(0xDEAD_BEEF));
        assert_eq!(r.grammar_fingerprint(py_id), None); // not stored
        assert_eq!(r.grammar_fingerprint(0), None); // slot 0 reserved
    }

    // ── Adversarial: truncated sub-sections ───────────────────────────────

    #[test]
    fn truncated_skeleton_records_rejected() {
        let entries = vec![(0u32, sk("function_item", None, Some("f"), true, 0, 1))];
        let (mut bytes, fps) = build_and_read(&entries, &[]);
        bytes.skeleton_records.pop(); // 1 byte short
        let result = PatternSkeletonReader::new(
            &bytes.skeleton_records,
            &bytes.kind_path_arena,
            &bytes.ident_pool,
            &bytes.file_index,
            fps,
        );
        assert!(
            result.is_err(),
            "truncated skeleton_records must return Err"
        );
    }

    #[test]
    fn truncated_file_index_header_word_rejected() {
        let entries = vec![(0u32, sk("function_item", None, None, false, 0, 0))];
        let (mut bytes, fps) = build_and_read(&entries, &[]);
        bytes.file_index.truncate(3); // less than 4-byte count word
        let result = PatternSkeletonReader::new(
            &bytes.skeleton_records,
            &bytes.kind_path_arena,
            &bytes.ident_pool,
            &bytes.file_index,
            fps,
        );
        assert!(
            result.is_err(),
            "truncated file_index header must return Err"
        );
    }

    #[test]
    fn truncated_file_index_entries_rejected() {
        let entries = vec![(0u32, sk("function_item", None, None, false, 0, 0))];
        let (mut bytes, fps) = build_and_read(&entries, &[]);
        // Keep only the 4-byte count word; remove the entry data.
        bytes.file_index.truncate(4);
        let result = PatternSkeletonReader::new(
            &bytes.skeleton_records,
            &bytes.kind_path_arena,
            &bytes.ident_pool,
            &bytes.file_index,
            fps,
        );
        assert!(
            result.is_err(),
            "truncated file_index entries must return Err"
        );
    }

    // ── Skeleton without ident ────────────────────────────────────────────

    #[test]
    fn skeleton_without_ident_round_trips() {
        let entries = vec![(0u32, sk("arrow_function", None, None, true, 3, 3))];
        let (bytes, fps) = build_and_read(&entries, &[]);
        let r = reader(&bytes, fps);
        let result = r.skeletons_for_file(0);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].ident, None);
        assert_eq!(result[0].kind, "arrow_function");
        assert!(result[0].has_block);
    }

    // ── Lang-id out of range rejected ─────────────────────────────────────

    #[test]
    fn lang_id_255_returns_error() {
        let result = build_pattern_skeleton_section(&[], &mut no_intern, &[(255, 0xABCD)]);
        assert!(result.is_err(), "lang_id 255 must return Err");
    }

    #[test]
    fn lang_id_zero_returns_error() {
        let result = build_pattern_skeleton_section(&[], &mut no_intern, &[(0, 1)]);
        assert!(result.is_err(), "lang_id 0 must return Err");
    }

    // ── SkeletonRecord::SIZE sanity ───────────────────────────────────────

    #[test]
    fn skeleton_record_size_is_24() {
        // The spec calls for 24 bytes; verify at compile/test time.
        assert_eq!(
            SR::SIZE,
            24,
            "SkeletonRecord must be exactly 24 bytes (spec requirement)"
        );
    }
}
