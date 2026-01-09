# Disk Setup UI - With Reservation Option

## Updated Disk Setup Page

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Flint Storage Dashboard - Disk Setup                                   │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Node: master                                      [Refresh Devices]   │
│                                                                         │
│  ┌───────────────────────────────────────────────────────────────────┐ │
│  │ 📀 nvme0n1                                      Status: Healthy   │ │
│  │ ──────────────────────────────────────────────────────────────────│ │
│  │ PCI Address: 0000:03:00.0                                         │ │
│  │ Model: Samsung 990 PRO                                            │ │
│  │ Capacity: 1 TB                                                    │ │
│  │ Driver: nvme (kernel)                                             │ │
│  │                                                                   │ │
│  │ LVS Status: ✅ Initialized (lvs_nvme_0000_03_00_0n1)             │ │
│  │ Free Space: 800 GB                                                │ │
│  │ Volumes: 12 PVCs                                                  │ │
│  │                                                                   │ │
│  │ [View Volumes] [Delete LVS] [Reserve for Direct SPDK Access]    │ │
│  └───────────────────────────────────────────────────────────────────┘ │
│                                                                         │
│  ┌───────────────────────────────────────────────────────────────────┐ │
│  │ 📀 nvme1n1                                      Status: Healthy   │ │
│  │ ──────────────────────────────────────────────────────────────────│ │
│  │ PCI Address: 0000:02:00.0                                         │ │
│  │ Model: Intel Optane P5800X                                        │ │
│  │ Capacity: 1 TB                                                    │ │
│  │ Driver: vfio-pci                                                  │ │
│  │                                                                   │ │
│  │ LVS Status: ❌ Not Initialized                                    │ │
│  │                                                                   │ │
│  │ ℹ️  Choose how to use this device:                               │ │
│  │                                                                   │ │
│  │ ┌─────────────────────────┐  ┌─────────────────────────────────┐ │ │
│  │ │ CSI Managed Storage     │  │ Direct SPDK Access (Plugin)     │ │ │
│  │ │                         │  │                                 │ │ │
│  │ │ ✓ PVC provisioning      │  │ ✓ 6+ GB/s throughput           │ │ │
│  │ │ ✓ Kubernetes integration│  │ ✓ Zero-kernel I/O path         │ │ │
│  │ │ ✓ Snapshots & clones    │  │ ✓ Direct SPDK API access       │ │ │
│  │ │ ✓ Volume expansion      │  │ ⚠️  No PVC support             │ │ │
│  │ │ ~ 3-4 GB/s throughput   │  │ ⚠️  App must use SPDK APIs     │ │ │
│  │ │                         │  │                                 │ │ │
│  │ │ [Initialize LVS]        │  │ [Reserve for Plugin/Direct Use]│ │ │
│  │ └─────────────────────────┘  └─────────────────────────────────┘ │ │
│  └───────────────────────────────────────────────────────────────────┘ │
│                                                                         │
│  ┌───────────────────────────────────────────────────────────────────┐ │
│  │ 📀 nvme2n1                  🔒 RESERVED FOR DIRECT SPDK ACCESS   │ │
│  │ ──────────────────────────────────────────────────────────────────│ │
│  │ PCI Address: 0000:04:00.0                                         │ │
│  │ Model: Samsung 990 PRO                                            │ │
│  │ Capacity: 2 TB                                                    │ │
│  │ Driver: vfio-pci                                                  │ │
│  │                                                                   │ │
│  │ Reserved for: High-performance database workload                  │ │
│  │ Reserved by: admin@example.com                                    │ │
│  │ Reserved at: 2026-01-09 10:30 UTC                                │ │
│  │                                                                   │ │
│  │ ⚠️ This device is not available for PVC provisioning             │ │
│  │ ✅ Available for direct SPDK access via device plugin            │ │
│  │                                                                   │ │
│  │ Current Usage:                                                    │ │
│  │ • Pod: postgres-high-perf (Direct SPDK I/O: 6.2 GB/s)           │ │
│  │                                                                   │ │
│  │ [Unreserve Device] [View Usage Details]                          │ │
│  └───────────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────────────┘
```

## Reserve Device Dialog (from Disk Setup)

When user clicks "Reserve for Plugin/Direct Use":

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Reserve Device for Direct SPDK Access                                  │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Device: nvme1n1 (0000:02:00.0)                                        │
│  Model: Intel Optane P5800X                                            │
│  Capacity: 1 TB                                                        │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ What does this mean?                                            │  │
│  │                                                                 │  │
│  │ Reserving this device will:                                     │  │
│  │                                                                 │  │
│  │ ❌ Prevent CSI from initializing LVS on this device             │  │
│  │ ❌ Prevent PVC provisioning on this device                      │  │
│  │ ❌ Remove device from CSI-managed storage pool                  │  │
│  │                                                                 │  │
│  │ ✅ Enable direct SPDK API access for applications               │  │
│  │ ✅ Device driver: vfio-pci (userspace) - OK for reservation    │  │
│  │ ✅ Achieve maximum performance (6+ GB/s)                        │  │
│  │ ✅ Zero-kernel I/O path (vfio-pci)                             │  │
│  │ ✅ Perfect for databases, high-performance workloads            │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                         │
│  Reservation Details:                                                  │
│                                                                         │
│  Purpose/Description: *                                                │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ High-performance PostgreSQL database with direct SPDK access   │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                         │
│  Tags (comma-separated):                                               │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ database, high-performance, production                          │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                         │
│  Assigned to (optional):                                               │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ team-database@example.com                                       │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ ⚙️ Advanced Options                                             │  │
│  │                                                                 │  │
│  │ ☑ Bind device to vfio-pci driver now                            │  │
│  │ ☐ Install SPDK device plugin automatically                      │  │
│  │ ☑ Add to flint-reserved-devices ConfigMap                       │  │
│  │ ☑ Notify all CSI nodes to reload config                         │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                         │
│  [Cancel]                           [Reserve Device for Direct Access]│
└─────────────────────────────────────────────────────────────────────────┘
```

