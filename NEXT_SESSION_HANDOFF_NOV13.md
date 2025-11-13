# Session Handoff - November 13, 2025

**Branch:** feature/minimal-state  
**Latest Commit:** 9436faa  
**Cluster:** KUBECONFIG=/Users/ddalton/.kube/config.ublk  
**Kubernetes:** v1.33.5+rke2r1 (RKE2)

---

## 🎯 **MISSION STATUS: 90% Complete**

### What's Working: ✅

1. **Volume Creation** - Fast and reliable
2. **SPDK Clean Shutdown** - No more recovery! 🎉
3. **Disk Discovery** - Controller sees initialized disks
4. **Filesystem Detection** - Prevents reformatting
5. **NVMe-oF Connections** - Remote access works
6. **Pod Creation** - Fast, works on both nodes
7. **Pod Deletion** - Fast when no ghost mounts (1-3s)

### What's Broken: ❌

**Ghost Mount Bug** - THE FINAL BOSS

**Symptom:**
- Pod deletions hang (waiting for `sync` that never completes)
- Data doesn't persist across pod migration
- Force delete required for stuck pods

**Root Cause:**
- NodeUnstageVolume deletes ublk devices
- But mount entries persist (ghost mounts)
- Future I/O hangs on non-existent devices

**Impact:** Blocks pod migration and data persistence

---

## 🔥 **CRITICAL BUG: Ghost Mounts**

### The Issue:

```bash
# Mount table shows:
/dev/ublkb49642 on /path/to/mount type ext4 (rw,relatime)

# But device doesn't exist:
$ ls /dev/ublkb49642
ls: cannot access '/dev/ublkb49642': No such device
```

### Why It Happens:

**In NodeUnstageVolume (main.rs:738-792):**
```rust
// Unmount staging path
umount_output = Command::new("umount").arg(&path).output()?;
if !umount_output.status.success() {
    println!("⚠️ Unmount failed (may not be mounted)");  // Just a warning!
}

// Delete ublk device anyway  <-- PROBLEM!
self.driver.delete_ublk_device(ublk_id).await?;
```

**The bug:**
- Unmount fails (device busy, already broken, etc.)
- Code logs warning and continues anyway
- Deletes ublk device while mount still references it
- **Ghost mount created!**

### Consequences:

1. **Sync hangs** - tries to flush to non-existent device
2. **Pod deletion hangs** - waiting for sync to complete
3. **Kubernetes sends SIGKILL** after 30s grace period
4. **Data loss** - buffered writes never flushed
5. **Future pods affected** - same staging path has ghost mount

---

## 🔧 **THE FIX (Next Session)**

### File to Modify:

**`spdk-csi-driver/src/main.rs`** - `node_unstage_volume()` function (lines 738-792)

### Required Changes:

**1. Verify mount before unmount:**
```rust
let is_mounted = Command::new("mountpoint")
    .arg("-q")
    .arg(&staging_target_path)
    .status()
    .map(|s| s.success())
    .unwrap_or(false);
```

**2. Try normal unmount with retry:**
```rust
for attempt in 1..=3 {
    let success = Command::new("umount")
        .arg(&staging_target_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    
    if success { break; }
    
    if attempt < 3 {
        sleep(Duration::from_millis(100));
    }
}
```

**3. Fallback to lazy unmount:**
```rust
if still_mounted {
    Command::new("umount").arg("-l").arg(&staging_target_path).status()?;
    sleep(Duration::from_millis(500));
}
```

**4. VERIFY cleanup before device deletion:**
```rust
let still_mounted = Command::new("mountpoint")
    .arg("-q")
    .arg(&staging_target_path)
    .status()
    .map(|s| s.success())
    .unwrap_or(false);

if still_mounted {
    return Err(tonic::Status::internal("Cannot unmount staging path"));
}

// Only now delete ublk device
self.driver.delete_ublk_device(ublk_id).await?;
```

---

## 📊 **Test Plan for Next Session**

### Test 1: Local to Remote Migration

```bash
# Step 1: Create volume and write data on ublk-2
kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: migration-pvc
  namespace: flint-system
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: flint-single-replica
  resources: { requests: { storage: 1Gi } }
---
apiVersion: batch/v1
kind: Job
metadata:
  name: write-local
  namespace: flint-system
spec:
  template:
    spec:
      restartPolicy: Never
      nodeSelector: { kubernetes.io/hostname: ublk-2.vpc.cloudera.com }
      containers:
      - name: writer
        image: busybox
        command: ["/bin/sh", "-c", "echo 'TEST_DATA_12345' > /data/test.txt && sync && cat /data/test.txt"]
        volumeMounts: [{ name: data, mountPath: /data }]
      volumes: [{ name: data, persistentVolumeClaim: { claimName: migration-pvc } }]
EOF

# Step 2: Wait for job to complete
kubectl wait --for=condition=complete job/write-local -n flint-system --timeout=60s

# Step 3: Verify data written
kubectl logs -n flint-system job/write-local
# Should show: TEST_DATA_12345

# Step 4: Delete job (pod auto-deletes, PVC remains)
kubectl delete job write-local -n flint-system

# Step 5: Verify no ghost mounts
kubectl exec -n flint-system <csi-node-ublk2> -c flint-csi-driver -- sh -c \
  'mount | grep ublkb && for d in /dev/ublkb*; do [ -e "$d" ] && echo "OK: $d" || echo "GHOST: $d"; done'

# Step 6: Create reader pod on ublk-1
kubectl apply -f - <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: read-remote
  namespace: flint-system
spec:
  template:
    spec:
      restartPolicy: Never
      nodeSelector: { kubernetes.io/hostname: ublk-1.vpc.cloudera.com }
      containers:
      - name: reader
        image: busybox
        command: ["/bin/sh", "-c", "cat /data/test.txt || echo 'FILE NOT FOUND'"]
        volumeMounts: [{ name: data, mountPath: /data }]
      volumes: [{ name: data, persistentVolumeClaim: { claimName: migration-pvc } }]
EOF

# Step 7: Check if data survived migration
kubectl logs -n flint-system job/read-remote
# Should show: TEST_DATA_12345
```

