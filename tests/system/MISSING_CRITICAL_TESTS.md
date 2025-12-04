# Missing Critical System Tests - Flint CSI Driver

Analysis of critical system tests missing from the current test suite, prioritized for production readiness.

## Current Test Coverage ✅

1. ✅ RWO PVC migration between nodes (`rwo-pvc-migration`)
2. ✅ Multi-replica volumes (`multi-replica`)
3. ✅ Snapshot & restore (`snapshot-restore`)
4. ✅ Volume expansion (`volume-expansion`)
5. ✅ PVC cloning (`pvc-clone`)
6. ✅ Ephemeral inline volumes (`ephemeral-inline`)
7. ✅ Clean shutdown & fast remount (`clean-shutdown`)
8. ✅ Basic idempotency (partial in `csi-sanity/`)

---

## Missing Critical Tests

### 1. Node Failure & Recovery Tests ⚠️ **HIGH PRIORITY**

**Why Critical:** Production clusters experience node failures regularly. The CSI driver must handle these gracefully to prevent data loss and minimize downtime.

#### Tests Needed:

**1.1 Node Drain with Volumes Attached**
- Scenario: `kubectl drain` called on node with mounted volumes
- Expected: Pods gracefully terminate, volumes cleanly unmount, workloads migrate to new nodes
- Validates: Clean shutdown under controlled node maintenance

**1.2 Force Node Failure (Simulated Crash)**
- Scenario: Kill kubelet or simulate power failure while volumes mounted
- Expected: VolumeAttachment cleanup, volume accessible from another node within timeout
- Validates: Recovery from unplanned node loss

**1.3 Volume Attachment Cleanup**
- Scenario: Node becomes NotReady with attached volumes
- Expected: VolumeAttachment objects cleaned up, volumes reattachable elsewhere
- Validates: No orphaned attachments blocking pod rescheduling

**1.4 Replica Rebuild After Node Failure (Multi-Replica)**
- Scenario: Node with replica dies, volume has 2+ replicas
- Expected: Rebuild initiated automatically, degraded volume returns to healthy state
- Validates: High availability and automatic recovery

**1.5 Fencing/Split-Brain Prevention**
- Scenario: Network partition, node appears down but kubelet still running
- Expected: Volume not mounted on two nodes simultaneously, data corruption prevented
- Validates: Critical data safety requirement

**Example Test Structure:**
```yaml
# tests/system/tests-standard/node-failure-recovery/
# 00-pvc.yaml - Create PVC and write unique data
# 01-pod-node1.yaml - Pod writes data, explicitly scheduled to node1
# 02-assert.yaml - Verify data written successfully
# 03-cordon-node.yaml - kubectl cordon node1
# 04-delete-pod.yaml - Delete pod to trigger migration
# 05-assert-volumeattachment.yaml - Verify VolumeAttachment cleaned up
# 06-pod-node2.yaml - New pod scheduled on node2 (anti-affinity to node1)
# 07-assert-data.yaml - Verify data accessible and intact on node2
# 08-cleanup.yaml
```

---

### 2. Concurrent Access Tests ⚠️ **HIGH PRIORITY**

**Why Critical:** Kubernetes can schedule multiple operations concurrently. Race conditions are common sources of bugs in distributed systems.

#### Tests Needed:

**2.1 Multiple Pods Accessing Same RWO Volume**
- Scenario: Try to create 2 pods using same RWO PVC simultaneously
- Expected: Second pod fails with appropriate error (multi-attach not allowed)
- Validates: CSI driver enforces access mode restrictions

**2.2 Pod Startup Race Conditions**
- Scenario: Two pods try to mount same volume at exact same time
- Expected: One succeeds, one fails gracefully with clear error
- Validates: Controller locking mechanisms work correctly

**2.3 Concurrent Snapshot Operations**
- Scenario: Create 3 snapshots from same volume simultaneously
- Expected: All 3 snapshots created successfully with correct data
- Validates: Snapshot operation concurrency safety

**2.4 Parallel Volume Creation Stress Test**
- Scenario: Create 10-20 PVCs simultaneously
- Expected: All volumes created successfully, no conflicts
- Validates: Controller can handle concurrent CreateVolume calls

