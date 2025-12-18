#!/bin/bash
# Run performance comparison tests between pNFS and standalone NFS
# Usage: ./run-performance-tests.sh

set -e

KUBECONFIG=${KUBECONFIG:-/Users/ddalton/.kube/config.cdrv}
export KUBECONFIG

POD="pnfs-client"
NS="pnfs-test"

echo "======================================"
echo "pNFS vs Standalone NFS Performance Test"
echo "======================================"
echo ""

# Check if client pod is ready
echo "Checking client pod status..."
kubectl get pod $POD -n $NS
echo ""

# Install tools if not already installed
echo "Installing test tools (nfs-common, fio)..."
kubectl exec -n $NS $POD -- bash -c "apt-get update -qq && apt-get install -y -qq nfs-common fio" 2>/dev/null || true
echo ""

# Mount pNFS
echo "Mounting pNFS..."
kubectl exec -n $NS $POD -- bash -c "mkdir -p /mnt/pnfs && (mountpoint -q /mnt/pnfs || mount -t nfs -o vers=4.1 pnfs-mds:/ /mnt/pnfs)"
echo ""

# Mount standalone NFS
echo "Mounting standalone NFS..."
kubectl exec -n $NS $POD -- bash -c "mkdir -p /mnt/standalone && (mountpoint -q /mnt/standalone || mount -t nfs -o vers=4.1 standalone-nfs:/ /mnt/standalone)"
echo ""

# Check mounts
echo "Verifying mounts..."
kubectl exec -n $NS $POD -- mount | grep nfs
echo ""

# Check if pNFS is active
echo "======================================"
echo "Checking pNFS Status"
echo "======================================"
kubectl exec -n $NS $POD -- bash -c "cat /proc/self/mountstats | grep -A5 'device pnfs-mds'"
echo ""

# Test 1: pNFS Write Performance (100MB)
echo "======================================"
echo "Test 1: pNFS Write (100MB)"
echo "======================================"
kubectl exec -n $NS $POD -- bash -c "
cd /mnt/pnfs && \
fio --name=pnfs-write \
    --filename=test-100mb \
    --rw=write \
    --bs=1M \
    --size=100M \
    --direct=1 \
    --numjobs=1 \
    --group_reporting \
    --output-format=normal
" | tee /tmp/pnfs-write.txt
echo ""

# Extract pNFS write bandwidth
PNFS_WRITE=$(grep "WRITE:" /tmp/pnfs-write.txt | grep -oE "BW=[0-9.]+[KMGT]iB/s" | grep -oE "[0-9.]+")
PNFS_WRITE_UNIT=$(grep "WRITE:" /tmp/pnfs-write.txt | grep -oE "BW=[0-9.]+[KMGT]iB/s" | grep -oE "[KMGT]iB/s")
echo "pNFS Write: $PNFS_WRITE $PNFS_WRITE_UNIT"

# Test 2: pNFS Read Performance (100MB)
echo ""
echo "======================================"
echo "Test 2: pNFS Read (100MB)"
echo "======================================"
kubectl exec -n $NS $POD -- bash -c "
cd /mnt/pnfs && \
fio --name=pnfs-read \
    --filename=test-100mb \
    --rw=read \
    --bs=1M \
    --size=100M \
    --direct=1 \
    --numjobs=1 \
    --group_reporting \
    --output-format=normal
" | tee /tmp/pnfs-read.txt
echo ""

PNFS_READ=$(grep "READ:" /tmp/pnfs-read.txt | grep -oE "BW=[0-9.]+[KMGT]iB/s" | grep -oE "[0-9.]+")
PNFS_READ_UNIT=$(grep "READ:" /tmp/pnfs-read.txt | grep -oE "BW=[0-9.]+[KMGT]iB/s" | grep -oE "[KMGT]iB/s")
echo "pNFS Read: $PNFS_READ $PNFS_READ_UNIT"

# Test 3: Standalone NFS Write Performance (100MB)
echo ""
echo "======================================"
echo "Test 3: Standalone NFS Write (100MB)"
echo "======================================"
kubectl exec -n $NS $POD -- bash -c "
cd /mnt/standalone && \
fio --name=standalone-write \
    --filename=test-100mb \
    --rw=write \
    --bs=1M \
    --size=100M \
    --direct=1 \
    --numjobs=1 \
    --group_reporting \
    --output-format=normal
