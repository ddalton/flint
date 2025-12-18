# NFS Service Endpoint - Stable Network Access

**Date**: 2025-12-15  
**Pattern**: Longhorn share-manager  
**Commit**: `67c2994`

## Overview

Each NFS server pod gets a **dedicated Kubernetes Service** with a stable ClusterIP, ensuring reliable NFS mounts that survive pod restarts and rescheduling.

---

## Architecture

### For Each ROX/RWX Volume:

```
┌─────────────────────────────────────────────────────────┐
│ Namespace: flint-system                                 │
│                                                         │
│  ┌─────────────────────────────────────────────────┐   │
│  │ Service: flint-nfs-pvc-99ce9e17...              │   │
│  │   Type: ClusterIP                               │   │
│  │   ClusterIP: 10.96.123.45 ◄── STABLE IP        │   │
│  │   Port: 2049/TCP                                │   │
│  │   Selector:                                     │   │
│  │     app: flint-nfs-server                       │   │
│  │     flint.io/volume-id: pvc-99ce9e17...         │   │
│  └─────────────────────────────────────────────────┘   │
│                         ↓ Routes to                     │
│  ┌─────────────────────────────────────────────────┐   │
│  │ Pod: flint-nfs-pvc-99ce9e17...                  │   │
│  │   IP: 10.244.1.50 (may change)                  │   │
│  │   Labels:                                       │   │
│  │     app: flint-nfs-server                       │   │
│  │     flint.io/volume-id: pvc-99ce9e17...         │   │
│  │   Runs: flint-nfs-server --read-only            │   │
│  └─────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────┐
│ Workload Pods (any namespace)                           │
│                                                         │
│   mount -t nfs 10.96.123.45:/ /data                    │
│         ↑                                               │
│    Stable ClusterIP - survives pod restart!             │
└─────────────────────────────────────────────────────────┘
```

---

## Service Specification

```yaml
apiVersion: v1
kind: Service
metadata:
  name: flint-nfs-pvc-99ce9e17-97c6-4f5e-9479-cdebc2de19ac
  namespace: flint-system
  labels:
    app: flint-nfs-server
    flint.io/volume-id: pvc-99ce9e17-97c6-4f5e-9479-cdebc2de19ac
spec:
  type: ClusterIP  # Default - gets stable virtual IP
  selector:
    app: flint-nfs-server
    flint.io/volume-id: pvc-99ce9e17-97c6-4f5e-9479-cdebc2de19ac
  ports:
  - name: nfs
    port: 2049
    targetPort: 2049
    protocol: TCP
```

---

## ClusterIP vs DNS Name

### ✅ We Use: ClusterIP (Stable Virtual IP)

**Why ClusterIP is better for NFS:**

| Aspect | ClusterIP | DNS Name |
|--------|-----------|----------|
| **Stability** | ✅ IP never changes | ✅ Name never changes |
| **Resolution** | Direct IP | Requires DNS lookup |
| **Performance** | ✅ No DNS overhead | Extra DNS query |
| **NFS Compatibility** | ✅ Works everywhere | May have issues with old clients |
| **Failover** | ✅ Automatic (kube-proxy) | ✅ Automatic (DNS) |
| **Common Pattern** | ✅ Standard for services | Less common |

**Answer to your question:** Longhorn and most NFS implementations use **ClusterIP directly** for NFS mounts.

---

## How It Works

### 1. Service Creation (During ControllerPublishVolume)

```rust
// Create ClusterIP Service
Service {
  name: "flint-nfs-pvc-99ce9e17...",
  namespace: "flint-system",
  spec: ServiceSpec {
    type: ClusterIP,  // Gets stable virtual IP
    selector: {
      "flint.io/volume-id": "pvc-99ce9e17..."
    },
    ports: [{ port: 2049 }]
  }
}
```

**Kubernetes allocates:** `ClusterIP: 10.96.123.45` (from service CIDR)

### 2. Pod Selection

Service selector matches pod labels → kube-proxy routes traffic:

```
Client → 10.96.123.45:2049 
           ↓ (kube-proxy)
         Pod: 10.244.1.50:2049
```

### 3. NFS Mount (In Workload Pods)

```bash
# NodePublishVolume runs:
mount -t nfs -o vers=4.2,ro 10.96.123.45:/ /var/lib/kubelet/pods/.../mount
                    ↑
              Stable ClusterIP
```

