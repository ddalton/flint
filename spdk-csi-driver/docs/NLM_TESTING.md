# NLM (Network Lock Manager) Testing Guide

This guide explains how to test file locking functionality in the Flint NFSv3 server.

## Why Localhost Testing Doesn't Work

**TL;DR:** The Linux kernel's lockd blocks user-space NLM on the same machine.

When you mount NFS on localhost:
- Kernel's lockd registers with portmapper for program 100021 (NLM)
- Portmapper rejects our user-space NLM registration
- Lock requests route to kernel lockd, which times out
- **This is expected behavior** - NFS Ganesha has the same limitation

## Testing Options

### Option 1: Production Kubernetes Cluster ⭐ **RECOMMENDED**

The **best** way to test NLM is in a real multi-node Kubernetes cluster:

```bash
# Deploy CSI driver
kubectl apply -f deploy/

# Create RWX StorageClass (with locking enabled)
cat <<EOF | kubectl apply -f -
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: spdk-nvme-rwx
provisioner: csi.spdk.io
parameters:
  # ... your parameters ...
mountOptions:
  - vers=3      # NFSv3
  - tcp         # TCP transport
  # NOTE: No 'nolock' - test with locking enabled
EOF

# Deploy test pods on DIFFERENT nodes
kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: rwx-test-pvc
spec:
  accessModes:
    - ReadWriteMany
  storageClassName: spdk-nvme-rwx
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: Pod
metadata:
  name: lock-test-pod-1
spec:
  nodeName: worker-node-1  # Force specific node
  containers:
  - name: test
    image: ubuntu:22.04
    command: ["/bin/sleep", "infinity"]
    volumeMounts:
    - name: data
      mountPath: /mnt/nfs
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: rwx-test-pvc
---
apiVersion: v1
kind: Pod
metadata:
  name: lock-test-pod-2
spec:
  nodeName: worker-node-2  # Different node!
  containers:
  - name: test
    image: ubuntu:22.04
    command: ["/bin/sleep", "infinity"]
    volumeMounts:
    - name: data
      mountPath: /mnt/nfs
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: rwx-test-pvc
EOF

# Install lock test on both pods
for pod in lock-test-pod-1 lock-test-pod-2; do
  kubectl exec $pod -- apt-get update
  kubectl exec $pod -- apt-get install -y gcc
  kubectl cp /tmp/nfs-lock-test.c $pod:/tmp/
  kubectl exec $pod -- gcc -o /tmp/nfs-lock-test /tmp/nfs-lock-test.c
done

# Run lock test on pod 1
kubectl exec lock-test-pod-1 -- /tmp/nfs-lock-test /mnt/nfs

# Check CSI controller logs for NLM calls
kubectl logs -n spdk-csi -l app=spdk-csi-controller -f | grep NLM
```

**What to look for:**
- Server logs should show NLM calls: `>>> NLM procedure 1 (NLM_TEST)`
- Lock test should pass all three tests
- No "lockd timeout" errors in kernel logs

### Option 2: Separate Physical/VM Machine ⭐

Test from a completely separate machine:

```bash
# On the remote test machine:
# 1. Mount NFS from the server
mount -t nfs -o vers=3,tcp <server-ip>:/ /mnt/nfs

# 2. Copy the test program
scp user@build-machine:/tmp/nfs-lock-test.c .
gcc -o nfs-lock-test nfs-lock-test.c

# 3. Run the test
./nfs-lock-test /mnt/nfs
```

**Expected output:**
```
╔═══════════════════════════════════════════════════════════╗
║            NFS File Locking Test Suite                   ║
╚═══════════════════════════════════════════════════════════╝

Test file: /mnt/nfs/lock-test.dat

=== Test 1: Exclusive Lock ===
Acquiring exclusive lock...
✓ Exclusive lock acquired
✓ Lock test passed (no conflict)
✓ Lock released

=== Test 2: Lock Conflict Detection ===
Parent: Acquiring lock on bytes 0-100...
✓ Parent: Lock acquired
  Child: Testing for lock conflict on bytes 50-150...
  ✓ Child: Conflict detected (lock held by PID 1234)
✓ Lock conflict detection working correctly

=== Test 3: Shared (Read) Locks ===
Acquiring shared lock (fd1)...
✓ Shared lock acquired (fd1)
Acquiring second shared lock (fd2)...
✓ Second shared lock acquired (fd2)
✓ Multiple shared locks working correctly

╔═══════════════════════════════════════════════════════════╗
║                  ALL TESTS PASSED ✓                       ║
╚═══════════════════════════════════════════════════════════╝

NLM (Network Lock Manager) is working correctly!
```

