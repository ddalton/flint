# ROX (ReadOnlyMany) Architecture - Component Diagram

## Overview: How ROX Volumes Work with NFS

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        Kubernetes Cluster                                │
│                                                                           │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │  User Creates ROX PVC                                            │   │
│  │                                                                  │   │
│  │  apiVersion: v1                                                  │   │
│  │  kind: PersistentVolumeClaim                                     │   │
│  │  metadata:                                                       │   │
│  │    name: test-rox-volume                                         │   │
│  │  spec:                                                           │   │
│  │    accessModes:                                                  │   │
│  │      - ReadOnlyMany  ◄────────────────────────────┐             │   │
│  │    storageClassName: flint                        │             │   │
│  │    resources:                                     │             │   │
│  │      requests:                                    │             │   │
│  │        storage: 1Gi                               │             │   │
│  └───────────────────────────────────────────────────┼─────────────┘   │
│                                                      │                  │
│                          ▼                           │                  │
│  ┌──────────────────────────────────────────────────┼─────────────┐   │
│  │  CSI Controller (CreateVolume)                   │             │   │
│  │                                                   │             │   │
│  │  1. Detects: is_rox = true ◄──────────────────────             │   │
│  │  2. Detects: uses_nfs = true (ROX needs NFS)                   │   │
│  │  3. Creates logical volume on storage node                     │   │
│  │  4. Adds to volume_context:                                    │   │
│  │     - nfs.flint.io/enabled: "true"                             │   │
│  │     - nfs.flint.io/replica-nodes: "cdrv-1.vpc.cloudera.com"    │   │
│  │     - csi.storage.k8s.io/pvc/name: "test-rox-volume"           │   │
│  │                                                                  │   │
│  └──────────────────────────┬───────────────────────────────────────   │
│                             ▼                                           │
│  ┌────────────────────────────────────────────────────────────────┐   │
│  │  PersistentVolume Created                                      │   │
│  │                                                                 │   │
│  │  apiVersion: v1                                                │   │
│  │  kind: PersistentVolume                                        │   │
│  │  metadata:                                                     │   │
│  │    name: pvc-69dc8d11-...                                      │   │
│  │  spec:                                                         │   │
│  │    accessModes:                                                │   │
│  │      - ReadOnlyMany                                            │   │
│  │    claimRef:                                                   │   │
│  │      name: test-rox-volume                                     │   │
│  │    csi:                                                        │   │
│  │      driver: flint.csi.storage.io                              │   │
│  │      volumeHandle: pvc-69dc8d11-...                            │   │
│  │      volumeAttributes:  ◄─────── Stored from CreateVolume      │   │
│  │        flint.csi.storage.io/lvol-uuid: 4733dd49-...            │   │
│  │        flint.csi.storage.io/node-name: cdrv-1...               │   │
│  │        nfs.flint.io/enabled: "true"                            │   │
│  │        nfs.flint.io/replica-nodes: "cdrv-1..."                 │   │
│  │        csi.storage.k8s.io/pvc/name: "test-rox-volume"          │   │
│  │                                                                 │   │
│  └────────────────────────────────────────────────────────────────┘   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

## Component Interaction Flow

