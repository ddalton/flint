# SPDK Chainsaw suites on real nodes — 2026-10-02

**Result: every suite green.** standard 8/8, clean-shutdown 1/1,
replica-rebuild 1/1, nfs-only 4/4.

## What ran

- Cluster: trove `csw`, all spot, control plane + 3 workers, **i4i.large**
  (2 vCPU, 16 GiB, one 468 GB NVMe instance store), us-west-1, Amazon
  Linux 2023, kernel 6.18.51, Kubernetes v1.34.12.
- Driver: `dilipdalton/flint-driver:spdk-1002-10c9c0fe`
  (sha256:f6e8ef94…), built from commit 10c9c0fe the published way
  (cargo-zigbuild musl + `Dockerfile.csi.prebuilt`); `spdk-tgt:1.7.0`
  (the chart's pinned target, polling mode, 2 GiB of 2 MiB hugepages).
- Chart: this tree's `flint-csi-driver-chart`, three-container mode,
  `storageClass.name=flint`, dashboard off.
- Disks: each node's instance store, initialized through the production
  path — node agent `POST /api/disks/initialize` with the disk's PCI
  address (`0000:00:1f.0`); the agent had already attached it as a uring
  bdev.

## Results (JUnit XML beside this file)

| Suite | Tests | Result | Wall |
|---|---|---|---|
| standard | ephemeral-inline, rwo-pvc-migration, multi-replica, pvc-clone, volume-expansion, snapshot-restore, rwx-single-replica, rox-multi-pod | 8/8 PASS | 3 min |
| clean-shutdown | clean-shutdown | PASS (rerun) | 42 s |
| replica-rebuild | replica-rebuild | PASS | 348 s |
| nfs-only | ephemeral-inline, rwo-pvc-migration, volume-expansion, rwx-single-replica | 4/4 PASS (rerun) | 2.5 min |

The two reruns, and why the first runs failed (neither is the driver):

- **clean-shutdown** first run: step 02's assert file says "Verify pod is
  deleted" but asserted `phase: Succeeded` — true only while kuttl ignored
  the `$patch: delete`. Now an `error` (absence) operation.
- **nfs-only** first run: trove had pre-created a `flint-nfs`
  StorageClass with the OLD provisioner name `flint.csi.storage.io` and
  Immediate binding; a StorageClass's provisioner is immutable, so the
  rig's `kubectl apply` kept trove's. Recreated with `disk.csi.chert.us`.

`rox-multi-pod` ran with its fixed probe (stderr redirected before the
write) — the write to a ReadOnlyMany volume mounted without `readOnly`
was REFUSED.

## Why this ran on AWS, not kind

On one host, kind nodes share ONE kernel and so one NVMe-oF initiator and
one `/sys/class/nvme`: a second connect of a volume's NQN from another
"node" is refused, so every test that re-stages a volume fails there for
reasons a real cluster does not have. Same suites, kind on the box: 4/8
standard. On separate kernels: 8/8.

## Teardown

trove project 169 deleted 19:50Z; verified 19:52Z with an UNFILTERED
count: 0 non-terminated instances in us-west-1 (and us-east-1,
us-west-2, us-east-2), 0 volumes, 0 open/active spot requests, 0 EIPs, no
non-default security groups, trove 0 projects / 0 orphans. No S3, IAM or
KMS objects were created. Compute ≈ 4 × 50 min × ~$0.04/h.

Node-plugin logs (large) are kept off the repo:
box `~/chainsaw-migrate-2026-10-02/aws-node-logs-2026-10-02.tgz`.
