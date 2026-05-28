//! Phase 14.7 RED tests — blob-SHA addressed parse cache.
//!
//! These tests define the contract for `BlobCache` in
//! `src/index/parse_cache/mod.rs`. They WILL NOT COMPILE until Step 4
//! (GREEN) implements the module and adds the necessary
//! `Serialize`/`Deserialize` derives.
//!
//! ## What Step 4 must do to make these tests green
//!
//! ### New module
//! - Create `src/index/parse_cache/mod.rs` exporting `BlobCache`.
//! - Re-export it from `src/index/mod.rs` as `pub mod parse_cache;`.
//! - Re-export from `src/lib.rs` as `pub use index::parse_cache;` or keep
//!   the full path `vex::index::parse_cache::BlobCache`.
//!
//! ### Derives required on production types (DO NOT add in this PR)
//!
//! | Type | File | Missing derives |
//! |------|------|-----------------|
//! | `ParsedFile` | src/index/symbols.rs:296 | `Serialize`, `Deserialize` |
//! | `RawCallEdge` | src/index/symbols.rs:287 | `Serialize`, `Deserialize` |
//! | `BoundRef` | src/parse/scope/mod.rs:194 | `Serialize`, `Deserialize` |
//! | `BindTarget` | src/parse/scope/mod.rs:174 | `Serialize`, `Deserialize` |
//! | `RefKind` | src/parse/scope/mod.rs:186 | `Serialize`, `Deserialize` |
//! | `UsePath` | src/parse/scope/mod.rs:169 | `Serialize`, `Deserialize` |
//! | `Skeleton` | src/pattern/skeleton.rs:49 | `Serialize`, `Deserialize` |
//!
//! NOTE on `Skeleton`: the `kind` and `parent_kind` fields are `&'static str`.
//! For bincode serialization these serialize fine (as borrowed str slices) but
//! deserialization requires owned `String`. Step 4 must either:
//!   a) change `kind: &'static str` → `kind: String` (and `parent_kind`), or
//!   b) add a `#[serde(borrow)]` newtype wrapper and implement custom serde.
//! Option (a) is simpler. The roundtrip test below uses `String` literals for
//! those fields to force the issue.
//!
//! NOTE on `ParsedSymbol`: it already derives `Serialize`/`Deserialize` but
//! the `doc` and `body_tokens` fields use `#[serde(skip)]`. That means they
//! will be `None` after a cache roundtrip. This is acceptable: those fields
//! are embedding-context-only and are never persisted. The roundtrip test
//! below asserts `None` for them to document this contract.
//!
//! ### On-disk format contract (pinned in these tests)
//!
//! ```text
//! offset  size  field
//!      0     4  magic = b"VXBC"
//!      4     2  CACHE_FORMAT_VERSION: u16 (little-endian, starts at 1)
//!      6     4  grammar_fingerprint: u32 (little-endian)
//!     10     *  bincode payload of ParsedFile
//! ```
//!
//! ### Process-global cache override
//!
//! `set_cache_override` is a `OnceLock`; it can only be set once per process.
//! `BlobCache` therefore MUST NOT rely on the global override for its root.
//! Instead the `BlobCache::new(root: PathBuf)` constructor takes an explicit
//! root. The global `embed_cache_dir()` pattern is NOT followed here.
//! This avoids needing `serial_test` and keeps each test isolated via its own
//! `TempDir`.

use std::fs;
use std::io::Write as _;
use std::time::{Duration, SystemTime};

use tempfile::TempDir;
// Step 4 must create this module and re-export BlobCache.
// Until then this import causes a compile error (intended RED failure).
use vex::index::parse_cache::BlobCache;
use vex::index::symbols::{ParsedFile, ParsedRef, ParsedSymbol, RawCallEdge, SymbolKind};
use vex::parse::language::Language;
// BoundRef, BindTarget, RefKind, UsePath — Step 4 must add Serialize/Deserialize.
use vex::parse::scope::{BindTarget, BoundRef, RefKind, UsePath};
// Skeleton — Step 4 must add Serialize/Deserialize (and handle &'static str fields).
use vex::pattern::skeleton::Skeleton;
use vex::store::pattern_skeletons::grammar_fingerprint_for_lang;