**2.5 Concurrent Delete Operations**
- Scenario: Delete volume and snapshot of that volume simultaneously
- Expected: Both operations complete without errors or deadlocks
- Validates: Dependency handling and cleanup logic

---

### 3. Resource Exhaustion Tests ⚠️ **MEDIUM PRIORITY**

**Why Critical:** Prevents resource leaks and ensures proper error reporting to users when limits are reached.

#### Tests Needed:

**3.1 Out of Disk Space (PVC Creation)**
- Scenario: Try to create PVC when all disks at capacity
- Expected: PVC stays Pending with clear error message
- Validates: Scheduler integration and capacity tracking

**3.2 Maximum Volumes Per Node**
- Scenario: Create more volumes on single node than CSINode.spec.drivers.allocatable allows
- Expected: Additional PVCs pending until capacity available
- Validates: Volume limit enforcement

**3.3 Disk Full During Write**
- Scenario: Volume fills up while pod is writing data
- Expected: Write operations fail, pod gets appropriate error, no corruption
- Validates: ENOSPC handling and volume quota enforcement

**3.4 Out of Memory During Volume Operations**
- Scenario: Large volume operations under memory pressure
- Expected: Operations fail gracefully, no OOM kills of CSI driver
- Validates: Memory management and resource limits

---

### 4. Pod Lifecycle Edge Cases ⚠️ **MEDIUM PRIORITY**

**Why Critical:** These scenarios happen frequently in production and can cause stuck pods or resource leaks.

#### Tests Needed:

**4.1 Pod Killed While Mounting Volume**
- Scenario: Pod gets OOMKilled or forcefully terminated during mount operation
- Expected: Volume unmounts cleanly, no orphaned mounts, pod can restart
- Validates: Mount operation cleanup

**4.2 Pod Crash and Restart (Volume Still Attached)**
- Scenario: Container crashes, pod restarts while volume technically still attached
- Expected: Volume remounts successfully without manual intervention
- Validates: Idempotent mount operations

**4.3 Pod Stuck in Terminating**
- Scenario: Volume unable to unmount, pod stuck in Terminating state
- Expected: Timeout occurs, clear error logged, volume force-unmounted if possible
- Validates: Unmount timeout handling

**4.4 Fast Pod Restart (< 5 seconds)**
- Scenario: Delete and immediately recreate pod using same volume
- Expected: Volume unmount/mount cycle completes successfully
- Validates: Fast turnaround scenarios (CI/CD, rapid scaling)

**4.5 Init Container Failure**
- Scenario: Volume mounts successfully but init container fails
- Expected: Volume unmounts when pod cleaned up, no leaks
- Validates: Volume lifecycle tied to pod lifecycle correctly

---

### 5. Storage Capacity Tracking ⚠️ **MEDIUM PRIORITY**

**Why Critical:** Required for scheduler to make informed placement decisions.

#### Tests Needed:

**5.1 Capacity Reporting Accuracy**
- Scenario: Compare reported capacity with actual SPDK disk capacity
- Expected: Reported capacity matches actual (within reasonable margin)
- Validates: CSIStorageCapacity objects accurate

**5.2 PVC Pending When No Capacity**
- Scenario: Request volume larger than any node can provide
- Expected: PVC stays Pending with event "insufficient storage capacity"
- Validates: Scheduler integration with capacity tracking

**5.3 Capacity Updates After Volume Operations**
- Scenario: Create/delete volumes, watch capacity changes
- Expected: Capacity updates propagate to scheduler within 60 seconds
- Validates: Real-time capacity tracking

---

### 6. Volume Metrics & Monitoring ⚠️ **LOW PRIORITY**

**Why Useful:** Enables observability and performance debugging.

#### Tests Needed:

**6.1 Metrics Accuracy**
- Scenario: Generate known I/O workload, verify metrics match
- Expected: IOPS, throughput, latency metrics accurate within 5%
- Validates: Metrics collection correctness

**6.2 Metrics Persistence**
- Scenario: Restart CSI driver pods, check metrics still available
- Expected: Historical metrics retained or clearly reset
- Validates: Metrics storage and handling

