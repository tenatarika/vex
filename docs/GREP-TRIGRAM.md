# grep trigram skip-index

Authoritative design + as-shipped record for the `vex grep` trigram
skip-index (STORAGE-RESEARCH §2). Read this before touching
`src/grep/trigram.rs`, `src/store/trigram.rs`, the blob-cache entry
format (`src/index/parse_cache/`), or the trigram block in
`src/index/pipeline/output.rs`.

## Problem

`vex grep` walks the project and runs a regex over every file ≤1 MB —
one `read_to_string` + per-line scan per file, always. On a large repo
most of those reads are wasted: the pattern's literal isn't in the file.

The skip-index records, per code file, a **trigram presence bloom** built
from the file's raw bytes. At grep time we extract the trigrams the
pattern's required literal must contain and skip any file whose bloom
lacks one of them — the file provably cannot match, so it's never read.

## Core invariant — NO FALSE NEGATIVES

A file is skipped **only** when it provably cannot match. Every source of
uncertainty degrades to "read the file" (a full walk), never to a skip:

- **Non-literal / short pattern** → `required_trigrams` returns `None` →
  the whole query full-walks (no skip-index used).
- **Absent sidecar record**, malformed sidecar, or bloom-width change →
  the file (or all files) are full-read.
- **Staleness** — the file changed since it was indexed → full-read (see
  the guard below).

False *positives* (a kept file that turns out not to match) are fine: the
regex runs and finds nothing, exactly as today. Only false negatives —
skipping a file that would have matched — are forbidden, and every
uncertain path is resolved in the safe direction.

## Pieces

### 1. Extraction + bloom — `src/grep/trigram.rs` (P1)

- `required_trigrams(pattern) -> Option<Vec<[u8;3]>>`: parses the regex
  via `regex_syntax` and returns the byte-trigrams of an **exact required
  literal**. `Some` only when the whole HIR is a literal concatenation
  (transparent to capture groups and zero-width look assertions like `^`,
  `$`, `\b`). Any class — including `(?i)` case-folding — repetition, or
  alternation yields `None`. Literal < 3 bytes → `None`. Byte-oriented,
  so multibyte literals Just Work and match the byte domain grep searches.
- `TrigramBloom`: a fixed 2048-bit (`BLOOM_BYTES = 256`), k=1 per-file
  presence bloom. `from_bytes(&[u8])` sets every 3-byte window's bit;
  `might_contain_all(&[trigram])` is the skip test; `from_raw`/`as_bytes`
  cross the persistence boundary.

The P1 gate is a proptest no-false-negative property with the `regex`
crate as oracle: whenever a pattern matches a content string and we
derived trigrams, the bloom must admit them.

Per-file blooms (not inverted trigram postings) are deliberate: vex's
incremental `update` recomputes changed files, and a per-file bloom
updates exactly one record where inverted postings would touch many
lists. The tradeoff is O(files) probes with no rare-term selectivity.

### 2. Sidecar — `src/store/trigram.rs` (P2)

`<index_dir>/index.trigram`, one record per indexed code file:

```text
magic:        4  "VXTG"
version:      u32 = 1
bloom_bytes:  u32  (= grep::trigram::BLOOM_BYTES; load rejects a mismatch
                    so a future bloom-width tune can't read old records at
                    the wrong stride)
count:        u32  (≤ MAX_COUNT = 10_000_000)
records: count × {
    path_len:    u16  (UTF-8 bytes; ≤ MAX_PATH_LEN = 8192)
    path:        path_len bytes  (POSIX-separator rel path)
    bloom:       bloom_bytes bytes
    len:         u64  (file byte length when indexed)
    mtime_secs:  i64
    mtime_nanos: u32
}
```

Mirrors the `body_tokens` sidecar conventions: magic/version/count guard,
per-record bounds, atomic temp-write + rename, and a loader that bails on
any malformation so the caller can fall back to a full walk. `save`/`load`
are self-contained — grep loads only this file, no dependency on
`index.vex`.

**Staleness guard (correctness-critical).** grep runs *without* a
reindex, so a file may have been edited since it was indexed. Each record
stores the `(len, mtime)` the file had at index time; grep already stats
every file, so it compares the live `(len, mtime)` against the record and
falls back to a full read on any mismatch. Both writer and grep reader
funnel mtime through `mtime_parts(SystemTime) -> (i64, u32)` so the
comparison is apples-to-apples. Residual window: an edit that changes
neither `len` nor `mtime` (same-nanosecond, same-length) — the classic
mtime+size limitation shared with ripgrep/git; nanosecond mtime shrinks it
to nothing on APFS/ext4/NTFS.

### 3. Bloom delivery via the blob cache — `src/index/parse_cache/` (P2)

The bloom is built from raw bytes in `parse_files`. But Phase 14.7's
content-addressed parse cache skips the byte read on a blob-SHA hit — and
the dominant real-world path (re-running `vex index` on a project you
indexed before) is warm-cache, so building blooms only on the read path
would leave the sidecar nearly empty. Architect verdict (Option A):

