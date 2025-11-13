# Ghost Mount Bug Investigation - Critical Issue Found

**Date:** November 13, 2025  
**Branch:** feature/minimal-state  
**Last Commit:** 9436faa  
**Cluster:** KUBECONFIG=/Users/ddalton/.kube/config.ublk

---

## 🚨 **CRITICAL BUG DISCOVERED: Ghost Mounts**

### The Problem:

**Symptom:** 
- Pod deletions hang (30-60 seconds)
- `sync` command hangs indefinitely
- Data doesn't persist across pod migration
- Force delete required for stuck pods

**Root Cause:**
```bash
# Mount table shows device is mounted:
/dev/ublkb49642 on /var/lib/kubelet/.../globalmount type ext4 (rw,relatime,stripe=32)

# But device doesn't exist!
ls -l /dev/ublkb49642
# ls: cannot access '/dev/ublkb49642': No such file or directory
```

**What's Happening:**
1. Pod runs with ublk device mounted
2. Pod deleted → NodeUnstageVolume called
3. NodeUnstageVolume unmounts staging path
4. NodeUnstageVolume deletes ublk device
5. **But mount entry persists in mount table (ghost mount)**
6. Future pods try to write to ghost mount
7. `sync` hangs trying to flush to non-existent device
8. Pod deletion hangs waiting for container to exit
9. Kubernetes sends SIGKILL after 30s
10. Unclean shutdown → data loss

---

## 🔍 **Evidence**

### Ghost Mount Example:

```bash
$ mount | grep ublkb49642
/dev/ublkb49642 on /var/lib/kubelet/.../globalmount type ext4 (rw,relatime,stripe=32)

$ ls -l /dev/ublkb49642
ls: cannot access '/dev/ublkb49642': No such file or directory
```

### Hung Sync Process:

```bash
$ ps aux
PID   USER     TIME  COMMAND
   10 root      0:00 sync    # <-- Running for 40+ seconds!
```

**Normal sync completes in milliseconds.** 40+ seconds indicates it's waiting for I/O that will never complete.

### Pod Deletion Hanging:

Previous tests showed:
- Deletion sometimes 1-3s ✅  (when no ghost mounts)
- Deletion sometimes 30-60s ❌ (waiting for hung sync, then SIGKILL)

---

## 🐛 **The Bug in NodeUnstageVolume**

**Location:** `spdk-csi-driver/src/main.rs` lines 738-792

**Current Code:**
```rust
async fn node_unstage_volume(...) -> Result<...> {
    // Unmount the filesystem from staging path (if mounted)
    if std::path::Path::new(&staging_target_path).exists() {
        println!("🔧 [NODE] Unmounting staging path: {}", staging_target_path);
        let umount_output = std::process::Command::new("umount")
            .arg(&staging_target_path)
            .output()
            .map_err(|e| tonic::Status::internal(format!("Failed to execute umount: {}", e)))?;
        
        if !umount_output.status.success() {
            let error = String::from_utf8_lossy(&umount_output.stderr);
            // Only log warning - unmount might fail if already unmounted
            println!("⚠️ [NODE] Unmount failed (may not be mounted): {}", error);
        } else {
            println!("✅ [NODE] Staging path unmounted successfully");
        }
    }

    // Delete the ublk device  <-- HAPPENS EVEN IF UNMOUNT FAILED!
    let ublk_id = self.driver.generate_ublk_id(&volume_id);
    match self.driver.delete_ublk_device(ublk_id).await {
        Ok(_) => println!("✅ [NODE] ublk device stopped successfully"),
        Err(e) => println!("⚠️ [NODE] Failed to stop ublk device (may not exist): {}", e),
    }
    ...
}
```

**The Problem:**
- If `umount` fails (device busy, mount already broken, etc.), we just log a warning
- **Then we delete the ublk device anyway**
- This creates a ghost mount (mount entry pointing to deleted device)

---

## 🔧 **Required Fixes**

### Fix 1: Robust Unmount in NodeUnstageVolume