```
┌─────────────────────────────────────────────────────────────────────────┐
│  Step 1: User Creates Workload Pod Using ROX PVC                        │
└─────────────────────────────────────────────────────────────────────────┘
                             │
                             ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  Step 2: ControllerPublishVolume Called                                 │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │  CSI Controller                                                   │  │
│  │  • Reads PV.spec.csi.volumeAttributes                             │  │
│  │  • Detects: nfs.flint.io/enabled = "true"                         │  │
│  │  • Detects: accessMode = ReadOnlyMany                             │  │
│  │  • Gets: replica-nodes = "cdrv-1.vpc.cloudera.com"                │  │
│  │  • Gets: pvc/name = "test-rox-volume"                             │  │
│  │                                                                   │  │
│  │  Decision: Create NFS Server Pod with read_only=true             │  │
│  └───────────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────┘
                             │
                             ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  Step 3: NFS Server Pod Created on Storage Node (cdrv-1)                │
│                                                                           │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  Pod: flint-nfs-pvc-69dc8d11-...                                 │   │
│  │  Namespace: flint-system                                         │   │
│  │  Node: cdrv-1.vpc.cloudera.com (has the SPDK lvol)               │   │
│  │                                                                  │   │
│  │  Container:                                                      │   │
│  │    Command: /usr/local/bin/flint-nfs-server                     │   │
│  │    Args:                                                         │   │
│  │      --export-path /mnt/volume                                  │   │
│  │      --volume-id pvc-69dc8d11-...                               │   │
│  │      --port 2049                                                │   │
│  │      --verbose                                                  │   │
│  │      --read-only  ◄────────────────────── ROX MODE!             │   │
│  │                                                                  │   │
│  │  Volume Mounts:                                                 │   │
│  │    - name: volume-data                                          │   │
│  │      mountPath: /mnt/volume                                     │   │
│  │      pvc: test-rox-volume ◄─────── Mounts actual PVC            │   │
│  │                                                                  │   │
│  │  Exposed:                                                       │   │
│  │    - Port 2049/TCP (NFS)                                        │   │
│  │    - Pod IP: 10.244.x.x                                         │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                             │                                            │
│                             │ NFS exports /mnt/volume as read-only       │
│                             ▼                                            │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  Under the hood (on cdrv-1):                                     │   │
│  │                                                                  │   │
│  │  PVC test-rox-volume → PV → CSI Volume                          │   │
│  │                         │                                        │   │
│  │                         ▼                                        │   │
│  │  NodeStageVolume mounts SPDK lvol via ublk                      │   │
│  │  /var/lib/kubelet/plugins/.../globalmount/                      │   │
│  │           │                                                      │   │
│  │           └─ bind mount ─→ /mnt/volume (in NFS pod)             │   │
│  │                                 │                                │   │
│  │                                 ▼                                │   │
│  │  SPDK Logical Volume:                                           │   │
│  │    UUID: 4733dd49-de16-44a3-a723-24caca9b4004                   │   │
│  │    LVS: lvs_cdrv-1.vpc.cloudera.com_0000-01-00-0                │   │
│  │    Size: 1GB                                                    │   │
│  │    Type: ublk block device                                      │   │
│  └─────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────┘
                             │
                             │ NFS clients connect
                             ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  Step 4: Workload Pod(s) Mount via NFS (can be on any node)             │
│                                                                           │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  Pod: test-rox-reader (on cdrv-2)                               │   │
│  │                                                                  │   │
│  │  NodePublishVolume:                                             │   │
│  │    • Detects: volumeType = "nfs"                                │   │
│  │    • Gets: nfs.flint.io/server-ip = 10.244.x.x                  │   │
│  │    • Mounts: mount -t nfs -o vers=4.2,ro \                      │   │
│  │                10.244.x.x:/ /var/lib/kubelet/pods/.../volumes   │   │
│  │                                                                  │   │
│  │  Container sees:                                                │   │
│  │    /data → NFS mount (read-only) ◄──────────────────┐           │   │
│  └─────────────────────────────────────────────────────┼───────────┘   │
│                                                         │                │
│  ┌─────────────────────────────────────────────────────┼───────────┐   │
│  │  Pod: test-rox-reader-2 (on cdrv-1 or cdrv-2)      │           │   │
│  │                                                     │           │   │
│  │  Container sees:                                    │           │   │
│  │    /data → NFS mount (read-only) ◄──────────────────┘           │   │
│  │                                                                  │   │
│  │  Multiple pods can read simultaneously! ✅                       │   │
│  └──────────────────────────────────────────────────────────────────   │
└─────────────────────────────────────────────────────────────────────────┘
```

## Detailed Component Breakdown

### 1. Storage Layer (SPDK)

```
┌────────────────────────────────────────┐
│  Storage Node: cdrv-1.vpc.cloudera.com │
│  ┌──────────────────────────────────┐  │
│  │  SPDK                            │  │
│  │  ┌────────────────────────────┐  │  │
│  │  │  LVStore (LVS)             │  │  │
│  │  │  lvs_cdrv-1..._0000-01-00-0│  │  │
│  │  │                            │  │  │
│  │  │  ┌──────────────────────┐  │  │  │
│  │  │  │ Logical Volume (lvol)│  │  │  │
│  │  │  │                      │  │  │  │
│  │  │  │ UUID: 4733dd49-...   │  │  │  │
│  │  │  │ Size: 1GB            │  │  │  │
│  │  │  │ Format: ext4/xfs     │  │  │  │
│  │  │  └──────────────────────┘  │  │  │
│  │  └────────────────────────────┘  │  │
│  │                ▲                  │  │
│  └────────────────┼──────────────────┘  │
│                   │                     │
│                   │ exposed via         │
│                   ▼                     │
│  ┌────────────────────────────────┐    │
│  │  ublk device                   │    │
│  │  /dev/ublkb0                   │    │
│  │  (block device accessible      │    │
│  │   only on cdrv-1)              │    │
│  └────────────────────────────────┘    │
└────────────────────────────────────────┘
```

