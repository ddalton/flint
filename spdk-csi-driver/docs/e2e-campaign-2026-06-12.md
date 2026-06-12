# E2E cluster campaign — incremental replica rebuild, phases 0–5b

**Date:** 2026-06-12 (single session)
**Cluster:** trove-provisioned `runf`, 4× AWS `i4i.large` (1 CP + 3 workers,
all four SPDK-capable with one initialized NVMe each), Kubernetes v1.34.8,
chart `flint-csi-driver-chart:1.2.0-rc1`, spdk-tgt `1.1.1` throughout.
**Driver builds:** cross-compiled on an Apple-silicon host with
`cargo zigbuild --target x86_64-unknown-linux-musl` and assembled COPY-only
onto the previous image via `docker/Dockerfile.csi-prebuilt` (no native x86
build node needed; ~2 min per iteration).

| Tag | Digest (manifest) | Content |
|---|---|---|
| 1.2.0-rc1 | `5ad344d5…` | phases 1–5b machinery (campaign baseline) |
| 1.2.0-rc2 | `5673b4f4…` | + bug #1 fix (snapshot-name clamp) |
| 1.2.0-rc3 | `3c3ac93c…` | + bug #2 fix (stale-local-export drop) |
| 1.2.0-rc4 | `d2de744d…` | + restore groundwork (superseded by rc5) |
| 1.2.0-rc5 | `ddbddd9f…` | + bug #3 root fix (`superblock: false`) + format-refusal guard |

All rebuild machinery ships default-disabled; campaign knobs were applied
with `kubectl set env` on the controller (`FLINT_EPOCH_SCHEDULER=enabled`,
`FLINT_EPOCH_INTERVAL_SECS=30`, `FLINT_CATCHUP=enabled|disabled` as each
scenario required; `FLINT_CUTOVER` stayed disabled).

## Results by stage

**Stage 0 — zero-regression gate (rc1, machinery dormant).** Standard
kuttl suite (`tests/system/kuttl-testsuite.yaml`, 8 tests): **8/8 PASS in
132 s.** One environment fix: trove names the primary StorageClass
`flint-spdk`, the suite expects `flint` — a clone SC must be applied first
(recorded in project memory).

**Stage 1 — phases 1–4 live (rc1).** 2-replica volume (replicas aws-1 +
aws-2), consumer on aws-3, busybox writer appending one fsynced line/s.
- Epoch scheduler: cuts on both replicas at the configured cadence;
  retention K=6 retired the oldest epoch from the record **and** both
  lvstores.
- Degrade (delete replica node's flint-csi-node pod → spdk-tgt restart
  drops exports): `VolumeDegraded` + `ReplicaStale` within the monitor
  tick; raid served the writer from the surviving leg with zero I/O
  interruption.
- Incremental heal: `ReplicaCatchupStarted` (revert to newest shared
  epoch, delta copy) → `ReplicaStandby` **7 s after detection**; chase
  then tracked one epoch behind current.
- Phase-4 admission: writer-pod bounce → fenced final delta → both
  `in_sync`, `reverted_to` cleared, reason `admitted at reassembly after
  fenced final delta`. **§5 cornerstone pinned live:** raid online, all
  bases configured, `process: none` — no kernel-visible rebuild.
- Data: every line written before/during/after the cycle present.
- Unplanned bonus: an rc-rollout race assembled a 1-leg raid while the
  record still said both `in_sync`, cutting two divergent epochs on the
  frozen replica. The monitor demoted it and catch-up's `t_back` rewind
  selected a pre-divergence base (epoch-62, skipping shared-but-poisoned
  63–65) — the divergence-window guard worked unprompted.

**Stage 2 — phases 5/5b + §11 (rc2→rc5).**
- §11 cut: one `VolumeSnapshot` → same-named lvol snapshot on **both**
  replicas; CreateSnapshot retry converged after the rc2 fix (same CSI
  name → same clamped lvol name).
- 5b align-at-snapshot: snapshot cut while a replica chased as standby
  appeared on the standby at the next chase with the identical name
  (lineage walk + destination alignment).
