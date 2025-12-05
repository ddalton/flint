# ReadWriteMany (RWX) Support via NFS Server

**Feature Branch**: `feature/rwx-nfs-support`  
**Status**: Design & Planning  
**Target**: Flint CSI Driver v2.x

## Overview

This document outlines the design for adding ReadWriteMany (RWX) volume support to the Flint CSI driver using the existing NFS server implementation.

## Architecture

### High-Level Flow

```
┌─────────────────────────────────────────────────────────────────┐
│ User creates RWX PVC                                             │
└─────────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────────┐
│ CSI Controller: CreateVolume                                     │
│ - Detects RWX access mode                                       │
│ - Creates volume (single or multi-replica)                      │
│ - Stores replica nodes in volume_context                        │
└─────────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────────┐
│ CSI Controller: ControllerPublishVolume                          │
│ - Creates NFS server pod with node affinity to replica nodes    │
│ - Pod scheduled by K8s to one of the replica nodes              │
│ - Returns NFS server IP in publish_context                      │
└─────────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────────┐
│ NFS Server Pod (on replica node)                                │
│ - NodeStageVolume creates ublk device (or RAID bdev)            │
│ - flint-nfs-server exports /mnt/volume over NFS                 │
│ - Local access to replica = high performance                    │
└─────────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────────┐
│ Client Pods: NodePublishVolume (on any nodes)                   │
│ - Mounts NFS export instead of ublk device                      │
│ - Multiple pods can mount simultaneously                        │
│ - All pods see same filesystem                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Key Design Decisions

### 1. Node Affinity vs Fixed Placement

**Decision**: Use Kubernetes Node Affinity to constrain NFS pod to replica nodes, let scheduler choose the best one.

**Rationale**:
- ✅ Scheduler optimizes based on resources, load, topology
- ✅ Automatic failover if a replica node becomes unhealthy
- ✅ Better load balancing across cluster
- ❌ Hardcoded `spec.nodeName` would be inflexible

### 2. Volume Types Supported

| Volume Type | Replicas | NFS Pod Placement | Data Access |
|-------------|----------|-------------------|-------------|
| **Single Replica** | 1 | On the replica node | Local (fast) |
| **Multi-Replica** | 2-3 | On any replica node | Local via RAID bdev |

### 3. NFS Pod Lifecycle

- **Created**: During `ControllerPublishVolume` (first client attachment)
- **Managed**: By CSI controller, not user-visible
- **Deleted**: During `DeleteVolume` (when PVC is deleted)

### 4. Existing PV Handling

For pre-existing PVs with RWX access mode:
- `volume_context` will contain `nfs.flint.io/enabled=true`
- NFS pod should already exist from initial creation
- If pod is missing (e.g., deleted manually), `ControllerPublishVolume` will recreate it

## Implementation Checklist

### Phase 1: Configuration & Infrastructure
- [ ] Add `images.flintNfsServer` to values.yaml
- [ ] Add NFS configuration section to values.yaml
- [ ] Update controller template with NFS environment variables
- [ ] Update RBAC for pod management (create, delete, get, list, watch)

### Phase 2: CSI Controller Changes
- [ ] Detect RWX in `CreateVolume` via `VolumeCapability.access_mode`
- [ ] Store replica nodes in `volume_context["nfs.flint.io/replica-nodes"]`
- [ ] Implement `create_nfs_server_pod()` with node affinity
- [ ] Implement `wait_for_nfs_pod_ready()` helper
- [ ] Add NFS pod creation logic to `ControllerPublishVolume`
- [ ] Add NFS pod deletion logic to `DeleteVolume`

### Phase 3: CSI Node Changes
- [ ] Detect NFS volumes in `NodePublishVolume` via `publish_context`
- [ ] Mount NFS instead of ublk for RWX volumes
- [ ] Unmount NFS in `NodeUnpublishVolume`
- [ ] Handle NFS mount errors gracefully

### Phase 4: Testing & Documentation
- [ ] Build and push flint-nfs-server Docker image
- [ ] Create KUTTL test: single-replica RWX
- [ ] Create KUTTL test: multi-replica RWX
- [ ] Create KUTTL test: multiple pods writing concurrently
- [ ] Update README with RWX usage examples
- [ ] Create RWX troubleshooting guide

## Code Changes Overview

### 1. values.yaml

```yaml
images:
  flintNfsServer:
    name: flint-nfs-server
    tag: latest
    pullPolicy: IfNotPresent

nfs:
  enabled: true
  port: 2049
  resources:
    requests:
      memory: "128Mi"
      cpu: "100m"
    limits:
      memory: "256Mi"
      cpu: "500m"
```

### 2. RBAC (templates/rbac.yaml)

```yaml
rules:
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["get", "list", "create", "delete", "watch"]
  - apiGroups: [""]
    resources: ["pods/status"]
    verbs: ["get"]
```

### 3. CreateVolume (src/main.rs)

```rust
// Detect RWX
let is_rwx = req.volume_capabilities.iter().any(|cap| {
    cap.access_mode.mode == Mode::MultiNodeMultiWriter
});

