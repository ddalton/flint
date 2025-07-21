# SPDK CSI Driver Deployment Guide

This guide provides instructions for building, deploying, and testing the SPDK CSI driver.

## Quick Start

### Prerequisites
1. Kubernetes cluster (v1.19+)
2. Docker or compatible container runtime
3. Helm 3.x
4. Rust toolchain (for building from source)

### 1. Build Container Images

```bash
# Navigate to the CSI driver source
cd spdk-csi-driver

# Build all container images
./scripts/build.sh

# Or build individually:
docker build -t flint/flint-controller:latest -f docker/Dockerfile.controller .
docker build -t flint/flint-driver:latest -f docker/Dockerfile.csi .
docker build -t flint/flint-node-agent:latest -f docker/Dockerfile.node-agent .
docker build -t flint/spdk-tgt:latest -f docker/Dockerfile.spdk .
```

### 2. Push Images to Registry

```bash
# Tag and push to your registry
docker tag flint/flint-controller:latest your-registry.com/flint/flint-controller:latest
docker tag flint/flint-driver:latest your-registry.com/flint/flint-driver:latest
docker tag flint/flint-node-agent:latest your-registry.com/flint/flint-node-agent:latest
docker tag flint/spdk-tgt:latest your-registry.com/flint/spdk-tgt:latest

# Push to registry
docker push your-registry.com/flint/flint-controller:latest
docker push your-registry.com/flint/flint-driver:latest
docker push your-registry.com/flint/flint-node-agent:latest
docker push your-registry.com/flint/spdk-tgt:latest
```

### 3. Prepare Nodes

Follow the [Node Setup Guide](NODE_SETUP.md) to prepare your Kubernetes nodes.

### 4. Deploy CSI Driver

```bash
# Install using Helm
cd flint-csi-driver-chart

# Deploy with default configuration
helm install flint-csi . \
  --namespace spdk-system \
  --create-namespace \
  --set images.repository=your-registry.com/flint

# Or deploy with custom values
helm install flint-csi . \
  --namespace spdk-system \
  --create-namespace \
  --values custom-values.yaml
```

### 5. Verify Installation

```bash
# Check all pods are running
kubectl get pods -n spdk-system

# Verify CSI driver registration
kubectl get csidriver flint.csi.storage.io

# Check storage class
kubectl get storageclass flint

# Verify CRDs are installed
kubectl get crd | grep flint.csi.storage.io
```

## Custom Values Configuration

Create a `custom-values.yaml` file for your environment:

```yaml
# Custom values for SPDK CSI Driver
images:
  repository: your-registry.com/flint
  
driver:
  name: "flint.csi.storage.io"

storageClass:
  create: true
  name: "spdk-fast"
  isDefaultClass: false
  reclaimPolicy: "Delete"
  allowVolumeExpansion: true
  parameters:
    numReplicas: "2"
    autoRebuild: "true"

# Enable additional logging
logLevel: 9

# Node selector to run only on SPDK-enabled nodes
nodeSelector:
  spdk.csi.storage.io/nvme: "enabled"

# Tolerations for dedicated SPDK nodes
tolerations:
  - key: "spdk.csi.storage.io/dedicated"
    operator: "Equal"
    value: "true"
    effect: "NoSchedule"
```

## Testing the Installation

### 1. Basic Functionality Test

Create a test PVC:
```bash
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: spdk-test-pvc
  namespace: default
spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 1Gi
  storageClassName: flint
EOF
```

Create a test pod:
```bash
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: spdk-test-pod
  namespace: default
spec:
  containers:
  - name: test-container
    image: busybox
    command: ["sleep", "3600"]
    volumeMounts:
    - name: test-volume
      mountPath: /data
  volumes:
  - name: test-volume
    persistentVolumeClaim:
      claimName: spdk-test-pvc
EOF
```

Verify the volume:
```bash
# Check PVC status
kubectl get pvc spdk-test-pvc

# Check pod is running
kubectl get pod spdk-test-pod

# Test writing to the volume
kubectl exec spdk-test-pod -- sh -c "echo 'Hello SPDK!' > /data/test.txt"
kubectl exec spdk-test-pod -- cat /data/test.txt
```

