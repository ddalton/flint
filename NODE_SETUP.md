# Node Setup Guide for SPDK CSI Driver

This document provides step-by-step instructions for preparing Kubernetes nodes to run the SPDK-based CSI driver.

## Prerequisites

- Kubernetes cluster with version 1.19+
- Nodes with NVMe SSDs
- Root access to worker nodes
- Linux kernel 4.15+ with VFIO support

## 1. System Requirements

### Hardware Requirements
- **NVMe Devices**: At least 2 NVMe SSDs per node for RAID1 functionality
- **Memory**: Minimum 4GB RAM with 2GB+ hugepages
- **CPU**: x86_64 processor with DPDK support
- **Network**: 10Gbps+ network for optimal NVMe-oF performance

### Software Requirements
- **Kernel Modules**: `vfio-pci`, `uio_pci_generic`, `nvme`
- **Hugepages**: 2MB or 1GB hugepages configured
- **IOMMU**: Intel VT-d or AMD-Vi enabled in BIOS

## 2. Node Preparation Steps

### Step 1: Enable IOMMU and Hugepages

Add to `/etc/default/grub`:
```bash
GRUB_CMDLINE_LINUX="intel_iommu=on iommu=pt hugepagesz=1G hugepages=4 hugepagesz=2M hugepages=1024 default_hugepagesz=1G"
```

For AMD systems, use `amd_iommu=on` instead of `intel_iommu=on`.

Update GRUB and reboot:
```bash
sudo update-grub
sudo reboot
```

### Step 2: Load Required Kernel Modules

Create `/etc/modules-load.d/spdk.conf`:
```
vfio-pci
uio_pci_generic
vfio_iommu_type1
```

Load modules immediately:
```bash
sudo modprobe vfio-pci
sudo modprobe uio_pci_generic
sudo modprobe vfio_iommu_type1
```

### Step 3: Verify Hugepages Configuration

Check hugepages are available:
```bash
# Check 1GB hugepages
cat /proc/meminfo | grep HugePages
cat /sys/kernel/mm/hugepages/hugepages-1048576kB/nr_hugepages

# Check 2MB hugepages  
cat /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
```

Mount hugepages (if not already mounted):
```bash
sudo mkdir -p /mnt/huge
sudo mount -t hugetlbfs nodev /mnt/huge
```

### Step 4: Identify and Prepare NVMe Devices

List available NVMe devices:
```bash
lspci | grep -i nvme
lsblk -d -o NAME,SIZE,MODEL | grep nvme
```

**Important**: Ensure NVMe devices are not mounted or in use by the kernel:
```bash
# Check if devices are mounted
lsblk | grep nvme

# Unmount if necessary (CAUTION: This will destroy data!)
sudo umount /dev/nvme0n1*
sudo umount /dev/nvme1n1*

# Stop any services using the devices
sudo systemctl stop <any-service-using-nvme>
```

### Step 5: Bind NVMe Devices to VFIO (Optional for SPDK)

If you want SPDK to have exclusive control:
```bash
# Find device IDs
lspci -nn | grep -i nvme

# Example output: 01:00.0 Non-Volatile memory controller [0108]: Samsung Electronics Co Ltd ... [144d:a808]

# Bind to vfio-pci
echo 144d a808 | sudo tee /sys/bus/pci/drivers/vfio-pci/new_id
echo 0000:01:00.0 | sudo tee /sys/bus/pci/devices/0000:01:00.0/driver/unbind
echo 0000:01:00.0 | sudo tee /sys/bus/pci/drivers/vfio-pci/bind
```

### Step 6: Configure Node Labels (Optional)

Label nodes with SPDK capability:
```bash
kubectl label node <node-name> spdk.csi.storage.io/nvme=enabled
kubectl label node <node-name> spdk.csi.storage.io/hugepages=1G
```

## 3. Installation and Verification

### Install SPDK CSI Driver

