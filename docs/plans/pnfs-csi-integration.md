# pNFS CSI Integration — Plan

**Goal**: a `layout: pnfs` StorageClass parameter that provisions a
pNFS-striped volume which a pod can mount and write to. Builds on the
existing `flint-pnfs-mds` and `flint-pnfs-ds` binaries (which already
work — see `bench.sh`'s 2.10× write win), and adds the CSI gRPC plumbing
that connects them to Kubernetes.

**Non-goals (deferred)**: CB_LAYOUTRECALL (Task #4), state persistence
(Task #5), DS auto-discovery, locality-aware placement, multi-tenancy.
We're shipping the minimum useful slice. Everything else is a follow-up.

**Scope guarantee**: zero changes to SPDK code paths. New module sits
alongside `rwx_nfs.rs` and is selected by `parameters.layout == "pnfs"`
*before* the existing `rwx_nfs` / SPDK-block branches in
`main.rs::create_volume`.

## End state

```yaml
# What a user writes:
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata: { name: flint-pnfs }
provisioner: flint.csi.storage.io
parameters:
  layout: pnfs
volumeBindingMode: WaitForFirstConsumer

---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: my-vol }
spec:
  accessModes: [ReadWriteMany]
  resources: { requests: { storage: 10Gi } }
  storageClassName: flint-pnfs
```

What happens under the hood:

1. CSI provisioner calls `CreateVolume`. Driver sees `layout: pnfs`.
2. Driver opens gRPC to MDS service (env var
   `FLINT_PNFS_MDS_ENDPOINT=flint-pnfs-mds:20490`), calls a new
   `MdsControl.CreateVolume(name, size_bytes)` verb.
3. MDS creates a file at `<export>/<volume_id>`, ftruncates to size,
   returns OK.
4. Driver returns `volume_context` with `pnfs.flint.io/mds-ip`,
   `pnfs.flint.io/mds-port`, `pnfs.flint.io/export-path`,
   `pnfs.flint.io/volume-file`.
5. Pod schedules → kubelet calls `NodePublishVolume`. Driver reads
   the `pnfs.flint.io/*` keys, runs `mount -t nfs4
   -o minorversion=1,nconnect=4,rsize=1M,wsize=1M
   <mds-ip>:<export>/ <target>` (or with the volume's specific path).
6. Pod reads/writes through the mount; kernel does FILES-layout
   striping across DSes (already works).
7. On `DeleteVolume`, driver calls `MdsControl.DeleteVolume(name)`.

## Phasing

Five PRs, each independently revertible, each `cargo test` and `bench.sh` clean.

### PR 1 — extend MDS gRPC with volume verbs (~half day)

Add to `spdk-csi-driver/proto/pnfs_control.proto`:

```protobuf
service MdsControl {
  // ...existing DS-management verbs unchanged...

  // New:
  rpc CreateVolume(CreateVolumeRequest) returns (CreateVolumeResponse);
  rpc DeleteVolume(DeleteVolumeRequest) returns (DeleteVolumeResponse);
}

message CreateVolumeRequest {
  string volume_id = 1;
  uint64 size_bytes = 2;
}

message CreateVolumeResponse {
  string export_path = 1;   // e.g. "/tmp/flint-pnfs-mds-exports"
  string volume_file = 2;   // relative path within export, e.g. "pvc-abc123"
}
```

Implementation in `pnfs/grpc.rs::MdsControlService`: read export path
from existing config, `OpenOptions::create_new` the file, `set_len` to
`size_bytes`, return paths. Idempotent (Create on existing file with
matching size = success; mismatch = error).

Unit test: round-trip create+delete, verify file appears/disappears.

### PR 2 — new `pnfs_csi` module in driver (~1 day)

New file `spdk-csi-driver/src/pnfs_csi.rs`:

```rust
pub struct PnfsCsi {
    mds_endpoint: String,    // from FLINT_PNFS_MDS_ENDPOINT env var
}

impl PnfsCsi {
    pub fn from_env() -> Option<Self> { ... }  // None if env not set

    pub async fn create_volume(
        &self,
        volume_id: &str,
        size_bytes: u64,
    ) -> Result<HashMap<String, String>, BackendError>;

    pub async fn delete_volume(&self, volume_id: &str)
        -> Result<(), BackendError>;
}
```

`create_volume` opens a gRPC client to the MDS, calls the new verb,
builds volume_context with the four `pnfs.flint.io/*` keys.

No mount logic in this module — that lives in main.rs's NodePublish
section, alongside the existing rwx_nfs mount logic.

Unit test: mock-MDS server returns canned responses, verify
volume_context shape.

### PR 3 — wire into main.rs (~half day)

Three locations:

1. `create_volume` (main.rs:687–920): add branch right after parameter
   parsing, before the `nfs_empty_dir` check:

   ```rust
   if req.parameters.get("layout").map(|s| s.as_str()) == Some("pnfs") {
       if let Some(pnfs) = &self.pnfs_csi {
           let ctx = pnfs.create_volume(&volume_id, size_bytes).await?;
           return Ok(/* response with ctx */);
       }
       return Err(Status::failed_precondition("pNFS not configured"));
   }
   ```

2. `delete_volume` (main.rs:921–1043): symmetric branch, route to
   `pnfs.delete_volume(&volume_id)` if the PV's `volume_context` carries
   the `pnfs.flint.io/mds-ip` key.

3. `node_publish_volume` (main.rs:2288–2681): branch on
   `pnfs.flint.io/mds-ip` presence, run NFSv4.1 mount with explicit
   minorversion=1 + nconnect=4 + rsize=wsize=1M (from bench.sh).

The driver's `Self` gains `pnfs_csi: Option<PnfsCsi>` initialized from
env var at startup. SPDK code paths see `None` and behave identically.

### PR 4 — deployment + StorageClass example (~half day)

New `deployments/pnfs-mds-service.yaml` (ClusterIP exposing
`flint-pnfs-mds` Deployment on port 20490 — Deployment manifest already
exists at `deployments/pnfs-mds-deployment.yaml`).

New `deployments/pnfs-csi-storageclass.yaml`:

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata: { name: flint-pnfs }
provisioner: flint.csi.storage.io
parameters:
  layout: pnfs
volumeBindingMode: WaitForFirstConsumer
allowVolumeExpansion: false   # no expand support yet
```

Update helm chart `flint-csi-driver-chart/templates/controller.yaml` to
inject `FLINT_PNFS_MDS_ENDPOINT` env var (only if a values.yaml flag
`pnfs.enabled: true` is set; default false to keep zero impact on
existing deployments).

### PR 5 — end-to-end test (~half day)

New script `tests/lima/pnfs/csi-e2e.sh`:

1. Start MDS + DSes (reuse bench.sh helpers).
2. Skip CSI driver — exercise the `pnfs_csi` module directly via a tiny
   test binary that calls `create_volume → mount → write → unmount →
   delete_volume`.
3. Verify per-DS bytes show striping.
4. Verify the file is removed from MDS export after `delete_volume`.

This is *not* a full Kubernetes test — that comes when we have the
regression suite running. This validates the integration code without
needing a cluster.

## Total wall-time

~3 focused days. Each PR is ≤200 lines of mechanical change against
clearly-defined interfaces. SPDK paths untouched throughout.

## What this leaves out (and why it's OK)

- **No CSIDriver change**: same provisioner name (`flint.csi.storage.io`),
  same registration. From the kubelet's perspective, this is the same
  driver gaining a new parameter.
- **No replication/HA**: pNFS volumes are non-replicated for now. ADR
  0001 calls this out — this is a perf tier, durability story comes via
  S3 spillover (Phase D of the broader plan, not in scope here).
- **No multi-MDS**: one MDS per cluster. HA later.
- **No snapshot or clone**: future work; documented in StorageClass yaml
  with `allowVolumeExpansion: false`.
- **No metrics**: `pnfs_csi` module logs at info+; Prometheus exposition
  is a follow-up.

## Pass criteria for the whole plan

- `make test-pnfs-smoke` still passes (no regression on existing path).
- New `make test-pnfs-csi` passes — round-trips a CSI volume.
- Manual test on real cluster: apply StorageClass, create PVC + pod,
  see pod read/write a striped volume, see DSes accumulate bytes.
- `cargo test --release --lib` count grows by a small amount; all green.
- Existing `bench.sh` write-2.10× number unchanged.