## After Reservation Success

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Device Reserved Successfully! 🎉                                        │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  ✅ Device nvme1n1 (0000:02:00.0) is now reserved for direct access    │
│  ✅ Bound to vfio-pci driver                                           │
│  ✅ Added to flint-reserved-devices ConfigMap                          │
│  ✅ All CSI nodes notified to skip this device                         │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ 📋 Next Steps - How to Use This Device                          │  │
│  │                                                                 │  │
│  │ Option 1: Deploy SPDK Device Plugin (Recommended)              │  │
│  │ ───────────────────────────────────────────────────────────────  │  │
│  │                                                                 │  │
│  │ 1. Install device plugin:                                      │  │
│  │    $ helm install spdk-dp ./spdk-device-plugin-chart \         │  │
│  │        --set devices[0]=0000:02:00.0                           │  │
│  │                                                                 │  │
│  │ 2. Use in your pod:                                            │  │
│  │    ```yaml                                                     │  │
│  │    resources:                                                  │  │
│  │      limits:                                                   │  │
│  │        flint.io/nvme: 1                                        │  │
│  │        hugepages-2Mi: 1Gi                                      │  │
│  │    ```                                                         │  │
│  │                                                                 │  │
│  │ [📥 Download SPDK Device Plugin Helm Chart]                    │  │
│  │ [📖 View Device Plugin Documentation]                          │  │
│  │                                                                 │  │
│  │ ──────────────────────────────────────────────────────────────  │  │
│  │                                                                 │  │
│  │ Option 2: Manual SPDK Application                              │  │
│  │ ───────────────────────────────────────────────────────────────  │  │
│  │                                                                 │  │
│  │ PCI Address: 0000:02:00.0                                      │  │
│  │ vfio-pci Group: /dev/vfio/XX                                   │  │
│  │                                                                 │  │
│  │ Example C code:                                                │  │
│  │ ```c                                                           │  │
│  │ struct spdk_env_opts opts;                                     │  │
│  │ spdk_env_opts_init(&opts);                                     │  │
│  │ opts.core_mask = "0x1";                                        │  │
│  │ spdk_env_init(&opts);                                          │  │
│  │                                                                 │  │
│  │ spdk_nvme_probe(NULL, NULL, probe_cb, attach_cb, NULL);       │  │
│  │ // Device at 0000:02:00.0 will be discovered                  │  │
│  │ ```                                                            │  │
│  │                                                                 │  │
│  │ [📋 Copy Example Code]                                         │  │
│  │ [📖 View SPDK API Documentation]                               │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                         │
│  [View Reserved Devices] [Setup Another Device] [Close]               │
└─────────────────────────────────────────────────────────────────────────┘
```

## Comparison Table in UI

When hovering over "?" icon next to buttons:

```
┌─────────────────────────────────────────────────────────────────────────┐
│ CSI Managed vs Direct SPDK Access                                      │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Feature              │ CSI Managed (LVS)  │ Direct SPDK (Reserved)   │
│  ─────────────────────┼────────────────────┼──────────────────────────│
│  Setup                │ Click "Init LVS"   │ Click "Reserve"          │
│  PVC Support          │ ✅ Yes             │ ❌ No                    │
│  Snapshots            │ ✅ Yes             │ ❌ No                    │
│  Volume Expansion     │ ✅ Yes             │ ❌ No                    │
│  Multi-tenancy        │ ✅ Yes             │ ⚠️  Single app          │
│  Kubernetes Native    │ ✅ Yes             │ ⚠️  Requires plugin     │
│  ─────────────────────┼────────────────────┼──────────────────────────│
│  Throughput (128KB)   │ ~3-4 GB/s          │ ~6-7 GB/s                │
│  IOPS (4KB)           │ ~800k IOPS         │ ~1.2M IOPS               │
│  Latency              │ ~100-200μs         │ ~50-80μs                 │
│  Kernel Overhead      │ High (VFS+FS)      │ None (userspace)         │
│  CPU Efficiency       │ Medium             │ High (polling)           │
│  ─────────────────────┼────────────────────┼──────────────────────────│
│  Best For             │ General workloads  │ High-perf databases      │
│                       │ Multiple PVCs      │ Single high-perf app     │
│                       │ Standard K8s apps  │ Custom SPDK apps         │
│                                                                         │
│  [Close]                                                               │
└─────────────────────────────────────────────────────────────────────────┘
```

## State Machine

```
Disk State Transitions:

  ┌──────────────┐
  │ Uninitialized│
  │  (No LVS)    │
  └───────┬──────┘
          │
          ├─────────────────┐
          │                 │
          │                 │
     [Initialize LVS]  [Reserve for
          │            Direct Access]
          │                 │
          ▼                 ▼
  ┌──────────────┐   ┌──────────────┐
  │ CSI Managed  │   │   Reserved   │
  │  (Has LVS)   │   │ (Direct SPDK)│
  └───────┬──────┘   └───────┬──────┘
          │                  │
     [Delete LVS]      [Unreserve]
          │                  │
          ▼                  ▼
  ┌──────────────┐   ┌──────────────┐
  │ Uninitialized│◄──┤ Uninitialized│
  └──────────────┘   └──────────────┘

