#!/bin/bash
#
# NFS Server Performance Comparison Benchmark
#
# Compares baseline vs optimized NFS server implementations
# Tests: sequential writes, random writes, directory operations, concurrent access
#
# Platform: macOS compatible
#

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
BASELINE_BIN="../spdk-csi-driver/target/release/flint-nfs-server.baseline"
OPTIMIZED_BIN="../spdk-csi-driver/target/release/flint-nfs-server.optimized"
EXPORT_DIR="/tmp/nfs-benchmark-export"
MOUNT_POINT="/tmp/nfs-benchmark-mount"
NFS_PORT=2049
RESULTS_FILE="benchmark-results-$(date +%Y%m%d-%H%M%S).txt"

# Detect OS
OS_TYPE=$(uname -s)

# Ensure we're running from scripts directory
cd "$(dirname "$0")"

echo -e "${BLUE}╔═══════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║     Flint NFS Server Performance Comparison Benchmark     ║${NC}"
echo -e "${BLUE}╚═══════════════════════════════════════════════════════════╝${NC}"
echo ""

# Check if binaries exist
if [ ! -f "$BASELINE_BIN" ]; then
    echo -e "${RED}❌ Error: Baseline binary not found at $BASELINE_BIN${NC}"
    exit 1
fi

if [ ! -f "$OPTIMIZED_BIN" ]; then
    echo -e "${RED}❌ Error: Optimized binary not found at $OPTIMIZED_BIN${NC}"
    exit 1
fi

# Check if running as root (needed for NFS mount)
if [ "$OS_TYPE" = "Linux" ]; then
    if [ "$EUID" -ne 0 ]; then 
        echo -e "${YELLOW}⚠️  This script needs root privileges for mounting NFS${NC}"
        echo "Please run with: sudo $0"
        exit 1
    fi
elif [ "$OS_TYPE" = "Darwin" ]; then
    # macOS - check if we can mount (sudo will be asked when needed)
    echo -e "${BLUE}ℹ️  Running on macOS - sudo may be requested for NFS mount${NC}"
fi

