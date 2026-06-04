#!/bin/bash
# Generate seed corpus for fuzz_index_reader from existing test fixtures.
# Run from repo root: bash fuzz/generate_seeds.sh

set -e

CORPUS="fuzz/corpus/fuzz_index_reader"
mkdir -p "$CORPUS"

# Seed 1: minimal valid header (empty index, no symbols)
python3 -c "
import struct, sys
magic = b'VEXI'
version = struct.pack('<I', 3)
sym_count = struct.pack('<Q', 0)
vec_dim = struct.pack('<I', 384)
padding = struct.pack('<I', 0)
# Header is 192 bytes total — fill offsets with zeros
header = magic + version + sym_count + vec_dim + padding
header += b'\x00' * (192 - len(header))
sys.stdout.buffer.write(header)
" > "$CORPUS/seed_empty"

# Seed 2: valid header with bad symbol count (will pass open, but symbol reads return None)
python3 -c "
import struct, sys
magic = b'VEXI'
version = struct.pack('<I', 3)
sym_count = struct.pack('<Q', 9999)  # claims 9999 symbols but no data
vec_dim = struct.pack('<I', 384)
padding = struct.pack('<I', 0)
header = magic + version + sym_count + vec_dim + padding
header += b'\x00' * (192 - len(header))
sys.stdout.buffer.write(header)
" > "$CORPUS/seed_bad_count"

# Seed 3: just the magic bytes (truncated)
echo -n "VEXI" > "$CORPUS/seed_truncated"

# Seed 4: random garbage
dd if=/dev/urandom bs=512 count=1 of="$CORPUS/seed_random" 2>/dev/null

# Seed 5: valid magic + old version
python3 -c "
import struct, sys
magic = b'VEXI'
version = struct.pack('<I', 2)  # old version
sym_count = struct.pack('<Q', 0)
vec_dim = struct.pack('<I', 384)
padding = struct.pack('<I', 0)
header = magic + version + sym_count + vec_dim + padding
header += b'\x00' * (192 - len(header))
sys.stdout.buffer.write(header)
" > "$CORPUS/seed_v2"

