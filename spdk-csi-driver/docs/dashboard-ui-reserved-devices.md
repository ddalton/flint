# Dashboard UI: Reserved Devices Management

## Device List Page with Reservation Support

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Flint Storage Dashboard - Devices                                      │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  [All Devices] [CSI Managed] [Reserved for High Performance]           │
│                                                                         │
│  ┌───────────────────────────────────────────────────────────────────┐ │
│  │ Device: nvme0n1                                    ✅ CSI Managed │ │
│  │ PCI: 0000:03:00.0                                                 │ │
│  │ Capacity: 1TB                                                     │ │
│  │ Model: Samsung 990 PRO                                            │ │
│  │ Status: Healthy                                                   │ │
│  │ LVS: lvs_nvme_0000_03_00_0n1 (Free: 500GB, Used: 500GB)         │ │
│  │ Volumes: 12 PVCs                                                  │ │
│  │                                                                   │ │
│  │ [View Volumes] [Expand] [Reserve for High Performance] ────────┐ │ │
│  └───────────────────────────────────────────────────────────────────┘ │
│                                                                         │
│  ┌───────────────────────────────────────────────────────────────────┐ │
│  │ Device: nvme1n1                   🔒 RESERVED (High Performance) │ │
│  │ PCI: 0000:02:00.0                                                 │ │
│  │ Capacity: 1TB                                                     │ │
│  │ Model: Intel Optane P5800X                                        │ │
│  │ Status: Healthy                                                   │ │
│  │ Reserved for: Database direct SPDK access                         │ │
│  │ Reserved by: admin@example.com                                    │ │
│  │ Reserved at: 2026-01-09 10:30:00 UTC                             │ │
│  │                                                                   │ │
│  │ ⚠️ This device is not available for PVC provisioning             │ │
│  │ Applications can access it directly via SPDK device plugin        │ │
│  │                                                                   │ │
│  │ [Unreserve Device] [View Direct Access Pods] ──────────────────┐ │ │
│  └───────────────────────────────────────────────────────────────────┘ │
│                                                                         │
│  ┌───────────────────────────────────────────────────────────────────┐ │
│  │ Device: nvme2n1                                    ✅ CSI Managed │ │
│  │ PCI: 0000:04:00.0                                                 │ │
│  │ Capacity: 2TB                                                     │ │
│  │ Model: Samsung 990 PRO                                            │ │
│  │ Status: Healthy                                                   │ │
│  │ LVS: lvs_nvme_0000_04_00_0n1 (Free: 1.8TB, Used: 200GB)         │ │
│  │ Volumes: 5 PVCs                                                   │ │
│  │                                                                   │ │
│  │ [View Volumes] [Expand] [Reserve for High Performance]          │ │
│  └───────────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────────────┘
```

## Reserve Device Dialog

When user clicks "Reserve for High Performance":

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Reserve Device for High Performance Access                             │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Device: nvme0n1 (0000:03:00.0)                                        │
│  Capacity: 1TB (Samsung 990 PRO)                                       │
│                                                                         │
│  ⚠️ WARNING: Reserving this device will:                              │
│                                                                         │
│  ❌ Remove it from CSI managed storage                                 │
│  ❌ Prevent new PVC creation on this device                            │
│  ❌ Existing PVCs on this device: 12                                   │
│     → These will continue to work but no new PVCs can be created       │
│                                                                         │
│  ✅ Enable direct SPDK access for applications                         │
│  ✅ Achieve 6+ GB/s throughput (vs 3-4 GB/s with CSI)                 │
│  ✅ Zero-kernel path for maximum performance                           │
│                                                                         │
│  Reason for reservation:                                               │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ High-performance database workload                              │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                         │
│  Options:                                                              │
│  ☐ Delete existing PVCs on this device (WARNING: DATA LOSS!)          │
│  ☑ Keep existing PVCs (recommended)                                    │
│                                                                         │
│  [Cancel]                                     [Reserve Device] ───────┐ │
└─────────────────────────────────────────────────────────────────────────┘
```

