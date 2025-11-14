# Dashboard API Timeout Fix - November 14, 2025

## Problem
The dashboard backend was crashing with CrashLoopBackOff because it was making HTTP requests to node agents without timeouts. When the node agents couldn't respond (because SPDK was down due to CPU instruction incompatibility), the requests would hang indefinitely, causing:

1. **Liveness probe failures** - Dashboard not responding to health checks
2. **Container kills** - Exit code 137 (SIGKILL) from Kubernetes
3. **Cascading failures** - Dashboard couldn't start even though it should show available data

## Root Cause Chain
```
SPDK binary (spdk-tgt) crashes with SIGILL (exit code 132)
  ↓
Node agents can't query SPDK via Unix socket
  ↓
Dashboard backend HTTP requests to node agents hang forever (no timeout)
  ↓
Dashboard liveness probe times out after 5 seconds
  ↓
Kubernetes kills dashboard container (exit code 137)
  ↓
CrashLoopBackOff
```

## Solution

### 1. Added HTTP Client Timeouts
Modified `/Users/ddalton/projects/rust/flint/spdk-csi-driver/src/spdk_dashboard_backend_minimal.rs`:

**In `fetch_all_disks_from_node_agents()`:**
```rust
let http_client = HttpClient::builder()
    .timeout(std::time::Duration::from_secs(5))  // Client-level timeout
    .build()?;
    
// Per-request timeout
.timeout(std::time::Duration::from_secs(3))
```

**In `proxy_node_agent_endpoint()`:**
```rust
let http_client = HttpClient::builder()
    .timeout(std::time::Duration::from_secs(10))  // Client-level timeout
    .build()
    .map_err(|_| warp::reject::reject())?;

// Per-request timeout
.timeout(std::time::Duration::from_secs(8))
```

### 2. Graceful Degradation
Changed error handling to continue with other nodes instead of failing completely:

```rust
Err(e) => {
    println!("⚠️ [DISK_FETCH] Failed to connect to {} (timeout or connection error): {}", node_name, e);
    // Continue with other nodes instead of failing completely
}
```

### 3. Added Missing Proxy Routes
The frontend expects these endpoints that were missing:

- `POST /api/nodes/{node}/disks/reset` - Reset disk configuration
- `POST /api/nodes/{node}/disks/delete` - Delete disk and cleanup

These routes now properly proxy to the corresponding node agent endpoints with timeouts.

## Comparison with Old CRD-Based Backend

Reviewed the `raid_over_lv` branch and found:
- Old backend also did simple HTTP proxying to node agents
- **Old backend also lacked timeouts!** This was likely an existing issue
- Same basic architecture: dashboard → node agents → SPDK
- New minimal backend is simpler (no CRD queries), but needs same resilience

## Expected Behavior After Fix

1. **Dashboard starts successfully** even if some/all node agents are down
2. **Graceful degradation** - Shows data from responsive nodes, skips unresponsive ones
3. **No more hanging requests** - All HTTP calls timeout within 10 seconds
4. **Liveness probes succeed** - Dashboard responds quickly to health checks
5. **Partial data display** - Can show controller data even if node agents are unavailable

## Testing

To verify the fix works:

```bash
# Rebuild and redeploy
cd spdk-csi-driver
./scripts/build.sh
docker push docker-sandbox.infra.cloudera.com/ddalton/flint-driver:latest

# Restart dashboard pod
kubectl delete pod -n flint-system -l app=spdk-dashboard

# Check dashboard logs
kubectl logs -n flint-system -l app=spdk-dashboard -c dashboard-backend --follow

# Expected: Dashboard starts and shows "Failed to connect" warnings but doesn't crash
```

## Next Steps

1. **Fix SPDK CPU instruction issue** - Primary blocker for full functionality
2. **Test dashboard with working SPDK** - Verify data flows correctly
3. **Add retry logic** - Consider exponential backoff for transient failures
4. **Monitor metrics** - Track timeout rates in production

## Related Issues

- SPDK spdk-tgt container: Exit code 132 (SIGILL) - CPU instruction incompatibility
- Node agents: Can't connect to SPDK Unix socket (connection refused)
- Dashboard: Needs to be resilient to partial infrastructure failures