### 2. CSI Volume Provisioning Flow

```
┌──────────────────────────────────────────────────────────────────┐
│  CSI Provisioner (detects ROX PVC)                               │
│                                                                   │
│  CreateVolumeRequest {                                           │
│    name: "pvc-69dc8d11-8908-403d-8a86-fd0b3a2b4622"              │
│    volume_capabilities: [                                        │
│      { accessMode: ReadOnlyMany }  ◄───── Triggers ROX           │
│    ]                                                             │
│  }                                                               │
│                                                                   │
│  ▼                                                               │
│  CSI Controller.CreateVolume()                                   │
│    • Detects: is_rox = true                                      │
│    • Creates lvol on cdrv-1                                      │
│    • Returns volume_context with:                                │
│        - nfs.flint.io/enabled: "true"                            │
│        - nfs.flint.io/replica-nodes: "cdrv-1..."                 │
│        - csi.storage.k8s.io/pvc/name: "test-rox-volume"          │
│                                                                   │
│  ▼                                                               │
│  PersistentVolume Created by CSI Provisioner                     │
│    • Status: Bound to PVC                                        │
│    • volumeAttributes: (from volume_context)                     │
└──────────────────────────────────────────────────────────────────┘
```

### 3. NFS Server Pod Creation Flow

```
┌──────────────────────────────────────────────────────────────────┐
│  User Creates Workload Pod Referencing ROX PVC                   │
│                                                                   │
│  Pod {                                                           │
│    volumes: [                                                    │
│      { pvc: "test-rox-volume", readOnly: true }                 │
│    ]                                                             │
│  }                                                               │
└──────────────────────────────────────────────────────────────────┘
                             │
                             ▼
┌──────────────────────────────────────────────────────────────────┐
│  CSI Controller.ControllerPublishVolume()                        │
│                                                                   │
│  1. Reads PV.spec.csi.volumeAttributes                           │
│  2. Detects: nfs.flint.io/enabled = "true"                       │
│  3. Detects: accessMode = ReadOnlyMany                           │
│  4. Calls: create_nfs_server_pod(..., read_only=true)            │
│                                                                   │
│  ▼                                                               │
│  Creates Pod in flint-system namespace:                          │
│                                                                   │
│  apiVersion: v1                                                  │
│  kind: Pod                                                       │
│  metadata:                                                       │
│    name: flint-nfs-pvc-69dc8d11-...                              │
│    namespace: flint-system                                       │
│    labels:                                                       │
│      app: flint-nfs-server                                       │
│      flint.io/volume-id: pvc-69dc8d11-...                        │
│  spec:                                                           │
│    affinity:                                                     │
│      nodeAffinity:  ◄────── Runs on cdrv-1 (has the lvol)        │
│        requiredDuringScheduling:                                 │
│          nodeSelectorTerms:                                      │
│          - matchExpressions:                                     │
│            - key: kubernetes.io/hostname                         │
│              operator: In                                        │
│              values: ["cdrv-1.vpc.cloudera.com"]                 │
│                                                                   │
│    containers:                                                   │
│    - name: nfs-server                                            │
│      image: flint-driver:latest                                  │
│      command: ["/usr/local/bin/flint-nfs-server"]                │
│      args:                                                       │
│        - --export-path=/mnt/volume                               │
│        - --volume-id=pvc-69dc8d11-...                            │
│        - --port=2049                                             │
│        - --verbose                                               │
│        - --read-only  ◄──────────── READ-ONLY MODE FOR ROX!      │
│                                                                   │
│      volumeMounts:                                               │
│      - name: volume-data                                         │
│        mountPath: /mnt/volume                                    │
│                                                                   │
│    volumes:                                                      │
│    - name: volume-data                                           │
│      persistentVolumeClaim:                                      │
│        claimName: test-rox-volume  ◄──── Mounts user's PVC       │
│                                          (which uses ublk/RWO)   │
└──────────────────────────────────────────────────────────────────┘
```

### 4. NFS Server Pod Internal Mount Chain