### 2. Performance Test

Create a performance test pod:
```bash
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: spdk-perf-test
spec:
  containers:
  - name: fio
    image: nixery.dev/fio
    command: ["fio"]
    args:
      - "--name=randwrite"
      - "--ioengine=libaio"
      - "--iodepth=16"
      - "--rw=randwrite"
      - "--bs=4k"
      - "--direct=1"
      - "--size=1G"
      - "--numjobs=4"
      - "--runtime=60"
      - "--group_reporting"
      - "--filename=/data/test.fio"
    volumeMounts:
    - name: perf-volume
      mountPath: /data
  volumes:
  - name: perf-volume
    persistentVolumeClaim:
      claimName: spdk-perf-pvc
  restartPolicy: Never
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: spdk-perf-pvc
spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 5Gi
  storageClassName: flint
EOF
```

Monitor performance:
```bash
kubectl logs spdk-perf-test
```

### 3. Multi-Replica Test

Create a multi-replica volume:
```bash
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: spdk-raid-test
spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 2Gi
  storageClassName: flint
  # This will use the default numReplicas=2 from storage class
EOF
```

Verify RAID1 configuration:
```bash
# Check the created SpdkVolume
kubectl get spdkvolumes

# Describe the volume to see replica details
kubectl describe spdkvolume <volume-name>

# Check SPDK disks
kubectl get spdkdisks
```

## Monitoring and Troubleshooting

### Check Component Status

```bash
# Controller status
kubectl logs -n spdk-system deployment/flint-csi-controller -c flint-csi-controller

# Node agent status on each node
kubectl logs -n spdk-system daemonset/flint-csi-node -c node-agent

# CSI driver status
kubectl logs -n spdk-system daemonset/flint-csi-node -c flint-csi-driver

# SPDK daemon status
kubectl logs -n spdk-system daemonset/flint-csi-node -c spdk-tgt
```

### Check Resource Status

```bash
# Check discovered disks
kubectl get spdkdisks -o wide

# Check volumes
kubectl get spdkvolumes -o wide

# Check snapshots (if any)
kubectl get spdksnapshots -o wide
```

### Common Issues and Solutions

1. **Pods stuck in Pending**:
   - Check node selectors and tolerations
   - Verify hugepages are configured
   - Check resource requests vs available resources

2. **CSI driver not registering**:
   - Verify kubelet plugin directory is correctly mounted
   - Check socket permissions and paths
   - Verify CSI node driver registrar is running

3. **Volume creation fails**:
   - Check if enough healthy SpdkDisk resources exist
   - Verify SPDK daemon is running and accessible
   - Check network connectivity between nodes for NVMe-oF

4. **Performance issues**:
   - Verify hugepages are being used
   - Check NUMA configuration
   - Monitor network bandwidth for remote replicas

## Cleanup

### Remove Test Resources
```bash
kubectl delete pod spdk-test-pod spdk-perf-test
kubectl delete pvc spdk-test-pvc spdk-perf-pvc spdk-raid-test
```

### Uninstall CSI Driver
```bash
helm uninstall flint-csi -n spdk-system
kubectl delete namespace spdk-system
```

### Cleanup Node Configuration
Follow the cleanup section in [Node Setup Guide](NODE_SETUP.md).

## Production Deployment Considerations

1. **Security**:
   - Review and restrict RBAC permissions
   - Use non-root containers where possible
   - Implement network policies
   - Consider Pod Security Standards

2. **High Availability**:
   - Deploy across multiple availability zones
   - Configure anti-affinity for controller pods
   - Plan for node maintenance and upgrades

3. **Monitoring**:
   - Set up prometheus metrics collection
   - Monitor SPDK metrics and disk health
   - Configure alerting for volume degradation

4. **Backup and Recovery**:
   - Implement volume snapshot workflows
   - Plan for disaster recovery scenarios
   - Test restore procedures regularly

5. **Performance Tuning**:
   - Optimize hugepage allocation
   - Tune SPDK configuration parameters
   - Monitor and adjust replica placement

For more detailed configuration options, refer to the Helm chart values and SPDK documentation. 