// On-disk format constants pinned here so Step 4 has a concrete contract.
const MAGIC: &[u8; 4] = b"VXBC";
const CACHE_FORMAT_VERSION: u16 = 1;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Build a non-trivial `ParsedFile` with at least one entry in every Vec
/// field so roundtrip tests cover the full struct.
///
/// `doc` and `body_tokens` on `ParsedSymbol` are `#[serde(skip)]`; after a
/// bincode roundtrip they will be `None`. The helper therefore leaves them
/// `None` to match what comes back out of the cache.
fn make_rich_parsed_file() -> ParsedFile {
    let symbol = ParsedSymbol {
        name: "my_function".to_string(),
        kind: SymbolKind::Function,
        line: 10,
        signature: Some("fn my_function(x: u32) -> bool".to_string()),
        // These are #[serde(skip)] — will be None after cache roundtrip.
        doc: None,
        body_tokens: None,
    };

    let parsed_ref = ParsedRef {
        name: "some_dep".to_string(),
        line: 15,
        context: Some("call site".to_string()),
    };

    let call_edge = RawCallEdge {
        // Step 4: RawCallEdge must derive Serialize + Deserialize.
        caller_fn_name: "my_function".to_string(),
        caller_fn_line: 10,
        callee_name: "some_dep".to_string(),
        line: 15,
    };

    let bound_ref = BoundRef {
        // Step 4: BoundRef must derive Serialize + Deserialize.
        // Step 4: BindTarget and RefKind must derive Serialize + Deserialize.
        // Step 4: UsePath must derive Serialize + Deserialize.
        name: "some_dep".to_string(),
        line: 15,
        col: 4,
        target: BindTarget::Imported(UsePath {
            segments: vec!["crate".to_string(), "util".to_string()],
        }),
        kind: RefKind::Call,
    };

    let skeleton = Skeleton {
        // Step 4: Skeleton must derive Serialize + Deserialize.
        // The &'static str fields (kind, parent_kind) require either:
        //   a) changing to String in the struct, or
        //   b) a custom serde impl.
        // Step 4 must resolve this — option (a) recommended.
        start_row: 9,
        end_row: 20,
        kind: "function_item",
        parent_kind: None,
        ident: Some("my_function".to_string()),
        has_block: true,
    };

    ParsedFile {
        path: "src/lib.rs".to_string(),
        symbols: vec![symbol],
        refs: vec![parsed_ref],
        call_edges: vec![call_edge],
        bound_refs: vec![bound_ref],
        skeletons: vec![skeleton],
    }
}

/// Assert that two `ParsedFile` values are equal field-by-field. Defined
/// explicitly so test failures report which field diverged.
fn assert_parsed_file_eq(a: &ParsedFile, b: &ParsedFile) {
    assert_eq!(a.path, b.path, "path mismatch");
    assert_eq!(a.symbols.len(), b.symbols.len(), "symbols length mismatch");
    for (i, (sa, sb)) in a.symbols.iter().zip(b.symbols.iter()).enumerate() {
        assert_eq!(sa.name, sb.name, "symbols[{i}].name");
        assert_eq!(sa.kind, sb.kind, "symbols[{i}].kind");
        assert_eq!(sa.line, sb.line, "symbols[{i}].line");
        assert_eq!(sa.signature, sb.signature, "symbols[{i}].signature");
        // doc and body_tokens are #[serde(skip)] — both must be None after roundtrip.
        assert_eq!(sb.doc, None, "symbols[{i}].doc must be None post-cache");
        assert_eq!(
            sb.body_tokens, None,
            "symbols[{i}].body_tokens must be None post-cache"
        );
    }
    assert_eq!(a.refs.len(), b.refs.len(), "refs length mismatch");
    for (i, (ra, rb)) in a.refs.iter().zip(b.refs.iter()).enumerate() {
        assert_eq!(ra.name, rb.name, "refs[{i}].name");
        assert_eq!(ra.line, rb.line, "refs[{i}].line");
        assert_eq!(ra.context, rb.context, "refs[{i}].context");
    }
    assert_eq!(
        a.call_edges.len(),
        b.call_edges.len(),
        "call_edges length mismatch"
    );
    for (i, (ea, eb)) in a.call_edges.iter().zip(b.call_edges.iter()).enumerate() {
        assert_eq!(
            ea.caller_fn_name, eb.caller_fn_name,
            "call_edges[{i}].caller_fn_name"
        );
        assert_eq!(
            ea.caller_fn_line, eb.caller_fn_line,
            "call_edges[{i}].caller_fn_line"
        );
        assert_eq!(
            ea.callee_name, eb.callee_name,
            "call_edges[{i}].callee_name"
        );
        assert_eq!(ea.line, eb.line, "call_edges[{i}].line");
    }
    assert_eq!(
        a.bound_refs.len(),
        b.bound_refs.len(),
        "bound_refs length mismatch"
    );
    assert_eq!(
        a.skeletons.len(),
        b.skeletons.len(),
        "skeletons length mismatch"
    );
    for (i, (ska, skb)) in a.skeletons.iter().zip(b.skeletons.iter()).enumerate() {
        assert_eq!(ska.start_row, skb.start_row, "skeletons[{i}].start_row");
        assert_eq!(ska.end_row, skb.end_row, "skeletons[{i}].end_row");
        assert_eq!(ska.kind, skb.kind, "skeletons[{i}].kind");
        assert_eq!(
            ska.parent_kind, skb.parent_kind,
            "skeletons[{i}].parent_kind"
        );
        assert_eq!(ska.ident, skb.ident, "skeletons[{i}].ident");
        assert_eq!(ska.has_block, skb.has_block, "skeletons[{i}].has_block");
    }
}

