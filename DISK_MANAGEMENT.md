# Disk Management Guide

This guide explains how to add and manage storage disks with the SPDK CSI driver.

## 🎯 **Current Setup: Boot-Disk-Only Mode**

Your CSI driver is currently running in **boot-disk-only mode**:

- ✅ **Boot disk** (`nvme0n1`): Protected from SPDK (correctly mounted for OS)
- ⏳ **Storage disks**: Ready to be added when available
- ✅ **SPDK daemon**: Running and ready for disk addition

## 💾 **Adding Storage Disks**

### **Step 1: Add Physical Disks**

Add NVMe SSDs or other storage devices to your nodes:

- **Cloud environments**: Attach additional EBS/persistent disks
- **Bare metal**: Install additional NVMe SSDs
- **VM environments**: Add virtual disks

### **Step 2: Verify Disk Detection**

Check that Kubernetes nodes can see the new disks:

```bash
# On each node, check for new disks
lsblk | grep nvme
# Should show nvme0n1 (boot) + nvme1n1, nvme2n1, etc. (storage)

# Check PCI devices
lspci | grep -i nvme
```

### **Step 3: Prepare Disks (Important!)**

**⚠️ WARNING**: Only do this for **NON-BOOT** disks!

```bash
# Example for nvme1n1 (NOT nvme0n1 which is boot disk)
sudo umount /dev/nvme1n1* 2>/dev/null || true  # Unmount if mounted
sudo wipefs -a /dev/nvme1n1                    # Clear filesystem signatures
```

### **Step 4: Restart Node Agents**

Restart the CSI node agents to discover new disks:

```bash
kubectl rollout restart daemonset/flint-csi-node -n flint-system
```

### **Step 5: Verify Disk Discovery**

Check that the CSI driver discovered the new disks:

```bash
# Check for SPDK disk resources
kubectl get spdkdisks -A

# Should show entries like:
# NAME                NODE                  DEVICE        STATE
# nvme1n1-flnt-1     flnt-1.vpc.cloudera.com  /dev/nvme1n1  available
# nvme2n1-flnt-2     flnt-2.vpc.cloudera.com  /dev/nvme2n1  available
```

## 🧪 **Testing Without Real Disks**

For **testing purposes**, you can create loop devices:

```bash
# Create test "disks" using loop devices (for testing only)
sudo dd if=/dev/zero of=/tmp/spdk-test1.img bs=1G count=10
sudo losetup /dev/loop1 /tmp/spdk-test1.img

# The CSI driver can then discover and use /dev/loop1
```

## 📊 **Disk Requirements**

### **Minimum Requirements**
- **Size**: 1GB+ per disk  
- **Type**: NVMe (preferred), SSD, or loop devices
- **Count**: 2+ disks for RAID1 functionality
- **State**: Unmounted and unused

### **Recommended Setup**
- **Per Node**: 2-4 NVMe SSDs
- **Size**: 100GB+ each for production
- **Performance**: High-speed NVMe for best SPDK performance

## 🔧 **Current Status Check**

Check your current setup:

```bash
# Verify CSI driver is ready
kubectl get pods -n flint-system

# Check available storage capacity
kubectl get csistoragecapacities

# View node disk status
kubectl describe nodes | grep -A5 -B5 "storage"
```

## 🚀 **What Happens When You Add Disks**

1. **Automatic Discovery**: Node agents detect new disks
2. **Resource Creation**: `SpdkDisk` resources are created automatically  
3. **Volume Creation**: You can create `PersistentVolumeClaim`s
4. **RAID Configuration**: SPDK sets up RAID1 across available disks

## ⚡ **Quick Start After Adding Disks**

```bash
# 1. Add disks to nodes (physical/cloud/virtual)

# 2. Restart CSI driver
kubectl rollout restart daemonset/flint-csi-node -n flint-system

# 3. Verify disk discovery
kubectl get spdkdisks -A

# 4. Create test PVC
kubectl apply -f - << 'EOF'
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: spdk-test-pvc
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 10Gi
  storageClassName: flint
EOF

# 5. Check PVC status
kubectl get pvc spdk-test-pvc
```

## 🎯 **Current State: Ready for Disks**

Your SPDK CSI driver is correctly configured and **ready** for storage disks:

- ✅ **SPDK Target**: Running in standby mode
- ✅ **Node Agents**: Ready to discover new disks  
- ✅ **Controller**: Ready to create volumes
- ✅ **Dashboard**: Monitoring system status

Simply add storage disks when ready, and the system will automatically configure them! 🎉 