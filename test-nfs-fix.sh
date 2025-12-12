#!/bin/bash
# Test script for Flint NFS server permission fix
# Run on: root@tnfs.vpc.cloudera.com

set -x

# Cleanup
pkill -9 -f flint-nfs-server || true
umount /mnt/nfs-test 2>/dev/null || true
sleep 1

# Start server
cd /root/flint/spdk-csi-driver
./target/release/flint-nfs-server \
  --export-path /root/flint/spdk-csi-driver/target/nfs-test-export \
  --volume-id volume \
  --bind-addr 127.0.0.1 \
  --port 2049 \
  --verbose > /tmp/debug.log 2>&1 &

SERVER_PID=$!
echo "Server PID: $SERVER_PID"
sleep 3

# Verify running
if ! ps -p $SERVER_PID > /dev/null; then
    echo "ERROR: Server failed to start"
    cat /tmp/debug.log
    exit 1
fi

# Mount
echo "=== MOUNT TEST ==="
mount -t nfs -o vers=4.2,tcp 127.0.0.1:/ /mnt/nfs-test
echo "Mount exit code: $?"

# Check ownership
echo ""
echo "=== OWNERSHIP CHECK ==="
stat /mnt/nfs-test | grep "Uid\|Gid"

# Simple ls
echo ""
echo "=== SIMPLE LS (no attributes) ==="
ls /mnt/nfs-test/

# ls -la
echo ""
echo "=== LS -LA (with attributes) ==="
ls -la /mnt/nfs-test/

# Try cd  
echo ""
echo "=== CD INTO VOLUME ==="
cd /mnt/nfs-test/volume && pwd && ls -la

# Show server activity
echo ""
echo "=== SERVER OPERATIONS LOG ==="
grep -E '🔍|🔐|📂|✅|❌' /tmp/debug.log | tail -40

