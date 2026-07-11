#!/bin/bash
# Vex Benchmark Suite
#
# Compares vex vs ast-index on indexing, search, usages, and pattern matching.
# Results saved to benches/results/ (gitignored).
#
# Usage:
#   ./benches/bench.sh                    # run all benchmarks
#   ./benches/bench.sh --quick            # skip large projects
#   ./benches/bench.sh --project /path    # benchmark a specific project
#
# Requirements:
#   - vex built in release mode: cargo build --release
#   - ast-index installed: brew tap defendend/ast-index && brew install ast-index
#   - python3 available (for timing)

set -e

VEX="${VEX:-$(dirname "$0")/../target/release/vex}"
AST="${AST:-ast-index}"
RESULTS_DIR="$(dirname "$0")/results"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
RESULT_FILE="$RESULTS_DIR/bench_$TIMESTAMP.txt"

mkdir -p "$RESULTS_DIR"

# Check binaries
if [ ! -f "$VEX" ]; then
    echo "Error: vex not found at $VEX"
    echo "Run: cargo build --release"
    exit 1
fi

if ! command -v "$AST" &>/dev/null; then
    echo "Warning: ast-index not found, skipping comparison"
    AST=""
fi

RG="${RG:-rg}"
if ! command -v "$RG" &>/dev/null; then
    echo "Warning: ripgrep (rg) not found, skipping grep comparison"
    RG=""
fi

# Parse args
QUICK=false
CUSTOM_PROJECT=""
while [[ $# -gt 0 ]]; do
    case $1 in
        --quick) QUICK=true; shift ;;
        --project) CUSTOM_PROJECT="$2"; shift 2 ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

# Output to both terminal and file
log() {
    echo "$@" | tee -a "$RESULT_FILE"
}

bench_py() {
    python3 -c "$1"
}

# --- Header ---
log "============================================"
log "  Vex Benchmark — $(date)"
log "  vex: $($VEX --version)"
[ -n "$AST" ] && log "  ast-index: $($AST --version)"
log "============================================"
log ""