**The bloom rides inside the blob-cache entry.** It's a pure function of
content — content-addressed and cross-project-shareable exactly like the
parse. The entry format bumped **v3 → v4**, inserting a 1-byte `present`
flag + a 256-byte bloom slot between the header and the bincode payload:

```text
[magic "VXBC"][version:2][grammar_fingerprint:4][present:1][bloom:256][bincode(ParsedFile)]
```

- On a cache **hit**, `lookup` reads the slot and restores it into
  `ParsedFile.trigram_bloom` — no byte read, bloom for free.
- On a cache **miss** (or an untracked/non-git file, which has no blob
  SHA and always takes the read path), `parse_files` builds the bloom
  from the bytes it reads and it rides into the entry.
- Stale v3 entries fail the version check → treated as a miss → one-time
  re-parse rebuilds the bloom, after which v4 entries carry it.

The bloom is **not** in the bincode payload: `serde` has no built-in impl
for `[u8; 256]`, and a fixed slot keeps the payload offset constant.
`ParsedFile.trigram_bloom` is therefore `#[serde(skip)]` — a transient
carrier. The 256-const lives in parse_cache's own format block
(`BLOOM_SLOT`) with a `const _: () = assert!(BLOOM_SLOT == BLOOM_BYTES)`
compile-time cross-check, mirroring the existing `HEADER_SIZE` precedent
(so the index layer doesn't take a runtime dependency on the grep layer).

The **`present` flag** keeps two cases distinct: a genuine all-zero bloom
(an empty / <3-byte file — skipping it is *correct*) versus "no bloom
recorded" (`None` → sidecar omits the file → grep full-reads it).
Confusing them would turn a missing bloom into a silent skip — a false
negative.

### 4. Sidecar write + carry-forward — `src/index/pipeline/output.rs` (P2)

`write_output_locked` builds the sidecar from the final `parsed` set,
using each `ParsedFile.trigram_bloom` provenance:

- **`Some(bloom)`** ⟺ freshly parsed this run (read path or cache hit).
  Emit a fresh record: pair the bloom with a live `stat()` for
  `(len, mtime)`. A stat/mtime failure drops the record (→ absent → grep
  full-reads → safe).
- **`None`** ⟺ reconstructed from the prior index during `vex update`
  (no bytes read). Carry the **old sidecar record forward verbatim**.

This matches the `run` (full: every file re-parsed → complete fresh
sidecar) vs `update` (incremental: changed files fresh, unchanged carried
forward) asymmetry, the same split as the `body_tokens` sidecar.

**Carry-forward must-fix:** a changed-but-unparseable file (binary,
>1 MB, parse panic, grammar failure) is dropped from `parsed` entirely, so
it's absent from the sidecar → grep full-reads it. A `None` here is
therefore *only ever* a genuinely-unchanged file whose old bloom is still
valid — a stale bloom is never carried forward for changed content. Write
is best-effort: a save failure warns and leaves grep to full-walk.

## Limitations (frame P4 bench + LIMITATIONS.md accordingly)

- **Code files only.** Only files with a supported extension get a
  `ParsedFile`, hence a bloom. grep walks *all* files (md, json, toml,
  logs, …); those have no record → full-read. The win is a fraction of
  grep-walked files, not of all files.
- **>1 MB and binary/minified files** are dropped by the parse pipeline →
  no record → full-read. No speedup on large files.
- **Literal-only patterns.** `(?i)`, character classes, alternation, and
  literals < 3 bytes fall back to a full walk (no skip-index).
- **Same-nanosecond, same-length edits** evade the staleness guard (see
  §2) — the standard mtime+size tradeoff.

## Phasing

- **P1 (done):** pure `src/grep/trigram.rs` — extraction + bloom + the
  no-false-negative proptest gate. `regex-syntax` promoted to a direct dep.
- **P2 (done):** `index.trigram` sidecar (`src/store/trigram.rs`),
  blob-cache v4 bloom slot, `ParsedFile.trigram_bloom` carrier,
  `parse_files` build + `output.rs` write/carry-forward,
  `config::trigram_path`. Integration tests cover fresh index, warm-cache
  re-index, and update carry-forward.
- **P3 (next):** wire `grep::search` to consume the sidecar — extract
  trigrams → load sidecar → filter files by bloom + `(len, mtime)`
  freshness → read only survivors; else full walk. Integration test incl.
  edit-then-grep-without-reindex and the Windows POSIX-key round-trip.
  Also fold in the `trigram_persisted` manifest flag + `vex status` line
  (deferred from P2 — the sibling `body_tokens_persisted` pattern; P2
  leaves sidecar health discoverable only via `path.exists()`).
- **P4:** criterion bench + tune `BLOOM_BITS`/`k`; document the win as
  code-files-only in `docs/LIMITATIONS.md`.