// ── BlobCache constructor helper ──────────────────────────────────────────────

/// Construct a `BlobCache` rooted at `dir`. Each test has its own tempdir so
/// there is no process-global state to worry about.
fn cache_at(dir: &TempDir) -> BlobCache {
    // macOS tempdirs are symlinks; canonicalize to avoid path mismatch issues.
    // See memory/reference_criterion_bench_pattern.md for the macOS caveat.
    let root = dir.path().canonicalize().unwrap();
    BlobCache::new(root)
    // Step 4: BlobCache::new(root: PathBuf) -> BlobCache
}

// ── Test 1: bincode roundtrip of ParsedFile ───────────────────────────────────

/// Verify that `ParsedFile` (and all its nested types) can be serialized and
/// deserialized via bincode 1.3, preserving all fields.
///
/// This test drives the Serialize + Deserialize derive requirement on:
/// - ParsedFile
/// - RawCallEdge
/// - BoundRef, BindTarget, RefKind, UsePath
/// - Skeleton
///
/// It WILL FAIL with a compile error until Step 4 adds the derives.
#[test]
fn parsed_file_bincode_roundtrip_preserves_all_fields() {
    let original = make_rich_parsed_file();

    // Step 4: ParsedFile must derive serde::Serialize + serde::Deserialize.
    let bytes = bincode::serialize(&original).expect("bincode::serialize must succeed");
    let decoded: ParsedFile =
        bincode::deserialize(&bytes).expect("bincode::deserialize must succeed");

    assert_parsed_file_eq(&original, &decoded);
}

// ── Test 2: cache miss on empty directory ────────────────────────────────────

/// A freshly created cache root contains no entries. Lookup must return None.
#[test]
fn blob_cache_miss_returns_none_on_empty_dir() {
    let tmp = TempDir::new().unwrap();
    let cache = cache_at(&tmp);

    // 40-char hex sha — realistic git blob SHA
    let sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let result = cache.lookup(sha, Language::Rust);
    assert!(
        result.is_none(),
        "expected None for a miss on an empty cache, got Some"
    );
}

// ── Test 3: insert then lookup roundtrip ────────────────────────────────────

/// Insert a `ParsedFile` under a sha, then look it up. The returned value
/// must equal the inserted value field-by-field.
#[test]
fn blob_cache_insert_then_lookup_roundtrips_parsed_file() {
    let tmp = TempDir::new().unwrap();
    let cache = cache_at(&tmp);

    let sha = "aabbccddaabbccddaabbccddaabbccddaabbccdd";
    let original = make_rich_parsed_file();

    cache
        .insert(sha, Language::Rust, &original)
        .expect("insert must succeed");

    let hit = cache
        .lookup(sha, Language::Rust)
        .expect("expected a cache hit after insert");

    assert_parsed_file_eq(&original, &hit);
}

// ── Test 4: wrong language returns None ──────────────────────────────────────

/// An entry inserted under `Language::Rust` must NOT be returned when
/// `Language::Python` is requested for the same SHA, because the grammar
/// fingerprints differ and a mismatch triggers lazy invalidation.
#[test]
fn blob_cache_lookup_with_wrong_lang_returns_none() {
    let tmp = TempDir::new().unwrap();
    let cache = cache_at(&tmp);

    let sha = "1122334455667788112233445566778811223344";
    let pf = make_rich_parsed_file();

    cache
        .insert(sha, Language::Rust, &pf)
        .expect("insert must succeed");

    // Python has a different grammar_fingerprint_for_lang value than Rust —
    // the header check must detect the mismatch and return None.
    let rust_fp = grammar_fingerprint_for_lang(Language::Rust);
    let python_fp = grammar_fingerprint_for_lang(Language::Python);
    assert_ne!(
        rust_fp, python_fp,
        "test precondition: Rust and Python must have different grammar fingerprints"
    );

    let result = cache.lookup(sha, Language::Python);
    assert!(
        result.is_none(),
        "expected None when grammar fingerprint mismatches (Rust entry looked up as Python)"
    );
}

