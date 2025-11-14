# Blobstore Clean Shutdown Fix - November 14, 2025

## Problem Discovered

After adding blobstore recovery logging, we discovered that the blobstore was consistently showing:
```
clean flag: 0
REASON: Blobstore was not cleanly unmounted
DECISION: Recovery required
```

This meant **every pod restart triggers full blobstore recovery**, even though we have preStop hooks for graceful shutdown.

## Root Cause

Kubernetes event logs revealed the issue:
```
Warning  FailedKillPod  error killing pod: [failed to "KillContainer" for "flint-csi-driver" 
with KillContainerError: "rpc error: code = DeadlineExceeded desc = context deadline exceeded"]
```

**The pod termination was timing out!**

Even though we have:
- preStop hook that stops ublk devices, deletes NVMe-oF subsystems, and calls `spdk_kill_instance` (waits 12s)
- terminationGracePeriodSeconds of 30 seconds (default)

The container runtime was **force-killing (SIGKILL)** the pod before SPDK could:
1. Flush blobstore metadata
2. Mark the blobstore as clean
3. Shut down gracefully

## Why 30 Seconds Wasn't Enough

The preStop hook sequence:
1. Stop all ublk devices: ~2-5s (depending on number of volumes)
2. Delete NVMe-oF subsystems: ~1-3s  
3. Send SIGTERM to SPDK: immediate
4. Wait for SPDK shutdown: 12s
5. **Total: ~15-20 seconds**

But there's also:
- Kubernetes overhead for container termination
- Network I/O delays
- Blobstore flush operations can take longer under load
- Recovery itself (on dirty shutdown) delays the next restart

This created a **vicious cycle**: unclean shutdown → recovery on next start → slow startup → timeout on next shutdown → repeat.

## Solution

**Increase `terminationGracePeriodSeconds` to 90 seconds**

This gives ample time for:
- preStop hook execution: ~20s
- SPDK blobstore flush: ~5-10s  
- Thread cleanup and shutdown: ~5s
- Buffer for system delays: ~55s

### Change Made

`flint-csi-driver-chart/templates/node.yaml`:
```yaml
spec:
  terminationGracePeriodSeconds: 90  # Increased from default 30s
```

## Expected Behavior After Fix

### First Restart (after deploying fix):
```
clean flag: 0  
REASON: Blobstore was not cleanly unmounted  # From previous forced termination
Recovery: ~30-60 seconds
```

### Subsequent Restarts:
```
clean flag: 1
DECISION: Clean blobstore, no recovery needed  # ✅
Fast startup: ~5-10 seconds
```

## Verification Steps

After deploying the fix:

1. **First restart** - Will still show unclean (from before fix):
   ```bash
   kubectl rollout restart daemonset/flint-csi-node -n flint-system
   kubectl logs -n flint-system <pod> -c spdk-tgt | grep "BLOBSTORE LOAD"
   # Expect: clean flag: 0, recovery required
   ```

2. **Second restart** - Should be clean:
   ```bash
   kubectl rollout restart daemonset/flint-csi-node -n flint-system
   kubectl logs -n flint-system <pod> -c spdk-tgt | grep "BLOBSTORE LOAD"  
   # Expect: clean flag: 1, no recovery needed
   ```

3. **Check events** - No more FailedKillPod warnings:
   ```bash
   kubectl get events -n flint-system | grep -i "failed\|kill"
   # Should be empty
   ```

## Benefits

1. **Fast restarts**: No recovery needed → pods start in ~5-10s instead of 30-60s
2. **Data safety**: Blobstore metadata properly flushed
3. **Reliability**: No forced kills, clean shutdown every time
4. **Debugging**: Clear logging shows clean vs. unclean shutdowns

## Related Files

- `flint-csi-driver-chart/templates/node.yaml` - Added terminationGracePeriodSeconds
- `spdk-csi-driver/blob-recovery-progress.patch` - Added logging that exposed this issue

## Lessons Learned

1. **Default grace periods are often insufficient** for stateful storage systems
2. **Force kills (SIGKILL) corrupt state** even with preStop hooks
3. **Detailed logging is crucial** - without the blobstore recovery logging, we wouldn't have known about this issue
4. **Test graceful shutdown** as part of development, not just functionality


