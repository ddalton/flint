# macOS NFS Client Testing Steps

**Purpose**: Test pNFS from macOS which may not have Linux's strict Kerberos requirements for parallel I/O.

---

## Prerequisites

NodePort services have been created:
- MDS: `cdrv-1.vpc.cloudera.com:32049`
- DS #1: Accessible via cluster
- DS #2: Accessible via cluster

---

## Manual Testing Steps (Run in Terminal)

### Step 1: Create Mount Point
```bash
sudo mkdir -p /tmp/pnfs-test
```

### Step 2: Mount pNFS Server from macOS
```bash
# Mount using NFSv4 with sec=sys
sudo mount -t nfs -o vers=4,sec=sys,resvport cdrv-1.vpc.cloudera.com:32049 /tmp/pnfs-test

# Verify mount
mount | grep pnfs-test
df -h /tmp/pnfs-test
```

### Step 3: Test Basic I/O
```bash
# Write test
dd if=/dev/zero of=/tmp/pnfs-test/macos-test bs=1m count=100

# Read test  
dd if=/tmp/pnfs-test/macos-test of=/dev/null bs=1m

# List files
ls -lh /tmp/pnfs-test/
```

### Step 4: Check for pNFS Activity (macOS Console)
```bash
# Check if macOS is using pNFS
sudo fs_usage -w -f filesys | grep pnfs &
FSPID=$!

# Write a file
dd if=/dev/zero of=/tmp/pnfs-test/pnfs-check bs=1m count=50

# Stop monitoring
kill $FSPID
```

### Step 5: Check Server-Side (From This Terminal)
```bash
# Check if Data Servers received I/O
export KUBECONFIG=/Users/ddalton/.kube/config.cdrv
kubectl exec -n pnfs-test pnfs-ds-spfnh -- ls -lh /mnt/pnfs-data/
kubectl exec -n pnfs-test $(kubectl get pod -n pnfs-test -l app=pnfs-ds -o jsonpath='{.items[1].metadata.name}') -- ls -lh /mnt/pnfs-data/

# Check MDS logs for LAYOUTGET
kubectl logs -l app=pnfs-mds -n pnfs-test | grep -i layoutget

# Check DS logs for I/O operations
kubectl logs -l app=pnfs-ds -n pnfs-test | grep -E "READ|WRITE"
```

### Step 6: Cleanup
```bash
sudo umount /tmp/pnfs-test
sudo rmdir /tmp/pnfs-test
```

---

## What to Look For

### If pNFS Works on macOS:
- ✅ Files appear in `/mnt/pnfs-data/` on Data Servers
- ✅ LAYOUTGET operations in MDS logs
- ✅ I/O operations in DS logs
- ✅ Higher throughput (striping across DSes)

### If pNFS Doesn't Work:
- ❌ Files only on MDS (`/data/`)
- ❌ No LAYOUTGET operations
- ❌ Empty Data Server filesystems
- ⚠️ Regular NFS through MDS only

---

## Alternative: Test with Windows NFS Client

If macOS also has restrictions, we could try:
- Windows Server with NFS client
- FreeBSD
- Older Linux kernel (< 4.x) that may be less strict

---

## Why This Might Work

**Linux Requirement:**
- Modern Linux NFS client enforces Kerberos for pNFS DS connections
- Security policy: won't make direct unauth connections

**macOS/BSD:**
- May have different security policy
- Might allow `sec=sys` for DS connections
- Worth testing to prove server implementation works

**If it works:** Proves our pNFS implementation is correct and Linux is being overly strict.