// ── Test 5: stale CACHE_FORMAT_VERSION returns None ──────────────────────────

/// Write a cache file by hand with a bogus (future) CACHE_FORMAT_VERSION.
/// `lookup` must return `None` (lazy invalidation — no error, no panic).
#[test]
fn blob_cache_lookup_with_stale_format_version_returns_none() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    let sha = "cafebabecafebabecafebabecafebabecafebabe";
    // Shard directory: first two chars of sha.
    let shard_dir = root.join("blobs").join(&sha[..2]);
    fs::create_dir_all(&shard_dir).unwrap();
    let cache_file = shard_dir.join(format!("{sha}.bin"));

    // Write a valid-looking header with a BOGUS version number (999).
    let bogus_version: u16 = 999;
    let fingerprint = grammar_fingerprint_for_lang(Language::Rust);
    let mut f = fs::File::create(&cache_file).unwrap();
    f.write_all(MAGIC).unwrap();
    f.write_all(&bogus_version.to_le_bytes()).unwrap();
    f.write_all(&fingerprint.to_le_bytes()).unwrap();
    // Payload: minimal valid bincode for some ParsedFile would be complex to
    // craft here; a truncated/empty payload is fine because the version check
    // fires first and we never reach payload decoding.
    f.write_all(&[0u8; 4]).unwrap();
    drop(f);

    let cache = BlobCache::new(root);
    let result = cache.lookup(sha, Language::Rust);
    assert!(
        result.is_none(),
        "expected None when CACHE_FORMAT_VERSION does not match"
    );
}

// ── Test 6: corrupt payload returns None ────────────────────────────────────

/// Write a cache file with a valid header but a truncated / garbled payload.
/// `lookup` must return `None` (decode error → lazy invalidation).
#[test]
fn blob_cache_lookup_with_corrupt_payload_returns_none() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    let sha = "deadd00ddeadd00ddeadd00ddeadd00ddeadd00d";
    let shard_dir = root.join("blobs").join(&sha[..2]);
    fs::create_dir_all(&shard_dir).unwrap();
    let cache_file = shard_dir.join(format!("{sha}.bin"));

    // Valid header, random garbage payload.
    let fingerprint = grammar_fingerprint_for_lang(Language::Rust);
    let mut f = fs::File::create(&cache_file).unwrap();
    f.write_all(MAGIC).unwrap();
    f.write_all(&CACHE_FORMAT_VERSION.to_le_bytes()).unwrap();
    f.write_all(&fingerprint.to_le_bytes()).unwrap();
    // Truncated / garbage payload — bincode will fail to decode.
    f.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
    drop(f);

    let cache = BlobCache::new(root);
    let result = cache.lookup(sha, Language::Rust);
    assert!(
        result.is_none(),
        "expected None for a corrupt (truncated) bincode payload"
    );
}

// ── Test 7: evict_to_cap removes oldest entries ───────────────────────────────

