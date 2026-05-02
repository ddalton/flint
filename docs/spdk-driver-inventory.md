# SPDK CSI Driver — What We're Protecting

Pre-refactor inventory. This document answers one question: **before we carve `flint-pnfs-csi` out and lift `nfs/v4/` into `flint-shared`, what behaviour does the existing SPDK driver actually promise its users today?** Every claim cites a file:line so we can verify against the code, not against memory.

The SPDK driver's user-visible contract is what the regression suite has to lock down. Anything outside this contract is implementation detail and can change freely; anything inside this contract has to keep working byte-for-byte after the refactor.

## 1. Two products in one binary

The driver ships two distinct volume types behind the same provisioner. They share the gRPC entry points but diverge into separate code paths almost immediately.

| Product | Access modes | Backend | When it triggers |
|---|---|---|---|
| **SPDK block** | RWO | NVMe-oF + ublk + optional RAID-1 | Default — `numReplicas` ≥ 1, no `nfsEmptyDir`, no RWX/ROX capability |
| **NFS server pod** | RWX, ROX | Single-server NFS (`flint-nfs-server`) backed by emptyDir or PVC | `nfsEmptyDir: true` parameter, OR access mode is `MultiNodeMultiWriter` / `MultiNodeReaderOnly` |

The dispatch is parameter-and-capability-driven and lives in `main.rs::create_volume` (lines 687–920). Everything from there flows through one of two backends.

## 2. CSI gRPC surface — what's implemented

From `spdk-csi-driver/src/main.rs`. "Implemented" = not `Status::unimplemented`.

**Implemented (must keep working):**

- `CreateVolume` — main.rs:687–920
- `DeleteVolume` — main.rs:921–1043
- `ControllerPublishVolume` — main.rs:1093–1315
- `ControllerUnpublishVolume` — main.rs:1316–1357
- `CreateSnapshot` / `DeleteSnapshot` / `ListSnapshots` — main.rs:1470–1495 (delegate to `snapshot/`)
- `ControllerExpandVolume` — main.rs:1498–1573
- `NodeStageVolume` — main.rs:1613–2151
- `NodeUnstageVolume` — main.rs:2153–2287
- `NodePublishVolume` — main.rs:2288–2681
- `NodeUnpublishVolume` — main.rs:2684–2983
- `NodeExpandVolume` — main.rs:3015–3114
- `ValidateVolumeCapabilities` — main.rs:1362–1414

**Stubbed (no contract — fine to ignore in regression):**

- `ListVolumes` — main.rs:1416–1421
- `GetCapacity` — main.rs:1423–1428

## 3. StorageClass parameters honored today

Only four parameters actually do anything in `main.rs`. Anything else in user yaml is silently ignored.

- `nfsEmptyDir` — main.rs:697, 738. `"true"` forces NFS-server-pod backed by emptyDir; SPDK block path skipped entirely.
- `numReplicas` — main.rs:730. `u32`, default `1`. ≥2 triggers multi-replica RAID-1 path.
- `thinProvision` — main.rs:734. `bool`, default `false`. Passed to `driver.create_volume()`.
- `csi.storage.k8s.io/pvc/name` — main.rs:888. Provisioner-injected; used for NFS pod naming.

## 4. Volume access modes accepted

Validated at main.rs:1377–1380.

- **`SingleNodeWriter` (RWO)** → SPDK block path (NVMe-oF + ublk).
- **`MultiNodeReaderOnly` (ROX)** → NFS server pod, `--read-only`. Detection at main.rs:1120–1126.
- **`MultiNodeMultiWriter` (RWX)** → NFS server pod, full read-write. Detection at main.rs:788–793.

Other modes (e.g. `SingleNodeReaderOnly`, `SingleNodeMultiWriter`) are not explicitly handled and behaviour is untested.

## 5. The two backends, end-to-end

### Backend A — SPDK block (RWO)

| Phase | File:line | What it does |
|---|---|---|
| CreateVolume | main.rs:819 | `driver.create_volume()` creates lvol; metadata written to PV `volumeAttributes` (main.rs:841–865) |
| ControllerPublishVolume | main.rs:1208–1314 | Returns `bdevName` in `publish_context` (main.rs:1252) |
| NodeStageVolume | main.rs:1706–1740 | Mounts block device to staging path; creates ext4 |
| NodePublishVolume | main.rs:2430–2671 | Bind-mounts staging path to pod target path |
| NodeExpandVolume | main.rs:3015–3114 | Detects FS type, runs `resize2fs` / `fsadm` / `btrfs resize` |
| DeleteVolume | main.rs:953 | `driver.delete_lvol()` per replica |

Backing modules: `driver.rs` (lvol create/delete), `spdk_native.rs` (SPDK RPC), `node_agent.rs` (ublk, NVMe-oF orchestration), `raid/` (multi-replica), `snapshot/` (CoW snapshot mgmt), `nvmeof_utils.rs` (NQN formatting), `reserved_devices.rs` (device tracking).