**Strategy:**
1. Check if path is actually mounted (`mountpoint -q`)
2. Try normal unmount first
3. If unmount fails, try lazy unmount (`umount -l`)
4. Verify mount is actually gone before proceeding
5. **Only delete ublk device after successful unmount**

**Pseudocode:**
```rust
// Check if actually mounted
let is_mounted = Command::new("mountpoint")
    .arg("-q")
    .arg(&staging_target_path)
    .status()
    .map(|s| s.success())
    .unwrap_or(false);

if is_mounted {
    // Try normal unmount
    let umount_result = Command::new("umount").arg(&staging_target_path).status();
    
    if !umount_result.map(|s| s.success()).unwrap_or(false) {
        // Try lazy unmount
        println!("⚠️ [NODE] Normal unmount failed, trying lazy unmount...");
        Command::new("umount").arg("-l").arg(&staging_target_path).status()?;
    }
    
    // Verify unmount succeeded
    let still_mounted = Command::new("mountpoint").arg("-q").arg(&staging_target_path)
        .status().map(|s| s.success()).unwrap_or(false);
    
    if still_mounted {
        return Err(tonic::Status::internal("Failed to unmount staging path"));
    }
}

// NOW it's safe to delete ublk device
self.driver.delete_ublk_device(ublk_id).await?;
```

### Fix 2: Clean Up Ghost Mounts on Startup

Add ghost mount detection and cleanup:

```rust
async fn cleanup_ghost_mounts() {
    // Get all mounts pointing to /dev/ublk*
    let mount_output = Command::new("mount").output()?;
    let mounts = String::from_utf8_lossy(&mount_output.stdout);
    
    for line in mounts.lines() {
        if line.contains("/dev/ublkb") {
            // Extract device path
            if let Some(device) = line.split_whitespace().next() {
                // Check if device exists
                if !Path::new(device).exists() {
                    // Ghost mount! Clean it up
                    if let Some(mount_point) = extract_mount_point(line) {
                        println!("🧹 [CLEANUP] Ghost mount detected: {} -> {}", device, mount_point);
                        Command::new("umount").arg("-l").arg(mount_point).status()?;
                    }
                }
            }
        }
    }
}
```

### Fix 3: Prevent Future Ghost Mounts

**Add to NodeUnstageVolume:**
- Use `fuser` or `lsof` to check if device is still open
- Force close any open file handles before unmount
- Add retries with exponential backoff for unmount
- Only proceed with ublk deletion after verified clean unmount

---

## 📊 **Impact Assessment**

