# Known Issues - Flint CSI Driver

**Branch:** feature/minimal-state  
**Date:** November 13, 2025

## 🐛 Pod Deletion Hangs (~45 seconds)

### Severity: Medium
**Impact:** `kubectl delete pod` hangs for 30-45 seconds before completing

### Symptoms:
- Pods using Flint CSI volumes take 45+ seconds to delete
- Requires `--force --grace-period=0` to delete quickly
- Pod enters `Completed` status but object persists
- No errors in CSI driver logs - NodeUnpublishVolume succeeds immediately

### Comparison:
- **Longhorn CSI:** Pod deletion completes in ~2 seconds
- **Flint CSI:** Pod deletion hangs for ~45 seconds
- **Pod without volumes:** Deletes in ~1 second

### Investigation Status:

**What's Working:**
- ✅ NodeUnpublishVolume is called and succeeds immediately
- ✅ Unmount operations complete successfully
- ✅ Logs show "✅ Volume unpublished successfully"
- ✅ No errors or exceptions

**What's NOT Working:**
- ❌ kubectl delete hangs even after NodeUnpublishVolume returns
- ❌ Pod object lingers with deletionTimestamp set
- ❌ Eventually times out or requires --force

**Observed Sequence:**
```
T+0s:   kubectl delete pod issued
T+1s:   Kubelet sends SIGTERM to container
T+24s:  Container exits cleanly (code 0)
T+24s:  NodeUnpublishVolume called → succeeds immediately
T+24s:  Pod status changes to "Completed"
T+45s:  kubectl delete times out
T+???:  Eventually requires --force to remove
```

### Hypotheses:

1. **GRPC Response Not Reaching Kubelet:**
   - Added logging to track when responses are returned (commit 25043dd)
   - Need to verify response is sent immediately

2. **Kubelet Waiting for NodeUnstageVolume:**
   - NodeUnstageVolume is NOT called during deletion
   - Kubelet may be waiting for it before finalizing pod removal
   - But CSI spec says NodeUnstageVolume is optional/deferred

3. **Directory/Mount State Issue:**
   - Kubelet might be verifying mount state after unpublish
   - Could be stuck in a verification loop
   - Longhorn doesn't have this problem, so likely Flint-specific

4. **GRPC Connection Issue:**
   - Tonic/GRPC might have some timeout or retry logic
   - But logs show success, so unlikely

### Workaround:

Use `kubectl delete pod <name> --force --grace-period=0` for immediate deletion.

### Next Steps to Debug:

1. ✅ Deploy new image with GRPC response logging (commit 25043dd)
2. ⏳ Test deletion and verify "returning success response" appears immediately
3. ⏳ Compare our NodeUnpublishVolume timing with Longhorn's
4. ⏳ Check if directory removal is blocking (try removing that code)
5. ⏳ Test with main branch implementation to see if it has same issue

### Related Code:
- `spdk-csi-driver/src/main.rs` - NodeUnpublishVolume (lines 871-902)
- GRPC response logging added in commit 25043dd

---

## ✅ Successfully Resolved Issues

For documentation of all resolved issues, see:
- `NODESTAGE_DEBUG_SESSION.md` - Full troubleshooting journey
- `PERFORMANCE_COMPARISON.md` - Performance benchmarks vs Longhorn
- `CSI_LIFECYCLE_OBSERVATIONS.md` - CSI lifecycle behavior

---

**Status:** Core CSI functionality is working perfectly (volumes mount/unmount, data persists, performance excellent). Pod deletion delay is a usability issue that needs investigation but doesn't affect functionality.

