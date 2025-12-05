#!/bin/bash
# Minimal NFS Test - No backgrounding

cd "$(dirname "$0")/../spdk-csi-driver"

echo "=== Minimal NFS Performance Test ==="
echo ""

EXPORT_DIR="./target/nfs-test-export"
rm -rf "$EXPORT_DIR"
mkdir -p "$EXPORT_DIR"

# Test baseline
echo "1. Testing BASELINE version..."
pkill -f flint-nfs-server || true
sleep 1

./target/release/flint-nfs-server.baseline \
    --export-path "$EXPORT_DIR" \
    --volume-id test \
    --bind-addr 127.0.0.1 \
    --port 2049 > /tmp/nfs-baseline.log 2>&1 &

BASELINE_PID=$!
echo "   Server PID: $BASELINE_PID"
sleep 3

# Write test files sequentially (no backgrounding)
echo "   Writing files..."
time_start=$(perl -MTime::HiRes=time -e 'print time')

dd if=/dev/zero of="$EXPORT_DIR/file1" bs=1m count=10 2>/dev/null
dd if=/dev/zero of="$EXPORT_DIR/file2" bs=1m count=10 2>/dev/null
dd if=/dev/zero of="$EXPORT_DIR/file3" bs=1m count=10 2>/dev/null

time_end=$(perl -MTime::HiRes=time -e 'print time')
baseline_time=$(echo "$time_end $time_start" | awk '{printf "%.3f", $1-$2}')

echo "   Done in ${baseline_time}s"
kill $BASELINE_PID 2>/dev/null
wait $BASELINE_PID 2>/dev/null || true
rm -rf "$EXPORT_DIR"/*
sleep 2

# Test optimized
echo ""
echo "2. Testing OPTIMIZED version..."
./target/release/flint-nfs-server.optimized \
    --export-path "$EXPORT_DIR" \
    --volume-id test \
    --bind-addr 127.0.0.1 \
    --port 2049 > /tmp/nfs-optimized.log 2>&1 &

OPTIMIZED_PID=$!
echo "   Server PID: $OPTIMIZED_PID"
sleep 3

# Write test files sequentially
echo "   Writing files..."
time_start=$(perl -MTime::HiRes=time -e 'print time')

dd if=/dev/zero of="$EXPORT_DIR/file1" bs=1m count=10 2>/dev/null
dd if=/dev/zero of="$EXPORT_DIR/file2" bs=1m count=10 2>/dev/null
dd if=/dev/zero of="$EXPORT_DIR/file3" bs=1m count=10 2>/dev/null

time_end=$(perl -MTime::HiRes=time -e 'print time')
optimized_time=$(echo "$time_end $time_start" | awk '{printf "%.3f", $1-$2}')

echo "   Done in ${optimized_time}s"
kill $OPTIMIZED_PID 2>/dev/null
wait $OPTIMIZED_PID 2>/dev/null || true

# Results
echo ""
echo "=== RESULTS ==="
echo "BASELINE:  ${baseline_time}s ($(echo "30/$baseline_time" | awk '{printf "%.1f", $1}') MB/s)"
echo "OPTIMIZED: ${optimized_time}s ($(echo "30/$optimized_time" | awk '{printf "%.1f", $1}') MB/s)"
improvement=$(echo "$baseline_time $optimized_time" | awk '{if($2>0) printf "%.2f", $1/$2; else print "N/A"}')
echo "Speedup:   ${improvement}x"
echo ""
echo "Note: This test writes directly to the export directory, not through NFS protocol."
echo "      For true NFS testing, use the full benchmark with sudo mount."