**6.3 Per-Volume Metrics**
- Scenario: Multiple volumes with different workloads
- Expected: Metrics correctly tagged by volume ID, no cross-contamination
- Validates: Metrics attribution

---

### 7. Raw Block Volumes (IF SUPPORTED) ⚠️ **DEPENDS ON FEATURE**

#### Tests Needed:

**7.1 Raw Block PVC Creation**
- Scenario: Create PVC with `volumeMode: Block`
- Expected: PVC binds, volume created without filesystem
- Validates: Block mode support

**7.2 Pod Using Raw Block Device**
- Scenario: Pod mounts raw block volume at `/dev/xvda`
- Expected: Pod can read/write raw device directly
- Validates: Block device passthrough

**7.3 Raw Block Expansion**
- Scenario: Expand raw block volume while in use
- Expected: Device size increases, no data loss
- Validates: Online expansion for block volumes

---

### 8. ReadWriteMany (RWX) (IF SUPPORTED) ⚠️ **DEPENDS ON FEATURE**

#### Tests Needed:

**8.1 Multiple Pods on Different Nodes**
- Scenario: 3 pods on different nodes accessing same RWX volume
- Expected: All pods can read/write simultaneously
- Validates: Multi-attach capability

**8.2 Concurrent Writes from Multiple Pods**
- Scenario: Multiple pods writing different files simultaneously
- Expected: No data corruption, all writes succeed
- Validates: Data consistency under concurrent access

**8.3 RWX Volume Migration**
- Scenario: Drain node with pod using RWX volume
- Expected: Pod migrates, volume remains accessible to other pods
- Validates: Partial unmount handling

---

### 9. Topology & Scheduling ⚠️ **MEDIUM PRIORITY**

**Why Critical:** Ensures volumes created where they can actually be used.

#### Tests Needed:

**9.1 Topology-Aware Volume Creation**
- Scenario: Pod with node selector, volume should be created on accessible disk
- Expected: Volume created on disk accessible from selected node
- Validates: Topology constraints honored

**9.2 Volume Locality Affinity**
- Scenario: Create volume, ensure pod scheduled on same node (if topology keys match)
- Expected: Pod scheduled where data is located for performance
- Validates: Scheduler integration with topology

**9.3 Cross-Zone Volume Access Failure**
- Scenario: Try to use volume from zone A on node in zone B
- Expected: Pod fails to start with clear topology error
- Validates: Topology validation

---

### 10. Security Tests ⚠️ **MEDIUM PRIORITY**

**Why Critical:** Security vulnerabilities can expose data or allow privilege escalation.

#### Tests Needed:

**10.1 Volume Ownership (fsGroup)**
- Scenario: Pod with `fsGroup: 1000`, check volume ownership
- Expected: All files owned by group 1000
- Validates: Kubernetes fsGroup support

**10.2 SELinux Context Labeling**
- Scenario: Pod with SELinux enabled, verify volume labels
- Expected: Correct SELinux context on mounted volumes
- Validates: SELinux integration

**10.3 Read-Only Volume Mount**
- Scenario: Mount volume with `readOnly: true`, try to write
- Expected: Write operations fail with EROFS
- Validates: Read-only enforcement

**10.4 Subpath Mounts**
- Scenario: Mount subdirectory of volume to pod
- Expected: Pod only sees specified subdirectory
- Validates: Subpath isolation

**10.5 Volume Mount Escape Prevention**
- Scenario: Attempt to escape mount via symlinks
- Expected: Symlink following blocked, security boundaries enforced
- Validates: Mount security

---

### 11. Stress & Chaos Tests ⚠️ **LOW PRIORITY**

**Why Useful:** Uncovers rare race conditions and memory leaks.

#### Tests Needed:

**11.1 Rapid Create/Delete Cycles**
- Scenario: Create and delete 100 PVCs in tight loop
- Expected: No leaks, all operations complete successfully
- Validates: Resource cleanup under stress

**11.2 Controller Pod Restart During Operations**
- Scenario: Kill controller pod mid-CreateVolume
- Expected: Operation retried automatically, volume created
- Validates: Operation idempotency and recovery