State Rules:
- Cannot reserve device that has LVS
- Cannot initialize LVS on reserved device
- Must unreserve before initializing LVS
- Must delete LVS before reserving
- **CRITICAL: Can only reserve devices with userspace drivers (vfio-pci, uio_pci_generic)**
- Kernel driver devices (nvme) cannot be reserved - no performance benefit
```

## Kernel Driver Validation

When attempting to reserve a kernel-driver device, the UI shows an error:

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Cannot Reserve Device                                                  │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  ❌ ERROR: This device uses a kernel driver                           │
│                                                                         │
│  Device: nvme0n1 (0000:03:00.0)                                        │
│  Current Driver: nvme (kernel)                                         │
│                                                                         │
│  ⚠️  Kernel driver devices CANNOT be used for direct SPDK access      │
│                                                                         │
│  Why? SPDK device plugin requires:                                     │
│  • Direct PCI access (vfio-pci or uio_pci_generic)                    │
│  • Userspace driver for zero-copy I/O                                  │
│  • No kernel involvement in the I/O path                               │
│                                                                         │
│  Current kernel driver provides:                                       │
│  • 3-4 GB/s throughput (via io_uring)                                  │
│  • Already available through CSI-managed storage                       │
│  • No benefit from reservation                                         │
│                                                                         │
│  Solution: Bind device to vfio-pci first                              │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ # Bind to vfio-pci (requires node access)                       │  │
│  │ echo "0000:03:00.0" > /sys/bus/pci/devices/0000:03:00.0/driver/unbind│
│  │ echo "vfio-pci" > /sys/bus/pci/devices/0000:03:00.0/driver_override │
│  │ echo "0000:03:00.0" > /sys/bus/pci/drivers/vfio-pci/bind        │  │
│  │                                                                  │  │
│  │ # Verify                                                         │  │
│  │ ls -l /sys/bus/pci/devices/0000:03:00.0/driver                  │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                         │
│  After binding to vfio-pci:                                            │
│  • Refresh this page                                                   │
│  • Device will show "Driver: vfio-pci"                                │
│  • "Reserve" button will become enabled                                │
│                                                                         │
│  [Close]                                        [Copy Bind Commands]   │
└─────────────────────────────────────────────────────────────────────────┘
```


## API Endpoints

```typescript
// Backend API for disk setup with reservation

// Initialize LVS (existing)
POST /api/disks/initialize
{
  "bdev_name": "nvme_0000_02_00_0n1"
}

// Reserve device for direct access (new)
POST /api/disks/reserve
{
  "pci_address": "0000:02:00.0",
  "bdev_name": "nvme_0000_02_00_0n1",
  "purpose": "High-performance database",
  "tags": ["database", "production"],
  "assigned_to": "team-database@example.com",
  "bind_vfio": true  // Auto-bind to vfio-pci
}

Response:
{
  "success": true,
  "device": {
    "pci_address": "0000:02:00.0",
    "bdev_name": "nvme_0000_02_00_0n1",
    "reserved": true,
    "driver": "vfio-pci",
    "configmap_updated": true
  },
  "next_steps": {
    "device_plugin_install": "helm install spdk-dp ...",
    "example_pod_yaml": "..."
  }
}

// Unreserve device (new)
POST /api/disks/unreserve
{
  "pci_address": "0000:02:00.0"
}

// Get reservation status (new)
GET /api/disks/reserved
Response:
{
  "reserved_devices": [
    {
      "pci_address": "0000:02:00.0",
      "bdev_name": "nvme_0000_02_00_0n1",
      "purpose": "High-performance database",
      "reserved_at": "2026-01-09T10:30:00Z",
      "reserved_by": "admin@example.com",
      "in_use_by_pods": ["postgres-high-perf"]
    }
  ]
}
```

This design provides a clear, user-friendly way to choose between CSI-managed storage and high-performance direct SPDK access!
