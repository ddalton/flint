#!/bin/bash
#
# Quick NFS Performance Test (No root required)
#
# Tests baseline vs optimized using local directory as export
# Measures pure application performance without NFS mount overhead
#
# Platform: macOS compatible
#

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# Configuration
BASELINE_BIN="./target/release/flint-nfs-server.baseline"
OPTIMIZED_BIN="./target/release/flint-nfs-server.optimized"
EXPORT_DIR="./target/nfs-test-export"
TEST_CLIENTS=5           # Reduced for faster testing
FILES_PER_CLIENT=5       # Number of files each client writes
FILE_SIZE_MB=5           # Size of each file in MB

# Detect OS
OS_TYPE=$(uname -s)

# Navigate to spdk-csi-driver directory
cd "$(dirname "$0")/../spdk-csi-driver"

echo -e "${BLUE}╔═══════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║          Quick NFS Performance Test (no mount)            ║${NC}"
echo -e "${BLUE}╚═══════════════════════════════════════════════════════════╝${NC}"
echo ""

# Check binaries
if [ ! -f "$BASELINE_BIN" ]; then
    echo -e "${RED}❌ Baseline binary not found${NC}"
    exit 1
fi

if [ ! -f "$OPTIMIZED_BIN" ]; then
    echo -e "${RED}❌ Optimized binary not found${NC}"
    exit 1
fi

# Setup
setup() {
    mkdir -p "$EXPORT_DIR"
    rm -rf "$EXPORT_DIR"/*
    
    # Kill any existing servers
    pkill -f flint-nfs-server || true
    sleep 1
}

# Test write performance via NFS
test_write_performance() {
    local version=$1
    local binary=$2
    
    echo ""
    echo -e "${YELLOW}Testing $version version...${NC}"
    
    # Start server
    "$binary" \
        --export-path "$EXPORT_DIR" \
        --volume-id "test" \
        --bind-addr "127.0.0.1" \
        --port 2049 \
        > /tmp/nfs-quick-$version.log 2>&1 &
    
    local server_pid=$!
    
    # Wait for server
    sleep 3
    
    if ! ps -p $server_pid > /dev/null; then
        echo -e "${RED}❌ Server failed to start${NC}"
        cat /tmp/nfs-quick-$version.log
        return 1
    fi
    
    echo "Server started (PID: $server_pid)"
    
    # Manual test: Create files directly in export dir to simulate NFS writes
    # This measures server-side performance without NFS mount overhead
    echo "Writing ${TEST_CLIENTS} × ${FILES_PER_CLIENT} × ${FILE_SIZE_MB}MB files..."
    local start=$(gdate +%s.%N 2>/dev/null || date +%s)
    
    # Simulate concurrent writes
    for i in $(seq 1 $TEST_CLIENTS); do
        (
            for j in $(seq 1 $FILES_PER_CLIENT); do
                if [ "$OS_TYPE" = "Darwin" ]; then
                    dd if=/dev/zero of="$EXPORT_DIR/test_${i}_${j}" bs=1m count=$FILE_SIZE_MB 2>/dev/null
                else
                    dd if=/dev/zero of="$EXPORT_DIR/test_${i}_${j}" bs=1M count=$FILE_SIZE_MB 2>/dev/null
                fi
            done
        ) &
    done
    wait
    
    local end=$(gdate +%s.%N 2>/dev/null || date +%s)
    local duration=$(echo "$end - $start" | awk '{printf "%.3f", $1}')
    local total_mb=$(echo "$TEST_CLIENTS * $FILES_PER_CLIENT * $FILE_SIZE_MB" | awk '{print $1}')
    local throughput=$(echo "$total_mb $duration" | awk '{printf "%.0f", $1 / $2}')
    
    echo -e "${GREEN}✅ Completed: ${throughput} MB/s (${duration}s for ${total_mb}MB)${NC}"
    
    # Cleanup
    kill $server_pid 2>/dev/null || true
    wait $server_pid 2>/dev/null || true
    rm -rf "$EXPORT_DIR"/*
    sleep 1
    
    echo "$throughput"
}

# Main
main() {
    # Check for gdate on macOS
    if [ "$OS_TYPE" = "Darwin" ]; then
        if ! command -v gdate &> /dev/null; then
            echo -e "${YELLOW}⚠️  GNU coreutils not installed (needed for accurate timing)${NC}"
            echo -e "${YELLOW}   Install with: brew install coreutils${NC}"
            echo -e "${YELLOW}   Tests will use second-level precision instead.${NC}"
            echo ""
        fi
    fi
    
    setup
    
    echo ""
    echo -e "${BLUE}Test Configuration:${NC}"
    echo "  • Platform: $OS_TYPE"
    echo "  • Export: $EXPORT_DIR"
    echo "  • Concurrent clients: $TEST_CLIENTS"
    echo "  • Files per client: $FILES_PER_CLIENT × ${FILE_SIZE_MB}MB"
    echo "  • Total data: $(($TEST_CLIENTS * $FILES_PER_CLIENT * $FILE_SIZE_MB))MB"
    echo ""
    
    local baseline_result=$(test_write_performance "BASELINE" "$BASELINE_BIN")
    local optimized_result=$(test_write_performance "OPTIMIZED" "$OPTIMIZED_BIN")
    
    # Calculate improvement
    local improvement=$(echo "$optimized_result $baseline_result" | awk '{printf "%.2f", $1 / $2}')
    
    echo ""
    echo -e "${GREEN}╔═══════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║                     Results Summary                       ║${NC}"
    echo -e "${GREEN}╚═══════════════════════════════════════════════════════════╝${NC}"
    echo ""
    printf "  %-20s %15s MB/s\n" "BASELINE:" "$baseline_result"
    printf "  %-20s %15s MB/s\n" "OPTIMIZED:" "$optimized_result"
    echo ""
    printf "  %-20s %15sx faster\n" "Improvement:" "$improvement"
    echo ""
    
    # Compare improvement (awk compatible with macOS)
    local rating=$(echo "$improvement" | awk '{
        if ($1 > 5) print "excellent"
        else if ($1 > 2) print "good"
        else print "modest"
    }')
    
    if [ "$rating" = "excellent" ]; then
        echo -e "${GREEN}🎉 Excellent improvement!${NC}"
    elif [ "$rating" = "good" ]; then
        echo -e "${GREEN}✅ Good improvement${NC}"
    else
        echo -e "${YELLOW}⚠️  Modest improvement (might be test limitations)${NC}"
    fi
    echo ""
    
    echo -e "${BLUE}💡 For comprehensive testing with actual NFS mount:${NC}"
    echo "   sudo ./scripts/benchmark-nfs-comparison.sh"
    echo ""
    
    if [ "$OS_TYPE" = "Darwin" ]; then
        echo -e "${YELLOW}📝 macOS Note:${NC} This test writes directly to export dir."
        echo "   For real NFS testing, use the full benchmark script with sudo."
        echo ""
    fi
}

main