```
┌────────────────────────────────────────────────────────────────────┐
│  NFS Server Pod (on cdrv-1)                                        │
│                                                                     │
│  PVC "test-rox-volume" ──┐                                         │
│                          ▼                                         │
│  ┌─────────────────────────────────────────────────────┐          │
│  │ CSI NodeStageVolume (RWO mode on NFS pod)           │          │
│  │                                                     │          │
│  │ 1. Detects: volumeType = "local" (on cdrv-1)       │          │
│  │ 2. Creates: /dev/ublkb0 ← SPDK lvol                │          │
│  │ 3. Formats: ext4 filesystem (if needed)            │          │
│  │ 4. Mounts: /var/lib/kubelet/plugins/flint/         │          │
│  │            mounts/pvc-69dc8d11.../globalmount      │          │
│  │                                                     │          │
│  └─────────────────────────────────────────────────────┘          │
│                          │                                         │
│                          ▼                                         │
│  ┌─────────────────────────────────────────────────────┐          │
│  │ CSI NodePublishVolume                               │          │
│  │                                                     │          │
│  │ Bind mount:                                         │          │
│  │   From: /var/lib/kubelet/plugins/flint/.../globalmount         │
│  │   To: /var/lib/kubelet/pods/<nfs-pod-id>/volumes/              │
│  │       kubernetes.io~csi/volume-data/mount           │          │
│  │                                                     │          │
│  │ Then bind mounted into container as: /mnt/volume   │          │
│  └─────────────────────────────────────────────────────┘          │
│                          │                                         │
│                          ▼                                         │
│  ┌─────────────────────────────────────────────────────┐          │
│  │ Flint NFS Server Process                            │          │
│  │                                                     │          │
│  │ • Reads from: /mnt/volume (ext4 on ublk)           │          │
│  │ • Exports via: NFSv4.2 on 0.0.0.0:2049              │          │
│  │ • Mode: READ-ONLY (--read-only flag)               │          │
│  │ • Serves: Pseudo-root "/" → /mnt/volume             │          │
│  └─────────────────────────────────────────────────────┘          │
│                          │                                         │
│                          │ Listens on pod IP:2049                  │
└──────────────────────────┼─────────────────────────────────────────┘
                           │
                           │ NFSv4.2 Protocol
                           ▼
```

### 5. Workload Pods Mount NFS

```
┌────────────────────────────────────────────────────────────────────┐
│  Workload Pod: test-rox-reader (on cdrv-2 or any node)             │
│                                                                     │
│  ┌─────────────────────────────────────────────────────┐          │
│  │ CSI NodeStageVolume                                 │          │
│  │                                                     │          │
│  │ 1. Reads publish_context:                          │          │
│  │    - volumeType: "nfs"                              │          │
│  │    - nfs.flint.io/server-ip: "10.244.x.x"           │          │
│  │    - nfs.flint.io/port: "2049"                      │          │
│  │                                                     │          │
│  │ 2. Mounts NFS:                                      │          │
│  │    mount -t nfs -o vers=4.2,ro \                    │          │
│  │      10.244.x.x:/ \                                 │          │
│  │      /var/lib/kubelet/plugins/flint/.../globalmount │          │
│  │                                 ▲                   │          │
│  │                                 │                   │          │
│  │                            ro = read-only!          │          │
│  └─────────────────────────────────────────────────────┘          │
│                          │                                         │
│                          ▼                                         │
│  ┌─────────────────────────────────────────────────────┐          │
│  │ Container: busybox                                  │          │
│  │                                                     │          │
│  │   /data → bind mount from NFS                       │          │
│  │           (read-only enforced)                      │          │
│  │                                                     │          │
│  │   $ ls /data       ✅ Works                         │          │
│  │   $ cat /data/file ✅ Works                         │          │
│  │   $ touch /data/x  ❌ Read-only file system         │          │
│  └─────────────────────────────────────────────────────┘          │
└────────────────────────────────────────────────────────────────────┘

Same NFS mount works for multiple pods simultaneously!
All readers see consistent data from the same SPDK lvol.
```

## Complete Data Path

```
┌───────────────────────────────────────────────────────────────────────┐
│                         READ OPERATION PATH                            │
└───────────────────────────────────────────────────────────────────────┘

Workload Pod Container                  NFS Server Pod              SPDK
   (cdrv-2)                              (cdrv-1)                  (cdrv-1)
      │                                      │                         │
      │ read(/data/file)                     │                         │
      ├──────────────────────────────────────►                         │
      │                                      │                         │
      │              NFSv4.2 READ RPC        │                         │
      │        (over pod network)            │                         │
      │                                      │                         │
      │                            read(/mnt/volume/file)              │
      │                                      ├─────────────────────────►
      │                                      │                         │
      │                                      │      ublk read from     │
      │                                      │      SPDK lvol          │
      │                                      │                         │
      │                                      │◄─────────────────────────
      │                                      │         data            │
      │◄──────────────────────────────────────                         │
      │              NFSv4.2 REPLY           │                         │
      │                data                  │                         │
      │                                      │                         │
      ▼                                      ▼                         ▼

  Application                         NFS Server                   Storage
  reads data                          serves data                  provides data
```