**11.3 Node Agent Pod Restart**
- Scenario: Kill node agent while volume mounted
- Expected: Volume remains accessible, agent recovers state
- Validates: Stateless node agent design

**11.4 Network Partition Simulation**
- Scenario: Drop packets between controller and nodes
- Expected: Operations timeout gracefully, retry when network recovers
- Validates: Network resilience

---

### 12. Upgrade & Rollback ⚠️ **HIGH PRIORITY** (for production)

**Why Critical:** Zero-downtime upgrades are essential for production environments.

#### Tests Needed:

**12.1 Driver Upgrade with Active Workloads**
- Scenario: Upgrade CSI driver version while pods running with mounted volumes
- Expected: Existing volumes remain mounted, no pod restarts required
- Validates: Upgrade compatibility

**12.2 Rollback Compatibility**
- Scenario: Upgrade driver, create volumes, rollback driver to old version
- Expected: Old driver can read new volume metadata
- Validates: Backward compatibility

**12.3 Zero-Downtime Upgrades**
- Scenario: Rolling update of CSI driver pods
- Expected: No disruption to existing workloads
- Validates: High availability during maintenance

**12.4 Mixed Version Cluster**
- Scenario: Some nodes running old driver, some running new
- Expected: All volumes accessible regardless of driver version
- Validates: Version skew tolerance

---

### 13. Backup & Disaster Recovery (IF APPLICABLE) ⚠️ **DEPENDS ON FEATURE**

#### Tests Needed:

**13.1 Snapshot Export/Import**
- Scenario: Create snapshot, export metadata, import on different cluster
- Expected: Snapshot restored with same data
- Validates: Cross-cluster DR capability

**13.2 Volume Backup and Restore**
- Scenario: Backup volume to object storage, restore on new cluster
- Expected: Data accessible and intact after restore
- Validates: Backup/restore workflow

**13.3 Snapshot Chain Limits**
- Scenario: Create 100 snapshots of same volume
- Expected: All snapshots succeed or clear limit error
- Validates: Snapshot chain management

---

## Recommended Implementation Priority

### **Phase 1: Production Readiness** (Implement First)

These tests are **critical** for production environments:

1. **Node drain with volumes attached** - Controlled maintenance scenario
2. **Force node failure recovery** - Unplanned outage handling
3. **Concurrent volume creation stress test** - Validate controller locking
4. **Pod crash and restart with volume** - Common pod lifecycle scenario
5. **Resource exhaustion (out of space)** - Capacity management

**Estimated Time:** 2-3 weeks

### **Phase 2: Reliability & Robustness**

These improve reliability under edge cases:

6. **Concurrent access violations** - Multiple pods trying same RWO volume
7. **Controller/Node agent pod restart during operations** - Chaos testing
8. **Fast pod restart cycles** - CI/CD scenarios
9. **Topology-aware scheduling** - Multi-zone clusters
10. **Volume attachment cleanup** - Orphaned attachment prevention

**Estimated Time:** 2-3 weeks

### **Phase 3: Advanced Features & Security**

These address advanced scenarios:

11. **Driver upgrade with active workloads** - Zero-downtime maintenance
12. **Security tests** (fsGroup, SELinux, read-only)
13. **Storage capacity tracking** - Scheduler integration
14. **Fencing/split-brain prevention** - Data safety

**Estimated Time:** 2-4 weeks

### **Phase 4: Optional Features** (as needed)

Implement only if features are supported:

15. Raw block volumes (if supported)
16. ReadWriteMany (if supported)
17. Volume metrics & monitoring
18. Backup & disaster recovery

---

## Test Template Example: Node Failure Recovery