## After Reservation - Usage Guide

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Device Reserved Successfully                                            │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  ✅ Device nvme1n1 (0000:02:00.0) is now reserved                      │
│                                                                         │
│  How to use this device in your applications:                          │
│                                                                         │
│  Option 1: Using Device Plugin (Recommended)                           │
│  ─────────────────────────────────────────────────────────────────────  │
│                                                                         │
│  Install SPDK Device Plugin:                                           │
│  $ helm install spdk-device-plugin ./spdk-device-plugin-chart          │
│                                                                         │
│  Use in Pod:                                                           │
│  ```yaml                                                               │
│  apiVersion: v1                                                        │
│  kind: Pod                                                             │
│  metadata:                                                             │
│    name: high-performance-app                                          │
│  spec:                                                                 │
│    containers:                                                         │
│    - name: app                                                         │
│      image: my-spdk-app:latest                                         │
│      resources:                                                        │
│        limits:                                                         │
│          flint.io/nvme: 1  # Request direct NVMe access                │
│          hugepages-2Mi: 1Gi                                            │
│  ```                                                                   │
│                                                                         │
│  Option 2: Manual SPDK Configuration                                   │
│  ─────────────────────────────────────────────────────────────────────  │
│                                                                         │
│  PCI Address: 0000:02:00.0                                             │
│  Device is bound to: vfio-pci                                          │
│                                                                         │
│  Use in your SPDK application:                                         │
│  ```c                                                                  │
│  spdk_env_init(...);                                                   │
│  spdk_nvme_probe("0000:02:00.0", ...);                                 │
│  // Direct I/O at 6+ GB/s!                                             │
│  ```                                                                   │
│                                                                         │
│  [View Example Apps] [Download SPDK Device Plugin] [Close]            │
└─────────────────────────────────────────────────────────────────────────┘
```

## Device Status Indicators

### Visual Design:

```css
/* CSI Managed Device */
.device-card.csi-managed {
  border-left: 4px solid #4caf50;  /* Green */
}
.device-card.csi-managed .status-badge {
  background: #4caf50;
  color: white;
  content: "✅ CSI Managed";
}

/* Reserved Device */
.device-card.reserved {
  border-left: 4px solid #ff9800;  /* Orange */
  background: #fff3e0;  /* Light orange tint */
}
.device-card.reserved .status-badge {
  background: #ff9800;
  color: white;
  content: "🔒 RESERVED";
}

/* Unhealthy Device */
.device-card.unhealthy {
  border-left: 4px solid #f44336;  /* Red */
}
```

## API Integration

### Frontend React Component Example:

```typescript
// DeviceCard.tsx
interface Device {
  name: string;
  pciAddress: string;
  capacity: number;
  model: string;
  healthy: boolean;
  isReserved: boolean;
  reservedReason?: string;
  lvsName?: string;
  freeSpace?: number;
  volumeCount?: number;
}

const DeviceCard: React.FC<{ device: Device }> = ({ device }) => {
  const [showReserveDialog, setShowReserveDialog] = useState(false);

  const handleReserve = async (reason: string) => {
    // Update ConfigMap
    const response = await fetch('/api/config/reserved-devices', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        pciAddress: device.pciAddress,
        reason: reason,
      }),
    });

    if (response.ok) {
      // Trigger reload on all nodes
      await fetch('/api/nodes/reload-reserved-devices', {
        method: 'POST',
      });

      // Refresh device list
      window.location.reload();
    }
  };

  const handleUnreserve = async () => {
    // Remove from ConfigMap
    await fetch(`/api/config/reserved-devices/${device.pciAddress}`, {
      method: 'DELETE',
    });

    // Trigger reload on all nodes
    await fetch('/api/nodes/reload-reserved-devices', {
      method: 'POST',
    });

    window.location.reload();
  };

  return (
    <div className={`device-card ${device.isReserved ? 'reserved' : 'csi-managed'}`}>
      <div className="device-header">
        <h3>{device.name}</h3>
        <span className="status-badge">
          {device.isReserved ? '🔒 RESERVED' : '✅ CSI Managed'}
        </span>
      </div>

      <div className="device-info">
        <p>PCI: {device.pciAddress}</p>
        <p>Capacity: {formatBytes(device.capacity)}</p>
        <p>Model: {device.model}</p>

        {device.isReserved && (
          <div className="reservation-info">
            <p>Reserved for: {device.reservedReason}</p>
            <p className="warning">
              ⚠️ Not available for PVC provisioning
            </p>
          </div>
        )}

        {!device.isReserved && (
          <div className="csi-info">
            <p>LVS: {device.lvsName}</p>
            <p>Free: {formatBytes(device.freeSpace)}</p>
            <p>Volumes: {device.volumeCount} PVCs</p>
          </div>
        )}
      </div>

      <div className="device-actions">
        {device.isReserved ? (
          <button onClick={handleUnreserve}>
            Unreserve Device
          </button>
        ) : (
          <button onClick={() => setShowReserveDialog(true)}>
            Reserve for High Performance
          </button>
        )}
      </div>

      {showReserveDialog && (
        <ReserveDeviceDialog
          device={device}
          onReserve={handleReserve}
          onCancel={() => setShowReserveDialog(false)}
        />
      )}
    </div>
  );
};
```

## Summary

This UI design provides:

1. **Clear Visual Distinction**: Reserved vs CSI-managed devices
2. **Easy Reservation**: One-click reserve with confirmation
3. **User Guidance**: Shows how to use reserved devices
4. **Safety Warnings**: Alerts about existing PVCs
5. **Annotations**: User can document why device is reserved
6. **Dynamic Updates**: ConfigMap changes apply without restart