## Component Responsibilities

### PersistentVolumeClaim (PVC)
```
Name: test-rox-volume
Role: User's storage request
Features:
  • AccessMode: ReadOnlyMany
  • Size: 1Gi
  • Used by: Multiple workload pods
```

### PersistentVolume (PV)
```
Name: pvc-69dc8d11-8908-403d-8a86-fd0b3a2b4622
Role: Kubernetes volume object
Features:
  • Binds to: test-rox-volume PVC
  • Driver: flint.csi.storage.io
  • Contains: volumeAttributes (metadata)
  • AccessMode: ReadOnlyMany
```

### SPDK Logical Volume (lvol)
```
UUID: 4733dd49-de16-44a3-a723-24caca9b4004
Location: cdrv-1.vpc.cloudera.com
Role: Actual storage backend
Features:
  • 1GB storage capacity
  • Formatted with filesystem (ext4/xfs)
  • Exposed via ublk device
  • Single writer (NFS pod)
```

### NFS Server Pod
```
Name: flint-nfs-pvc-69dc8d11-...
Namespace: flint-system
Node: cdrv-1.vpc.cloudera.com
Role: NFS export gateway
Features:
  • Mounts PVC (gets ublk device)
  • Runs flint-nfs-server with --read-only
  • Exports /mnt/volume via NFSv4.2
  • Listens on port 2049
  • Pod IP becomes NFS server IP
```

### Workload Pods (Readers)
```
Names: test-rox-reader, test-rox-reader-2, ...
Locations: Any node (cdrv-1, cdrv-2, etc.)
Role: Application consumers
Features:
  • Mount NFS from server pod IP
  • Read-only access enforced
  • Can run multiple instances
  • See consistent data
```

## Key Design Points

### Why This Architecture?

1. **SPDK Exclusive Access**
   - ✅ Only NFS pod touches ublk device (no conflicts)
   - ✅ No UUID alias collisions
   - ✅ Clean ownership model

2. **Multi-Pod Read Access**
   - ✅ NFS naturally supports multiple clients
   - ✅ Pods can be on any node
   - ✅ Consistent read-only view

3. **Read-Only Enforcement**
   - ✅ NFS server runs with --read-only flag
   - ✅ Mount uses ro option
   - ✅ Write attempts fail at filesystem level

4. **Scheduling Flexibility**
   - ✅ NFS pod: Runs on storage node (node affinity)
   - ✅ Workload pods: Can run anywhere
   - ✅ No pod anti-affinity needed

## Network Path

```
┌──────────────────────────────────────────────────────────────────┐
│  Pod Network (CNI - e.g., Calico, Flannel)                       │
│                                                                   │
│  ┌─────────────┐                            ┌─────────────┐      │
│  │ Workload Pod│──── NFS Protocol ──────────►│  NFS Pod    │      │
│  │ 10.244.1.x  │    (port 2049)              │ 10.244.0.y  │      │
│  │             │◄─── NFS Replies ────────────│             │      │
│  │ (cdrv-2)    │                             │ (cdrv-1)    │      │
│  └─────────────┘                            └─────────────┘      │
│       │                                            │              │
│       │                                            │              │
│  mount -t nfs                              /mnt/volume            │
│  10.244.0.y:/                              │                      │
│  /data (ro)                                │                      │
│                                            ▼                      │
│                                      ┌──────────┐                 │
│                                      │ /dev/ublkb0               │
│                                      │   (SPDK)                  │
│                                      └──────────┘                 │
└──────────────────────────────────────────────────────────────────┘
```

## Summary: Does the PV act as an NFS client?

**No, the PV is just metadata. Here's what actually happens:**

1. **PV (PersistentVolume)**
   - Metadata object in Kubernetes
   - Contains: volumeAttributes with NFS server info
   - Does NOT mount anything itself

2. **Workload Pod** = NFS Client
   - NodePublishVolume reads PV.volumeAttributes
   - Runs: `mount -t nfs 10.244.x.x:/ /data`
   - Pod becomes the NFS client

3. **NFS Server Pod** = NFS Server
   - Mounts the actual PVC (gets ublk device)
   - Runs flint-nfs-server with --read-only
   - Serves data over NFSv4.2

**Flow:**
```
PVC → PV (metadata) → Workload Pod (NFS client) 
                           ↓ NFSv4.2
                      NFS Pod (NFS server)
                           ↓ ublk
                      SPDK lvol (storage)
```

The **workload pod is the NFS client**, not the PV! The PV just stores the NFS server's address in its volumeAttributes.