" | tee /tmp/standalone-write.txt
echo ""

STANDALONE_WRITE=$(grep "WRITE:" /tmp/standalone-write.txt | grep -oE "BW=[0-9.]+[KMGT]iB/s" | grep -oE "[0-9.]+")
STANDALONE_WRITE_UNIT=$(grep "WRITE:" /tmp/standalone-write.txt | grep -oE "BW=[0-9.]+[KMGT]iB/s" | grep -oE "[KMGT]iB/s")
echo "Standalone Write: $STANDALONE_WRITE $STANDALONE_WRITE_UNIT"

# Test 4: Standalone NFS Read Performance (100MB)
echo ""
echo "======================================"
echo "Test 4: Standalone NFS Read (100MB)"
echo "======================================"
kubectl exec -n $NS $POD -- bash -c "
cd /mnt/standalone && \
fio --name=standalone-read \
    --filename=test-100mb \
    --rw=read \
    --bs=1M \
    --size=100M \
    --direct=1 \
    --numjobs=1 \
    --group_reporting \
    --output-format=normal
" | tee /tmp/standalone-read.txt
echo ""

STANDALONE_READ=$(grep "READ:" /tmp/standalone-read.txt | grep -oE "BW=[0-9.]+[KMGT]iB/s" | grep -oE "[0-9.]+")
STANDALONE_READ_UNIT=$(grep "READ:" /tmp/standalone-read.txt | grep -oE "BW=[0-9.]+[KMGT]iB/s" | grep -oE "[KMGT]iB/s")
echo "Standalone Read: $STANDALONE_READ $STANDALONE_READ_UNIT"

# Summary
echo ""
echo "======================================"
echo "Performance Summary"
echo "======================================"
echo ""
printf "%-20s %-20s %-20s %-20s\n" "Test" "pNFS" "Standalone" "Improvement"
echo "------------------------------------------------------------------------"
printf "%-20s %-20s %-20s " "Write (100MB)" "$PNFS_WRITE $PNFS_WRITE_UNIT" "$STANDALONE_WRITE $STANDALONE_WRITE_UNIT"
if [ -n "$PNFS_WRITE" ] && [ -n "$STANDALONE_WRITE" ]; then
    WRITE_RATIO=$(echo "scale=2; $PNFS_WRITE / $STANDALONE_WRITE" | bc)
    echo "${WRITE_RATIO}x"
else
    echo "N/A"
fi

printf "%-20s %-20s %-20s " "Read (100MB)" "$PNFS_READ $PNFS_READ_UNIT" "$STANDALONE_READ $STANDALONE_READ_UNIT"
if [ -n "$PNFS_READ" ] && [ -n "$STANDALONE_READ" ]; then
    READ_RATIO=$(echo "scale=2; $PNFS_READ / $STANDALONE_READ" | bc)
    echo "${READ_RATIO}x"
else
    echo "N/A"
fi
echo ""

# Check MDS logs for layout information
echo "======================================"
echo "MDS Logs (Layout Generation)"
echo "======================================"
kubectl logs -l app=pnfs-mds -n $NS --tail=100 | grep -i "layout\|segment\|device" || echo "No layout logs found"
echo ""

# Check DS logs for I/O activity
echo "======================================"
echo "DS Logs (I/O Activity)"
echo "======================================"
kubectl logs -l app=pnfs-ds -n $NS --tail=50 | grep -i "read\|write\|i/o" || echo "No I/O logs found"
echo ""

echo "======================================"
echo "Test Complete!"
echo "======================================"
echo ""
echo "If pNFS is working correctly with 2 DSs, you should see ~2x improvement."
echo "If you see similar or worse performance, check:"
echo "1. MDS logs for 'Generated pNFS layout with 2 segments'"
echo "2. /proc/self/mountstats for 'pnfs=files' (not 'pnfs=not configured')"
echo "3. DS registration in MDS logs"
echo ""

