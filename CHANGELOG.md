# Changelog

All notable changes to Flint CSI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The public API surface for SemVer purposes is the CSI gRPC verbs, the
StorageClass `parameters` schema, and the `volume_context` key
namespace. Internal Rust types and node-agent HTTP routes are not
covered by the stability guarantee.

## [Unreleased]

_Nothing yet._

## [1.3.0] - 2026-06-12

Self-healing release: every common single-failure (replica node loss,
consumer spdk-tgt restart, lone container restart, same-node reschedule
race) now heals autonomously, typically within ~3 minutes and without
workload restarts. All changes validated live on AWS i4i clusters with
forced failure injection.

### Added

- **Consumer data-path self-healing (4 layers).** Storage-baseline
  recovery re-adopts disks after a lone `spdk-tgt` restart (~30 s);
  data-path-lost detection flags volumes whose raid vanished under a
  live attachment (3-strike, PV annotation + events); in-place repair
  rebuilds the raid and loopback export with a **pinned NVMe namespace
  identity** so the kernel initiator reattaches without a workload
  restart; and the cutover orchestrator bounces as a last-resort
  fallback. Escape hatch: `FLINT_DATA_PATH_REPAIR=disabled`.
- **Scheduling escalation for cutover bounces.** Every bounce applies a
  self-expiring `NoSchedule` taint (`flint.csi.storage.io/bounce`,
  TTL `FLINT_CUTOVER_TAINT_SECS`, default 120 s) to the bounced node so
  the replacement cannot reuse the stale staged volume — reassembly
  bounces are now deterministic instead of scheduler-dependent.
- **Orphan sweep (§10-14).** Node agents reap lvols and NVMe-oF
  subsystems whose owning PV no longer exists (3-strike confirmation,
  strict parsers, ublk-verified ephemeral handling).
  `FLINT_ORPHAN_SWEEP=disabled` to opt out.
- Dashboard backend `/healthz` endpoint; liveness/readiness probes
  moved off the aggregate `/api/dashboard` endpoint.

### Fixed

- **RWX volume identity aliasing (six fixes).** An RWX volume's three
  identities (user PV, synthetic backing PV, volumeHandle) corrupted
  each other: zombie raids at unstage blocked every later restage; a
  permanent data-path false positive drove endless NFS-pod bounce
  loops; duplicate epoch/catch-up streams broke snapshot lineage and
  standby admission; replica exports were squatted under alias NQNs;
  an RWX consumer's unstage could detach the live raid's legs; and NFS
  server bounces invalidated every client file handle (now pinned per
  volume via `PNFS_INSTANCE_ID`; foreign handles answer `NFS4ERR_STALE`
  so clients recover by re-walking).
- Retention pin lifecycle: held until standby admission (not copy
  completion) and advanced with the standby's chase mark — epoch
  history no longer grows unbounded behind a chasing standby.
- Dashboard: unreachable nodes can no longer hang the aggregate fetch
  past the liveness deadline (bounded per-node timeouts), and the
  frontend no longer substitutes mock data when the backend is
  unreachable — it keeps last-known data and shows an error banner.

### Known limitations

- **RWX cutover transparency requires clean client state.** A client
  holding dirty open state (unsynced writes) across an NFS server
  bounce can have those writes dropped: the server's NFSv4 state is
  in-memory and does not survive pod replacement. Read-mostly and
  fsync-disciplined workloads ride through transparently. Persistent
  state (SQLite backend on the exported volume) is the next milestone.
- Migration from ≤1.2.0: existing volumes cross onto the pinned
  namespace identity at their next detach/restage; existing NFS server
  pods mint stable file-handle ids at their next recreation.

## [1.0.0] - 2026-05-04

First stable release. Production-ready for SPDK-based deployments;
no-SPDK deployments supported with documented feature subsets. From
this release onward, breaking changes to the CSI gRPC surface,
StorageClass parameters, or `volume_context` keys require a `MAJOR`
version bump.

### Storage architecture

- **High-performance local block storage via SPDK userspace I/O.**
  Bypasses the kernel block layer; delivers full NVMe bandwidth from
  a userspace target backed by `ublk` on each worker. Per-worker
  hugepage and disk requirements documented in the README.
- **Multi-replica volumes via NVMe-oF RAID across nodes.** RAID-1
  mirrors and optional RAID-5f, transparent to the NFS protocol layer.
  Survives single-disk and single-node loss without client-visible
  outages beyond the underlying NVMe-oF reconnect window.
- **pNFS data path** (RFC 8881 FILE layout). Parallel-server NFSv4.1
  with stripes across multiple data servers; opt-in via StorageClass
  `parameters.layout: pnfs`. Single-host bench shows ~1.6× write
  throughput over single-server NFS at fsync=1 (ADR 0003); cross-host
  scaling measurable via the included Kubernetes bench harness
  (`make test-pnfs-cross-host`).
- **Volume snapshots and clones** in SPDK mode via `bdev_lvol_snapshot`
  and `bdev_lvol_clone`. Instant copy-on-write; space-efficient.
- **Online volume expansion** without downtime.
- **CSI inline ephemeral volumes** for pod-scoped temporary storage.

### pNFS production hardening

- **Persistent NFSv4.1 / pNFS server state** (`Phase B`). Client IDs,
  sessions, stateids, layouts, and pNFS file handles survive MDS pod
  restarts via a SQLite-backed `StateBackend` (WAL + NORMAL crash-
  safe). Kernel clients reconnecting after a restart resume against
  the same record set with no `STALE_CLIENTID` or `BAD_STATEID` storm.
  Verified end-to-end via `make test-pnfs-restart` with byte-for-byte
  hash matching across restart.
