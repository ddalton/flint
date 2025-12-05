#!/bin/bash
# Bulletproof concurrent performance test for Linux
# Tests 10 clients writing different files concurrently

set -ex  # -e = exit on error, -x = print commands

EXPORT_DIR="/tmp/nfs-perf-export"
MOUNT_DIR="/tmp/nfs-perf-mount"

cd ~/flint/spdk-csi-driver

cleanup() {
    echo "Cleaning up..."
    pkill -f flint-nfs-server 2>/dev/null || true
    sleep 1
    # If still running, force kill
    pkill -9 -f flint-nfs-server 2>/dev/null || true
    umount -l $MOUNT_DIR 2>/dev/null || true
    umount -l $MOUNT_DIR 2>/dev/null || true  # Twice to catch double-mounts
    sleep 2
}

test_version() {
    local BINARY=$1
    local VERSION=$2
    
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "Testing $VERSION: 10 concurrent clients"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    
    cleanup
    rm -rf $EXPORT_DIR
    mkdir -p $EXPORT_DIR $MOUNT_DIR
    sleep 2
    
    # Start server with verbose logging
    echo "Starting NFS server..."
    $BINARY --export-path $EXPORT_DIR --volume-id test --verbose > /tmp/$VERSION-test.log 2>&1 &
    local SERVER_PID=$!
    echo "  Server PID: $SERVER_PID, logs: /tmp/$VERSION-test.log"
    sleep 5
    
    # Verify server running
    if ! ps -p $SERVER_PID > /dev/null; then
        echo "❌ Server failed to start!"
        cat /tmp/$VERSION-test.log
        return 1
    fi
    echo "✓ Server running"
    
    # Show recent server output
    echo "  Server status:"
    tail -3 /tmp/$VERSION-test.log | sed 's/^/    /'
    
    # Mount (always unmount first to prevent double-mount)
    umount $MOUNT_DIR 2>/dev/null || true
    mount -t nfs -o vers=3,tcp,soft,timeo=10 127.0.0.1:/ $MOUNT_DIR
    
    if ! mount | grep -q $MOUNT_DIR; then
        echo "❌ Mount failed!"
        return 1
    fi
    echo "✓ Mounted at $MOUNT_DIR"
    
    # Test: 10 clients writing different files
    echo "Starting 10 concurrent writers (100MB each)..."
    
    START=$(date +%s)
    
    # Launch background jobs
    for i in {0..9}; do
        dd if=/dev/zero of=$MOUNT_DIR/file_$i bs=1M count=100 status=none &
    done
    
    # Wait for all to complete
    wait
    
    END=$(date +%s)
    DURATION=$((END - START))
    
    # Check if server is still running
    if ! ps -p $SERVER_PID > /dev/null; then
        echo "❌ Server died during test!"
        echo "Server logs:"
        tail -30 /tmp/$VERSION-test.log | sed 's/^/    /'
        return 1
    fi
    
    echo "✓ Completed in ${DURATION}s"
    
    # Show any errors from server logs
    if grep -qi "error\|panic\|fatal" /tmp/$VERSION-test.log; then
        echo "⚠️  Server errors detected:"
        grep -i "error\|panic\|fatal" /tmp/$VERSION-test.log | tail -10 | sed 's/^/    /'
    fi
    
    # Calculate throughput
    THROUGHPUT=$((1000 / DURATION))
    echo "  Aggregate throughput: ${THROUGHPUT} MB/s"
    
    # Verify files were written
    FILE_COUNT=$(ls $MOUNT_DIR/file_* 2>/dev/null | wc -l)
    echo "  Files written: $FILE_COUNT/10"
    
    # Show server log summary
    echo ""
    echo "Server log summary (last 20 lines):"
    tail -20 /tmp/$VERSION-test.log | sed 's/^/  /'
    echo ""
    
    cleanup
    
    echo "$DURATION"
}

# Main
echo "╔═══════════════════════════════════════════════════════════╗"
echo "║          NFS Concurrent Performance Comparison            ║"
echo "╚═══════════════════════════════════════════════════════════╝"

# Ensure binaries exist
if [ ! -f "./target/release/flint-nfs-server.baseline" ]; then
    echo "❌ Baseline binary not found!"
    exit 1
fi

if [ ! -f "./target/release/flint-nfs-server.optimized" ]; then
    echo "❌ Optimized binary not found!"
    exit 1
fi

# Run tests
BASELINE_TIME=$(test_version "./target/release/flint-nfs-server.baseline" "BASELINE")
sleep 3
OPTIMIZED_TIME=$(test_version "./target/release/flint-nfs-server.optimized" "OPTIMIZED")

# Summary
echo ""
echo "╔═══════════════════════════════════════════════════════════╗"
echo "║                      RESULTS SUMMARY                      ║"
echo "╚═══════════════════════════════════════════════════════════╝"
echo ""
printf "  BASELINE:   %3ds  (%3d MB/s aggregate)\n" $BASELINE_TIME $((1000 / BASELINE_TIME))
printf "  OPTIMIZED:  %3ds  (%3d MB/s aggregate)\n" $OPTIMIZED_TIME $((1000 / OPTIMIZED_TIME))
echo ""

if [ $OPTIMIZED_TIME -lt $BASELINE_TIME ]; then
    IMPROVEMENT=$((BASELINE_TIME * 100 / OPTIMIZED_TIME - 100))
    echo "  ✅ OPTIMIZED is ${IMPROVEMENT}% faster!"
elif [ $OPTIMIZED_TIME -eq $BASELINE_TIME ]; then
    echo "  ≈  Same performance (disk-bound)"
else
    PCT_SLOWER=$(((OPTIMIZED_TIME - BASELINE_TIME) * 100 / BASELINE_TIME))
    echo "  ⚠️  BASELINE was ${PCT_SLOWER}% faster (noise or regression)"
fi
echo ""

