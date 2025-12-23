# Pure Rust Storage Stack with Kernel nvmet

**Architecture: Rust for everything, kernel nvmet for NVMe-oF**

**Version**: 1.0
**Date**: 2025-12-22
**Status**: Recommended Design

---

## Executive Summary

This design uses:
- ✅ **Pure Rust** for all storage management code
- ✅ **Kernel nvmet** for NVMe-oF target (export volumes)
- ✅ **Kernel NVMe-oF initiator** for remote access
- ✅ **Smart device selection**: ublk for local, nvme for remote

### Key Insight

**The volume location determines the block device type:**
- 🏠 **Local volume** → `/dev/ublkb0` (Rust-managed ublk device)
- 🌐 **Remote volume** → `/dev/nvme0n1` (kernel NVMe-oF initiator)

This eliminates ALL userspace NVMe-oF code while providing optimal performance for each case!

---

## Table of Contents

1. [Architecture Overview](#architecture-overview)
2. [Local vs Remote Volume Flows](#local-vs-remote-volume-flows)
3. [Component Design](#component-design)
4. [Data Paths](#data-paths)
5. [API Design](#api-design)
6. [Implementation Plan](#implementation-plan)
7. [Performance Analysis](#performance-analysis)
8. [Advantages](#advantages)

---

## Architecture Overview

### High-Level System Architecture

```
┌─────────────────────────────────────────────────────────────┐
│              Kubernetes Pod (Application)                   │
│                                                             │
│  Local Mount:  /dev/ublkb0 ────────┐                       │
│  Remote Mount: /dev/nvme0n1 ───────┼───────┐               │
└────────────────────────────────────┼───────┼───────────────┘
                                     │       │
                   ┌─────────────────┘       └────────────┐
                   │ (local access)        (remote access)│
                   ▼                                       ▼
┌──────────────────────────────────┐    ┌──────────────────────────────┐
│     Local Storage Node           │    │    Remote Storage Node       │
│                                  │    │                              │
│  ┌────────────────────────────┐  │    │  ┌────────────────────────┐ │
│  │  Rust Storage Daemon       │  │    │  │  Rust Storage Daemon   │ │
│  │                            │  │    │  │                        │ │
│  │  ┌──────────────────────┐  │  │    │  │  ┌──────────────────┐ │ │
│  │  │  Volume Manager      │  │  │    │  │  │  Volume Manager  │ │ │
│  │  │  (LVM thin volumes)  │  │  │    │  │  │  (LVM)          │ │ │
│  │  └──────────┬───────────┘  │  │    │  │  └────────┬─────────┘ │ │
│  │             │               │  │    │  │           │           │ │
│  │  ┌──────────▼───────────┐  │  │    │  │  ┌────────▼─────────┐ │ │
│  │  │  ublk Manager        │  │  │    │  │  │  nvmet Manager   │ │ │
│  │  │  (exposes locally)   │  │  │    │  │  │  (exports over   │ │ │
│  │  └──────────┬───────────┘  │  │    │  │  │   network)       │ │ │
│  │             │               │  │    │  │  └────────┬─────────┘ │ │
│  └─────────────┼───────────────┘  │    │  └───────────┼───────────┘ │
│                │                  │    │              │             │
│       ┌────────▼────────┐         │    │    ┌─────────▼──────────┐  │
│       │  /dev/ublkb0    │         │    │    │  configfs (nvmet)  │  │
│       └─────────────────┘         │    │    └─────────┬──────────┘  │
│                                   │    │              │             │
└───────────────────────────────────┘    └──────────────┼─────────────┘
                                                        │
                                         Network (RDMA/TCP)
                                                        │
┌───────────────────────────────────────────────────────┼─────────────┐
│     Local Node (NVMe-oF Initiator)                    │             │
│                                                       │             │
│  ┌────────────────────────────────────────────────────▼──────────┐  │
│  │  Kernel NVMe-oF Initiator (nvme-fabrics)                      │  │
│  │  • Connects to remote nvmet target                            │  │
│  │  • Creates /dev/nvme0n1                                       │  │
│  └────────────────────────────────────────────────────┬──────────┘  │
│                                                        │             │
│                                           ┌────────────▼──────────┐  │
│                                           │   /dev/nvme0n1        │  │
│                                           │   (remote volume)     │  │
│                                           └───────────────────────┘  │
└─────────────────────────────────────────────────────────────────────┘
```

### Component Responsibilities

| Component | Responsibility | Technology |
|-----------|---------------|------------|
| **Rust Storage Daemon** | Volume lifecycle, ublk creation, nvmet config | Rust |
| **Volume Manager** | LVM thin volumes, snapshots, clones | Rust + devicemapper |
| **ublk Manager** | Expose local volumes as /dev/ublkbN | Rust + rublk |
| **nvmet Manager** | Configure kernel nvmet for export | Rust + configfs/nvmetcfg |
| **Kernel nvmet** | NVMe-oF target (export volumes) | Linux kernel |
| **Kernel nvme-fabrics** | NVMe-oF initiator (connect to remote) | Linux kernel |

---

## Local vs Remote Volume Flows

### Scenario 1: Local Volume Access (Same Node)

```
┌───────────────────────────────────────────────────────────────┐
│  CSI Driver: "Create and mount volume on node-1"              │
└───────────────────────────────────┬───────────────────────────┘
                                    ▼
┌─────────────────────────────────────────────────────────────┐
│  Rust RPC: volume_create("vol1", 10GB, local=true)         │
└───────────────────────────────┬─────────────────────────────┘
                                ▼
┌─────────────────────────────────────────────────────────────┐
│  Volume Manager: Create LVM thin volume                     │
│  → /dev/mapper/thin-pool/vol1                               │
└───────────────────────────────┬─────────────────────────────┘
                                ▼
┌─────────────────────────────────────────────────────────────┐
│  ublk Manager: Create ublk device                           │
│  → /dev/ublkb0 backed by /dev/mapper/thin-pool/vol1        │
└───────────────────────────────┬─────────────────────────────┘
                                ▼
┌─────────────────────────────────────────────────────────────┐
│  Return to CSI: device_path="/dev/ublkb0"                   │
└───────────────────────────────┬─────────────────────────────┘
                                ▼
┌─────────────────────────────────────────────────────────────┐
│  CSI mounts /dev/ublkb0 into pod                            │
│  (standard block device mount)                               │
└─────────────────────────────────────────────────────────────┘
```

**I/O Path:**
```
Application in Pod
  │ read/write syscall
  ▼
Filesystem (ext4/xfs)
  │
  ▼
Linux Block Layer
  │
  ▼
ublk Driver (kernel)
  │ io_uring
  ▼
Rust ublk Handler (userspace)
  │
  ▼
LVM Device Mapper
  │
  ▼
NVMe Device

Latency: ~15-30μs
```

---

### Scenario 2: Remote Volume Access (Different Node)

```
┌────────────────────────────────────────────────────────────┐
│  CSI Driver on node-2: "Mount volume from node-1"         │
└────────────────────────────────┬───────────────────────────┘
                                 ▼
┌─────────────────────────────────────────────────────────────┐
│  Step 1 (on node-1 - storage node):                        │
│  Rust RPC: volume_export("vol1", transport="rdma")         │
└─────────────────────────────────┬───────────────────────────┘
                                  ▼
┌─────────────────────────────────────────────────────────────┐
│  Volume Manager: Ensure volume exists                       │
│  → /dev/mapper/thin-pool/vol1                               │
└─────────────────────────────────┬───────────────────────────┘
                                  ▼
┌─────────────────────────────────────────────────────────────┐
│  ublk Manager: Create ublk device for nvmet                 │
│  → /dev/ublkb5                                              │
└─────────────────────────────────┬───────────────────────────┘
                                  ▼
┌─────────────────────────────────────────────────────────────┐
│  nvmet Manager: Configure kernel nvmet                      │
│                                                             │
│  # Create subsystem                                         │
│  echo "vol1" > /sys/kernel/config/nvmet/subsystems/        │
│                nqn.2025.flint:vol1/attr_allow_any_host     │
│                                                             │
│  # Add namespace                                            │
│  echo "/dev/ublkb5" > /sys/kernel/config/nvmet/subsystems/  │
│                       nqn.2025.flint:vol1/namespaces/1/     │
│                       device_path                           │
│  echo 1 > .../namespaces/1/enable                          │
│                                                             │
│  # Create port and listener                                │
│  echo "rdma" > /sys/kernel/config/nvmet/ports/1/addr_trtype│
│  echo "192.168.1.10" > .../ports/1/addr_traddr             │
│  echo "4420" > .../ports/1/addr_trsvcid                    │
│                                                             │
│  # Link subsystem to port                                  │
│  ln -s /sys/kernel/config/nvmet/subsystems/                │
│        nqn.2025.flint:vol1 .../ports/1/subsystems/         │
└─────────────────────────────────┬───────────────────────────┘
                                  ▼
┌─────────────────────────────────────────────────────────────┐
│  Return to CSI: nqn="nqn.2025.flint:vol1",                 │
│                 target="192.168.1.10:4420",                 │
│                 transport="rdma"                            │
└─────────────────────────────────┬───────────────────────────┘
                                  ▼
┌─────────────────────────────────────────────────────────────┐
│  Step 2 (on node-2 - compute node):                        │
│  CSI calls: volume_attach_remote(nqn, target, transport)   │
└─────────────────────────────────┬───────────────────────────┘
                                  ▼
┌─────────────────────────────────────────────────────────────┐
│  Rust daemon calls kernel nvme-cli:                         │
│                                                             │
│  nvme connect -t rdma -n nqn.2025.flint:vol1 \             │
│                -a 192.168.1.10 -s 4420                      │
└─────────────────────────────────┬───────────────────────────┘
                                  ▼
┌─────────────────────────────────────────────────────────────┐
│  Kernel NVMe-oF initiator:                                  │
│  • Connects via RDMA to node-1                              │
│  • Discovers namespace                                      │
│  • Creates /dev/nvme0n1                                     │
└─────────────────────────────────┬───────────────────────────┘
                                  ▼
┌─────────────────────────────────────────────────────────────┐
│  Return to CSI: device_path="/dev/nvme0n1"                  │
└─────────────────────────────────┬───────────────────────────┘
                                  ▼
┌─────────────────────────────────────────────────────────────┐
│  CSI mounts /dev/nvme0n1 into pod                           │
│  (standard block device mount)                               │
└─────────────────────────────────────────────────────────────┘
```

**I/O Path:**
```
Application on node-2
  │ read/write syscall
  ▼
Filesystem
  │
  ▼
Linux Block Layer
  │
  ▼
NVMe-oF Initiator (kernel)
  │ RDMA/TCP
  ▼
Network
  │
  ▼
NVMe-oF Target (kernel nvmet on node-1)
  │
  ▼
/dev/ublkb5
  │ io_uring
  ▼
Rust ublk Handler
  │
  ▼
LVM Device Mapper
  │
  ▼
NVMe Device on node-1

Latency: Network RTT (5-50μs) + storage (15-30μs) = ~20-80μs
```

---

## Component Design

### 1. Rust Storage Daemon

**Main Components:**

```rust
pub struct StorageDaemon {
    volume_manager: VolumeManager,
    ublk_manager: UblkManager,
    nvmet_manager: NvmetManager,
    initiator_manager: InitiatorManager,
    rpc_server: RpcServer,
}

impl StorageDaemon {
    pub async fn run(&mut self) -> Result<()> {
        // Start RPC server
        self.rpc_server.listen("/var/run/flint/storage.sock").await?;

        // Start ublk polling threads
        self.ublk_manager.start_pollers().await?;

        // Reconcile existing state
        self.reconcile_volumes().await?;
        self.reconcile_exports().await?;
        self.reconcile_connections().await?;

        // Main event loop
        loop {
            tokio::select! {
                rpc_req = self.rpc_server.recv() => {
                    self.handle_rpc(rpc_req?).await?;
                }
                _ = tokio::signal::ctrl_c() => {
                    break;
                }
            }
        }

        self.shutdown().await
    }
}
```

---

### 2. Volume Manager (LVM Integration)

```rust
use devicemapper::{DeviceMapper, ThinPoolDev, ThinDev};

pub struct VolumeManager {
    dm: DeviceMapper,
    pool_name: String,
    volumes: HashMap<String, VolumeInfo>,
}

#[derive(Clone)]
pub struct VolumeInfo {
    pub id: String,
    pub name: String,
    pub size_bytes: u64,
    pub dm_device: String,  // e.g., "/dev/mapper/thin-pool/vol1"
    pub state: VolumeState,
}

#[derive(Clone, PartialEq)]
pub enum VolumeState {
    Creating,
    Ready,
    Deleting,
    Error(String),
}

impl VolumeManager {
    pub async fn create_volume(
        &mut self,
        name: &str,
        size_bytes: u64,
    ) -> Result<VolumeInfo> {
        // Create thin volume in LVM pool
        let thin_dev = self.dm.create_thin_volume(
            &self.pool_name,
            name,
            size_bytes,
        )?;

        let info = VolumeInfo {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            size_bytes,
            dm_device: thin_dev.device_path(),
            state: VolumeState::Ready,
        };

        self.volumes.insert(info.id.clone(), info.clone());
        Ok(info)
    }

    pub async fn delete_volume(&mut self, volume_id: &str) -> Result<()> {
        let volume = self.volumes.get(volume_id)
            .ok_or_else(|| anyhow!("Volume not found"))?;

        self.dm.remove_thin_volume(&volume.name)?;
        self.volumes.remove(volume_id);
        Ok(())
    }

    pub async fn create_snapshot(
        &mut self,
        source_id: &str,
        snapshot_name: &str,
    ) -> Result<VolumeInfo> {
        let source = self.volumes.get(source_id)
            .ok_or_else(|| anyhow!("Source volume not found"))?;

        // Create COW snapshot
        let snapshot = self.dm.create_thin_snapshot(
            &self.pool_name,
            &source.name,
            snapshot_name,
        )?;

        let info = VolumeInfo {
            id: Uuid::new_v4().to_string(),
            name: snapshot_name.to_string(),
            size_bytes: source.size_bytes,
            dm_device: snapshot.device_path(),
            state: VolumeState::Ready,
        };

        self.volumes.insert(info.id.clone(), info.clone());
        Ok(info)
    }
}
```

---

### 3. ublk Manager (Local Block Device Exposure)

```rust
use libublk::{UblkDev, UblkCtrl, io::UblkQueue};

pub struct UblkManager {
    devices: HashMap<u32, UblkDeviceInfo>,
    next_device_id: u32,
}

#[derive(Clone)]
pub struct UblkDeviceInfo {
    pub device_id: u32,
    pub device_path: String,  // "/dev/ublkb0"
    pub backend_path: String,  // "/dev/mapper/thin-pool/vol1"
    pub volume_id: String,
}

impl UblkManager {
    pub async fn create_device(
        &mut self,
        volume_id: &str,
        backend_path: &str,
    ) -> Result<UblkDeviceInfo> {
        let device_id = self.allocate_device_id();

        // Create ublk device using rublk/libublk
        let dev = UblkDev::new(device_id)?;

        // Set parameters
        let backend_size = get_device_size(backend_path)?;
        dev.set_params(backend_size, 4096)?;  // 4K block size

        // Start device
        dev.start().await?;

        let info = UblkDeviceInfo {
            device_id,
            device_path: format!("/dev/ublkb{}", device_id),
            backend_path: backend_path.to_string(),
            volume_id: volume_id.to_string(),
        };

        self.devices.insert(device_id, info.clone());
        Ok(info)
    }

    pub async fn delete_device(&mut self, device_id: u32) -> Result<()> {
        let dev = UblkCtrl::new(device_id)?;
        dev.stop().await?;
        dev.delete()?;

        self.devices.remove(&device_id);
        Ok(())
    }
}
```

---

### 4. nvmet Manager (Kernel NVMe-oF Target Configuration)

```rust
use std::path::Path;
use std::fs;

pub struct NvmetManager {
    exports: HashMap<String, ExportInfo>,
    configfs_root: PathBuf,  // "/sys/kernel/config/nvmet"
}

#[derive(Clone)]
pub struct ExportInfo {
    pub volume_id: String,
    pub nqn: String,
    pub namespace_id: u32,
    pub device_path: String,  // "/dev/ublkb5"
    pub port: u16,
    pub transport: TransportType,
}

#[derive(Clone)]
pub enum TransportType {
    Rdma,
    Tcp,
}

impl NvmetManager {
    pub async fn export_volume(
        &mut self,
        volume_id: &str,
        device_path: &str,
        transport: TransportType,
        listen_addr: &str,
        port: u16,
    ) -> Result<ExportInfo> {
        let nqn = format!("nqn.2025-01.com.flint:{}", volume_id);

        // 1. Create subsystem
        self.create_subsystem(&nqn)?;

        // 2. Add namespace
        self.add_namespace(&nqn, 1, device_path)?;

        // 3. Create/configure port
        let port_id = self.allocate_port();
        self.configure_port(port_id, &transport, listen_addr, port)?;

        // 4. Link subsystem to port
        self.link_subsystem(&nqn, port_id)?;

        let info = ExportInfo {
            volume_id: volume_id.to_string(),
            nqn,
            namespace_id: 1,
            device_path: device_path.to_string(),
            port,
            transport,
        };

        self.exports.insert(volume_id.to_string(), info.clone());
        Ok(info)
    }

    fn create_subsystem(&self, nqn: &str) -> Result<()> {
        let path = self.configfs_root
            .join("subsystems")
            .join(nqn);

        fs::create_dir_all(&path)?;

        // Allow any host
        fs::write(path.join("attr_allow_any_host"), "1")?;

        Ok(())
    }

    fn add_namespace(&self, nqn: &str, nsid: u32, device_path: &str) -> Result<()> {
        let ns_path = self.configfs_root
            .join("subsystems")
            .join(nqn)
            .join("namespaces")
            .join(nsid.to_string());

        fs::create_dir_all(&ns_path)?;
        fs::write(ns_path.join("device_path"), device_path)?;
        fs::write(ns_path.join("enable"), "1")?;

        Ok(())
    }

    fn configure_port(
        &self,
        port_id: u16,
        transport: &TransportType,
        addr: &str,
        port: u16,
    ) -> Result<()> {
        let port_path = self.configfs_root
            .join("ports")
            .join(port_id.to_string());

        fs::create_dir_all(&port_path)?;

        let trtype = match transport {
            TransportType::Rdma => "rdma",
            TransportType::Tcp => "tcp",
        };

        fs::write(port_path.join("addr_trtype"), trtype)?;
        fs::write(port_path.join("addr_adrfam"), "ipv4")?;
        fs::write(port_path.join("addr_traddr"), addr)?;
        fs::write(port_path.join("addr_trsvcid"), port.to_string())?;

        Ok(())
    }

    fn link_subsystem(&self, nqn: &str, port_id: u16) -> Result<()> {
        let subsys_path = self.configfs_root
            .join("subsystems")
            .join(nqn);

        let link_path = self.configfs_root
            .join("ports")
            .join(port_id.to_string())
            .join("subsystems")
            .join(nqn);

        std::os::unix::fs::symlink(subsys_path, link_path)?;
        Ok(())
    }
}
```

---

### 5. Initiator Manager (NVMe-oF Remote Connections)

```rust
use std::process::Command;

pub struct InitiatorManager {
    connections: HashMap<String, ConnectionInfo>,
}

#[derive(Clone)]
pub struct ConnectionInfo {
    pub nqn: String,
    pub target_addr: String,
    pub target_port: u16,
    pub transport: TransportType,
    pub device_path: String,  // "/dev/nvme0n1"
}

impl InitiatorManager {
    pub async fn connect(
        &mut self,
        nqn: &str,
        target_addr: &str,
        target_port: u16,
        transport: TransportType,
    ) -> Result<ConnectionInfo> {
        let trtype = match transport {
            TransportType::Rdma => "rdma",
            TransportType::Tcp => "tcp",
        };

        // Use nvme-cli to connect
        let output = Command::new("nvme")
            .args(&[
                "connect",
                "-t", trtype,
                "-n", nqn,
                "-a", target_addr,
                "-s", &target_port.to_string(),
            ])
            .output()?;

        if !output.status.success() {
            anyhow::bail!("nvme connect failed: {}",
                String::from_utf8_lossy(&output.stderr));
        }

        // Wait for device to appear
        let device_path = self.wait_for_device(nqn).await?;

        let info = ConnectionInfo {
            nqn: nqn.to_string(),
            target_addr: target_addr.to_string(),
            target_port,
            transport,
            device_path,
        };

        self.connections.insert(nqn.to_string(), info.clone());
        Ok(info)
    }

    async fn wait_for_device(&self, nqn: &str) -> Result<String> {
        // Poll /sys/class/nvme/ for new device
        for _ in 0..50 {  // 5 second timeout
            if let Some(path) = self.find_device_by_nqn(nqn)? {
                return Ok(path);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("Timeout waiting for NVMe device")
    }

    fn find_device_by_nqn(&self, nqn: &str) -> Result<Option<String>> {
        // Scan /sys/class/nvme/nvme*/subsysnqn
        for entry in fs::read_dir("/sys/class/nvme")? {
            let entry = entry?;
            let subsysnqn_path = entry.path().join("subsysnqn");

            if let Ok(device_nqn) = fs::read_to_string(&subsysnqn_path) {
                if device_nqn.trim() == nqn {
                    // Found it, get namespace device
                    let nvme_name = entry.file_name();
                    return Ok(Some(format!("/dev/{}n1",
                        nvme_name.to_string_lossy())));
                }
            }
        }
        Ok(None)
    }

    pub async fn disconnect(&mut self, nqn: &str) -> Result<()> {
        Command::new("nvme")
            .args(&["disconnect", "-n", nqn])
            .status()?;

        self.connections.remove(nqn);
        Ok(())
    }
}
```

---

## Data Paths

### Local I/O Path (Optimized)

```
Application
    ↓ (syscall)
Kernel VFS
    ↓
Filesystem (ext4/xfs)
    ↓
Block Layer
    ↓
ublk Driver ← ← ← ← ← (kernel)
    ↓ (io_uring)
    ↑ (userspace)
Rust ublk Handler
    ↓
Device Mapper (dm-thin)
    ↓
NVMe Block Driver
    ↓
NVMe Device (DMA)

Total: ~15-30μs
```

### Remote I/O Path (Network)

```
Application (node-2)
    ↓
Kernel VFS
    ↓
Filesystem
    ↓
Block Layer
    ↓
NVMe-oF Initiator Driver
    ↓
RDMA/TCP Stack
    ↓
Network NIC (RDMA)
    ↓
──── Network ────
    ↓
NIC on node-1 (RDMA)
    ↓
nvmet Target (kernel)
    ↓
ublk Driver
    ↓ (io_uring)
Rust ublk Handler
    ↓
Device Mapper
    ↓
NVMe Device

Total: Network RTT + 20-40μs = ~25-90μs
(RDMA: ~25-50μs, TCP: ~40-90μs)
```

---

## API Design

### Public RPC API

```rust
// Volume Lifecycle
rpc volume_create(name: String, size_bytes: u64) -> VolumeInfo
rpc volume_delete(volume_id: String) -> ()
rpc volume_resize(volume_id: String, new_size: u64) -> ()
rpc volume_snapshot(volume_id: String, snapshot_name: String) -> VolumeInfo
rpc volume_clone(source_id: String, target_name: String) -> VolumeInfo
rpc volume_list() -> Vec<VolumeInfo>

// Local Attachment (same node)
rpc volume_attach_local(volume_id: String) -> LocalAttachment {
    device_path: String,  // "/dev/ublkb0"
}
rpc volume_detach_local(volume_id: String) -> ()

// Remote Export (for access from other nodes)
rpc volume_export(
    volume_id: String,
    transport: TransportType,  // rdma or tcp
) -> ExportInfo {
    nqn: String,
    target_addr: String,
    target_port: u16,
}
rpc volume_unexport(volume_id: String) -> ()

// Remote Attachment (from other node)
rpc volume_attach_remote(
    nqn: String,
    target_addr: String,
    target_port: u16,
    transport: TransportType,
) -> RemoteAttachment {
    device_path: String,  // "/dev/nvme0n1"
}
rpc volume_detach_remote(nqn: String) -> ()

// System
rpc health_check() -> HealthStatus
rpc get_metrics() -> Metrics
```

---

## Implementation Plan

### Phase 1: Core Foundation (Weeks 1-3)

**Week 1**: Project Setup
- [ ] Cargo workspace
- [ ] CI/CD (GitHub Actions)
- [ ] Logging and tracing
- [ ] Basic RPC server

**Week 2**: Volume Manager
- [ ] Integrate devicemapper crate
- [ ] LVM thin volume create/delete
- [ ] Snapshot support
- [ ] Unit tests

**Week 3**: ublk Integration
- [ ] Integrate rublk/libublk
- [ ] Create ublk devices from LVM
- [ ] Handle I/O requests
- [ ] Integration tests with kernel

**Deliverables**: Local volumes working with ublk devices

---

### Phase 2: nvmet Integration (Weeks 4-5)

**Week 4**: nvmet Manager
- [ ] configfs abstraction for nvmet
- [ ] Subsystem creation
- [ ] Namespace management
- [ ] Port configuration
- [ ] RDMA and TCP support

**Week 5**: Export Workflow
- [ ] End-to-end export testing
- [ ] Error handling
- [ ] State management
- [ ] Documentation

**Deliverables**: Volumes exported via kernel nvmet

---

### Phase 3: Initiator Integration (Weeks 6-7)

**Week 6**: Initiator Manager
- [ ] nvme-cli integration
- [ ] Connection management
- [ ] Device discovery
- [ ] Auto-reconnect on failure

**Week 7**: Remote Access
- [ ] End-to-end remote access testing
- [ ] Performance benchmarking
- [ ] Multi-path support (optional)
- [ ] Documentation

**Deliverables**: Full remote volume access working

---

### Phase 4: CSI Integration (Weeks 8-10)

**Week 8**: CSI Driver Updates
- [ ] Implement volume locality detection
- [ ] Local vs remote attachment logic
- [ ] Error handling and rollback

**Week 9**: Testing
- [ ] Kubernetes integration tests
- [ ] Volume lifecycle tests
- [ ] Failover scenarios
- [ ] Performance validation

**Week 10**: Production Readiness
- [ ] Metrics and monitoring
- [ ] Operational docs
- [ ] Troubleshooting guide
- [ ] Deployment automation

**Deliverables**: Production-ready system

---

### Total Timeline: 10 weeks (~2.5 months)

**Compare to other options:**
- Pure Rust with custom NVMe-oF: 24 weeks
- Hybrid SPDK: 12 weeks
- **This design: 10 weeks** ✅

---

## Performance Analysis

### Local Access

| Component | Latency |
|-----------|---------|
| ublk kernel driver | ~2-3μs |
| io_uring overhead | ~1-2μs |
| Rust handler | ~1-2μs |
| Device mapper | ~2-3μs |
| NVMe device | ~10-20μs |
| **Total** | **~16-30μs** |

**Compare to SPDK direct**: ~10-20μs
**Overhead**: ~6-10μs (acceptable for most workloads)

---

### Remote Access (RDMA)

| Component | Latency |
|-----------|---------|
| NVMe-oF initiator (kernel) | ~3-5μs |
| RDMA send | ~2-5μs |
| Network RTT | ~5-20μs |
| RDMA receive | ~2-5μs |
| nvmet target (kernel) | ~3-5μs |
| ublk + storage | ~16-30μs |
| **Total** | **~31-70μs** |

**Compare to SPDK NVMe-oF**: ~20-40μs
**Overhead**: ~11-30μs due to kernel overhead

**Acceptable?** YES for most storage workloads!

---

### Remote Access (TCP)

| Component | Latency |
|-----------|---------|
| TCP/IP stack | ~10-20μs |
| Network RTT | ~10-30μs |
| nvmet + storage | ~20-40μs |
| **Total** | **~40-90μs** |

**Still under 100μs** - excellent for network storage

---

## Advantages

### 1. **Simplest Possible Design** ✅
- No custom NVMe-oF code
- Reuse kernel nvmet (battle-tested)
- Standard Linux tools (nvme-cli)

### 2. **Performance Optimized** ⚡
- **Local**: Direct ublk access (~20μs)
- **Remote**: Kernel NVMe-oF (~50μs)
- No extra hops for remote case

### 3. **Pure Rust** 🦀
- 100% Rust for storage daemon
- Memory safety guaranteed
- Easy to maintain

### 4. **Kernel Compatibility** ✅
- rublk solves ublk 6.17+ issues
- nvmet is kernel-maintained
- No userspace ublk threading issues

### 5. **Standard Interfaces** 📐
- /dev/ublkbN for local
- /dev/nvmeXnY for remote
- Works with any filesystem
- Standard Linux block I/O

### 6. **Easy Debugging** 🔍
- Can inspect volumes manually
- Standard Linux tools work
- Clear separation of concerns

### 7. **Cost Effective** 💰
- No SPDK dependency
- Smaller container images
- Less code to maintain

---

## Comparison with Other Options

| Aspect | Pure Rust (Custom NVMe-oF) | Hybrid SPDK | **This Design** |
|--------|---------------------------|-------------|-----------------|
| **Implementation Time** | 24 weeks | 12 weeks | **10 weeks** ✅ |
| **Complexity** | High | Medium | **Low** ✅ |
| **Memory Safety** | 100% | ~80% | **100%** ✅ |
| **Local Latency** | ~15-20μs | ~15-20μs | **~20-30μs** |
| **Remote Latency (RDMA)** | ~40-60μs | ~30-50μs | **~30-70μs** |
| **Maintenance** | High (custom code) | Medium (SPDK+Rust) | **Low** ✅ |
| **Dependencies** | Many new | SPDK | **Kernel only** ✅ |
| **Production Ready** | Unknown | Proven | **Kernel = Proven** ✅ |

---

## Decision Matrix

### Choose This Design If:
- ✅ You want fastest time to market (10 weeks)
- ✅ You value simplicity and maintainability
- ✅ Local latency ~20-30μs is acceptable
- ✅ Remote latency ~50-70μs is acceptable
- ✅ You want pure Rust codebase
- ✅ You trust kernel nvmet stability

### Consider Alternatives If:
- ⚠️ You need absolute minimum latency (<20μs local)
- ⚠️ You have specific NVMe-oF protocol needs
- ⚠️ You want userspace-only solution

---

## Risks and Mitigations

| Risk | Probability | Impact | Mitigation |
|------|-------------|--------|------------|
| **Kernel nvmet performance** | Low | Medium | Benchmark early, can add SPDK later |
| **nvme-cli reliability** | Low | Low | Well-tested tool, fallback to direct kernel |
| **Remote connection issues** | Medium | Medium | Implement retry logic, health monitoring |
| **ublk 6.17+ regressions** | Low | High | rublk actively maintained, track kernel changes |

---

## Next Steps

1. **Week 1**: Prototype
   - Create simple Rust daemon
   - Test ublk device creation
   - Configure nvmet manually
   - Measure baseline performance

2. **Week 2**: Validate Approach
   - Benchmark local access
   - Benchmark remote access (RDMA)
   - Compare to SPDK baseline
   - **Decision point**: If performance is acceptable, continue

3. **Weeks 3-10**: Full Implementation
   - Follow the phased roadmap above

---

## Conclusion

This design provides the **optimal balance** of:
- ✅ **Simplicity**: Leverage kernel nvmet
- ✅ **Speed**: 10 weeks to production
- ✅ **Performance**: Good enough for most workloads
- ✅ **Safety**: 100% Rust
- ✅ **Maintainability**: Minimal custom code

**Recommendation**: Start with this approach. You can always optimize later if needed.