# --- Indexing + Search for a project ---
bench_project() {
    local project="$1"
    local label="$2"

    log "=== $label ==="
    log "  Path: $project"

    # vex
    rm -rf ~/Library/Caches/vex/* 2>/dev/null
    local vex_ms=$(bench_py "
import subprocess, time
start = time.time()
subprocess.run(['$VEX', 'index', '--path', '$project'], capture_output=True)
print(f'{(time.time()-start)*1000:.0f}')
")
    local vex_json=$($VEX status --path "$project" --format json 2>/dev/null)
    # `vex status --format json` uses the standard envelope
    # ({protocol_version, capabilities, _meta, results}); the counters live
    # under `results`. Fall back to the flat shape for a pre-envelope vex.
    local vex_syms=$(echo "$vex_json" | python3 -c "import sys,json; d=json.load(sys.stdin); r=d.get('results',d); print(r.get('symbols',0))" 2>/dev/null)
    local vex_size=$(echo "$vex_json" | python3 -c "import sys,json; d=json.load(sys.stdin); r=d.get('results',d); b=r.get('size_bytes',0); print(f'{b/1024:.0f}K' if b<1048576 else f'{b/1048576:.1f}M')" 2>/dev/null)
    log "  vex:       ${vex_ms}ms  ${vex_syms} symbols  ${vex_size}"

    # ast-index
    if [ -n "$AST" ]; then
        rm -rf ~/Library/Caches/ast-index/* 2>/dev/null
        local ast_ms=$(bench_py "
import subprocess, time
start = time.time()
subprocess.run(['$AST', 'rebuild'], capture_output=True, cwd='$project')
print(f'{(time.time()-start)*1000:.0f}')
")
        local ast_stats=$(cd "$project" && $AST stats 2>/dev/null)
        local ast_syms=$(echo "$ast_stats" | grep "Symbols:" | awk '{print $2}')
        local ast_size=$(echo "$ast_stats" | grep "DB size:" | awk '{print $3, $4}')
        log "  ast-index: ${ast_ms}ms  ${ast_syms} symbols  ${ast_size}"

        local speedup=$(python3 -c "print(f'{$ast_ms/$vex_ms:.1f}x')" 2>/dev/null)
        log "  speedup:   ${speedup} (indexing)"
    fi
    log ""
}

# --- Search benchmark ---
bench_search() {
    local project="$1"
    local label="$2"
    shift 2
    local queries=("$@")

    log "=== Search: $label (avg 10 runs) ==="

    # Ensure indexes exist
    $VEX index --path "$project" > /dev/null 2>&1
    [ -n "$AST" ] && (cd "$project" && $AST rebuild > /dev/null 2>&1)

    for query in "${queries[@]}"; do
        local result=$(bench_py "
import subprocess, time

def bench(cmd, cwd=None, n=10):
    start = time.time()
    for _ in range(n):
        subprocess.run(cmd, capture_output=True, cwd=cwd)
    return (time.time() - start) / n * 1000

q = '$query'
vt = bench(['$VEX', 'search', q])
line = f'  \"{q}\"  vex: {vt:.1f}ms'

ast_str = ''
if '$AST':
    try:
        at = bench(['$AST', 'search', q], cwd='$project')
        ast_str = f'  ast: {at:.1f}ms'
    except:
        pass

rg_str = ''
if '$RG':
    try:
        rt = bench(['$RG', '-w', '--no-heading', q, '$project'])
        sp = f'{rt/vt:.1f}x' if vt > 0 else '?'
        rg_str = f'  rg: {rt:.1f}ms ({sp} vs vex)'
    except:
        pass

print(line + ast_str + rg_str)
")
        log "$result"
    done
    log ""
}

# --- Grep comparison (raw text search) ---
bench_grep() {
    local project="$1"
    local label="$2"
    shift 2
    local patterns=("$@")

    [ -z "$RG" ] && return

    log "=== Grep: $label (avg 10 runs) ==="
    log "  vex search (symbol index) vs rg (raw text scan)"

    $VEX index --path "$project" > /dev/null 2>&1

    for pattern in "${patterns[@]}"; do
        local result=$(bench_py "
import subprocess, time

def bench(cmd, n=10):
    start = time.time()
    for _ in range(n):
        subprocess.run(cmd, capture_output=True)
    return (time.time() - start) / n * 1000

def count_lines(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    return len([l for l in r.stdout.splitlines() if l.strip()])

p = '$pattern'

# vex symbol search (indexed)
vt = bench(['$VEX', 'search', p])
vc = count_lines(['$VEX', 'search', p])

# rg word search (full scan)
rt = bench(['$RG', '-w', '--no-heading', '-c', p, '$project'])
rc_out = subprocess.run(['$RG', '-w', '--no-heading', '-c', p, '$project'], capture_output=True, text=True)
rc = sum(int(l.split(':')[-1]) for l in rc_out.stdout.splitlines() if l.strip())

# rg with file type filter
rt_typed = bench(['$RG', '-w', '--no-heading', '-c', '-t', 'rust', '-t', 'py', '-t', 'go', '-t', 'java', '-t', 'kotlin', '-t', 'ts', '-t', 'js', '-t', 'cs', '-t', 'ruby', '-t', 'swift', p, '$project'])

sp = f'{rt/vt:.1f}x' if vt > 0 else '?'
print(f'  \"{p}\"')
print(f'    vex search: {vt:.1f}ms  ({vc} symbol results)')
print(f'    rg -w:      {rt:.1f}ms  ({rc} text matches)')
print(f'    rg -t:      {rt_typed:.1f}ms  (filtered by lang)')
print(f'    ratio:      vex {sp} faster than rg')
")
        log "$result"
    done
    log ""
}

# --- Pattern benchmark ---
bench_pattern() {
    local project="$1"
    local label="$2"
    shift 2

    log "=== Pattern: $label ==="

    while [[ $# -gt 0 ]]; do
        local pattern="$1"
        local lang="$2"
        shift 2

        local result=$(bench_py "
import subprocess, time
pat = '$pattern'
lng = '$lang'
start = time.time()
r = subprocess.run(['$VEX', 'pattern', pat, '--lang', lng, '--path', '$project'], capture_output=True, text=True)
ms = (time.time() - start) * 1000
lines = [l for l in r.stdout.splitlines() if 'matches' in l]
count = lines[0].split()[0] if lines else '?'
print(f'  \"{pat}\" --lang {lng}  {ms:.0f}ms  {count} matches')
")
        log "$result"
    done
    log ""
}

# --- Run benchmarks ---

if [ -n "$CUSTOM_PROJECT" ]; then
    bench_project "$CUSTOM_PROJECT" "Custom: $(basename "$CUSTOM_PROJECT")"
    bench_search "$CUSTOM_PROJECT" "$(basename "$CUSTOM_PROJECT")" "search" "Service" "Config"
    bench_grep "$CUSTOM_PROJECT" "$(basename "$CUSTOM_PROJECT")" "Service" "Config" "Error"
else
    # Default projects
    SELF="$(cd "$(dirname "$0")/.." && pwd)"
    bench_project "$SELF" "Small (vex itself)"

    AST_INDEX_DIR="$SELF/../Claude-ast-index-search"
    if [ -d "$AST_INDEX_DIR" ]; then
        bench_project "$AST_INDEX_DIR" "Medium (ast-index, 31K Rust)"
        bench_search "$AST_INDEX_DIR" "Medium" "search" "SymbolKind" "parse_file" "IndexReader"
        bench_grep "$AST_INDEX_DIR" "Medium" "search" "SymbolKind" "parse_file"
        bench_pattern "$AST_INDEX_DIR" "Medium" \
            'fn $NAME($$$) -> Result' rust \
            'pub struct $NAME' rust \
            'fn $NAME($$$)' rust
    fi

    if [ "$QUICK" = false ] && [ -n "${VEX_BENCH_LARGE_PROJECTS:-}" ]; then
        for large_dir in $VEX_BENCH_LARGE_PROJECTS; do
            if [ -d "$large_dir" ]; then
                bench_project "$large_dir" "Large ($(basename "$large_dir"))"
                bench_grep "$large_dir" "Large" "Service" "Repository" "Config"
            fi
        done
    fi
fi

log "============================================"
log "  Results saved to: $RESULT_FILE"
log "============================================"