# Setup
setup_test() {
    echo -e "${YELLOW}🔧 Setting up test environment...${NC}"
    
    # Create directories
    mkdir -p "$EXPORT_DIR"
    mkdir -p "$MOUNT_POINT"
    
    # Unmount if already mounted
    umount "$MOUNT_POINT" 2>/dev/null || true
    
    # Clean export directory
    rm -rf "$EXPORT_DIR"/*
    
    # Kill any existing NFS servers on our port
    lsof -ti :$NFS_PORT | xargs kill -9 2>/dev/null || true
    sleep 1
    
    echo -e "${GREEN}✅ Environment ready${NC}"
}

# Cleanup
cleanup() {
    echo -e "${YELLOW}🧹 Cleaning up...${NC}"
    
    # Unmount
    umount "$MOUNT_POINT" 2>/dev/null || true
    
    # Stop NFS server
    kill $NFS_SERVER_PID 2>/dev/null || true
    
    # Wait for port to be free
    sleep 2
    
    echo -e "${GREEN}✅ Cleanup complete${NC}"
}

# Start NFS server
start_nfs_server() {
    local binary=$1
    local version=$2
    
    echo -e "${YELLOW}🚀 Starting NFS server ($version)...${NC}"
    
    "$binary" \
        --export-path "$EXPORT_DIR" \
        --volume-id "benchmark-test" \
        --bind-addr "127.0.0.1" \
        --port $NFS_PORT \
        > /tmp/nfs-server-$version.log 2>&1 &
    
    NFS_SERVER_PID=$!
    
    # Wait for server to be ready
    echo -n "Waiting for server to start"
    for i in {1..10}; do
        if lsof -i :$NFS_PORT >/dev/null 2>&1; then
            echo " ✓"
            sleep 2  # Give it a moment to fully initialize
            return 0
        fi
        echo -n "."
        sleep 1
    done
    
    echo -e "${RED} ✗${NC}"
    echo -e "${RED}❌ Failed to start NFS server${NC}"
    cat /tmp/nfs-server-$version.log
    exit 1
}

# Mount NFS
mount_nfs() {
    echo -e "${YELLOW}📁 Mounting NFS...${NC}"
    
    if [ "$OS_TYPE" = "Darwin" ]; then
        # macOS NFS mount
        mount -t nfs -o vers=3,tcp,rw,soft,timeo=10,retrans=2 127.0.0.1:/ "$MOUNT_POINT"
    else
        # Linux NFS mount
        mount -t nfs -o vers=3,tcp,rw,soft,timeo=10 127.0.0.1:/ "$MOUNT_POINT"
    fi
    
    if [ $? -eq 0 ]; then
        echo -e "${GREEN}✅ NFS mounted at $MOUNT_POINT${NC}"
    else
        echo -e "${RED}❌ Failed to mount NFS${NC}"
        echo -e "${YELLOW}💡 Note: macOS may block NFS server on localhost. Try:${NC}"
        echo "   System Preferences > Security & Privacy > Firewall"
        exit 1
    fi
}

# Clear caches (OS-specific)
clear_caches() {
    if [ "$OS_TYPE" = "Darwin" ]; then
        # macOS - purge command
        sync
        sudo purge 2>/dev/null || true
    else
        # Linux
        sync
        echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
    fi
}

# Test 1: Sequential Write Performance
test_sequential_write() {
    local version=$1
    echo ""
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}Test 1: Sequential Write (1GB file)${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    
    clear_caches
    
    local result=$(dd if=/dev/zero of="$MOUNT_POINT/sequential_write_test" bs=1m count=1024 2>&1 | tail -1)
    local throughput=$(echo "$result" | grep -oE '[0-9.]+ [MG]B/sec' | head -1)
    
    echo -e "Result: ${GREEN}$throughput${NC}"
    echo "$version - Sequential Write (1GB): $throughput" >> "$RESULTS_FILE"
    
    rm -f "$MOUNT_POINT/sequential_write_test"
}

# Test 2: Random Write Performance
test_random_write() {
    local version=$1
    echo ""
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}Test 2: Random Write (100MB, 4K blocks)${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    
    clear_caches
    
    # Check if fio is available
    if command -v fio &> /dev/null; then
        local result=$(fio --name=randwrite \
            --ioengine=sync \
            --rw=randwrite \
            --bs=4k \
            --size=100M \
            --numjobs=1 \
            --filename="$MOUNT_POINT/random_write_test" \
            --group_reporting \
            --output-format=normal 2>&1 | grep "write: IOPS")
        
        local iops=$(echo "$result" | grep -oE 'IOPS=[0-9]+' | cut -d= -f2)
        local bw=$(echo "$result" | grep -oE 'BW=[0-9.]+[MK]iB/s' | cut -d= -f2)
        
        echo -e "Result: ${GREEN}IOPS=$iops, BW=$bw${NC}"
        echo "$version - Random Write (4K): IOPS=$iops, BW=$bw" >> "$RESULTS_FILE"
        
        rm -f "$MOUNT_POINT/random_write_test"
    else
        echo -e "${YELLOW}⚠️  fio not installed, skipping random write test${NC}"
        echo -e "${YELLOW}   Install with: brew install fio${NC}"
        echo "$version - Random Write: SKIPPED (fio not installed)" >> "$RESULTS_FILE"
    fi
}

# Test 3: Small File Creation
test_small_files() {
    local version=$1
    echo ""
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}Test 3: Small File Creation (1000 files)${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    
    clear_caches
    
    local start=$(gdate +%s.%N 2>/dev/null || date +%s)
    for i in {1..1000}; do
        echo "test" > "$MOUNT_POINT/small_file_$i"
    done
    sync
    local end=$(gdate +%s.%N 2>/dev/null || date +%s)
    
    local duration=$(echo "$end - $start" | awk '{printf "%.3f", $1}')
    local ops_per_sec=$(echo "$duration" | awk '{printf "%.0f", 1000 / $1}')
    
    echo -e "Result: ${GREEN}${duration}s (${ops_per_sec} ops/sec)${NC}"
    echo "$version - Small File Creation: ${duration}s (${ops_per_sec} ops/sec)" >> "$RESULTS_FILE"
    
    rm -f "$MOUNT_POINT"/small_file_*
}

# Test 4: Directory Listing (READDIRPLUS)
test_directory_listing() {
    local version=$1
    echo ""
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}Test 4: Directory Listing (1000 files)${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    
    # Create test files
    mkdir -p "$MOUNT_POINT/dirtest"
    for i in {1..1000}; do
        touch "$MOUNT_POINT/dirtest/file_$i"
    done
    sync
    
    # Clear caches
    clear_caches
    
    # Time directory listing
    local start=$(gdate +%s.%N 2>/dev/null || date +%s)
    ls -la "$MOUNT_POINT/dirtest" > /dev/null
    local end=$(gdate +%s.%N 2>/dev/null || date +%s)
    
    local duration=$(echo "$end - $start" | awk '{printf "%.3f", $1}')
    
    echo -e "Result: ${GREEN}${duration}s${NC}"
    echo "$version - Directory Listing (1000 files): ${duration}s" >> "$RESULTS_FILE"
    
    rm -rf "$MOUNT_POINT/dirtest"
}

# Test 5: Concurrent Writes
test_concurrent_writes() {
    local version=$1
    local num_concurrent=$2
    echo ""
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}Test 5: Concurrent Writes ($num_concurrent parallel)${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    
    clear_caches
    
    local start=$(gdate +%s.%N 2>/dev/null || date +%s)
    for i in $(seq 1 $num_concurrent); do
        dd if=/dev/zero of="$MOUNT_POINT/concurrent_$i" bs=1m count=50 2>/dev/null &
    done
    wait
    local end=$(gdate +%s.%N 2>/dev/null || date +%s)
    
    local duration=$(echo "$end - $start" | awk '{printf "%.3f", $1}')
    local total_mb=$(echo "$num_concurrent * 50" | awk '{print $1}')
    local throughput=$(echo "$total_mb $duration" | awk '{printf "%.0f", $1 / $2}')
    
    echo -e "Result: ${GREEN}${throughput} MB/s aggregate (${duration}s for ${total_mb}MB)${NC}"
    echo "$version - Concurrent Writes ($num_concurrent): ${throughput} MB/s (${duration}s)" >> "$RESULTS_FILE"
    
    rm -f "$MOUNT_POINT"/concurrent_*
}

# Run all tests for a version
run_benchmark_suite() {
    local binary=$1
    local version=$2
    
    echo ""
    echo -e "${GREEN}╔═══════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║           Testing: $version Version                       ${NC}"
    echo -e "${GREEN}╚═══════════════════════════════════════════════════════════╝${NC}"
    
    start_nfs_server "$binary" "$version"
    sleep 2
    mount_nfs
    
    test_sequential_write "$version"
    test_random_write "$version"
    test_small_files "$version"
    test_directory_listing "$version"
    test_concurrent_writes "$version" 5
    test_concurrent_writes "$version" 10
    
    cleanup
}

# Main execution
main() {
    setup_test
    
    # Check for gdate on macOS
    if [ "$OS_TYPE" = "Darwin" ]; then
        if ! command -v gdate &> /dev/null; then
            echo -e "${YELLOW}⚠️  GNU coreutils not installed (needed for nanosecond timing)${NC}"
            echo -e "${YELLOW}   Install with: brew install coreutils${NC}"
            echo -e "${YELLOW}   Tests will use second-level precision instead.${NC}"
            echo ""
        fi
    fi
    
    # Initialize results file
    echo "# Flint NFS Server Performance Comparison" > "$RESULTS_FILE"
    echo "# Date: $(date)" >> "$RESULTS_FILE"
    echo "# System: $(uname -a)" >> "$RESULTS_FILE"
    echo "# Platform: $OS_TYPE" >> "$RESULTS_FILE"
    echo "" >> "$RESULTS_FILE"
    
    # Run benchmarks
    run_benchmark_suite "$BASELINE_BIN" "BASELINE"
    echo ""
    sleep 3
    run_benchmark_suite "$OPTIMIZED_BIN" "OPTIMIZED"
    
    # Summary
    echo ""
    echo -e "${GREEN}╔═══════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║                  Benchmark Complete!                      ║${NC}"
    echo -e "${GREEN}╚═══════════════════════════════════════════════════════════╝${NC}"
    echo ""
    echo -e "${BLUE}📊 Results Summary:${NC}"
    echo ""
    cat "$RESULTS_FILE"
    echo ""
    echo -e "${GREEN}✅ Full results saved to: $RESULTS_FILE${NC}"
    echo ""
    
    # Calculate improvements
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}Performance Improvements Summary${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo "See $RESULTS_FILE for detailed numbers"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
}

# Trap cleanup on exit
trap cleanup EXIT INT TERM

# Run main
main "$@"