### Expected Results (After Fix):

| Step | Expected | Current (Broken) |
|------|----------|------------------|
| Write data | ✅ Works | ✅ Works |
| Job completes | ✅ < 5s | ❌ Hangs on sync |
| Pod deletion | ✅ 1-3s | ❌ 30s (SIGKILL) |
| No ghost mounts | ✅ Clean | ❌ Ghost mounts |
| Read on remote | ✅ Data found | ❌ Data lost |

### Test 2: Remote to Local Migration

Same as above, but reverse:
- Write on ublk-1 (volume on ublk-2 via NVMe-oF)
- Read on ublk-2 (local access)

---

## 🛠️ **Implementation Checklist**

### NodeUnstageVolume Fix:

- [ ] Add `mountpoint -q` check before unmount
- [ ] Implement unmount retry loop (3 attempts)
- [ ] Add lazy unmount (`-l`) as fallback
- [ ] Add post-unmount verification with `mountpoint -q`
- [ ] Return error if unmount verification fails
- [ ] Only delete ublk device after successful unmount
- [ ] Add detailed logging for each step
- [ ] Test with hung mounts (manually create to test)

### Ghost Mount Cleanup:

- [ ] Add startup routine to scan for ublk mounts
- [ ] Check if device exists for each mount
- [ ] Lazy unmount any ghost mounts
- [ ] Log all cleanup actions
- [ ] Run before starting CSI server

### Testing:

- [ ] Test normal pod deletion (no hang)
- [ ] Test local → remote migration
- [ ] Test remote → local migration
- [ ] Test CSI driver restart with active volumes
- [ ] Verify no ghost mounts after each operation
- [ ] Confirm deletion times < 5s consistently

---

## 📁 **Documentation Created This Session**

1. **POD_DELETION_RESOLVED.md** - Pod deletion improvements (partial - ghost mounts still an issue)
2. **GHOST_MOUNT_INVESTIGATION.md** - Comprehensive ghost mount bug analysis

---

## 🎓 **What We Learned**

### About SPDK:

- SPDK won't unload LVS if any lvol has `ref_count > 0`
- NVMe-oF subsystems hold lvol references
- Must delete subsystems before shutdown for clean unload
- Blobstore recovery is fast (milliseconds) but avoidable
- Thread exit requires all messages to be processed

### About Kubernetes CSI:

- VolumeAttachment doesn't auto-delete on force pod delete
- `WaitForFirstConsumer` binding delays PVC creation
- terminationGracePeriodSeconds includes preStop time
- Completed pods don't auto-delete (stay for debugging)

### About Our Driver:

- Ghost mounts are the root cause of multiple issues
- Unmount failures must return errors, not warnings
- Device deletion must wait for verified unmount
- `sync` hanging is a symptom of deeper I/O problems

---

## 🚀 **Bottom Line**

**We're 90% there!**

✅ Fixed: SPDK recovery, disk discovery, filesystem detection  
❌ Remaining: Ghost mount cleanup in NodeUnstageVolume

**Estimated time to fix:** 1-2 hours
- 30 min: Implement robust unmount
- 15 min: Add ghost cleanup
- 30 min: Test and verify
- 15 min: Documentation

**After this fix:**
- Pod migration will work
- Data will persist
- Deletions will be fast (<5s)
- CSI driver production-ready!

---

## 🔬 **Quick Verification Commands**

```bash
# Check for ghost mounts on ublk-2:
kubectl exec -n flint-system $(kubectl get pod -n flint-system -l app=flint-csi-node -o jsonpath='{.items[?(@.spec.nodeName=="ublk-2.vpc.cloudera.com")].metadata.name}') -c flint-csi-driver -- sh -c 'mount | grep ublkb | while read line; do dev=$(echo $line | awk "{print \$1}"); [ -e "$dev" ] && echo "✅ $dev exists" || echo "❌ GHOST: $dev"; done'

# Check for hung sync processes:
kubectl get pod -n flint-system -o wide | grep Running | while read name rest; do
  echo "Checking $name..."
  kubectl exec -n flint-system $name -- ps aux 2>/dev/null | grep sync
done
```

---

**Good luck with the ghost mount fix! You're one commit away from a fully working CSI driver!** 🎯

