# Multi-Replica Quick Start Guide

## Overview

The Flint CSI driver now supports **distributed RAID 1** multi-replica volumes for high availability. This guide shows you how to use this feature.

## Prerequisites

- Kubernetes cluster with at least 2 nodes
- Flint CSI driver deployed
- Each node has at least one initialized disk (with LVS)

## Usage

### 1. Create a Multi-Replica StorageClass

Create a StorageClass with the desired number of replicas:

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-ha
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "2"        # Number of replicas (2 or more)
  thinProvision: "false"   # Optional: thin provisioning
allowVolumeExpansion: false
volumeBindingMode: Immediate
```

Apply it:
```bash
kubectl apply -f storageclass.yaml
```

### 2. Create a PVC

Create a PersistentVolumeClaim using the StorageClass:

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: my-ha-volume
spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 10Gi
  storageClassName: flint-ha
```

Apply it:
```bash
kubectl apply -f pvc.yaml
```

### 3. Verify PVC is Bound

Check the PVC status:

```bash
kubectl get pvc my-ha-volume
```

Expected output:
```
NAME           STATUS   VOLUME                                     CAPACITY   ACCESS MODES   STORAGECLASS   AGE
my-ha-volume   Bound    pvc-12345678-1234-1234-1234-123456789abc   10Gi       RWO            flint-ha       5s
```

### 4. Verify Replica Metadata

Check the PV for replica metadata:

```bash
PV_NAME=$(kubectl get pvc my-ha-volume -o jsonpath='{.spec.volumeName}')
kubectl get pv $PV_NAME -o jsonpath='{.spec.csi.volumeAttributes}' | jq
```

Expected output:
```json
{
  "flint.csi.storage.io/replica-count": "2",
  "flint.csi.storage.io/replicas": "[{\"node_name\":\"node1\",...},{\"node_name\":\"node2\",...}]"
}
```

### 5. Use the Volume in a Pod

Create a Pod that uses the PVC:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: my-app
spec:
  containers:
  - name: app
    image: nginx:latest
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: my-ha-volume
```

Apply it:
```bash
kubectl apply -f pod.yaml
```

### 6. Verify RAID Creation

Check the CSI driver logs on the node where the Pod is running:

```bash
# Get the node name
NODE=$(kubectl get pod my-app -o jsonpath='{.spec.nodeName}')
echo "Pod running on: $NODE"

# Check CSI driver logs
kubectl logs -n kube-system -l app=flint-csi-node --tail=100 | grep -i raid
```

Expected log entries:
```
🔧 [DRIVER] Creating RAID 1 on node: node1
🔧 [DRIVER] Processing 2 replicas...
   Replica 1: LOCAL access (lvol: 12345678-...)
   Replica 2: REMOTE access (node: node2, setting up NVMe-oF...)
✅ [DRIVER] RAID 1 bdev created: raid_pvc-12345678...
```

## Verification Commands

### Check Replica Distribution

```bash
# Get PV name
PV_NAME=$(kubectl get pvc my-ha-volume -o jsonpath='{.spec.volumeName}')

# Get replica info
kubectl get pv $PV_NAME -o jsonpath='{.spec.csi.volumeAttributes.flint\.csi\.storage\.io/replicas}' | jq '.[].node_name'
```

This should show replicas on different nodes:
```
"node1"
"node2"
```

### Check SPDK RAID Status

SSH to the node where the Pod is running and check SPDK:

```bash
# Get SPDK RPC URL (adjust for your setup)
SPDK_RPC_URL="http://localhost:8081/api/spdk/rpc"