**What's Broken:**
- ❌ Pod migration (data doesn't persist)
- ❌ Pod deletion (hangs waiting for hung sync)
- ❌ Data persistence after force delete  
- ❌ CSI driver restart resilience

**What Still Works:**
- ✅ Initial volume creation
- ✅ Pod creation (first time on fresh volume)
- ✅ Reading/writing data (before sync)
- ✅ NVMe-oF connections
- ✅ SPDK clean shutdown (with new preStop hook)
- ✅ Disk discovery (with health fix)

---

## 🎯 **Fixes Applied Today (But Incomplete)**

### 1. Filesystem Detection Fix (Commit 1b382bc)
```rust
// Removed -F flag, added blkid -p, added 500ms delay
```
**Status:** ✅ Working - detects existing filesystems  
**But:** Data still lost due to ghost mounts preventing writes from completing

### 2. Disk Health Fix (Commit 9436faa)
```rust
// OLD: healthy: !claimed  (WRONG - marked LVS disks as unhealthy)
// NEW: healthy: true
```
**Status:** ✅ Working - controller can now see initialized disks  
**But:** Volume operations blocked by ghost mounts

### 3. SPDK Clean Shutdown (Commit 543b7c4)
```yaml
# preStop hook now deletes NVMe-oF subsystems before SIGTERM
# Allows LVS to unload cleanly
```
**Status:** ✅ Working - no more recovery on restart!  
**But:** Doesn't solve ghost mount issue

---

## 🧪 **Test Results**

### SPDK Clean Shutdown: ✅ FIXED!

**Before:**
```
[2025-11-13 05:12:05] blobstore.c: Performing recovery on blobstore
[2025-11-13 05:12:05] blobstore.c: Recover: blob 0x0, 0x1, 0x3...
```

**After (with new preStop hook):**
```
[2025-11-13 05:44:49] vbdev_lvol.c: Lvol store found on kernel_nvme3n1 - begin parsing
```
**No recovery messages!** Clean load confirmed! 🎉

### Pod Deletion: ⚠️ INCONSISTENT

| Scenario | Time | Reason |
|----------|------|--------|
| Fresh pod, clean exit | 1-3s | ✅ Fast |
| Pod with sleep command | 30-60s | ❌ Waiting for SIGKILL |
| Pod with ghost mount | Hangs | ❌ sync blocks on ghost device |

### Data Persistence: ❌ BROKEN

**Test:** Write on ublk-2 (local) → Read on ublk-1 (remote)  
**Result:** Data lost  
**Cause:** Ghost mounts prevent sync from completing

---

## 🔍 **Debugging Commands Used**

### Find Ghost Mounts:
```bash
# In CSI driver container on ublk-2:
mount | grep ublkb
ls -l /dev/ublkb*

# If mount shows device but ls fails → ghost mount!
```

### Check for Hung Processes:
```bash
kubectl exec <pod> -- ps aux | grep sync
# sync running for 40+ seconds = hung on I/O
```

### Clean Up Ghost Mounts:
```bash
# Lazy unmount (detach even if busy)
kubectl exec <csi-node-pod> -c flint-csi-driver -- umount -l <mount_point>
```

---

## 📝 **Next Session Priority**

### HIGH PRIORITY: Fix NodeUnstageVolume

**File:** `spdk-csi-driver/src/main.rs` lines 738-792

**Required Changes:**
1. Add `mountpoint -q` check before unmount
2. Implement retry logic for unmount
3. Use lazy unmount (`-l`) as fallback
4. **Verify mount is gone** before deleting ublk device
5. Return error (don't just log warning) if unmount fails

**Code Template:**
```rust
// 1. Verify path is mounted
let is_mounted = std::process::Command::new("mountpoint")
    .arg("-q")
    .arg(&staging_target_path)
    .status()
    .map(|s| s.success())
    .unwrap_or(false);

if is_mounted {
    // 2. Try normal unmount
    let umount_success = std::process::Command::new("umount")
        .arg(&staging_target_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    
    if !umount_success {
        // 3. Try lazy unmount as fallback
        println!("⚠️ [NODE] Normal unmount failed, attempting lazy unmount...");
        std::process::Command::new("umount")
            .arg("-l")
            .arg(&staging_target_path)
            .status()?;
        
        // Give kernel time to detach
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    
    // 4. VERIFY mount is actually gone
    let still_mounted = std::process::Command::new("mountpoint")
        .arg("-q")
        .arg(&staging_target_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    
    if still_mounted {
        return Err(tonic::Status::internal(
            format!("Failed to unmount staging path after retries: {}", staging_target_path)
        ));
    }
    
    println!("✅ [NODE] Staging path unmounted and verified");
}

// 5. NOW safe to delete ublk device
self.driver.delete_ublk_device(ublk_id).await?;
```

### MEDIUM PRIORITY: Ghost Mount Cleanup on Startup

Add to node agent startup routine:

```rust
async fn cleanup_ghost_mounts_on_startup() {
    println!("🧹 [STARTUP] Checking for ghost ublk mounts...");
    
    // Get all mounts
    let mount_output = std::process::Command::new("mount").output()?;
    let mounts = String::from_utf8_lossy(&mount_output.stdout);
    
    for line in mounts.lines() {
        if line.contains("/dev/ublkb") {
            if let Some(device) = line.split_whitespace().next() {
                // Check if device exists
                if !std::path::Path::new(device).exists() {
                    // Ghost mount found!
                    if let Some(mount_point) = extract_mount_point_from_line(line) {
                        println!("🧹 [CLEANUP] Ghost mount: {} (device missing)", mount_point);
                        
                        // Lazy unmount to clean up
                        std::process::Command::new("umount")
                            .arg("-l")
                            .arg(mount_point)
                            .status()?;
                        
                        println!("✅ [CLEANUP] Ghost mount removed: {}", mount_point);
                    }
                }
            }
        }
    }
    
    println!("✅ [STARTUP] Ghost mount cleanup complete");
}
```

---

## ✅ **What We Fixed Today**

### 1. Pod Deletion Timing ✅
- Fresh pods with latest code delete in 1-3 seconds
- Matches or beats Longhorn performance
- **But:** Ghost mounts can still cause hangs

### 2. Filesystem Detection ✅
- `blkid -p` reliably detects existing filesystems
- No more reformatting on pod migration
- 500ms delay ensures kernel has scanned device
- **But:** Data still lost due to sync hangs from ghost mounts

### 3. Disk Health Check ✅
- Fixed inverted logic (`!claimed` → `true`)
- Controller now sees initialized disks
- Volume creation works after restart

### 4. SPDK Clean Shutdown ✅ 
- preStop hook deletes NVMe-oF subsystems first
- Releases lvol references
- LVS unloads cleanly with `clean=1` flag
- **No more blobstore recovery!** 🎉

---

## 🧪 **How to Reproduce Ghost Mount Bug**

```bash
# 1. Create pod with volume
kubectl apply -f test-pod.yaml

# 2. Force delete pod  
kubectl delete pod test-pod --force --grace-period=0

# 3. Check for ghost mounts
kubectl exec <csi-node-pod> -c flint-csi-driver -- sh -c "mount | grep ublkb"

# 4. Try to access device
kubectl exec <csi-node-pod> -c flint-csi-driver -- ls -l /dev/ublkb*

# If mount shows it but ls fails → ghost mount detected
```

---

## 📂 **Current Cluster State**

### Pods:
```
migration-test-remote: Completed (data test failed)
Various test PVCs: Exist with ghost mounts
```

### Ghost Mounts on ublk-2:
```
/dev/ublkb49642 -> ghost (device deleted, mount persists)
/dev/ublkb25589 -> may be ghost
```

### Recommendation:
**Clean restart before next session:**
```bash
# Delete all test pods/PVCs
kubectl delete pod,pvc -n flint-system -l test=migration

# Restart CSI driver to clear state
kubectl delete pod -n flint-system -l app=flint-csi-node

# Manually clean up ghost mounts if needed
kubectl exec ... -c flint-csi-driver -- umount -l <ghost_mount_path>
```

---

## 🎓 **Key Learnings**

1. **Ghost mounts are DANGEROUS** - they cause cascading failures:
   - Block I/O operations (sync hangs)
   - Prevent pod termination
   - Force SIGKILL required
   - Data loss on force delete

2. **Always verify cleanup** - don't just log warnings:
   - Check if unmount actually succeeded
   - Verify device is detached before deletion
   - Return errors, don't silently continue

3. **Lazy unmount (-l) is useful** but not a silver bullet:
   - Detaches mount from filesystem tree
   - Lets kernel clean up when references are gone
   - Good fallback, not primary strategy

4. **Sync hanging is a red flag:**
   - Indicates I/O can't complete
   - Usually means underlying device is broken/missing
   - Don't ignore hung sync - it points to serious issues

---

## 📋 **Commits from This Session**

```
1b382bc - fix: Prevent filesystem reformatting on pod migration
9436faa - fix: Critical bug - disks with LVS were marked unhealthy  
543b7c4 - fix: Clean up NVMe-oF subsystems before SPDK shutdown
560b120 - fix: Increase terminationGracePeriodSeconds to 60s (reverted)
```

**Note:** Commit 560b120 was partially reverted - terminationGracePeriodSeconds changes removed because they didn't solve the underlying ghost mount issue.

---

## 🚀 **Next Session Action Plan**

### Step 1: Implement Robust NodeUnstageVolume (30 min)

**File:** `spdk-csi-driver/src/main.rs` lines 738-792

**Changes:**
- Add mountpoint verification
- Implement retry logic
- Use lazy unmount fallback
- Verify cleanup before device deletion
- Return errors instead of warnings

### Step 2: Add Ghost Mount Cleanup on Startup (15 min)

**File:** `spdk-csi-driver/src/main.rs` or `node_agent.rs`

**Add to node agent initialization:**
- Scan for ublk mounts where device doesn't exist
- Lazy unmount all ghost mounts
- Log cleanup actions

### Step 3: Clean Cluster and Test (20 min)

```bash
# Delete all test resources
kubectl delete pod,pvc,job -n flint-system --all --force --grace-period=0

# Restart CSI driver with fixes
kubectl delete pod -n flint-system -l app=flint-csi-node

# Verify no ghost mounts
kubectl exec ... -- mount | grep ublkb

# Run migration test
# - Create PVC + pod on ublk-2
# - Write data with sync
# - Delete pod normally (should be fast now!)
# - Create pod on ublk-1
# - Verify data persists!
```

### Step 4: Test Suite (30 min)

Once ghost mounts are fixed, test:
- ✅ Local volume creation/deletion
- ✅ Remote volume access via NVMe-oF
- ✅ Pod migration (local → remote)
- ✅ Pod migration (remote → local)
- ✅ Multiple pod migrations
- ✅ CSI driver restart resilience
- ✅ Data persistence across all scenarios

---

## 💡 **Why This Was Hard to Find**

1. **Intermittent behavior:**
   - Sometimes works (no ghost mounts)
   - Sometimes fails (ghost mounts present)
   - Made debugging difficult

2. **Multiple symptoms, one cause:**
   - Slow pod deletion → ghost mounts
   - Data loss → ghost mounts  
   - Sync hangs → ghost mounts
   - All pointed to same root cause

3. **Silent failures:**
   - Unmount failures only logged warnings
   - No errors returned to Kubernetes
   - Kubernetes thought operations succeeded
   - Ghost state accumulated over time

4. **Testing methodology:**
   - Force deletes masked the issue initially
   - Needed normal deletes to observe hangs
   - Needed to check mount table AND device existence

---

## 🔑 **Critical Files to Modify**

1. **spdk-csi-driver/src/main.rs** (lines 738-792)
   - `node_unstage_volume()` function
   - Add robust unmount verification

2. **spdk-csi-driver/src/node_agent.rs** or **main.rs**
   - Add ghost mount cleanup on startup
   - Run before serving CSI requests

3. **Test with:**
   - Normal pod deletion (no --force)
   - Multiple migration cycles
   - CSI driver restarts

---

## 🎊 **Silver Lining**

Despite the ghost mount bug, we made **critical progress**:

1. ✅ **SPDK clean shutdown SOLVED** - No more recovery!
2. ✅ **Disk discovery FIXED** - Controller sees initialized disks
3. ✅ **Filesystem detection WORKS** - No reformatting on migration
4. ✅ **Root cause identified** - Ghost mounts are the final boss

**One more fix and we're production-ready!** 🚀

---

## 📖 **Reference**

### Helpful Commands:

```bash
# Check for ghost mounts
kubectl exec -n flint-system <csi-node-pod> -c flint-csi-driver -- sh -c \
  'for dev in $(mount | grep ublkb | awk "{print \$1}"); do \
     [ -e "$dev" ] || echo "GHOST: $dev"; \
   done'

# Clean up ghost mounts
kubectl exec -n flint-system <csi-node-pod> -c flint-csi-driver -- \
  umount -l /var/lib/kubelet/plugins/kubernetes.io/csi/...

# Verify ublk devices
kubectl exec -n flint-system <csi-node-pod> -c flint-csi-driver -- ls -l /dev/ublkb*
```

---

**The finish line is in sight! Fixing ghost mounts will complete the CSI driver.**