// Store replica nodes in volume_context
if is_rwx {
    let replica_nodes = volume_result.replicas
        .iter()
        .map(|r| r.node_name.clone())
        .collect::<Vec<_>>()
        .join(",");
    
    volume_context.insert("nfs.flint.io/enabled", "true");
    volume_context.insert("nfs.flint.io/replica-nodes", replica_nodes);
}
```

### 4. ControllerPublishVolume (src/main.rs)

```rust
if is_nfs_enabled {
    let replica_nodes = parse_replica_nodes(&volume_context)?;
    
    if !nfs_pod_exists(volume_id).await? {
        create_nfs_server_pod(volume_id, &replica_nodes).await?;
    }
    
    let (nfs_node, nfs_ip) = wait_for_nfs_pod_ready(volume_id).await?;
    
    publish_context.insert("nfs.flint.io/server-ip", nfs_ip);
    publish_context.insert("nfs.flint.io/server-node", nfs_node);
    publish_context.insert("nfs.flint.io/export-path", format!("/exports/{}", volume_id));
}
```

### 5. NodePublishVolume (src/main.rs)

```rust
if let Some(nfs_ip) = publish_context.get("nfs.flint.io/server-ip") {
    let export_path = publish_context["nfs.flint.io/export-path"];
    
    // Mount NFS
    Command::new("mount")
        .args(&["-t", "nfs", "-o", "vers=3,tcp",
                &format!("{}:{}", nfs_ip, export_path),
                &target_path])
        .output()?;
}
```

## Volume Context Schema

| Key | Value | Set By | Used By |
|-----|-------|--------|---------|
| `nfs.flint.io/enabled` | `"true"` | CreateVolume | ControllerPublish, NodePublish |
| `nfs.flint.io/replica-nodes` | `"node-1,node-2,node-3"` | CreateVolume | ControllerPublish |
| `nfs.flint.io/server-ip` | `"10.244.1.5"` | ControllerPublish | NodePublish |
| `nfs.flint.io/server-node` | `"node-2"` | ControllerPublish | Monitoring |
| `nfs.flint.io/export-path` | `"/exports/pvc-abc"` | ControllerPublish | NodePublish |

## NFS Pod Specification

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: flint-nfs-<volume-id>
  labels:
    app: flint-nfs-server
    volume-id: <volume-id>
spec:
  # Node affinity to replica nodes (scheduler picks best)
  affinity:
    nodeAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        nodeSelectorTerms:
        - matchExpressions:
          - key: kubernetes.io/hostname
            operator: In
            values: [node-1, node-2, node-3]  # Replica nodes
  
  containers:
  - name: nfs-server
    image: <registry>/flint-nfs-server:latest
    args:
    - --export-path=/mnt/volume
    - --volume-id=<volume-id>
    - --port=2049
    
    ports:
    - containerPort: 2049
      protocol: TCP
    
    volumeMounts:
    - name: volume-data
      mountPath: /mnt/volume
  
  volumes:
  - name: volume-data
    persistentVolumeClaim:
      claimName: <pvc-name>  # The RWX PVC itself
```

## Testing Strategy

### Test 1: Single-Replica RWX
```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: rwx-single-test
spec:
  accessModes:
  - ReadWriteMany
  resources:
    requests:
      storage: 1Gi
  storageClassName: flint
---
# Deploy 3 pods on different nodes, all writing to same volume
```

### Test 2: Multi-Replica RWX
```yaml
storageClass:
  parameters:
    numReplicas: "3"
---
# Same test, verify RAID bdev created on NFS pod node
```

### Test 3: Concurrent Writes
```bash
# Pod 1: while true; do echo "pod1-$(date)" >> /data/log; sleep 1; done
# Pod 2: while true; do echo "pod2-$(date)" >> /data/log; sleep 1; done
# Pod 3: while true; do echo "pod3-$(date)" >> /data/log; sleep 1; done
# Verify: cat /data/log shows interleaved writes
```

## Performance Expectations

| Access Pattern | Single Replica | Multi-Replica |
|----------------|----------------|---------------|
| **NFS Pod (local)** | ~1-2 GB/s | ~1-2 GB/s (via RAID) |
| **Client Pods (remote)** | ~500-800 MB/s | ~500-800 MB/s |
| **Latency** | ~500μs (metadata) | ~500μs (metadata) |

## Security Considerations

1. **NFS Authentication**: Currently AUTH_NULL (pod isolation via K8s)
2. **Network Policies**: Ensure clients can reach NFS pod on port 2049
3. **RBAC**: Controller needs pod create/delete permissions
4. **Pod Security**: NFS pod may need privileged access for mounting

## Limitations & Future Work

### Current Limitations
- ❌ No HA for NFS pod (single point of failure)
- ❌ No NFS server health monitoring
- ❌ No automatic failover if NFS pod dies

### Future Enhancements
- Add NFS pod replica set with VIP for HA
- Implement health monitoring and auto-restart
- Support NFSv4 for better performance
- Add pNFS for parallel access

## References

- **NFS Server Implementation**: `spdk-csi-driver/src/nfs/`
- **NFS Roadmap**: `NFS_IMPLEMENTATION_ROADMAP.md`
- **CSI Spec**: [Container Storage Interface Specification](https://github.com/container-storage-interface/spec/blob/master/spec.md)
- **Kubernetes Node Affinity**: [Assigning Pods to Nodes](https://kubernetes.io/docs/concepts/scheduling-eviction/assign-pod-node/)

## Decision Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2025-12-05 | Use Node Affinity over spec.nodeName | More flexible, allows scheduler optimization |
| 2025-12-05 | Create NFS pod in ControllerPublish | Ensures pod exists before client mounts |
| 2025-12-05 | Store all replica nodes in volume_context | Enables node affinity constraint |
| 2025-12-05 | Use existing flint-nfs-server binary | Already implemented and tested |

## Open Questions

- [ ] Should we support NFSv4 in addition to NFSv3?
- [ ] How to handle NFS pod OOM/crash scenarios?
- [ ] Should we implement read-only NFS exports for ROX access mode?
- [ ] Network policy defaults for NFS access?

---

**Next Steps**: Begin Phase 1 implementation (Configuration & Infrastructure)