/// Insert 3 cache entries (by writing them as files directly), backdate
/// the mtime of the oldest entry, set a cap that forces eviction of the
/// oldest, then assert the oldest is gone and the newer two remain.
///
/// We manipulate mtimes via `std::fs::File` metadata + platform APIs because
/// `filetime` is not currently in dev-dependencies. If `filetime` is added in
/// Step 4 as a dev-dep this test can be simplified; until then we use the
/// `set_modified` API from `std::fs::FileTimes` (stable since Rust 1.75).
///
/// Step 4 note: `BlobCache::evict_to_cap(cap_bytes: u64) -> anyhow::Result<()>`.
#[test]
fn blob_cache_evict_to_cap_removes_oldest_entries() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    // Helper: write a minimal valid cache file for `sha` with `Language::Rust`
    // and a small payload so we can predict sizes. Entries live at
    // `<root>/<sha[0..2]>/<sha>.bin` — no extra `blobs/` layer; the `BlobCache`
    // root is the shard parent itself.
    let write_entry = |sha: &str| {
        let shard = root.join(&sha[..2]);
        fs::create_dir_all(&shard).unwrap();
        let path = shard.join(format!("{sha}.bin"));
        let fingerprint = grammar_fingerprint_for_lang(Language::Rust);
        // Build a real serialized ParsedFile payload so the entry is non-trivial.
        // Step 4: bincode::serialize(ParsedFile) must work.
        let pf = ParsedFile {
            path: format!("src/{sha}.rs"),
            symbols: Vec::new(),
            refs: Vec::new(),
            call_edges: Vec::new(),
            bound_refs: Vec::new(),
            skeletons: Vec::new(),
        };
        let payload = bincode::serialize(&pf).expect("serialize");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(MAGIC).unwrap();
        f.write_all(&CACHE_FORMAT_VERSION.to_le_bytes()).unwrap();
        f.write_all(&fingerprint.to_le_bytes()).unwrap();
        f.write_all(&payload).unwrap();
        path
    };

    let sha_old = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let sha_mid = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let sha_new = "cccccccccccccccccccccccccccccccccccccccc";

    let path_old = write_entry(sha_old);
    let path_mid = write_entry(sha_mid);
    let path_new = write_entry(sha_new);

    // Set mtimes so old < mid < new. Use std::fs::FileTimes (stable Rust 1.75).
    // "oldest" gets a time far in the past; "newest" gets now.
    let now = SystemTime::now();
    let old_mtime = now - Duration::from_secs(3600 * 24 * 30); // 30 days ago
    let mid_mtime = now - Duration::from_secs(3600 * 24 * 7); // 7 days ago

    {
        let f = fs::OpenOptions::new().write(true).open(&path_old).unwrap();
        f.set_modified(old_mtime).unwrap();
    }
    {
        let f = fs::OpenOptions::new().write(true).open(&path_mid).unwrap();
        f.set_modified(mid_mtime).unwrap();
    }
    // path_new keeps the current mtime (newest).

    // Measure total size of all 3 entries.
    let size_old = fs::metadata(&path_old).unwrap().len();
    let size_mid = fs::metadata(&path_mid).unwrap().len();
    let size_new = fs::metadata(&path_new).unwrap().len();
    let total = size_old + size_mid + size_new;

    // Cap: exactly the size of mid + new, so the oldest must go but the rest
    // fit. Strict eviction stops as soon as `remaining <= cap`; setting cap
    // to total - size_old lands `remaining` exactly on the cap boundary after
    // the first deletion, leaving mid + new intact.
    let cap = total - size_old;

    let cache = BlobCache::new(root.clone());
    cache
        .evict_to_cap(cap)
        .expect("evict_to_cap must not error");

    // Oldest entry must be gone.
    assert!(
        !path_old.exists(),
        "oldest cache entry must have been evicted"
    );
    // Newer entries must survive.
    assert!(path_mid.exists(), "mid-age cache entry must survive");
    assert!(path_new.exists(), "newest cache entry must survive");
}

// ── Test 8: evict_to_cap under cap is a no-op ────────────────────────────────

/// When the total cache size is already under the cap, `evict_to_cap` must
/// not remove any files.
#[test]
fn blob_cache_evict_to_cap_under_cap_is_noop() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    let sha = "1234567890abcdef1234567890abcdef12345678";
    let shard = root.join(&sha[..2]);
    fs::create_dir_all(&shard).unwrap();
    let path = shard.join(format!("{sha}.bin"));

    let fingerprint = grammar_fingerprint_for_lang(Language::Rust);
    let pf = ParsedFile {
        path: "src/small.rs".to_string(),
        symbols: Vec::new(),
        refs: Vec::new(),
        call_edges: Vec::new(),
        bound_refs: Vec::new(),
        skeletons: Vec::new(),
    };
    let payload = bincode::serialize(&pf).expect("serialize");
    let mut f = fs::File::create(&path).unwrap();
    f.write_all(MAGIC).unwrap();
    f.write_all(&CACHE_FORMAT_VERSION.to_le_bytes()).unwrap();
    f.write_all(&fingerprint.to_le_bytes()).unwrap();
    f.write_all(&payload).unwrap();
    drop(f);

    let file_size = fs::metadata(&path).unwrap().len();
    // Cap is larger than the single entry — nothing should be evicted.
    let cap = file_size * 100;

    let cache = BlobCache::new(root.clone());
    cache
        .evict_to_cap(cap)
        .expect("evict_to_cap must not error");

    assert!(
        path.exists(),
        "cache entry must NOT be evicted when total size is under cap"
    );
}