# Seed 6+: copy real index files from local vex cache (if available)
VEX_CACHE="${HOME}/Library/Caches/vex"
if [ -d "$VEX_CACHE" ]; then
    count=0
    for idx in "$VEX_CACHE"/*/index.vex; do
        [ -f "$idx" ] || continue
        size=$(wc -c < "$idx")
        # Skip tiny (< 1KB) and huge (> 20MB) indexes
        if [ "$size" -gt 1024 ] && [ "$size" -lt 20971520 ]; then
            hash=$(basename "$(dirname "$idx")")
            cp "$idx" "$CORPUS/seed_cache_${hash}"
            count=$((count + 1))
        fi
    done
    [ "$count" -gt 0 ] && echo "Copied $count real indexes from vex cache"
fi

echo "Generated $(ls "$CORPUS" | wc -l | tr -d ' ') seeds in $CORPUS"
ls -lh "$CORPUS"

# ---------------------------------------------------------------------
# fuzz_bloom_load seed corpus (v1.12.0 T4)
# ---------------------------------------------------------------------

BLOOM_CORPUS="fuzz/corpus/fuzz_bloom_load"
mkdir -p "$BLOOM_CORPUS"

python3 - <<'PY'
import os, struct
CORPUS = "fuzz/corpus/fuzz_bloom_load"

def write(name, payload):
    open(os.path.join(CORPUS, name), "wb").write(payload)

# Empty file — load returns Err(truncated).
write("seed_empty", b"")

# Header w/ valid VEXB magic but otherwise zeroed → degenerate n_bits/k_num
# regression (fuzz_bloom_load found this; load now rejects).
write("seed_zero_header", b"VEXB" + struct.pack("<I", 1) + b"\x00" * 56)

# Valid magic + version, mismatched n_bits/bitmap_len → consistency guard.
write(
    "seed_truncated_bitmap",
    b"VEXB"
    + struct.pack("<I", 1)
    + struct.pack("<Q", 64)
    + struct.pack("<I", 1)
    + b"\x00" * 4
    + b"\x00" * 32
    + struct.pack("<Q", 8),
)

# Minimal valid sidecar — load succeeds, drives the may_contain path.
write(
    "seed_valid_minimal",
    b"VEXB"
    + struct.pack("<I", 1)
    + struct.pack("<Q", 64)
    + struct.pack("<I", 1)
    + b"\x00" * 4
    + b"\x00" * 32
    + struct.pack("<Q", 8)
    + b"\xff" * 8,
)

# Implausibly large bitmap_len → MAX_BITMAP_LEN guard.
write(
    "seed_oversized_bitmap_len",
    b"VEXB"
    + struct.pack("<I", 1)
    + struct.pack("<Q", 0)
    + struct.pack("<I", 0)
    + b"\x00" * 4
    + b"\x00" * 32
    + struct.pack("<Q", 2 ** 40),
)

# Huge k_num (DoS regression from fuzz_bloom_load) → MAX_K_NUM guard.
write(
    "seed_regression_huge_knum",
    b"VEXB"
    + struct.pack("<I", 1)
    + struct.pack("<Q", 64)
    + struct.pack("<I", 0x7E000001)
    + b"\x00" * 4
    + b"\x00" * 32
    + struct.pack("<Q", 8)
    + b"\xff" * 8,
)

# Future-version sidecar → version-mismatch bail.
write("seed_future_version", b"VEXB" + struct.pack("<I", 999) + b"\x00" * 56)

print(f"Generated {len(os.listdir(CORPUS))} bloom seeds in {CORPUS}")
PY
ls -lh "$BLOOM_CORPUS"

# ---------------------------------------------------------------------
# fuzz_pattern_parser seed corpus (v1.12.0 round-2 fuzz pass)
# ---------------------------------------------------------------------

PATTERN_CORPUS="fuzz/corpus/fuzz_pattern_parser"
mkdir -p "$PATTERN_CORPUS"

# Valid Phase 11.4 examples — get the parser past the empty-input bail
# and into the metavar/composition paths.
echo -n "fn \$NAME(\$\$\$BODY)" > "$PATTERN_CORPUS/seed_metavars"
echo -n "fn \$N() || struct \$N" > "$PATTERN_CORPUS/seed_or_composition"
echo -n "impl \$T && fn \$M" > "$PATTERN_CORPUS/seed_and_composition"
echo -n "let x = \"a && b\"" > "$PATTERN_CORPUS/seed_string_with_operator"
echo -n '$X.then($X)' > "$PATTERN_CORPUS/seed_backref"
# Degenerate / boundary inputs the parser must reject without panicking.
: > "$PATTERN_CORPUS/seed_empty"
echo -n "   " > "$PATTERN_CORPUS/seed_whitespace_only"
echo -n "||" > "$PATTERN_CORPUS/seed_just_or"
echo -n "&&" > "$PATTERN_CORPUS/seed_just_and"
echo -n "((((" > "$PATTERN_CORPUS/seed_unbalanced_paren"

echo "Generated $(ls "$PATTERN_CORPUS" | wc -l | tr -d ' ') pattern seeds in $PATTERN_CORPUS"

# ---------------------------------------------------------------------
# fuzz_manifest_load seed corpus
# ---------------------------------------------------------------------

MANIFEST_CORPUS="fuzz/corpus/fuzz_manifest_load"
mkdir -p "$MANIFEST_CORPUS"

: > "$MANIFEST_CORPUS/seed_empty"
echo -n "{}" > "$MANIFEST_CORPUS/seed_empty_object"
echo -n '{"files":{}}' > "$MANIFEST_CORPUS/seed_empty_files"
# Minimal-but-valid-ish manifest — should round-trip via serde.
cat > "$MANIFEST_CORPUS/seed_minimal_valid" <<'JSON'
{
  "files": {"src/lib.rs": "deadbeef"},
  "git_head": "abc123",
  "indexed_at": 1700000000,
  "embedder_id": null,
  "call_graph": true,
  "bm25": true,
  "pattern_index": true,
  "pattern_index_full": true
}
JSON
# Malformed / truncated variants the loader must reject gracefully.
echo -n '{"files": {' > "$MANIFEST_CORPUS/seed_truncated_json"
echo -n 'not json at all' > "$MANIFEST_CORPUS/seed_plaintext"

echo "Generated $(ls "$MANIFEST_CORPUS" | wc -l | tr -d ' ') manifest seeds in $MANIFEST_CORPUS"
