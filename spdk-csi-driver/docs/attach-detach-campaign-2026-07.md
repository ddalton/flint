# CSI attach/detach chaos campaign — 2026-07

**Status:** PAUSED 2026-07-13 at drill 1.3 — **probable P0 found** (lost
fsync-acked write under force-delete detach/reattach, see Findings); cluster
torn down on request, campaign resumes on a fresh cluster. **Under test:**
shipped **v1.15.0** (chart 1.15.0, `dilipdalton/flint-driver:1.15.0`), no code
changes. **Cluster:** trove project 36 `runs` — 4× i4i.xlarge (1 CP + 3
workers), k8s v1.34.9, 937 GB instance-store NVMe per worker (DELETED
2026-07-13). **Harness:** `tests/chaos/` (Postgres 16 + pgbench + acked-write
ledger oracle).

## Goal

Torture the CSI attach/detach path (ControllerPublish/Unpublish +
NodeStage/Unstage + NodePublish/Unpublish) under a real stateful workload,
progressing **RWO r1 → RWO r2(/r3) → RWX**, and prove flint recovers with
**zero lost acknowledged writes** and **no manual VolumeAttachment surgery**.
Closes the node-failure / VA-cleanup / pod-lifecycle gaps flagged
HIGH-PRIORITY in `tests/system/MISSING_CRITICAL_TESTS.md`.

## Method

- **Oracle:** `pg-load` (anti-affinity to the DB) runs pgbench pressure plus a
  ledger loop that appends a seq to `/acked/acked.log` only after the
  `INSERT ... RETURNING` commit is acknowledged (`synchronous_commit=on`).
  Any acked seq later missing from the `ledger` table = **lost acked write**.
- **Per-drill verdict** (`verify-drill.sh`, one `results.csv` row each): pod
  Ready + attribution; ledger reconciliation + `pg_amcheck --heapallindexed` +
  WAL-corruption log grep + writability; VA consistency; NVMe session health
  (live volume `live`, no rise in orphaned sessions); orphaned-mount scan;
  driver-log error scan; timing capture.
- NVMe recovery config under test (v1.15.0 defaults): `ctrl_loss_tmo=1800s`,
  `reconnect_delay=5s`.

## Phase 0 — provisioning + smoke (DONE)

- Cluster provisioned; campaign SCs `flint`/`flint-r2`/`flint-r3` applied (all
  WaitForFirstConsumer).
- **Finding P0-a (trove):** SPDK-mode install did not initialize the
  instance-store disks — controller saw 0 capacity, every CreateVolume failed.
  Fixed manually (`/api/disks/initialize` on `0000:00:1f.0` per worker).
- **Finding P0-b (trove):** spot/instance-type launch spec not honored — all
  nodes came up on-demand i4i.xlarge regardless of request.
- kuttl smoke green after disk-init: rwo-pvc-migration, multi-replica,
  rwx-single-replica, clean-shutdown all PASS.

## Phase 1 — RWO, numReplicas=1

_Results table filled from `tests/chaos/results.csv` as drills complete._

| # | Kill vector | Ready | Stall | Verdict | Notes |
|---|---|---|---|---|---|
| 1.1 | in-container postmaster SIGKILL | 22s | 22s | PASS | in-place restart, zero CSI calls, DB recovered |
| 1.2 | graceful pod delete | 6s | 6s | PASS | same-node replace, clean shutdown |
| 1.3 | force delete (`--grace-period=0 --force`) | — | — | **FAIL (P0)** | postgres unrecoverable: WAL redo segments missing; see Finding F1 |
| 1.4–1.15 | — | | | NOT RUN | campaign paused after F1 |

## Phase 2 — RWO, numReplicas=2 (RAID1)

_TBD._

## Phase 3 — RWX (NFS)

_TBD._

## Findings

### F1 (probable flint P0) — lost fsync-acked write under force-delete detach/reattach

