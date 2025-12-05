# Zero-Regression Design for RWX/NFS Support

**Feature Branch**: `feature/rwx-nfs-support`  
**Commit**: `4d96264` (Infrastructure)  
**Status**: ✅ Phase 1 Complete - Infrastructure Established

## Design Principles

###1. Feature Flag Gating

**All NFS functionality is disabled by default:**

```yaml
# values.yaml
nfs:
  enabled: false  # ← DEFAULT: Maintains existing RWO-only behavior
```

```rust
// Environment variable check at every entry point
let config = match NfsConfig::from_env() {
    Some(c) => c,
    None => return Ok(()),  // ← Early return if disabled
};
```

### 2. Isolated Module Architecture

```
spdk-csi-driver/src/
├── main.rs                # Existing CSI implementation (UNCHANGED)
├── driver.rs              # Volume management (UNCHANGED)
├── node_agent.rs          # Node operations (UNCHANGED)
└── rwx_nfs.rs             # ← NEW: All NFS code isolated here
```

**Benefits:**
- ✅ Easy to identify new vs existing code
- ✅ Can be removed entirely if needed
- ✅ No scattered modifications across codebase
- ✅ Clear ownership and testing boundary

### 3. Additive-Only Changes

**What Changed:**
- ✅ Added environment variables (controlled by Helm)
- ✅ Added RBAC permissions (pod create/delete)
- ✅ Added new module (`rwx_nfs.rs`)
- ✅ Added configuration to values.yaml

**What Did NOT Change:**
- ❌ No modifications to existing RWO code paths
- ❌ No changes to volume creation logic
- ❌ No changes to ublk device management
- ❌ No changes to RAID or snapshot code

### 4. Safe Integration Points

The NFS module will integrate at these specific, well-defined points:

| CSI RPC | Integration | Gated By | Fallback |
|---------|-------------|----------|----------|
| **CreateVolume** | Add `nfs.flint.io/*` to `volume_context` | Check `access_mode == MultiNodeMultiWriter` | Normal RWO creation |
| **ControllerPublishVolume** | Create NFS pod if RWX | Check `volume_context["nfs.flint.io/enabled"]` | Normal RWO publish |
| **NodePublishVolume** | Mount NFS | Check `publish_context["nfs.flint.io/server-ip"]` | Normal ublk mount |
| **DeleteVolume** | Delete NFS pod | Check `volume_context["nfs.flint.io/enabled"]` | Normal deletion |

**Each integration point:**
1. Checks if NFS is enabled
2. Checks if volume requests RWX
3. Returns early if either is false
4. Executes existing RWO code path (UNCHANGED)

## Code Changes Summary

### Phase 1: Infrastructure (✅ Complete)

#### 1. Helm Configuration

```yaml
# flint-csi-driver-chart/values.yaml
images:
  flintNfsServer:
    name: flint-nfs-server
    tag: latest
    pullPolicy: IfNotPresent

nfs:
  enabled: false  # ← DISABLED BY DEFAULT
  port: 2049
  resources:
    requests:
      memory: "128Mi"
      cpu: "100m"
    limits:
      memory: "256Mi"
      cpu: "500m"
```

#### 2. Controller Environment Variables

```yaml
# templates/controller.yaml (only added when nfs.enabled=true)
{{- if .Values.nfs.enabled }}
env:
  - name: NFS_ENABLED
    value: "true"
  - name: NFS_IMAGE_REPOSITORY
    value: {{ .Values.images.repository }}
  # ... (15 more env vars)
{{- else }}
env:
  - name: NFS_ENABLED
    value: "false"  # ← Explicit disable
{{- end }}
```

#### 3. RBAC Permissions

```yaml
# templates/rbac.yaml - Added permissions (only used when NFS enabled)
rules:
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["get", "list", "watch", "create", "delete", "patch", "update"]
  - apiGroups: [""]
    resources: ["pods/status", "pods/log"]
    verbs: ["get"]
```

#### 4. Isolated NFS Module

```rust
// src/rwx_nfs.rs - 600+ lines of isolated NFS functionality
pub fn is_nfs_enabled() -> bool { /* ... */ }
pub async fn create_nfs_server_pod(...) -> Result<(), Status> { /* ... */ }
pub async fn wait_for_nfs_pod_ready(...) -> Result<(String, String), Status> { /* ... */ }
pub async fn delete_nfs_server_pod(...) -> Result<(), Status> { /* ... */ }
```

**Key safety features:**
- Every function checks `NFS_ENABLED` environment variable
- Early returns if disabled
- Comprehensive logging (🚀, ✅, ⚠️, ❌ prefixes)
- No side effects when disabled

### Phase 2: CSI Integration (Next)