- §11 restore (rc5): restored volume read **exactly** the data through
  the snapshot cut (`seq=26` marker; writer was at seq=30 by the time the
  snapshot reported ready). Restored volumes are single-replica clones —
  §10-13.
- Phase 5 full build: catch-up frozen, replica degraded, retention aged
  its `last_epoch` out (~6 min) → on re-enable, `ReplicaFullBuildStarted`
  ("no shared epoch history within retention"), head recreated empty,
  full lineage replayed from aws-1 in **~9 s** (1 GiB thin volume), the
  user snapshot **preserved** on the rebuilt replica, final chain the
  designed minimal shape (user snap + head + target epoch only).
- Admission after full build: new writer landed **on** the rebuilt
  replica's node — exercising the bug-#2 path again — staged in 21 s,
  both `in_sync`, raid 2/2 `process: none`, all 603 writer lines intact.
- Tombstone deletion: copy with no dependents deleted immediately; the
  copy pinned by a live restore clone went tombstone-pending and was
  reaped (tombstone cleared) once the clone was deleted.

**Final regression gate — rc5 (superblock-less layout): 8/8 PASS in
144.8 s**, including snapshot-restore, pvc-clone, multi-replica, and
rwo-pvc-migration.

## Bugs found and fixed (the campaign's yield)

1. **§11 snapshot names overflowed SPDK's lvol-name limit** (rc2,
   `snapshot_csi.rs`). `snap_<pvc-uuid>_<fnv64-as-20-digits>` = 65 chars;
   SPDK caps usable length at 63 → `-32602` on every cut, both replicas.
   Fix: reduce the suffix modulo the digit budget left by `snap_<vol>_`
   (still deterministic, still strict-`u64`-parseable). Unit FakeRpc never
   enforced name length — only the cluster could catch this.
2. **Re-stage onto a replica-hosting node failed EPERM** (rc3,
   `driver.rs`). The consumer landed on a node hosting one of the volume's
   replicas; that replica's NVMe-oF export (left from the previous
   consumer era, correctly fenced *to* the new consumer) still held a
   write-mode open on the lvol, so `bdev_raid_create` could not claim the
   local base. Fix: at local attach, delete any flint `:volume:` subsystem
   exporting the bdev (matched by namespace, covering `active_lvol_uuid`
   overrides). Hit twice more later in the campaign; both staged clean.
3. **Restore from a multi-replica snapshot destroyed the data** (rc5,
   `driver.rs`). Superblocked raids put the filesystem 1 MiB into every
   base lvol; the restore clone inherited that layout; bare-lvol staging
   probed LBA 0, found no filesystem, and formatted — silent data loss,
   unreachable by the kuttl suite (single-replica volumes are bare lvols).
   Root fix: **create raids `superblock: false`** (§10-7 answered, §3
   hazard class structurally eliminated; layout break pre-release only).
   Defense-in-depth: NodeStage now refuses to format any volume marked
   `filesystem-initialized`. Interim attempts that informed the decision:
   raid-wrapping the clone (SPDK rejects single-base creates, `-22` for
   raid1 *and* concat on v26.01) and a loop-device offset mount (worked,
   rejected as a permanent extra failure point).

## Observations for follow-up (none blocking)

- **Epoch cadence halves under contention**: 30 s configured, 60 s
  observed once the health monitor's periodic record patches began —
  likely scheduler ticks lost to 409 retries. Functionally correct.
- **Deletion-path lvol leaks** → §10-14: DeleteVolume with a stale
  replica left heads/epochs/exports; clone-pinned snapshot copies whose
  PV disappears lose their reconciler. Manual sweep was required
  (subsystems first, then leaf-first lvol passes).
- **Epoch GC noise**: per-minute retry warnings on epochs pinned as a
  standby chain's base; consider holding the retention pin until
  admission.
- `GetPluginInfo` still reports `vendor_version: 1.1.1` (Cargo.toml not
  bumped before the campaign builds; cosmetic).
- kuttl leftovers: `ephemeral-inline`/`pvc-clone` leave `eph_*`/
  `temp_pvc_clone_*`/`snap_*` lvols behind on PASS — pre-existing,
  same §10-14 sweep would cover them.