### Backend B — RWX-via-NFS-server-pod

| Phase | File:line | What it does |
|---|---|---|
| CreateVolume | main.rs:784–812 | Detects RWX/ROX; creates SPDK volume as backing storage; stamps `nfs.flint.io/enabled` etc. into volume_context |
| ControllerPublishVolume | main.rs:1190 | First publish creates PV + PVC + Pod via `rwx_nfs::create_nfs_server_pod()` |
| NodeStageVolume | main.rs:1665–1677 | NFS path: just creates staging directory (no block mount) |
| NodePublishVolume | main.rs:2403 | Runs `mount -t nfs4 -o vers=4.2 <ip>:<export> <target>` |
| DeleteVolume | main.rs:932–935 | `rwx_nfs::delete_nfs_server_pod()` removes pod, PVC, PV |

Backing modules: `rwx_nfs.rs` (pod/PVC/PV orchestration), `nfs_main.rs` (the `flint-nfs-server` binary entry), `nfs/v4/` (NFSv4.1 server protocol implementation — the same code lifted to `flint-shared` under Option C).

## 6. `volume_context` keys — the gRPC contract surface

These are the actual bytes that flow through Kubernetes PV objects. Both drivers are sensitive to changes here.

**Written by `CreateVolume`:**

- `size` (Gi format)
- `flint.csi.storage.io/replica-count`
- `flint.csi.storage.io/node-name` (single-replica only)
- `flint.csi.storage.io/lvol-uuid` (single-replica only)
- `flint.csi.storage.io/lvs-name` (single-replica only)
- `flint.csi.storage.io/replicas` (JSON, multi-replica only)
- `flint.csi.storage.io/source-volume` (clones)
- `flint.csi.storage.io/source-snapshot` (snapshot-derived)
- `nfs.flint.io/enabled` (RWX/ROX volumes)
- `nfs.flint.io/replica-nodes` (CSV of node names)
- `nfs.flint.io/backend` (`emptydir` or implicit `pvc`)
- `csi.storage.k8s.io/pvc/name`

**Written by `ControllerPublishVolume` (publish_context):**

- `nfs.flint.io/server-ip`
- `nfs.flint.io/export-path`
- `nfs.flint.io/port` (default `2049`)
- `volumeType` (`nfs`, `local`, `multi-replica`)
- `bdevName` (local volumes)
- `replicas` (JSON, multi-replica volumes)
- `originalVolumeId` (synthetic NFS PV handle mapping)

These keys are part of the user-visible contract — they appear in `kubectl describe pv`, in any `velero` backup, in any tooling that introspects PV state. Keeping them stable is non-negotiable.

## 7. Snapshot, clone, expand — completeness

- **CreateSnapshot / DeleteSnapshot / ListSnapshots** — fully implemented via `snapshot/` module. `snapshot_csi.rs` reports `ready_to_use: true` immediately (snapshots are CoW, instant).
- **Clone from snapshot** — main.rs:712–714 → `create_volume_from_snapshot()` (main.rs:372–513). Implemented end-to-end.
- **Clone from volume** — main.rs:716–718 → `create_volume_from_volume()` (main.rs:515–686). Implemented end-to-end.
- **ControllerExpandVolume** — main.rs:1498–1573. Updates PV capacity in K8s; handles RWX correctly.
- **NodeExpandVolume** — main.rs:3015–3114. FS-type-aware resize.

No obvious TODOs or stub returns in any of these paths.

## 8. State storage

The driver is **stateless on disk**. All persistent state lives in Kubernetes:

- Per-volume metadata → PV `volumeAttributes` (driver.rs:1467–1510 reads via `get_volume_info_from_pv()`).
- Multi-replica info → JSON-serialized into a single key.
- NFS metadata → `nfs.flint.io/*` keys on the PV.

Runtime state (driver.rs:49–62) is in-memory only:

- `spdk_node_urls: HashMap<String, String>` — node→RPC URL cache.
- `capacity_cache` — 30s TTL.

No etcd, no SQLite, no on-disk persistence. This is **important for the refactor** — there's no state migration to worry about.

## 9. Kubernetes resources the driver creates

Only via `rwx_nfs.rs::create_nfs_server_pod()` (lines 219–451). Per RWX/ROX volume:

1. PersistentVolume (synthetic handle `nfs-server-{volume_id}`)
2. PersistentVolumeClaim (in `flint-system` namespace)
3. Pod running `/usr/local/bin/flint-nfs-server`
4. Service for stable DNS endpoint

`controller_operator.rs` reconciles Pod health but does not create additional resources. The SPDK block path creates **no** Kubernetes resources — it's pure block-device manipulation.