Will add minimal, gated integration points to existing CSI RPCs:

```rust
// Pseudocode - actual implementation will be in next commits
async fn create_volume(...) -> Result<...> {
    // Existing RWO logic (UNCHANGED)
    let volume_result = self.driver.create_volume(...).await?;
    
    // NEW: RWX check (additive only)
    let is_rwx = req.volume_capabilities.iter().any(|cap| {
        cap.access_mode.mode == MultiNodeMultiWriter
    });
    
    if is_rwx && rwx_nfs::is_nfs_enabled() {
        // Add NFS metadata to volume_context
        volume_context.insert("nfs.flint.io/enabled", "true");
        volume_context.insert("nfs.flint.io/replica-nodes", ...);
    }
    
    // Return (no change to RWO path)
    Ok(Response::new(CreateVolumeResponse { volume }))
}
```

## Testing Strategy

### Unit Tests

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_nfs_disabled_by_default() {
        // Ensure NFS returns early when disabled
        assert!(!is_nfs_enabled());
    }
    
    #[test]
    fn test_parse_replica_nodes() {
        let mut ctx = HashMap::new();
        ctx.insert("nfs.flint.io/replica-nodes".to_string(), "node-1,node-2".to_string());
        assert_eq!(parse_replica_nodes(&ctx).unwrap(), vec!["node-1", "node-2"]);
    }
}
```

### Integration Tests

```yaml
# Test 1: RWO volumes unaffected (nfs.enabled=false)
# Test 2: RWX rejected when nfs.enabled=false
# Test 3: RWX works when nfs.enabled=true
# Test 4: Multiple RWO and RWX volumes coexist
```

## Regression Prevention Checklist

- [x] **Feature disabled by default** (`nfs.enabled=false`)
- [x] **All code in isolated module** (`src/rwx_nfs.rs`)
- [x] **Early returns when disabled** (every function checks `NFS_ENABLED`)
- [x] **No modifications to existing RWO paths**
- [x] **Additive RBAC only** (permissions only used when NFS enabled)
- [x] **Comprehensive logging** (visibility into code paths taken)
- [x] **Environment variable gating** (Helm controls feature)
- [ ] **Integration tests** (verify RWO unchanged)
- [ ] **Rollback plan** (can disable via Helm values)

## Rollback Plan

If issues arise, disable NFS support instantly:

```bash
# Option 1: Helm upgrade with NFS disabled
helm upgrade flint-csi ./flint-csi-driver-chart \
  --set nfs.enabled=false \
  --reuse-values

# Option 2: Revert to main branch
git checkout main
helm upgrade flint-csi ./flint-csi-driver-chart
```

**Impact of rollback:**
- ✅ All existing RWO volumes continue working
- ✅ No data loss
- ❌ New RWX PVC requests will be rejected
- ❌ Existing RWX volumes will become unavailable (NFS pods deleted)

## Deployment Strategy

### Phase 1: Internal Testing (Current)
- Deploy with `nfs.enabled=false` to production
- Verify zero impact on existing volumes
- Enable NFS in dev/test environment only

### Phase 2: Limited Rollout
- Enable NFS for specific namespaces/teams
- Monitor NFS pod lifecycle
- Collect feedback

### Phase 3: General Availability
- Document RWX support in README
- Enable by default for new installations
- Provide migration guide for RWO → RWX

## Monitoring & Observability

### Log Prefixes

| Prefix | Meaning | When to Expect |
|--------|---------|----------------|
| `ℹ️ [NFS]` | Informational | NFS disabled message |
| `🚀 [NFS]` | Starting operation | Pod creation started |
| `✅ [NFS]` | Success | Pod created, pod ready |
| `⏳ [NFS]` | Waiting | Waiting for pod readiness |
| `⚠️ [NFS]` | Warning | Non-fatal issues |
| `❌ [NFS]` | Error | Fatal failures |

### Metrics to Monitor

- NFS pod creation time (<60s)
- NFS pod readiness (<60s)
- Number of active NFS pods
- Number of RWX volumes
- RWO vs RWX volume ratio

## Next Steps

**Phase 2: CSI Integration**
1. Add RWX detection in CreateVolume
2. Add NFS pod creation in ControllerPublishVolume
3. Add NFS mount in NodePublishVolume
4. Add NFS cleanup in DeleteVolume

Each integration point will:
- Check feature flag first
- Log entry/exit clearly
- Fall back to RWO path if not RWX
- Have unit tests

---

**Summary**: This design ensures that adding RWX/NFS support has **absolutely zero impact** on existing RWO functionality. The feature is:
- Disabled by default
- Fully isolated
- Easily removable
- Comprehensively logged
- Ready for gradual rollout
