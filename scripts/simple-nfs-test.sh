#!/bin/bash
#
# Simple NFS Performance Test - Step by Step
#

set -e

BASELINE_BIN="./target/release/flint-nfs-server.baseline"
OPTIMIZED_BIN="./target/release/flint-nfs-server.optimized"
EXPORT_DIR="./target/nfs-test-export"

cd "$(dirname "$0")/../spdk-csi-driver"

echo "=== NFS Performance Test ==="
echo ""

# Cleanup
pkill -f flint-nfs-server || true
rm -rf "$EXPORT_DIR"
mkdir -p "$EXPORT_DIR"
sleep 1

# Function to test one version
test_version() {
    local binary=$1
    local version=$2
    
    echo "Testing $version..."
    
    # Start server
    "$binary" --export-path "$EXPORT_DIR" --volume-id test \
        --bind-addr 127.0.0.1 --port 2049 > /tmp/nfs-$version.log 2>&1 &
    local pid=$!
    
    echo "  Server PID: $pid"
    sleep 3
    
    # Check if server is running
    if ! ps -p $pid > /dev/null; then
        echo "  ❌ Server failed to start!"
        cat /tmp/nfs-$version.log
        return 1
    fi
    
    echo "  ✓ Server started"
    
    # Simple write test: 3 files of 10MB each
    echo "  Writing test files..."
    local start=$(gdate +%s.%N 2>/dev/null || date +%s)
    
    dd if=/dev/zero of="$EXPORT_DIR/file1" bs=1m count=10 2>/dev/null &
    dd if=/dev/zero of="$EXPORT_DIR/file2" bs=1m count=10 2>/dev/null &
    dd if=/dev/zero of="$EXPORT_DIR/file3" bs=1m count=10 2>/dev/null &
    wait
    
    local end=$(gdate +%s.%N 2>/dev/null || date +%s)
    local duration=$(echo "$end - $start" | awk '{printf "%.2f", $1}')
    
    echo "  ✓ Completed in ${duration}s"
    echo "  Throughput: $(echo "30 / $duration" | awk '{printf "%.1f", $1}') MB/s"
    
    # Stop server
    kill $pid 2>/dev/null || true
    wait $pid 2>/dev/null || true
    
    # Cleanup files
    rm -f "$EXPORT_DIR"/*
    sleep 1
    
    echo "$duration"
}

# Test baseline
baseline_time=$(test_version "$BASELINE_BIN" "BASELINE")
echo ""

# Test optimized  
optimized_time=$(test_version "$OPTIMIZED_BIN" "OPTIMIZED")
echo ""

# Results
echo "=== Results ==="
echo "BASELINE:  ${baseline_time}s"
echo "OPTIMIZED: ${optimized_time}s"
improvement=$(echo "$baseline_time $optimized_time" | awk '{printf "%.2f", $1/$2}')
echo "Improvement: ${improvement}x faster"
echo ""

if (( $(echo "$improvement > 1.5" | bc -l 2>/dev/null || echo 0) )); then
    echo "✅ Optimization successful!"
else
    echo "⚠️  Little to no improvement (might be disk speed limited)"
fi