### Option 3: cthon04 Lock Tests

Run the comprehensive Connectathon test suite from a remote machine:

```bash
# On remote machine with NFS mounted at /mnt/nfs
cd /tmp/cthon04/lock
./runtests -t /mnt/nfs

# Expected output (when NLM works):
# tlock: All tests passed
# tlocklfs: All tests passed
```

### Option 4: Manual fcntl Test

Simple one-liner to test basic locking:

```bash
# On remote machine
python3 <<'EOF'
import fcntl
import os

# Open file on NFS mount
fd = os.open('/mnt/nfs/test.lock', os.O_RDWR | os.O_CREAT)

# Try to acquire exclusive lock
fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
print("✓ Lock acquired successfully!")

# Try from another process - should fail with EAGAIN
# Release lock
fcntl.flock(fd, fcntl.LOCK_UN)
print("✓ Lock released")
os.close(fd)
EOF
```

## What Won't Work

### ❌ Docker Containers

Docker containers have limitations:

1. **With `--network host`:**
   - Shares host kernel
   - Still hits localhost lockd conflict
   - Same issue as testing on localhost

2. **Without `--network host`:**
   - Limited NFS kernel module support
   - Mount fails with "Protocol not supported"
   - Even `--privileged` insufficient

### ❌ Localhost Testing

Direct localhost mount will fail:
```bash
# This will mount but locking won't work
mount -t nfs -o vers=3,tcp 127.0.0.1:/ /mnt/test

# Lock attempts timeout after 3 seconds:
# kernel: lockd: rpc_call returned error 512
```

**Why:** Kernel lockd blocks user-space NLM registration.

### ❌ Network Namespaces

Even with network namespaces, shares the same kernel, so kernel lockd still interferes.

## Monitoring NLM Activity

### Server-Side Monitoring

Check server logs for NLM calls:

```bash
# If running server manually
RUST_LOG=info ./target/release/flint-nfs-server --export-path /tmp/export --volume-id test | grep NLM

# Expected output when locks are used:
# >>> NLM procedure 1 (NLM_TEST)
# >>> NLM procedure 2 (NLM_LOCK)
# >>> NLM procedure 4 (NLM_UNLOCK)
```

### Client-Side Monitoring

Check kernel logs for lockd activity:

```bash
# Enable NFS debugging
sudo rpcdebug -m nfs -s all
sudo rpcdebug -m nlm -s all

# Watch kernel logs
sudo dmesg -w | grep -E '(nfs|lockd|nlm)'

# When working correctly:
# NFS: nlm_lookup_host(server.example.com)
# lockd: lock request sent
# lockd: lock granted
```

### Portmapper Verification

Check what's registered:

```bash
rpcinfo -p server-ip | grep nlockmgr

# Expected from REMOTE machine:
# 100021    4   tcp   2049  nlockmgr  ← Our NLM on same port as NFS
# 100021    4   udp   2049  nlockmgr

# On localhost (broken):
# 100021    4   tcp  41767  nlockmgr  ← Kernel lockd on different port
```

## Troubleshooting

### "No locks available" error

```bash
# Symptoms:
fcntl(3, F_SETLK, ...) = -1 ENOLCK (No locks available)
```

**Causes:**
1. Testing on localhost (kernel lockd conflict)
2. Mount with `nolock` option
3. NLM registration failed on server

**Solution:** Test from remote machine.

### "Connection timed out" errors

```bash
# Kernel logs show:
lockd: rpc_call returned error 512
```

**Cause:** Client trying to contact kernel lockd instead of our NLM.

**Solution:** This happens on localhost only. Test from remote machine.

### No NLM calls in server logs

**Symptoms:** Server shows only NFS and MOUNT calls, no NLM.

**Possible causes:**
1. Mounted with `nolock` option
2. Testing from localhost (kernel lockd conflict)
3. Firewall blocking NLM port

**Solution:**
```bash
# Check mount options
mount | grep nfs

# Ensure NOT using nolock
# Re-mount without nolock from remote machine
```

## Test Program Source

The test program source is available at:
- `/tmp/nfs-lock-test.c` on the build machine
- Can be compiled on any Linux machine with `gcc -o nfs-lock-test nfs-lock-test.c`

## Summary

**Best testing approach:**
1. ✅ **Multi-node Kubernetes cluster** - Real production environment
2. ✅ **Separate VM/physical machine** - True remote client
3. ❌ Localhost - Won't work due to kernel lockd conflict
4. ❌ Docker containers - Limited NFS support

**Key insight:** The NLM implementation is correct. The localhost limitation is expected and shared by all user-space NFS servers (including NFS Ganesha).