# List RAID bdevs
curl -X POST $SPDK_RPC_URL -d '{
  "jsonrpc": "2.0",
  "method": "bdev_raid_get_bdevs",
  "params": {"category": "all"},
  "id": 1
}' | jq
```

### Write Test Data

```bash
kubectl exec my-app -- sh -c 'dd if=/dev/urandom of=/data/testfile bs=1M count=100'
kubectl exec my-app -- sh -c 'md5sum /data/testfile'
```

## Troubleshooting

### PVC Stuck in Pending

**Symptom**: PVC remains in `Pending` state

**Possible Causes**:
1. Not enough nodes with sufficient capacity
2. Only 1 node available but requested 2+ replicas

**Check**:
```bash
kubectl describe pvc my-ha-volume
kubectl get events --field-selector involvedObject.name=my-ha-volume
```

**Solution**:
- Ensure at least N nodes have sufficient free space (where N = numReplicas)
- Check CSI controller logs:
  ```bash
  kubectl logs -n kube-system -l app=flint-csi-controller --tail=50
  ```

### RAID Creation Failed

**Symptom**: Pod fails to start, volume not mounting

**Check**:
```bash
kubectl describe pod my-app
kubectl logs -n kube-system -l app=flint-csi-node --tail=100
```

**Common Issues**:
- Remote node not reachable (network issues)
- NVMe-oF target setup failed
- Minimum 2 replicas not available

### Replica on Wrong Node

**Symptom**: All replicas created on same node

**Check**:
```bash
PV_NAME=$(kubectl get pvc my-ha-volume -o jsonpath='{.spec.volumeName}')
kubectl get pv $PV_NAME -o jsonpath='{.spec.csi.volumeAttributes.flint\.csi\.storage\.io/replicas}' | jq '.[].node_name'
```

**Note**: If you only have 1 node in your cluster, multi-replica volumes will fail. This is by design - replicas MUST be on different nodes for true HA.

## Performance Considerations

### RAID 1 Performance Characteristics

- **Read**: Can be faster (reads from any replica)
- **Write**: Slower than single replica (writes to all replicas)
- **Network**: Remote replicas use NVMe-oF (network overhead)

### Recommended Use Cases

✅ **Good for**:
- Critical data requiring high availability
- Databases that can tolerate slightly higher write latency
- Applications that prioritize data durability
- Read-heavy workloads

❌ **Not ideal for**:
- Write-heavy workloads with strict latency requirements
- Temporary data that doesn't need redundancy
- Applications on single-node clusters

## Cleanup

```bash
# Delete Pod
kubectl delete pod my-app

# Delete PVC (this will delete all replicas)
kubectl delete pvc my-ha-volume

# Delete StorageClass
kubectl delete storageclass flint-ha
```

## Advanced: 3-Way Mirroring

For maximum redundancy, use 3 replicas:

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: flint-ha-3way
provisioner: flint.csi.storage.io
parameters:
  numReplicas: "3"
```

This creates RAID 1 with 3 base bdevs, allowing the volume to survive 2 node failures.

**Requirements**:
- At least 3 nodes in cluster
- Each node has sufficient free space

## Comparison: Single vs Multi-Replica

| Feature | Single Replica | Multi-Replica (2-way) | Multi-Replica (3-way) |
|---------|----------------|----------------------|----------------------|
| Nodes Required | 1 | 2 | 3 |
| Node Failure Tolerance | 0 | 1 | 2 |
| Write Performance | Fastest | Moderate | Slower |
| Network Traffic | None | Moderate | High |
| Storage Efficiency | 100% | 50% | 33% |
| Use Case | Dev/Test | Production | Mission Critical |

## Next Steps

1. **Test Failure Scenarios**: Stop a node and verify volume still accessible
2. **Monitor Performance**: Use `kubectl top` and SPDK metrics
3. **Plan Capacity**: Account for N×storage when using N replicas

## Support

For issues or questions:
- Check CSI driver logs: `kubectl logs -n kube-system -l app=flint-csi-controller`
- Review implementation docs: `MULTI_REPLICA_IMPLEMENTATION_COMPLETE.md`
- Run system tests: `cd tests/system && kubectl kuttl test --test multi-replica`

---

**Happy Multi-Replicating! 🚀**