- **DS death recovery** (`Phase A`). Heartbeat monitor detects a dead
  data server, fans out `CB_LAYOUTRECALL` to all affected client
  sessions via the back-channel, and forcibly revokes layouts after
  the RFC 5661 §12.5.5.2 deadline if clients don't return them.
  Verified end-to-end via `make test-pnfs-recall`.
- **NFSv4.1 RFC conformance.** Pynfs full suite: 167 PASS / 4 FAIL /
  91 SKIP (5.8× the original audit baseline of 26 PASS). Six suites
  at 100%, nine more above 70%. The four remaining failures are
  documented niche cases that do not cascade or corrupt data.

### CSI integration

- **StorageClass `parameters.layout: pnfs`** opts a volume into the
  pNFS data path. Default StorageClasses use single-server NFS or
  direct SPDK block per existing chart configuration.
- **`volume_context` namespaces.** Production keys live under
  `flint.csi.storage.io/*` (SPDK mode) and `pnfs.flint.io/*`
  (pNFS mode). These namespaces are stable from 1.0.0; new keys may
  be added in `MINOR` releases, removals or renames require `MAJOR`.
- **VolumeSnapshot CRD preflight.** At controller startup, the driver
  checks for the cluster-wide `VolumeSnapshot{,Class,Content}` CRDs
  and logs a one-line warning with the install command if any are
  missing. Non-fatal: non-snapshot RPCs work without the CRDs.
- **Snapshot guards for unsupported volume types.** `CreateSnapshot`
  and `CreateVolume`-from-snapshot/PVC return `FAILED_PRECONDITION`
  (final, non-retryable per CSI) for pNFS volumes, replacing a prior
  `NOT_FOUND`-induced retry loop in `external-snapshotter`.

### Operations & ergonomics

- **Helm chart** for installation under Kubernetes 1.21+. Optional
  pNFS mode (`pnfs.enabled: true`); SPDK enabled by default.
- **Web dashboard** for disk discovery, initialization, and monitoring.
- **`NOTES.txt`** rendered after `helm install` surfacing the
  `VolumeSnapshot` CRD prerequisite explicitly.
- **Test surface:** 330 Rust unit tests, KUTTL system tests across
  SPDK + pNFS paths, Lima e2e harnesses for pNFS protocol / restart /
  recall flows, and a scaffolded cross-host bench harness.

### Deployment modes

| Mode | Storage backend | Snapshots | Replication | Status |
|---|---|---|---|---|
| Production-SPDK | SPDK blobstore | ✅ Native COW | ✅ NVMe-oF RAID | Recommended |
| Production-no-SPDK (single-server NFS) | Filesystem | ⏸️ Roadmap | ❌ Customer-provided | Supported |
| Production-no-SPDK (pNFS) | Filesystem | ❌ Not supported | ❌ Customer-provided | Supported with limits |
| Dev/QE (Kind/Lima) | Loopback | Optional | None | Dev only |

### Container images

Published to Docker Hub under the `dilipdalton/` namespace for
`linux/amd64`:

```
dilipdalton/flint-csi-driver:1.0.0
dilipdalton/spdk-target:1.0.0
dilipdalton/flint-dashboard:1.0.0
```

Aliases: `:1.0`, `:1`, `:latest`. **Production deployments should pin
to an immutable tag (`:1.0.0`).** The chart's `values.yaml` defaults
to `:latest` for development convenience; production users should set
each `images.<component>.tag` to `"1.0.0"`.

### Known limitations

- **pNFS volumes do not support snapshots in any deployment mode.**
  Snapshot RPCs against pNFS sources return `FAILED_PRECONDITION`.
  Workaround: use a non-pNFS StorageClass for volumes that need
  snapshots, or use SPDK mode for performance + snapshot capability.
- **No-SPDK volumes have no Flint-level replication.** Durability
  comes from the underlying block volume (EBS/PD/Ceph RBD/etc.). For
  cross-node redundancy without external durable storage, use SPDK
  mode with NVMe-oF RAID.
- **`linux/arm64` container images are not published in this release.**
  ARM64 is a planned target; x86-64 ships first to match the primary
  deployment fleet (Cloudera customer infrastructure and current QE/CI).
  ARM64 builds will follow in a subsequent release.
- **`VolumeSnapshot` CRDs are a cluster-wide prerequisite** not
  installed by the Flint chart (cluster-singleton; bundling them
  would conflict with other CSI drivers). Without them, the bundled
  `snapshot-controller` Deployment will `CrashLoopBackOff`. See
  README "Snapshot Prerequisites" for the install command. The Flint
  controller logs a startup warning if missing.
- **pNFS Flex Files (FFL) layout is not implemented and is deferred
  indefinitely.** Replication is handled at the SPDK NVMe-oF RAID
  layer (below the protocol); FFL would duplicate that capability
  with client-side write amplification and a separate rebuild
  scanner. Decision recorded in
  `docs/plans/pnfs-production-readiness.md`.

### Upgrade notes

This is the first tagged release. There are no prior stable versions
to upgrade from. Operators running pre-1.0 builds should reinstall
fresh against `v1.0.0`. The pre-1.0 git history is preserved at the
`archive/config` and `archive/disk_mgmt` tags for forensic reference;
neither tag represents a supported upgrade source.

### Security

No security advisories at this release.

[Unreleased]: https://github.com/ddalton/flint/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/ddalton/flint/releases/tag/v1.0.0