### 4. Pod Restart/Reschedule

**Scenario:** NFS pod crashes and restarts on different node

**Before Service:**
```
❌ Pod IP changes: 10.244.1.50 → 10.244.2.30
❌ Workload pods mounting 10.244.1.50 break
❌ Need to remount with new IP
```

**With Service:**
```
✅ ClusterIP stays: 10.96.123.45 (unchanged)
✅ Service routes to new pod IP automatically
✅ Workload pods continue working
✅ No remount needed
```

---

## Implementation Details

### Created Resources Per Volume:

```
Volume: pvc-99ce9e17-97c6-4f5e-9479-cdebc2de19ac
  ↓
├─ Pod:     flint-nfs-pvc-99ce9e17... (in flint-system)
└─ Service: flint-nfs-pvc-99ce9e17... (in flint-system)
            ClusterIP: 10.96.123.45 (stable)
```

### Publish Context (Returned to Workload Pods):

```rust
publish_context.insert("nfs.flint.io/server-ip", "10.96.123.45");  // ClusterIP
publish_context.insert("nfs.flint.io/port", "2049");
publish_context.insert("volumeType", "nfs");
```

### Cleanup on Volume Delete:

```rust
delete_nfs_server_pod() now deletes:
1. Service (ClusterIP released back to pool)
2. Pod (Container stopped, resources freed)
```

---

## Benefits

### ✅ Stable Endpoint
- ClusterIP allocated once, never changes
- Survives pod restarts, upgrades, node failures
- No DNS caching issues

### ✅ Automatic Failover
- Pod crashes → Kubernetes restarts it
- Service automatically routes to new pod IP
- NFS clients reconnect transparently

### ✅ Standard Pattern
- Matches how most Kubernetes services work
- Familiar to cluster administrators
- Works with NetworkPolicies, service mesh, etc.

### ✅ Multi-Namespace Support
- Service in flint-system
- Workload pods in any namespace can mount via ClusterIP
- ClusterIP is cluster-wide routable

---

## Comparison with Alternatives

### Option 1: Pod IP Directly (OLD - What we had)
```
❌ mount -t nfs 10.244.1.50:/ /data
   - IP changes on pod restart
   - Breaks NFS mounts
   - Manual remount required
```

### Option 2: Service DNS Name
```
⚠️  mount -t nfs flint-nfs-pvc-99ce9e17....svc.cluster.local:/ /data
   - Stable name ✅
   - Requires DNS resolution
   - May have caching issues
   - Longer endpoint string
```

### Option 3: Service ClusterIP (OUR CHOICE)
```
✅ mount -t nfs 10.96.123.45:/ /data
   - Stable IP ✅
   - No DNS overhead ✅
   - Standard pattern ✅
   - Clean and simple ✅
```

---

## Testing After Rebuild

Once CSI driver is rebuilt with commit `67c2994`:

```bash
export KUBECONFIG=/Users/ddalton/.kube/config.cdrv

# Create ROX PVC
kubectl apply -f /tmp/test-rox-pvc.yaml
kubectl apply -f /tmp/test-rox-pod.yaml

# Verify Service created
kubectl get svc -n flint-system | grep nfs
# Should show: flint-nfs-pvc-... with ClusterIP

# Check ClusterIP
kubectl get svc -n flint-system flint-nfs-pvc-... -o jsonpath='{.spec.clusterIP}'
# Should show: 10.96.x.x

# Verify mount uses ClusterIP
kubectl exec test-rox-reader -- mount | grep /data
# Should show: 10.96.x.x:/ on /data type nfs4

# Test failover
kubectl delete pod -n flint-system flint-nfs-pvc-...
# Wait for pod to restart
# Service ClusterIP should remain the same
# Workload pods should continue working
```

---

## Summary

✅ **Implemented stable Service endpoint following Longhorn pattern**

**Each NFS pod now has:**
- Dedicated ClusterIP Service (stable virtual IP)
- Service selector ensures traffic routes correctly
- Automatic failover on pod restart
- Works across all namespaces

**Workload pods mount via:**
- ClusterIP (not pod IP)
- Stable endpoint that never changes
- Survives pod lifecycle events

**Matches Longhorn share-manager architecture perfectly!** 🎯