## 10. What testing exists today

### Unit tests (`cargo test --release --lib`)

~20 tests, fast (1.5s wall), in-process only. Coverage by module:

- `minimal_disk_service.rs`, `spdk_native.rs` — RPC stubs / JSON parsing
- `reserved_devices.rs`, `nvmeof_utils.rs` — device tracking, NQN formatting
- `capacity_cache.rs` — TTL eviction, concurrency
- `snapshot/` — encoding, error handling, routes
- `spdk_dashboard_backend_minimal.rs` — query parsing
- `nfs::v4::compound::tests` — 11 tests including the new VERIFY/SECINFO/LAYOUTRETURN ones

These cover internals, **not** end-to-end behaviour.

### Integration tests (`spdk-csi-driver/tests/`)

12 files, all NFS-protocol-shaped:

- `nfs_conformance_test.rs`, `secinfo_wire_format_test.rs`, etc. — wire-format and protocol unit tests
- `nfs_client_test.rs` — NFS client library
- `nfs-lock/nfs-lock-test.c` — C-level lock semantics

These test **the NFS server**, not the CSI driver. None of them exercise CreateVolume → Publish → I/O → Delete.

### End-to-end tests

- `tests/lima/pnfs/smoke.sh` — pNFS data path (24 MiB striped write/read).
- `tests/lima/pnfs/pynfs.sh` — pNFS pynfs subset (1 PASS / 3 FAIL / 4 SKIP).
- `make test-nfs-protocol` — full pynfs against `flint-nfs-server` (153/18/91 currently).
- `make test-nfs-mount`, `test-nfs-frag` — minimal mount sanity.

**No SPDK CSI end-to-end test.** No script verifies CreateVolume → bound PVC → pod schedules → I/O works → delete cleans up. This is the gap.

### CI

No `.github/workflows/` directory. No automated CI pipeline. All tests run manually via Makefile or developer workflow.

---

# What this means for the regression suite

The inventory makes the gap concrete. We have strong coverage of:

- **NFS protocol behaviour** (pynfs 153/18/91, integration tests, unit tests).
- **pNFS data path** (smoke).
- **In-process driver internals** (cargo lib tests).

We have zero coverage of:

- **The SPDK CSI gRPC contract under realistic conditions.** No automated test runs the full CreateVolume → Publish → Stage → I/O → Unstage → Unpublish → Delete cycle on a real kubelet.
- **`volume_context` and `publish_context` key stability.** Nothing asserts that `nfs.flint.io/server-ip` is still a key after a refactor, or that `bdevName` still appears in publish_context for local volumes.
- **Snapshot, clone, expand end-to-end.** Implemented, untested.
- **Multi-replica RAID-1 path.** Implemented, untested.

Before the refactor, we need a regression suite that closes that gap. The suite has to assert behaviour at the level of the contract listed in this document — not the implementation.

## Proposed regression suite (next step)

Seven scenarios, each a Bash + `kubectl` test against a kind cluster:

1. **`spdk-rwo-basic`** — PVC → pod → write file → restart pod → read file → delete. Backend A.
2. **`spdk-rwo-snapshot`** — PVC → write → snapshot → new PVC from snapshot → read matches.
3. **`spdk-rwo-clone`** — PVC → write → clone PVC → read matches.
4. **`spdk-rwo-expand`** — PVC at 1Gi → expand to 2Gi → pod sees 2Gi.
5. **`spdk-rwo-multi-replica`** — `numReplicas: 2` → write → kill one replica's node → read still works.
6. **`spdk-rwx-emptydir`** — `nfsEmptyDir: true` RWX PVC → two pods on different nodes → both write/read consistently.
7. **`spdk-rwx-pvc`** — RWX PVC backed by PVC (not emptyDir) → same.

Each scenario asserts:
- All gRPC calls return success.
- The expected `volume_context` keys appear (per §6).
- The expected `publish_context` keys appear (per §6).
- The actual data written matches what's read back (sha256).
- Cleanup leaves no orphan resources (`kubectl get pv,pvc,pod -n flint-system` is empty after deletion).

Baseline: run on `main`, capture results to `tests/baseline/spdk-e2e/`. Every refactor PR must produce identical results (modulo timestamps, UIDs, and IPs).

This is the work that lets us verify "the SPDK driver still works" after each refactor PR. It's the load-bearing safety net.

---

## What's NOT in this inventory (intentionally)

- Performance numbers — not a contract; refactor shouldn't measurably change them, but we don't promise specific latencies.
- Internal helper APIs — those can change freely.
- Log content — we'll capture it as a baseline diff but tolerate format changes.
- Test fixtures and dev tooling — not user-visible.

The contract is: gRPC surface, parameters honored, access modes accepted, volume_context keys, publish_context keys, end-to-end data integrity. Everything else is implementation detail.
