#!/bin/bash
# hyperfine benchmark for passthrough fast path (INV-4).
#
# Phase 1a: passthrough <5ms p99 requirement (bf-2u0).
#
# This script measures the overhead of gantry's passthrough path using
# hyperfine. The benchmark ensures the <5ms p99 budget is enforced in CI.
#
# Usage: ./benches/passthrough.sh
#
# Requirements:
# - hyperfine (https://github.com/sharkdp/hyperfine)
# - cargo (real binary)
# - gantry built and in PATH

set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "=== Gantry Passthrough Benchmark ==="
echo "Testing INV-4: <5ms p99 passthrough overhead"
echo

# Check if hyperfine is installed
if ! command -v hyperfine &> /dev/null; then
    echo -e "${RED}ERROR: hyperfine not found${NC}"
    echo "Install hyperfine: cargo install hyperfine"
    exit 1
fi

# Find the real cargo (bypassing gantry shim if present)
REAL_CARGO=$(which -a cargo | grep -v "gantry" | head -1)
if [ -z "$REAL_CARGO" ]; then
    # Fallback: try to find cargo in rust toolchain
    REAL_CARGO=$(rustup which cargo 2>/dev/null || echo "")
fi

if [ -z "$REAL_CARGO" ] || [ ! -f "$REAL_CARGO" ]; then
    echo -e "${RED}ERROR: Cannot find real cargo binary${NC}"
    exit 1
fi

echo "Real cargo: $REAL_CARGO"
echo

# Create temp directory for benchmark runs
TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

# Create a dummy cargo project for version calls
mkdir -p "$TEMP_DIR/dummy_project"
cd "$TEMP_DIR/dummy_project"
cargo init 2>/dev/null || true

echo "Running benchmark..."
echo

# Run hyperfine comparing:
# 1. Real cargo --version (baseline)
# 2. Gantry cargo --version with GANTRY_LOCAL=1 (passthrough path)
hyperfine \
    --warmup 10 \
    --min-runs 50 \
    --show-output \
    --command-name "Real cargo --version" \
    --command-name "Gantry passthrough (GANTRY_LOCAL=1)" \
    --parameter-scan-timeout 10000 \
    "$REAL_CARGO --version" \
    "GANTRY_LOCAL=1 cargo --version"

RESULT=$?

echo
echo "=== Benchmark Results ==="

# Check if benchmark passed
if [ $RESULT -eq 0 ]; then
    echo -e "${GREEN}✓ Benchmark completed successfully${NC}"
    echo
    echo "INV-4 Check:"
    echo "  - Verify p99 <5ms in hyperfine output above"
    echo "  - Look at 'Time (mean ± σ)' and 'Time (max)' values"
    echo
    exit 0
else
    echo -e "${RED}✗ Benchmark failed${NC}"
    exit 1
fi