```yaml
# tests/system/tests-standard/node-failure-recovery/README.md
# Node Failure Recovery Test
Tests volume recovery when a node fails unexpectedly

# tests/system/tests-standard/node-failure-recovery/00-pvc.yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: failure-test-pvc
spec:
  accessModes:
    - ReadWriteOnce
  storageClassName: flint
  resources:
    requests:
      storage: 5Gi

# tests/system/tests-standard/node-failure-recovery/00-assert.yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: failure-test-pvc
status:
  phase: Bound

# tests/system/tests-standard/node-failure-recovery/01-pod-node1.yaml
apiVersion: v1
kind: Pod
metadata:
  name: writer-pod
spec:
  # Explicitly schedule to first worker node
  affinity:
    nodeAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        nodeSelectorTerms:
        - matchExpressions:
          - key: kubernetes.io/hostname
            operator: In
            values:
            - worker-node-1  # Replace with actual node name
  containers:
  - name: writer
    image: busybox
    command:
    - sh
    - -c
    - |
      echo "Writing unique data: $(date +%s)" > /data/test-file.txt
      cat /data/test-file.txt
      sync
      echo "Data written successfully"
      sleep 3600
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: failure-test-pvc

# tests/system/tests-standard/node-failure-recovery/01-assert.yaml
apiVersion: v1
kind: Pod
metadata:
  name: writer-pod
status:
  phase: Running

# tests/system/tests-standard/node-failure-recovery/02-cordon-node.yaml
apiVersion: kuttl.dev/v1beta1
kind: TestStep
commands:
  - command: kubectl cordon worker-node-1
  - command: echo "Node cordoned, simulating maintenance"

# tests/system/tests-standard/node-failure-recovery/03-delete-pod.yaml
apiVersion: v1
kind: Pod
metadata:
  name: writer-pod
  annotations:
    $patch: delete

# tests/system/tests-standard/node-failure-recovery/03-assert.yaml
apiVersion: kuttl.dev/v1beta1
kind: TestAssert
commands:
  - command: kubectl wait --for=delete pod/writer-pod --timeout=60s
  - command: |
      echo "Verifying VolumeAttachment cleaned up..."
      sleep 10  # Allow time for cleanup
      VA_COUNT=$(kubectl get volumeattachment -o json | jq '[.items[] | select(.spec.source.persistentVolumeName == "failure-test-pvc")] | length')
      if [ "$VA_COUNT" -ne "0" ]; then
        echo "ERROR: VolumeAttachment not cleaned up"
        exit 1
      fi
      echo "✅ VolumeAttachment cleaned up successfully"

# tests/system/tests-standard/node-failure-recovery/04-pod-node2.yaml
apiVersion: v1
kind: Pod
metadata:
  name: reader-pod
spec:
  # Schedule to different node
  affinity:
    nodeAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        nodeSelectorTerms:
        - matchExpressions:
          - key: kubernetes.io/hostname
            operator: In
            values:
            - worker-node-2  # Different node
  containers:
  - name: reader
    image: busybox
    command:
    - sh
    - -c
    - |
      echo "Verifying data on different node..."
      if [ ! -f /data/test-file.txt ]; then
        echo "ERROR: Data file not found"
        exit 1
      fi
      cat /data/test-file.txt
      echo "✅ Data successfully accessed from different node"
      sleep 10
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: failure-test-pvc

# tests/system/tests-standard/node-failure-recovery/04-assert.yaml
apiVersion: kuttl.dev/v1beta1
kind: TestAssert
commands:
  - command: kubectl wait --for=condition=Ready pod/reader-pod --timeout=60s
  - command: |
      LOGS=$(kubectl logs reader-pod)
      if echo "$LOGS" | grep -q "Data successfully accessed"; then
        echo "✅ Node failure recovery test PASSED"
      else
        echo "❌ Test FAILED"
        exit 1
      fi

# tests/system/tests-standard/node-failure-recovery/05-cleanup.yaml
apiVersion: kuttl.dev/v1beta1
kind: TestStep
commands:
  - command: kubectl uncordon worker-node-1
  - command: kubectl delete pod reader-pod --ignore-not-found
  - command: kubectl delete pvc failure-test-pvc --ignore-not-found
```

---

## References

- [CSI Specification](https://github.com/container-storage-interface/spec)
- [Kubernetes CSI Documentation](https://kubernetes-csi.github.io/docs/)
- [CSI Driver Testing Best Practices](https://kubernetes-csi.github.io/docs/testing-drivers.html)
- [KUTTL Documentation](https://kuttl.dev/)
- [Flint CSI Architecture](../../FLINT_CSI_ARCHITECTURE.md)

---

**Document Version:** 1.0  
**Last Updated:** 2024-12-04  
**Status:** Planning Document

