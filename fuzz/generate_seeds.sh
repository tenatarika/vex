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

echo "Generated $(ls "$CORPUS" | wc -l | tr -d ' ') seeds in $CORPUS"
ls -la "$CORPUS"