```bash
# Add Helm repository (adjust URL to your actual repository)
helm repo add flint-csi ./flint-csi-driver-chart

# Install with custom values
helm install flint-csi flint-csi/flint-csi-driver-chart \
  --namespace spdk-system \
  --create-namespace \
  --set images.repository=your-registry.com/flint \
  --set storageClass.isDefaultClass=false
```

### Verify Installation

1. **Check pods are running**:
```bash
kubectl get pods -n spdk-system
```

2. **Check CSI driver registration**:
```bash
kubectl get csidriver
kubectl get csistoragecapacities
```

3. **Verify Custom Resource Definitions**:
```bash
kubectl get crd | grep flint.csi.storage.io
```

4. **Check node agent logs**:
```bash
kubectl logs -n spdk-system daemonset/flint-csi-node -c node-agent
```

5. **Verify SPDK disk discovery**:
```bash
kubectl get spdkdisks -n spdk-system
```

## 4. Testing the Installation

### Create a test PVC:
```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: spdk-test-pvc
spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 10Gi
  storageClassName: flint
```

### Create a test pod:
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: spdk-test-pod
spec:
  containers:
  - name: test
    image: nginx
    volumeMounts:
    - name: spdk-volume
      mountPath: /data
  volumes:
  - name: spdk-volume
    persistentVolumeClaim:
      claimName: spdk-test-pvc
```

Apply and verify:
```bash
kubectl apply -f test-pvc.yaml
kubectl apply -f test-pod.yaml
kubectl get pvc,pods
```

## 5. Troubleshooting

### Common Issues

1. **Hugepages not available**:
   - Verify GRUB configuration and reboot
   - Check `/proc/meminfo | grep HugePages`

2. **NVMe devices not detected**:
   - Ensure devices are not mounted
   - Check kernel modules are loaded
   - Verify IOMMU is enabled

3. **Pod stuck in ContainerCreating**:
   - Check node agent logs
   - Verify RBAC permissions
   - Check if SPDK daemon is running

4. **Volume creation fails**:
   - Check controller logs
   - Verify SpdkDisk resources exist
   - Check network connectivity between nodes

### Logs and Debugging

```bash
# Controller logs
kubectl logs -n spdk-system deployment/flint-csi-controller -c flint-csi-controller

# Node agent logs
kubectl logs -n spdk-system daemonset/flint-csi-node -c node-agent

# CSI driver logs
kubectl logs -n spdk-system daemonset/flint-csi-node -c flint-csi-driver

# SPDK daemon logs
kubectl logs -n spdk-system daemonset/flint-csi-node -c spdk-tgt
```

### Cleanup Commands

```bash
# Remove test resources
kubectl delete -f test-pod.yaml
kubectl delete -f test-pvc.yaml

# Uninstall CSI driver
helm uninstall flint-csi -n spdk-system

# Cleanup hugepages (if needed)
echo 0 | sudo tee /sys/kernel/mm/hugepages/hugepages-1048576kB/nr_hugepages

# Rebind NVMe to kernel driver
echo 0000:01:00.0 | sudo tee /sys/bus/pci/devices/0000:01:00.0/driver/unbind
echo 0000:01:00.0 | sudo tee /sys/bus/pci/drivers/nvme/bind
```

## 6. Production Considerations

- **Backup Data**: Always backup data before binding NVMe devices to SPDK
- **Resource Limits**: Configure appropriate CPU and memory limits
- **Monitoring**: Set up monitoring for SPDK metrics and volume health
- **Network Policies**: Configure appropriate network policies for NVMe-oF traffic
- **Security**: Review security contexts and capabilities
- **Updates**: Plan for rolling updates with data migration

## Security Notes

The SPDK CSI driver requires privileged containers and host access for:
- Direct hardware access to NVMe devices
- Hugepage memory management
- Kernel module loading
- Network interface configuration

Review your security policies before deployment in production environments. 