**Drill 1.3** (`kubectl delete pod pg-0 --grace-period=0 --force`, same-node
replacement, RWO r1 on `runs-aws-3`): the replacement pod's postgres went into
permanent CrashLoopBackOff —
`PANIC: could not find redo location 3/655168B8 referenced by checkpoint
record at 3/6E9FEDD8`. Every CSI-level check passed (1 VA on the right node,
NVMe session `live`, no orphaned mounts, no unresolved driver-log errors) —
the volume *attached* fine; its **contents** were inconsistent.

**On-disk state** (forensics preserved in
`tests/chaos/artifacts/1-1.3-1783920814/`):

- `pg_control`: checkpoint `3/6E9FEDD8`, redo `3/655168B8`, cluster state
  `shutting down`, mtime 05:33:35 (kill was T0=05:33:34).
- `pg_wal`: segments `65,66,67,68,69,6B,6C,6D` **missing**; `6A` present but
  zeroed (xlp_pageaddr=0 — a fresh pre-allocated shell, not the original);
  `6E` present with real WAL; ~90 recycled future segments `6F`–`C8`.
- `pg_waldump` of `6E`: after the `CHECKPOINT_ONLINE` record that `pg_control`
  points at, there is a **completed `CHECKPOINT_SHUTDOWN` record at
  `3/6E9FEE50`** (redo `3/6E9FEE50`) — the fast-shutdown checkpoint triggered
  by the pod kill ran to completion.
- dmesg: **clean** ext4 unmount at detach, clean mount at reattach — no fs
  errors, no journal complaints. One `nvme nvme2: Property Set error: 880,
  offset 0x14` (NVMe-oF controller-shutdown register write failed during the
  disconnect).

**Why this indicts the storage path:** postgres orders a shutdown checkpoint
strictly: ① flush WAL through the `CHECKPOINT_SHUTDOWN` record (fsync) →
② rewrite `pg_control` pointing at it, state `shut down` (write+fsync) →
③ unlink/recycle now-obsolete segments (`65`–`6D`). The disk shows ③
persisted, ② **reverted to its previous version** (the state-`shutting down`
write from checkpoint start), and ① partially present. A write that fsync
returned for was lost while *later* writes to other blocks survived — i.e.
lost/reordered acked write on one LBA range, not a torn suffix. The ledger
oracle showed no lost acked *transactions* (stall began at kill), but
`pg_control` is itself an fsynced write that vanished.

**Prime suspect:** the NodeUnstage NVMe disconnect racing in-flight/cached
writes in spdk-tgt — the failed CC-register shutdown write in dmesg says the
disconnect path did not cleanly quiesce the controller. Force delete is the
only drill so far that unmounts within ~1s of heavy dirty-page flushing.

**Repro/next steps (fresh cluster):** rerun 1.3 under load N times (expect
flaky — it's a race); instrument spdk-tgt flush handling (does lvol honor
NVMe FLUSH before disconnect teardown?); check NodeUnstage ordering
(umount → flush → controller shutdown → disconnect). Until root-caused,
treat **force-delete of a busy pod on v1.15.0 as data-loss-capable**.

Evidence files: `pg_controldata.txt`, `pg_wal-forensics.txt`,
`pg_control.bin`, `wal-segment-6E.bin.gz`, `dmesg-runs-aws-3.txt`,
`driver-logs.txt`, `db-verdict.txt`.

### Other findings

- **P0-a / P0-b** (trove provisioning) — see Phase 0. Not flint bugs; recorded
  for the trove backlog.
- Pre-existing orphaned NVMe session observed after kuttl create/delete churn
  (a controller stuck `connecting` for a deleted PV, 1800s ctrl-loss-tmo).
  Cleaned before drills; flagged to reproduce deliberately in the churn drill
  (1.10) to determine whether flint leaks NVMe sessions on rapid volume delete.

## Teardown

2026-07-13, on user request (pause + stop costs): `flint-chaos` ns deleted —
PV released and deleted through the driver in ~30 s **from a CrashLoopBackOff
consumer** with no finalizer hang, zero VAs left (a clean-detach datapoint in
itself). Trove project 36 deleted; all 4 EC2 terminated; spot/EBS/EIP orphan
sweep clean; kubeconfig removed. Campaign resumes on a fresh cluster at
drill 1.3 (repro of F1).
