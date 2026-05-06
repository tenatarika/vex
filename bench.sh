#!/bin/bash
# Performance benchmark: vex vs ast-index
# Compares indexing time, search time, and index size

set -e

VEX="/Users/furcas/WORK/pets/vex/target/release/vex"
AST="ast-index"

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Test projects
SMALL="/Users/furcas/WORK/pets/vex"
MEDIUM="/Users/furcas/WORK/pets/Claude-ast-index-search"
LARGE="/Users/furcas/WORK/work/Interexy/Metrikus/metrikus-app"

bench_index() {
    local project="$1"
    local name="$2"
    local files=$(find "$project" -name "*.rs" -o -name "*.py" -o -name "*.go" -o -name "*.kt" -o -name "*.ts" -o -name "*.java" -o -name "*.cs" -o -name "*.rb" -o -name "*.swift" 2>/dev/null | grep -v node_modules | grep -v target | grep -v venv | grep -v __pycache__ | wc -l | tr -d ' ')

    echo -e "\n${YELLOW}=== $name ($files source files) ===${NC}"
    echo "Path: $project"
    echo ""

    # --- ast-index ---
    echo -e "${BLUE}[ast-index] Indexing...${NC}"
    # Clear ast-index cache
    rm -rf ~/Library/Caches/ast-index/* 2>/dev/null
    local ast_start=$(python3 -c 'import time; print(time.time())')
    (cd "$project" && $AST rebuild --verbose 2>&1) > /tmp/ast_bench.log 2>&1 || true
    local ast_end=$(python3 -c 'import time; print(time.time())')
    local ast_time=$(python3 -c "print(f'{($ast_end - $ast_start)*1000:.0f}')")
    local ast_symbols=$(cd "$project" && $AST stats --format json 2>/dev/null | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("symbol_count",0))' 2>/dev/null || echo "?")
    local ast_db_size=$(find ~/Library/Caches/ast-index/ -name "index.db" -newer /tmp/ast_bench.log 2>/dev/null | head -1 | xargs ls -lh 2>/dev/null | awk '{print $5}' || echo "?")
    echo -e "  Time:    ${GREEN}${ast_time}ms${NC}"
    echo "  Symbols: $ast_symbols"
    echo "  DB size: $ast_db_size"

    # --- vex ---
    echo -e "${BLUE}[vex] Indexing...${NC}"
    # Clear vex cache
    rm -rf ~/Library/Caches/vex/* 2>/dev/null
    local vex_start=$(python3 -c 'import time; print(time.time())')
    $VEX index --path "$project" > /tmp/vex_bench.log 2>&1 || true
    local vex_end=$(python3 -c 'import time; print(time.time())')
    local vex_time=$(python3 -c "print(f'{($vex_end - $vex_start)*1000:.0f}')")
    local vex_json=$($VEX status --path "$project" --format json 2>/dev/null)
    local vex_symbols=$(echo "$vex_json" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("symbols",0))' 2>/dev/null || echo "?")
    local vex_size=$(echo "$vex_json" | python3 -c 'import sys,json; b=json.load(sys.stdin).get("size_bytes",0); print(f"{b/1024:.1f}K")' 2>/dev/null || echo "?")
    echo -e "  Time:    ${GREEN}${vex_time}ms${NC}"
    echo "  Symbols: $vex_symbols"
    echo "  Index:   $vex_size"

    echo ""
    echo -e "  ${YELLOW}Speedup: $(python3 -c "print(f'{$ast_time/$vex_time:.1f}x')" 2>/dev/null || echo '?')${NC} (index)"
}

bench_search() {
    local project="$1"
    local name="$2"
    local query="$3"

    echo -e "\n${YELLOW}=== Search: \"$query\" in $name ===${NC}"

    # ast-index search
    local ast_start=$(python3 -c 'import time; print(time.time())')
    for i in $(seq 1 10); do
        (cd "$project" && $AST search "$query" > /dev/null 2>&1) || true
    done
    local ast_end=$(python3 -c 'import time; print(time.time())')
    local ast_avg=$(python3 -c "print(f'{($ast_end - $ast_start)*100:.1f}')")
    local ast_count=$(cd "$project" && $AST search "$query" 2>/dev/null | wc -l | tr -d ' ')

    # vex search
    local vex_start=$(python3 -c 'import time; print(time.time())')
    for i in $(seq 1 10); do
        $VEX search "$query" > /dev/null 2>&1 || true
    done
    local vex_end=$(python3 -c 'import time; print(time.time())')
    local vex_avg=$(python3 -c "print(f'{($vex_end - $vex_start)*100:.1f}')")
    local vex_count=$($VEX search "$query" --format json 2>/dev/null | python3 -c 'import sys,json; print(len(json.load(sys.stdin)))' 2>/dev/null || echo "?")

    echo -e "  ast-index: ${GREEN}${ast_avg}ms${NC} avg (10 runs), $ast_count results"
    echo -e "  vex:       ${GREEN}${vex_avg}ms${NC} avg (10 runs), $vex_count results"
    echo -e "  ${YELLOW}Speedup: $(python3 -c "r=$ast_avg/$vex_avg if $vex_avg>0 else 0; print(f'{r:.1f}x')" 2>/dev/null || echo '?')${NC}"
}

echo "============================================"
echo "  vex vs ast-index Performance Benchmark"
echo "============================================"
echo "vex:       $($VEX --version)"
echo "ast-index: $($AST --version)"
echo ""

# Index benchmarks
bench_index "$SMALL" "Small (vex — 2K lines Rust)"
bench_index "$MEDIUM" "Medium (ast-index — 31K lines Rust)"
bench_index "$LARGE" "Large (metrikus — 1247 Python files)"

# Search benchmarks (re-index for search)
(cd "$MEDIUM" && $AST rebuild > /dev/null 2>&1) || true
$VEX index --path "$MEDIUM" > /dev/null 2>&1

bench_search "$MEDIUM" "ast-index project" "search"
bench_search "$MEDIUM" "ast-index project" "SymbolKind"
bench_search "$MEDIUM" "ast-index project" "parse_file"

echo ""
echo "============================================"
echo "  Benchmark complete"
echo "============================================"
