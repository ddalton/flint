# CSI attach/detach chaos campaign — 2026-07

**Status:** PHASE 1 COMPLETE 2026-07-17 — **F8/F9 fixed and live-validated.**
Stock v1.15.0 run found F8 (amnesiac csi-node restart, drills 1.9b + 1.15)
and F9 (stale NodeUnstage kills live cross-node subsystem); fixes landed
(`f723440`: ground-truth export rehydration + NodeUnstage sole-consumer
guard) and the phase-1 rerun on `flint-driver:1.15.0-f8f9.0` is green —
headline: **1.9b self-heals in 15s and 1.15 (full DS roll) rides through
with NO pod bounce, 46s max stall, both rehydrator paths exercised on a
live cross-node attachment** (details in the rerun section below). 1.14
node-terminate deferred to campaign end; F9 guard still needs its own
dedicated drill (kubelet-dead + cross-node re-attach + revive). **Under
test:** v1.15.0 + F8/F9 fixes + `spdk-tgt:1.6.0-f5fix.1`. **Cluster:**
trove project 38 `runu` (torn down 2026-07-17 at pause; provision fresh to
resume — the trove spot bug is fixed, see Other findings). **Harness:**
`tests/chaos/` (Postgres 16 + pgbench + acked-write ledger oracle).
**Next:** phase 2 (r2) + phase 3 (RWX) via `drills/phase2.sh`/`phase3.sh`,
F9 dedicated drill, 1.14, VolumeCondition surfacing (F8 residual).

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
| 1.1 | in-container postmaster SIGKILL | 22s | 22s | PASS | runs; repro'd on runt — in-place restart, zero CSI calls |
| 1.2 | graceful pod delete | 11s | 9s | PASS | runt, F5-fixed bits; clean shutdown |
| 1.3 | force delete (`--grace-period=0 --force`) | 6s | 3s | CSI PASS / db N/A | DB corruption = expected F1 semantics (bar re-scoped to CSI hygiene); runt |
| 1.4 | cordon + delete, cross-node | 22s | 20s | PASS | runu rerun (runt verdict was contamination-invalidated, see below); clean cross-node migration |
| 1.5 | drain | 32s | 15s | PASS | runu rerun; drain-ordered detach clean |
| 1.6 | controller killed mid-attach | 16s | 13s | PASS | runu; in-flight ControllerPublish survives controller restart |
| 1.7 | controller killed mid-detach | 10s | 5s | PASS | runu |
| 1.8 | controller scaled 0 for 60s mid-migration | 98s | 66s | PASS | runu; attach parks until controller returns, no VA surgery |
| 1.9 | spdk-tgt hard kill (process only) | 49s | 41s | PASS | runu; v1.15.0 graceful recovery, io_resume=49s, restartCount stable |
| 1.9b | csi-node POD delete on pg's node | never | never | **FAIL — F8** | landmine mechanism exposed: amnesiac reconciler, exports never rebuilt, health check lies; recovery = consumer bounce (cross-node) |
| 1.10 | churn ×20 create/delete | 241s | 15s | PASS | runu; tot_gm=1 vas=1, no NVMe session leak |
| 1.11 | kubelet stop, slow path | 989s | 944s | PASS | runu; k8s eviction timing dominates (notready=48s evict=347s) |
| 1.12 | kubelet stop + oos taint | 314s | 262s | PASS | runu clean rerun (first attempt READY-contaminated by F3 disk-pressure taint, env) |
| 1.13 | ☠ EC2 stop of pg's node | — | — | PASS | teardown clean with dead backing volume; node restore needed manual EC2 start (rolesanywhere lacks ec2:StartInstances) + disk re-init |
| 1.14 | ☠ EC2 terminate of pg's node | | | DEFERRED | destroys a worker; run at campaign end with trove replacement queued |
| 1.15 | ☠ full csi-node DS roll | 1756s | 1261s | **FAIL — F8b** | landmine reproduced; **pod-bounce recovery failed same-node** (see F8 addendum); db PASS (zero lost acked writes); manual recovery 44s once dead session force-dropped |

### Phase-1 rerun on the F8/F9 fixes (2026-07-17, `flint-driver:1.15.0-f8f9.0`)

Fixes in `f723440` — **F8:** `rehydrate_exports_from_ground_truth` in the
node agent (startup + 60s monitor tick) rebuilds exports from PVs +
VolumeAttachments: loopback exports for locally-staged volumes
(registry-tracked, the 10s loss-detector owns them from there),
storage-side exports fenced to cross-node consumers, and the
remote-attach + loopback chain on consumer nodes. **F9:** the agent's
delete_nvmeof endpoint (NodeUnstage path) fails closed — skips
`nvmf_delete_subsystem` and just fences this node out whenever live
foreign controllers exist or the VA names another node. DS-only image
swap (controller paths untouched); 689 lib tests (6 new).

| # | Result | Notes |
|---|---|---|
| 1.1 | PASS 8s | in-place, zero CSI calls |
| 1.2 | PASS 6s | |
| 1.3 | expected-FAIL (F1) | WAL replay startup failure, CrashLoop — same class as runs/runt runs; CSI hygiene clean; mandatory reset applied |
| 1.4 | PASS 17s | cross-node migration |
| 1.5 | PASS 32s | drain |
| 1.6 | PASS 16s | controller killed mid-attach |
| 1.7 | PASS 10s | controller killed mid-detach |
| 1.8 | PASS 102s | controller scaled 0 for 60s |
| 1.9 | PASS 38s | tgt hard kill (SSM), io-resume path unchanged |
| **1.9b** | **PASS, 15s stall** | **F8 validated**: csi-node POD delete self-heals — `[REHYDRATE] rebuilt loopback export from ground truth` at agent startup, kernel session reconnects, zero intervention (was: wedged forever, consumer bounce required) |
| 1.10 | PASS | churn ×20, tot_gm=1 vas=1, no leaks |
| 1.11/1.12 | skipped | kubelet-death paths untouched by the fixes; both PASSED same-day on stock v1.15.0. (A first 1.11 attempt was killed by a runner timeout mid-drill; the `trap restore EXIT` restored kubelet via SSM as designed. Aftermath exposed a kubelet post-restart quirk: it never issued NodeUnstage for the orphaned mount, wedging the VA 25min until a kubelet restart — k8s-level, recorded for the backlog) |
| **1.15** | **PASS, 46s stall, NO bounce** | **F8 validated at DS-roll scale**: roll hit BOTH sides of a live cross-node attachment; aws-2 rehydrated the storage-side export, aws-1 the remote-consumer chain; I/O resumed with no pod action. A concurrent F3-class ephemeral-storage eviction of pg-0 on aws-1 was absorbed too (VA handoff 4s — vs 25min stuck that morning) |

Ledger reconciliation: zero lost acked writes across the rerun (1.3's
loss is the documented F1 force-delete semantics). F9's guard shipped in
the same image but still needs a dedicated drill (kubelet-dead +
cross-node re-attach + revived-node stale unstage).

### Verify contamination across drills (2026-07-13 batch — 1.4/1.5 verdicts invalidated)

The 1.3→1.4→1.5 batch ran back-to-back with **no harness reset**: the
inter-drill health gate (pg-0 Ready + ledger acking) passed even though 1.3's
by-design two-postmaster overlap had corrupted the DB (amcheck FAIL). Both
1.3's and 1.4's verifies reported the **identical** 93 missing acked seqs
(2477…) — and 1.5's verify then found **all 20,471 acked writes present**,
including those 93, on the same uninterrupted ledger lineage. Genuine storage
loss cannot un-lose writes; delayed visibility of the doomed postmaster's
flushed-but-orphaned commits (shared PGDATA, shutdown checkpoint racing the
replacement's recovery) can. Conclusion: 1.4's "LOST ACKED WRITES" is 1.3
residue, not a cross-node-migration bug — but the only honest verdict is a
rerun. **Rule adopted: any drill whose expected outcome includes DB corruption
(1.3, ☠ drills) is followed by a mandatory `deploy-harness.sh reset` before
the next drill's T0.**

## Phase 1u — ublk backend (2026-07-17, cluster runv, `flint-driver:1.15.0-ublk.2`)

Release gate: rerun the phase-1 checks with `blockDevice.backend=ublk` —
same-node volumes are PURE ublk (no NVMe-oF anywhere in the path), remote
volumes are the NVMe-oF + ublk HYBRID (SPDK initiator between nodes, ublk
for the kernel-facing exposure; no loopback re-export). Cluster: runv, 4×
i4i.xlarge SPOT workers (first real spot provision — trove fix 9963af2
validated live) + i4i.large on-demand CP, workers on mainline kernel
6.18.29 (AL2023 deb-extraction recipe; kmod-29 can't read .ko.zst —
decompress the module tree; force nvme/ena into the dracut initramfs or
the node bricks: AL2023's 6.1 kernel has nvme BUILT-IN so hostonly mode
omits it). spdk-tgt v26.05 sets UBLK_F_USER_RECOVERY(_REISSUE) on every
disk on this kernel.

### Bring-up findings (all fixed before the drills)

- **U1 (P1): CSI ublk mode could not mount a volume at all —
  `numQueues=8` vs 4 vCPUs.** The kernel EINVALs UBLK ADD_DEV when
  nr_hw_queues > CPU count; the chart's tuning default (8) exceeded
  i4i.xlarge's 4 vCPUs, so every `ublk_start_disk` failed. Bisected live
  (nq=1/2/4 ok at any depth, nq=8 EINVAL). Fix: agent clamps num_queues
  to host CPUs (41a3290).
- **U2: ublk ids — misdiagnosis corrected.** First read of U1's EINVAL
  blamed the legacy 20-bit volume-id hash (419736 as ublk id); in fact
  the kernel accepts ids up to ~1M and `ublks_max` (default 64) bounds
  the CONCURRENT DEVICE COUNT per node, not the id value — the live disk
  runs happily as id 419736. The allocator that came out of the
  misdiagnosis (bca965e) is kept deliberately: fresh stages get small
  sequential ids, the create endpoint is now idempotent-by-bdev, the
  ACTUAL id rides back in the response and lands in the PV annotation
  (the authority for unstage/rehydration). 64 devices/node is a real
  ublk-mode limit nvmeof mode doesn't have — document, don't fight.
- **U3 (validation of the F8 machinery): the ground-truth rehydrator
  staged the first volume.** With the stage path broken by U1, the fixed
  agent's startup rehydration pass found the attached PV and started the
  ublk disk (hash-id fallback, clamped queues) BEFORE kubelet's next
  mount retry — which then adopted it idempotently. The F8-for-ublk
  design worked on its very first live exercise.

Sanity (both data-path shapes, one PVC): same-node = pure ublk
(`/dev/ublkb419736` on the lvol bdev, ZERO nvmf subsystems); pod moved to
aws-2 = hybrid (storage-side export fenced to `node:runv-aws-2`, SPDK
initiator controller on the consumer, ublk id 0 on the `nvme_…n1` bdev);
data written same-node read back intact cross-node.

Verify oracle: step 4 is backend-aware (088cec0) — ublk mode checks the
PV's ublk disk is served on the pod's node and counts orphaned
disks/initiator controllers; there are no kernel nvme sessions to check.

### The 1.9b saga — five runs, four root causes (U4 chain)

The csi-node pod-delete drill took five runs to pass; each failure
peeled a distinct layer. Recorded in run order:

- **Run 1 (ublk.2, original preStop): FAIL.** The preStop's explicit
  `ublk_stop_disk` sweep deleted the kernel gendisks under live mounts
  on every graceful DS roll — a fresh start mints a NEW device the old
  mount cannot follow (unlike nvmeof mode, where the kernel initiator
  reconnects to the re-created export). First U4 fix: skip the sweep,
  hard-kill SPDK instead.
- **Run 2 (hard-kill preStop): FAIL — one level deeper.** spdk-tgt
  1.5.0 (pre-F5) lost the LVOL on the dirty restart (LVS reloads with
  lvols:0). **F5 is a HARD ublk-mode dependency**: every roll is now a
  dirty tgt restart by design. tgt upgraded to 1.6.0-f5fix.1.
  Corollary found cleaning up: a store damaged by a pre-F5 dirty kill
  is POISONED — even f5fix cannot load it (bs_recover replays, vbdev
  reports store-not-found, the LVS registers briefly then unregisters
  terminally). Re-init is the only remedy (was provisionally called
  F10; downgraded — clean-lineage stores reload fine, ~4s).
- **Run 4 (clean store): FAIL — the kill was fake.** The preStop's
  `spdk_kill_instance SIGKILL` is a silent no-op: spdk_tgt is the
  container's PID 1, and a pid-namespace init ignores even SIGKILL
  from inside its own namespace (the RPC returns true; nothing dies —
  verified live: tgt and device survived the "kill"). k8s's SIGTERM
  then ran the graceful fini, STOP_DEVing every disk. systemd swept
  the dead mounts (BindsTo device units) and the restarted postgres
  re-initdb'd onto the node's ROOT DISK — the harness looked healthy
  while measuring the wrong disk (caught by the write-probe; a
  PGDATA-on-ublk df gate now runs before every drill). Drill 1.9's
  pkill worked all along because SSM signals from the HOST namespace.
- **Run 5 (entrypoint trap wrapper, chart aefaea7): PASS, 18s stall,
  zero pod action.** ublk mode now runs spdk_tgt as a CHILD of a bash
  PID 1 whose TERM trap SIGKILLs it — every pod stop is an unclean tgt
  death by construction. Devices quiesce, f5fix replays the dirty
  store, the new agent logs `recovered quiesced kernel device (mount
  preserved)`, and the ledger resumes on the SAME mount. All 7 db
  checks green.

Residuals filed: U6 — a recovery-impossible quiesced device (bdev
gone) is an unmount tarpit: teardown wedges on statfs, lazy-detach
lets the workload write to the underlying root-fs dir, and namespace
deletion hangs on the residue (documented unblock recipe: rm the
non-mountpoint volume dirs via SSM + force-delete the pod). Driver
follow-up: NodeUnpublish should clear post-detach residue under
block-backed volumes itself. Also: coredump storage disabled fleet-wide
(a single spdk-tgt crash dumped 1.1GB onto an 8G root and tainted the
node with DiskPressure); kernel devices of dead malloc probes linger
quiescent until reboot (harmless).

### Leg A — same-node (pure ublk) — ALL PASS

| # | Result | Notes |
|---|---|---|
| 1.1 | PASS 8s | in-place |
| 1.2 | PASS 6s | DB pod kill, pure ublk — no NVMe-oF anywhere |
| 1.3 | PASS 22s | historically expected-FAIL (F1 WAL-replay corruption); probabilistic — do not celebrate |
| 1.4 | PASS 17s | cross-node migration (hybrid attach + F9-guarded unstage) |
| 1.5 | PASS 33s | drain |
| 1.6 | PASS 16s | controller killed mid-attach |
| 1.7 | PASS 15s | controller killed mid-detach |
| 1.8 | PASS 108s | controller scaled 0 ×60s mid-migration |
| 1.9 | PASS 43s | tgt SIGKILL (host pkill): quiesce → recover, mount preserved, agent's 10s detector |
| 1.9b | PASS 18s | csi-node POD delete — after the five-run saga (see above); `recovered quiesced kernel device (mount preserved)` |
| 1.10 | PASS | churn ×20; agent-allocated ids, no leaks |
| 1.15 | PASS 18s (rerun; first run 425s via bounce) | ☠ full DS roll under load: **in-place, zero pod action** on the seeded-detector stack — BEATS the v1.15.0 documented known-limit (roll kills single-replica mounts). An unlabeled extra data point: pg also rode through the ublk.4→ublk.5 deployment rolls untouched |

### Leg B — remote placement (NVMe-oF + ublk hybrid; storage node cordoned to pin placement)

| # | Result | Notes |
|---|---|---|
| B1 (1.2 remote) | PASS 11s | pod kill: replacement rescheduled to the STORAGE node (VA handoff, disk-locality winning); remote placement dissolves naturally |
| B2 (1.9 consumer-side) | PASS 54s | consumer tgt SIGKILL: initiator re-attach + ublk recover, mount preserved |
| B3 (storage-side tgt SIGKILL) | first run FAIL → **U7**; rerun **PASS 59s** | without reconnect tuning the initiator DROPS the bdev during the outage → SPDK bdev-event stops the ublk disk → gendisk destroyed → mounts swept → crash-loop until pod bounce. Fix: `ctrlr_loss_timeout_sec=-1, reconnect_delay_sec=2` on both attach sites (the SPDK-initiator mirror of v1.15.0 #2's kernel ctrl-loss-tmo). Rerun: consumer chain held, same mount, stall ≈ storage outage |

### U8 (found during Phase-I completion, 2026-07-17) — no fsck before mount

Drill 1.12u (kubelet stop + out-of-service taint, forced reschedule)
FAILED: postgres crash-looped with `pg_wal/...: Structure needs
cleaning` (EUCLEAN). Root cause is NOT ublk-specific — NodeStage mounts
an existing filesystem directly (`mount <dev> <path>`) with **no fsck
first**, missing the `SafeFormatAndMount` parity every production CSI
driver has. A kubelet-death + force-detach cuts writes off mid-cycle
and leaves ext4 with its error flag set; the journal replay on the next
mount is insufficient, so the fs comes back read-only-errored and the
workload cannot use it until a manual e2fsck. nvmeof mode has the SAME
gap — its earlier 1.12 PASS was luck (the lazy-umount + bounded-sync
happened to flush cleanly before the device died); the brutal ublk
force-detach hit the window reliably. Fix: `e2fsck -p` before mount for
ext-family filesystems (0/1/2 = clean/fixed/fixed-reboot → proceed; ≥4
= refuse to mount a corrupt fs so kubelet retries and the state
surfaces). xfs is exempt (log-replays on mount; fsck.xfs is a stub).
Ships in v1.16.0.

**Escalation follow-up (same day):** the actual 1.12u volume exceeded
preen — `e2fsck -fn` showed multiply-claimed blocks shared between two
`pg_wal` segments, directory entries pointing at deleted inodes, and
bitmap drift (all confined to `pg_wal`: WAL recycling was in flight at
the sever). `e2fsck -p` exits 4 on that class, so preen-only wedges the
volume in a MountDevice retry loop forever — fail-closed but
unattended-unrecoverable. Fix v2: on preen exit ≥4, escalate ONCE to
`e2fsck -fy` (the exact command an operator would run by hand); mount
if ≤2, refuse only if full repair also fails. Durability arbiter above
the fs layer is the workload's own crash recovery (postgres WAL
replay), which the drill's acked-write ledger oracle checks — fs-level
`-fy` answers can only drop data postgres never fsynced or can rebuild;
if that assumption is ever wrong the oracle fails the drill.

**Escalation validated + durability forensics (2026-07-17):** ublk.7's
first retry on the wedged volume logged the full designed sequence
(preen code 4 → `-fy` code 1 → mount), pg-0 Ready in 45s — driver-side
A/B complete. The data verdict on that volume, however, was REAL loss:
4,298 acked writes gone from the heap (mid-range hole ~18785..23082),
every one of them fsync-acked BEFORE the drill (up to 19 min earlier).
Redo stopped at the end of segment 0xAB; 0xAC was one of the
fsck-cleared deleted-inode files — postgres treats the first unreadable
WAL segment as end-of-WAL and silently discards the rest. pg_xact SLRU
damage ("could not access status of transaction" under new load) makes
the damage logical and unrepairable → volume condemned per the reset
rule. Read: U8 is a DURABILITY bug, not just availability — a fleet
that never fscks accumulates ext4 metadata divergence across dirty
recoveries until fsync-durable files (WAL!) are destroyed at the
metadata layer. The per-drill "all acked present" PASSes were true at
their T0s. Full forensics:
`tests/chaos/artifacts/1u-1.12-1784321057/fsck-escalation-forensics.md`.
The clean 1.12u verdict comes from a fresh volume with fsck-on-stage
active from first mount.

**Fix v3 — `-f` is load-bearing (same day):** the fresh-volume 1.12u
rerun (T0=1784323394) FAILED THROUGH the v2 fix: `e2fsck -p` exited 0
(journal replayed, superblock clean flag trusted, full check skipped),
mount proceeded, postgres died at runtime on `base/5/pg_internal.init`
EUCLEAN and crash-looped — container restarts don't re-stage, so a
corrupt-mounted volume never heals. A force-detach corrupts metadata
WITHOUT setting the ext4 error flag (the kernel sets it only when it
later trips over the damage), so preen's clean-flag shortcut is exactly
wrong here — and upstream `fsck -a` (SafeFormatAndMount) shares the
shortcut, so "parity" was a weaker bar than the drill demands. v3:
`e2fsck -fp` (forced full check, preen repairs) on every ext stage,
`-fy` escalation unchanged. Cost = metadata scan per stage; stages are
attach-time-rare and correctness wins.

**Corrupt-mount window is itself destructive:** v3's restage of that
volume repaired exactly the file postgres died on (`pg_internal.init` →
deleted inode, CLEARED, code 1) and mounted — but recovery then died
FATAL on a TRUNCATED `pg_xact/0000` ("read too few bytes" mid-redo),
postgres's own hint being restore-from-backup. The clog tail was
checkpoint-fsynced pre-sever; what killed it was the pre-v3 window
where postgres ran 3-6 WAL-redo attempts ON the corrupt-mounted fs,
writing pg_xact through broken metadata. Lesson: every mount of an
unchecked post-sever fs compounds damage — the fix must be in place
BEFORE the first post-sever mount, which is precisely what v3
guarantees from here on. Volume condemned (second one); the clean
1.12u verdict needs the pristine sequence sever → `-fp` → first mount.

**1.12u PASS (ublk.8, T0=1784324839):** pristine sequence on a fresh
volume with v3 active from first mount — ready 61s, max stall 16s,
attribution rescheduled aws-3→aws-2 restarts 0, db verdict FULL PASS
(ledger reconciliation zero acked loss + amcheck clean), VA consistent,
data path clean, no orphan growth. Stage log on the target node:
`fsck -fp` → "recovering journal" + preen repairs (code 1) → mount.
Single-sever damage is exactly the class `-fp` repairs unattended; the
`-fy` escalation stays as the backstop for compounded damage. U8
CLOSED: v3 (fcd4578) is the shipping behavior.

### 1.14u (EC2 terminate, 2026-07-17) — PASS with caveat

First-ever run of 1.14 (open since the nvmeof phase). Terminated pg's
node (aws-2) for real; topology had split compute from storage after
1.12u's reschedule, so the single-replica lvol (on aws-3) survived.
Data-plane teardown was clean and prompt: lvol deleted, export removed,
NFS residue cleaned, provisioner deleted the PV object. The only
residue was the VolumeAttachment for the mid-teardown rescheduled pod
on aws-3 — kube-controller-manager's attach-detach forced-detach timer
cleared it ~6 min after pod deletion (upstream pacing, not a flint
leak), which exceeded deploy-harness down's 120s PV-wait budget and
flagged a spurious "finalizer hang". Verdict PASS; harness note: the
node-loss teardown path needs a ~7 min budget.

### Verdict

ublk mode passes the full phase-1 matrix on the final stack
(`flint-driver:1.15.0-ublk.8` + `spdk-tgt:1.6.0-f5fix.1` + chart
entrypoint wrapper) with recovery times equal to or better than nvmeof
mode, and the DS-roll landmine — nvmeof mode's documented known limit —
is FIXED in ublk mode (18s in-place ride-through). Phase-I completion
(2026-07-17): dedicated F9 drill PASS (guard_hits=1, stall 41s), 1.12u
PASS after the U8 fsck wave (v1 no-fsck → v2 -fy escalation → v3
forced -f; two condemned volumes on the way — see U8), 1.14u PASS
(clean node-loss teardown, AD-timer caveat). The fsck work means
ublk.8, not ublk.5, is the release digest. **Release gate:**
the fixes are validation-tagged only; packaging must ship the f5fix
spdk-tgt (HARD ublk dependency — every roll is a dirty tgt restart by
design) and the chart wrapper together with the driver. Residuals for
the backlog: U6 teardown tarpit (NodeUnpublish residue clearing),
storage-side export rehydrate rides the 60s tick (case-(b) seeding
would cut B3's stall to ~20s), stores damaged by pre-F5 dirty kills
are terminally unloadable (re-init required — keep wipefs+init recipe
handy), lingering quiesced kernel devices for dead unattributable
bdevs clear only on reboot.

## Phase 2 — RWO, numReplicas=2 (RAID1)

### Phase 2u (ublk backend), 2026-07-17/18 — runv, v1.16.0 base + ublk.N fixes

**2.1 PASS** (remote-leg csi-node delete): 20s worst ack age, raid back
to online 2/2 via survivable reconnect — no degradation persists, no
rebuild. (First run FAILed on a checker bug: r2 controller names carry
a `_<idx>` suffix the live-PV match didn't strip.)

**2.2a — U9, FIXED (df663af + a6ee2a7):** RAID-host spdk-tgt SIGKILL
left the volume dead FOREVER: `repair_data_path` refused ublk volumes
("restage required" — a pre-USER_RECOVERY assumption) while the 10s
detector retried `ublk_recover_disk` against a raid bdev nobody was
rebuilding. Fix v1: ublk-aware repair (reassemble raid via
create_raid_from_replicas, then recover the quiesced kernel device in
place — mount survives). Fix v2: registered raid chains repair on the
10s detector tick (a registered-but-missing raid is always post-stage
loss; no in-flight-NodeStage false positive), monitor stays as backstop.
Result: **PASS, I/O resumed 31s** after SIGKILL, zero acked loss,
restarts 0. Also: stale detector registry entries (teardown racing a DS
roll) now reaped when no live PV claims the id.

**2.2b — F8-amnesia on r2, FIXED (f3b0bba):** full csi-node POD delete
on the RAID host = agent restart = empty in-memory registry → nothing
recovered (old pod's tgt serves through graceful termination ~30s, then
dead). The 60s monitor healed it at ~3.4min; fix seeds r2 raid chains
into the detector at rehydration → **PASS, actual outage 33s** (drill
timing fixed to measure from old-pod-GONE, not delete-issued).

**2.3 + 2.5 — "F10" RETRACTED: the observed loss was a harness
artifact.** The initial run reported 1,077 acked writes missing after
node-kill (2.3) + migration (2.5) with orchestrators off, first written
up as an unsynced-rejoin rollback. Re-litigation with a fixed harness
OVERTURNED it: verify-db piped `sort -n` output into `comm`, which
requires LEXICOGRAPHIC order — the merge desynchronizes whenever the
two lists aren't near-identical, and an oracle-pod replacement
mid-drill (evict_load_from raced the terminating pod) made acked.log a
mid-stream subset, fabricating sparse "missing" seqs. A direct heap
probe disproved the loss on the volumes still alive; the original
volume was destroyed before heap verification, so its number is
untrustworthy. The HONEST reproducer (orchestrators OFF, fixed comm +
relocation, independent python set-diff heap probe): 2.3 rode through
the node kill (worst ack age 1s — kubelet death never severs the data
plane; the tgt keeps serving and the leg never leaves the raid), 2.5
migrated in 18s, **MISSING=0 of 2,192 acked writes**. No divergence
vector was demonstrated in the phase-2u matrix: with ctrlr_loss_tmo=-1
legs queue rather than fail out. The code-level observation stands
(assembly without epoch history is attach-everything; the tier2
runbook's own hazard warning) — the orchestrator trio also ran the
same sequence green — but there is no live-proven loss, and the
chart's replication.orchestrators block is justified by the runbook
hazard, not by a demonstrated rollback. Harness fixes from this arc:
lex-sort comm, evict_load_from wait-for-delete, PRE_ORPHANS baseline,
r2-suffix orphan checker. The same comm artifact explains the r3-chain
db FAILs (2.7/2.2a/2.5 on flint-r3): all flipped to PASS on re-verify
(3x stable, all acked present).

**2.5 storage verdict (orthogonal to F10): PASS** — cross-node
migration onto a replica-less node (aws-5) assembled a FULLY-REMOTE
raid1 (both legs SPDK-initiator) under ublk, ready 23s, stall 17s.
**2.6 churn ×10 while serving: all cycles 6-16s**, no mount/VA leaks.

**2.4 (☠ REAL node terminate) — PASS after two driver fixes.** The
r2 headline drill EC2-terminated pg's node (which held pg + one
replica). Two gaps wedged the replacement pod forever, both fixed live:
(1) the degraded-assembly floor `total.min(2)` refused to stage with
one available replica — while a LIVE raid losing the same leg keeps
serving (ea948ed: floor is now 1; staleness is policed explicitly by
the sync-record admission); (2) SPDK v26.05 raid1 refuses a
single-base create (EINVAL, verified live), so a multi-replica volume
down to ONE in-sync leg now serves that leg DIRECT — no raid layer, r1
semantics, `flint.io/degraded-direct` PV annotation; the
consumer-blindness monitor skips annotated PVs and the controller
reaper protects direct-serve initiator bdevs like raid legs (819262c).
Validation: pg-0 Ready on the survivor path, **heap probe MISSING=0 of
2,924 acked writes** — everything acked before the terminate survived
on the remaining replica. **U11 follow-up (feature work): replica
RE-PLACEMENT** — nothing rebuilds redundancy onto a healthy node after
permanent node loss; the volume stays r1-degraded until then (the dead
spdkOperator "replacement rebuild" flow was for this and is off/dead;
catch-up heals returning legs, not vanished nodes).

Cluster note: runv-aws-5/6 added live mid-phase (manual clone of the
trove worker bootstrap: run-instances + kubeadm join + kernel 6.18.29
swap + lvstore init — no trove backend needed, ~12min for both).

## Backlog fix wave — 2026-07-18 (post-v1.17.0)

The RWO-production backlog implemented in one wave (aafe958 + ffaca67 +
drills 9d18d2c), validation on runw:

**U11 — replica re-placement (`replica_replace.rs`).** A pre-pass of
each per-volume catch-up task (same claim): a STALE, unmarked leg whose
Node object is deleted — or NotReady past
`FLINT_REPLICA_REPLACE_AFTER_SECS` (600) — gets its identity swapped to
the max-free Ready node hosting no other leg. PV `volumeAttributes` are
API-immutable, so the swapped list lives in a new
`flint.csi.storage.io/replicas-override` annotation that every reader
prefers (`raw_replicas_json` funnel; NodeStage re-reads the PV, so a
restage picks up swaps with zero VA surgery). The swap writes override
+ sync record + node-label change in ONE rv-guarded metadata patch —
`reconcile_membership` defaults unknown identities to in_sync, so the
new uuid must enter STALE atomically with the identity. From there the
EXISTING machinery rebuilds: §9-5 thin-aware full build
(`revert_head_to_empty` recreates the placeholder head sized to the
source) → standby → chase → hot-rejoin admits into the live degraded
raid. Design consequence worth stating: when a node dies under a
running consumer the raid object survives in degraded state, so
redundancy restores LIVE with no pod disruption; only the
restage-into-direct-serve path (2.4 shape) waits for the next stage —
live direct→raid conversion is the remaining follow-up. Guards: needs
an in_sync source + epoch history, one swap per volume/tick, RWX
skipped (the synthetic backing PV mirrors identity attrs — swap
choreography there is future work), placeholder lvol unwound on patch
conflict. The dead `autoRebuild` SC parameter is now the per-volume
opt-out (echoed to volumeAttributes at CreateVolume; "false" = no
re-placement, catch-up-on-return unaffected).

**nvmeof detector-tick repair parity.** `reconcile_exports_if_lost`
(10s tick) now mirrors the ublk detector's fast path: a REGISTERED
export whose backing RAID bdev is missing is always a post-stage loss,
so the tick drives `repair_data_path` directly instead of retrying
add_ns forever while the 60s 3-strike monitor ambles toward the same
repair. This was the whole ublk-vs-nvmeof recovery gap on RAID-host
kills (31-33s vs 139-208s in the phase-2 matrix).

**F11 — store-loss detection + guarded self-heal**
(`check_store_health`, 60s monitor). Ground truth (PV replica
identities naming this node's lvstores, override-aware) vs live
lvstores, 3 consecutive strikes; RPC failure never counts (tgt-down ≠
store-lost). Self-heal (`FLINT_STORE_REINIT`, default on): when every
expecting volume is multi-replica, re-init the store in place — the
live-validated F11 remediation; `identity::lvs_name` derives from
node+PCI so the re-created store carries the exact expected name, and
the catch-up full build recreates the heads. ANY single-replica
expectation blocks re-init (events only): never destroy the only,
possibly-recoverable copy. Without the self-heal a live node with a
dead store was permanently stuck — `catchup_stale` reads its failing
lvol-list as "not returned yet" and U11 correctly refuses (node is
Ready).

**Degraded visibility.** Stage-time PV events `DegradedDirectServe` /
`DegradedAssembly`; orchestrator events `ReplicaReplaced` /
`ReplicaReplacementBlocked` / `ReplicaStoreLost` /
`ReplicaStoreReinitialized`. The retracted-F10 wording in the chart's
orchestrators comment was corrected in passing.

New drills: **2.8** (☠ U11 live re-placement — terminate + delete the
remote-leg node, assert override swap → in_sync with acks fresh
throughout) and **2.9** (F11 — destroy the remote leg's lvstore via
RPC, assert 3-strike detection → in-place re-init → in_sync).

Cluster: **runw** (trove project 40) — 4× i4i.xlarge spot storage
workers (kernel 6.18.29) + cordoned builder + on-demand CP. The trove
disk-init gap reproduced AGAIN (zero lvstores after provision) —
initialized via the agent's `/api/disks/initialize_blobstore` on the
non-system NVMe and VERIFIED before any drill, per the standing gate.

_Validation results: TBD (this session)._

## Phase 3 — RWX (NFS)

**F12 (P1, found at harness bring-up, FIXED 0940c44): no ownership
round-trip over RWX.** The first-ever postgres-on-RWX deploy failed
before any drill ran: initdb's bootstrap FATALs with "data directory
has wrong ownership". Three stacked causes: the CSI NFS mount
negotiated AUTH_NULL (no `sec=` option; SECINFO lists AUTH_NONE first)
so no uid ever reached the server; files were created by the server
process → owned by root; and SETATTR OWNER was decoded-but-ignored by
design, so the image entrypoint's chown was a silent no-op — while
GETATTR truthfully reported the backing uid (root), which is exactly
what postgres checks. Every ownership-sensitive workload (databases,
anything running non-root with a 0700 data dir) was structurally unable
to run on flint RWX. Fix: mount `sec=sys`; thread AUTH_SYS (uid,gid)
per-COMPOUND; stamp creator identity on OPEN-create/CREATE (client
permission checks compare mode vs st_uid); honor numeric owner SETATTR
via chown on the backing fs (non-numeric → BADOWNER; no idmapping).
Validated by the harness itself: pg-0 Ready on the fixed image where
it crashlooped before.

**F13 (P1, found at harness init, FIXED ee60ba2): second-granularity
change attribute corrupts rapid-write workloads.** With F12 fixed,
initdb ran — and `pgbench -i -s 200` then died mid-COPY with
postgres's "unexpected data beyond EOF in block 0" (its
buggy-kernel/NFS-incoherence signature). The fattr4 CHANGE attribute
was ctime in WHOLE SECONDS: every write inside the same second carried
the same change value, and change is the kernel client's
cache-ordering key — with ties, an out-of-order GETATTR reply carrying
a stale (shorter) size was accepted into the inode cache, and postgres
read past its own writes. Fix: both real-file encoders compose ctime
sec·1e9+nsec (knfsd's no-i_version behavior; ctime so chmod/chown also
invalidate). Together F12+F13 mean flint RWX had never actually been
exercised by an ownership-sensitive, write-intensive real application
— the cutover-chain and kuttl validations of June ran shell/fio-class
consumers that never noticed either. This is precisely what the
pg-oracle harness exists to catch.

**F14 (P1, found on the F13-fixed image, FIXED 003375a): ctime ties
still lose the cache race — server now keeps a per-file monotonic
change counter.** With ns-granularity change attrs the client
invalidates properly — and the very next harness attempt showed the
residual: postgres shut itself down with "lock file postmaster.pid
contains wrong PID: 0" (a re-read of its own just-written lock file
returned the CREATE-time size-0 view). ext4 stamps ctime from the
coarse clock (~1 jiffy), so create+first-write — or two COPY extends —
inside one tick carry IDENTICAL change values, and an out-of-order
GETATTR reply is indistinguishable from fresh. Fix, in TWO halves — the counter alone did not stop it: (1)
`change_counter` module (userspace i_version): every mutating op bumps
a per-(dev,ino) counter floored by post-mutation ctime ns (files AND
affected parent dirs on create/remove/rename/link); GETATTR reports
max(counter, floor); across restarts it degrades to exactly
knfsd-without-i_version. (2) `fattr4_change_attr_type` (attr 79) =
MONOTONIC_INCR, advertised by all three GETATTR encoders with
supported_attrs always 3 bitmap words — the kernel client only ORDERS
attribute replies by change value when the server declares the type;
undefined means any differing change is applied newest-received,
stale size included, no matter how monotonic the values are. The
capabilities GETATTR lands on the pseudo-root filehandle, so that
encoder's arm is the one that reaches the client. The F12→F13→F14
chain is one lesson three layers deep: RWX had never been exercised
by a real database, and each fix peeled the next latency-of-truth
defect into view.

**F15 (P0, THE phase-3 corruption root cause, FIXED f29ba62):
NFSv4.2 ALLOCATE was a fake-OK stub.** F13/F14 were necessary but the
corruption persisted; a standalone rig (privileged pod, own NFS
mounts, pgbench -i, <90s per cycle) plus server-side per-op debug
capture nailed it: PG16's bulk relation extend calls posix_fallocate
→ the client sends ALLOCATE → `handle_allocate` returned Ok WITHOUT
ALLOCATING ("TODO: Integrate with SPDK backend"). The client extends
its cached i_size on the fake OK, postgres fills buffers it believes
are backed, the file stays size 0 server-side (the op capture shows 8
GETATTRs size=0 and NOT ONE WRITE reaching the failing relation), and
the next server-refreshed size check collapses postgres's world —
"unexpected data beyond EOF". Control: the identical workload against
knfsd on the same kernel/client/mount-opts passes. The audit found
FIVE fake-OK 4.2 stubs; fixed: ALLOCATE (real fallocate + change-attr
bump), DEALLOCATE (real PUNCH_HOLE|KEEP_SIZE — the no-op left
unpunched holes reading stale data), SEEK (real SEEK_DATA/SEEK_HOLE —
the stub truncated sparse-aware readers), READ_PLUS (NOTSUPP instead
of "every file is empty" — client falls back to READ), IO_ADVISE kept
advisory-Ok. Lesson for the backlog: audit every op the dispatcher
accepts for silent-success stubs — a protocol server that says OK
must have DONE it.

Fleet note: runw-aws-2 was SPOT-RECLAIMED mid-bring-up
(`instance-terminated-no-capacity`, the campaign's third real reclaim)
— no data impact (the RWX backing volume lived on aws-3); replacement
runw-aws-6 added via the validated manual clone recipe (~12min:
SSM+IMDS userdata fetch, WG stripped, kernel swap, lvstore init
verified).

**F16 (P2 infra event, UNREPRODUCED — needs a dedicated drill):
aws-4's blobstore refused to load after harness teardown + DS roll.**
On the u11.5 rollout (zero PVs, zero attachments), aws-4's spdk-tgt
re-examine failed: `blob_parse: Blobid (0x100000000) doesn't match
what's in metadata (0x100000005)` → super blob unreadable → lvstore
not found. aws-3/aws-6 re-examined cleanly through the identical roll.
Timing: the mass lvol deletion of harness teardown completed minutes
before the tgt SIGTERM — suspicion is a shutdown racing still-dirty
blobstore metadata from the delete burst. Evidence captured
(aws4-blobstore-corrupt-tgt.log, job tmp); disk re-initialized via the
agent API (nothing was lost — the store was empty by then). Note the
F11 detector correctly did NOT auto-reinit: zero volumes expected that
store, so the auto-heal gate (all expecting volumes multi-replica)
never opened. Backlog: a teardown-churn + immediate-kill drill against
a populated store, and a look at whether spdk_tgt shutdown waits for
blobstore md_sync.

**F17 (P0, RWX-blocking, FIXED e60c2fb): path-hash filehandles rebind
across rename-over — kernel client fileid-change livelock.** With F15
fixed, initdb + CREATE DATABASE passed for the first time… and then
every postgres connection took 25s+ (readiness probe's 1s exec timeout
→ pg-0 never Ready → pg service endpointless → pg-init spun silently).
Chain, each step measured: `pg_isready` via UNIX SOCKET 25s while NFS
reads ran at 1.1GB/s → backends in D-state on the NFS mount → server
idle ("Waiting for RPC") while the client wouldn't send → client stuck
in a TEST_STATEID loop, same stateid answered "not found" every ~5s
cycle → `dmesg`: **`NFS: server 10.104.18.34 error: fileid changed`**.
Root cause: v1 handles are `hash(path)+path` — a filehandle named a
NAME, not a file. Postgres rename-overs (`pg_internal.init` et al.)
made an outstanding handle silently resolve to the NEW file at that
path; same handle bytes, different fileid; the client's recovery can
never converge because the aliasing is structural. Fix: v3 handles
embed the mint-time inode ([3][inst][hash][ino][len][path], ino folded
into the hash), lstat verification at mint-from-cache and resolve
answers STALE for replaced/removed objects, and `note_fs_rename` also
purges the clobbered destination subtree. v1 still accepted (legacy
semantics) so pre-upgrade handles survive restarts;
`parse_path_lenient` (DS striped pins) reads both layouts. Residuals:
inode reuse at the same path is undetectable from userspace; v2
long-path handles keep rename-transparent semantics by design; opens
anchored via the stateid fd-cache keep serving the original inode
(POSIX unlink-open semantics), path-resolved ops on dead generations
go STALE — knfsd would serve the old inode for those too, a semantic
gap we accept and document. Second lesson of the phase: F12→F15 were
all *server tells the client a comforting lie* bugs; F17 is the same
disease in the identity layer.

**F18 (P1, found by drill 3.1's verify layer): dead client connection
pins the NFS server into a CPU-burning spin.** After 3.1's clean
cross-node migration, the server sat at ~60% CPU with 83% SYSTEM time
while serving ~25 ops/s; live-client READ rtt averaged 1.75s (client
mountstats), amcheck timed out, readiness flapped. Diagnosis chain:
one socket in CLOSE_WAIT + exactly two of the original tokio workers
pegged (epoll_pwait hot loop; replacement workers spawned around
them). Cause: the dispatcher's back-channel registry holds a STRONG
`Arc<BackChannelWriter>`; when the migrated-away client's connection
closed, the read loop exited cleanly but the registry Arc kept the
write half — and therefore the socket fd — alive forever: permanent
CLOSE_WAIT whose HUP readiness the async driver re-polls in a tight
loop. Fix: the connection guard now purges this connection's writer
from the registry on every exit path (same fix applied to the MDS
server loop, which shares the pattern). Bonus finds in the same pass:
two per-RPC `info!` lines (the only thing hotter than the spin in the
profile) demoted to debug. Lesson: every strong registry needs an
owner responsible for removal — the Weak-based conn-binding table two
fields down had it right, and its doc comment even warned about this.

**F17b/F19/F20 (P1 chain, each unmasked by the previous fix): the
recovery-churn trilogy.** With F17's honest STALE answers, the
remaining symptom was periodic multi-second connect stalls (db verdict
isready/amcheck/write-probe failures) — a low-grade TEST_STATEID cycle
that never converged. Three server defects stacked underneath:
(1) **F17b (7a1144f)**: STALE for a renamed-over file the client still
holds OPEN triggers a recovery cycle per rename (~1/s under postgres).
knfsd semantics implemented instead: READ/WRITE fall back to the
stateid's cached open fd, and GETATTR — fh-only — reaches the io
handler's fd cache via a shared OpenFileView and answers by fstat with
the ORIGINAL fileid; the file keeps serving until its last CLOSE, then
STALE. (2) **F19 (2e71e1e)**: validate_for_read rejected the RFC 8881
§8.2.2 seqid-0 "current stateid" form that validate() already
accepted; the client retries seqid-0 READs in a tight BadStateId loop
(~2k/s observed — wedged pg bring-up entirely once F17b removed the
masking churn). (3) **F20 (92e1a67)**: FREE_STATEID — the disposal
half of the recovery cycle — answered BadStateId for
already-forgotten stateids and LocksHeld for revoked opens, so the
client could never retire dead state and re-tested it every cycle,
forever. Unknown → Ok, revoked → dropped+Ok, live opens keep
LocksHeld (pynfs CSID9). Meta-lesson, now three-for-three in this
phase: protocol edges that pynfs/simple workloads never exercise
(rename-over-open, seqid-0 forms, recovery-path disposal) are exactly
what a real database client leans on hardest.

**F22 (P0, THE bulk-load wedge root cause — client-side kernel
poisoning by orphaned NFS mounts).** The signature that survived
F17b/F19/F20: ~5-10min into sustained writes, live-mount I/O freezes
in bursts (writes 1s+ rtt, ZERO retransmissions, server provably
idle), dirty-throttling blocks all writers, a lease miss sweeps the
client's state and CLAIM_PREVIOUS dead-ends it. Root cause chain,
proven by kernel ftrace (nfs4 tracepoints) on the client node: (1)
unhappy-path RWX teardowns leave the kubelet NFS mount behind; lazy
unmount (the obvious cleanup!) detaches it but leaves the kernel
nfs_client ALIVE, pinned by dirty pages (observed: use-count 7,
invisible in /proc/mounts); (2) the flint-nfs Service is gone (or
leaked endpoint-less) → the orphan's ClusterIP BLACKHOLES under
Cilium (drop, no RST) — and the poisoned transport eventually stops
emitting wire traffic entirely (tcpdump: zero packets) while
CHECK_LEASE cycles ETIMEDOUT every ~10.7s forever; (3) those cycles
run on the node's SHARED SUNRPC/nfs workqueues, freezing every live
NFS mount on the node in sympathy — ftrace shows the healthy
session's traffic (63/63 slots, µs completions) halting in lockstep
~49s at a time. Remediations shipped: (a) NFS-RECONCILER inverse
sweep — NFS infra (service/pod/companions) whose PV no longer exists
is deleted every 30s tick (a leaked endpoint-less service is the
blackhole half); (b) node-agent 60s sweeper force-unmounts
(MNT_FORCE, NEVER lazy) csi-scoped NFS mounts whose server is no
live flint-nfs Service. Once a node is poisoned only a reboot clears
it (tombstone-service RSTs can't reach a transport that no longer
dials). Also: binaries now built with frame pointers (host-level
gdb/perf work in production), and the diagnosis burned two red
herrings worth recording — verbose per-op logging is itself a
throughput collapse at bulk rates (hex-dump formatting), and a
100ms-uniform op latency under verbose was the logging tax, not a
code loop.

**F23 (P1 — filehandles must follow the file across rename-AWAY).**
With F22's kernel poisoning cured, a fresh-kernel bring-up still
wedged: 7 postgres backends in `rpc_wait_bit_killable`, the client
transport with NO TCP connection and no reconnect attempts (network
verified healthy end-to-end), preceded by server ESTALE bursts ×4 on
`pg_internal.init.<pid>` names. Postgres writes its relcache init
file as write-temp-then-`rename(temp, final)`; the v3 inode-pinned
handles (F17) correctly stale the REPLACED destination, but the
handle held on the TEMP name also staled the moment the temp path was
renamed away — even though its inode is alive and well at the new
name. RFC 8881 filehandles name the FILE, not the path; a rename must
not stale anything. The 6.18 client answers a burst of ESTALEs on
dirty-page writeback by wedging its transport (it neither errors the
pages nor reconnects — arguably a client bug, but one we must never
trigger). Fix: `rename_aliases` table in the filehandle manager —
`note_fs_rename` records old→new (ino-verified per hop, chains
collapsed at insert, cap 8192 / 8 hops), and stale resolution follows
the alias when the pinned ino matches at the destination. Unit-tested
(rename_away_handle_follows_the_file) alongside the F17 stale
semantics, which are unchanged for genuine replace/remove.

**F24 (P0 — DashMap shard self-deadlock in F17c fd seeding freezes
the whole NFS server).** The u11.15 bring-up (all prior fixes in)
wedged differently: pg's shutdown checkpoint hung mid-bring-up,
client xprt showed 11 outstanding RPCs / idle 373s / zero
retransmits, the TCP connection ESTABLISHED with 2488 bytes unread in
the server's rx_queue — and ALL server runtime threads parked in
futex (gdb via SSM, frame-pointer builds paying off: worker 1 in
`handle_open→DashMap::_insert`, worker 2 in
`dispatch_operation→DashMap::_remove`). Root cause: `seed_open_fd`
(F17c) scanned `fd_cache` for an existing fd of the same path as an
`if let` scrutinee. Scrutinee temporaries live to the end of the
block, so the DashMap `Iter` — holding the matched shard's READ guard
— was still alive during the `insert` inside the block; when the new
stateid hashed to that same shard the write acquisition queued behind
our own read guard forever. One shard permanently locked; both tokio
workers (2-worker runtime) soon blocked on it; epoll unattended;
server frozen while the socket stays ESTABLISHED — so the client
never reconnects, it just waits. Postgres makes the collision
near-certain: every backend OPENs `pg_internal.init`, hammering the
shared-path seeding branch (P≈1/shards per OPEN). Fix: bind the scan
result through a standalone `let` (guard drops before insert);
regression test seeds 512 stateids of one path under a watchdog,
verified to deadlock on the old code. Audited every other
`if let`-over-guard site in the v4 tree (READ/WRITE cache lookups,
COMMIT path-scan, filehandle caches) — all already drop guards via
`let` statements or explicit scopes before mutating. Recovery note:
deleting the frozen pod un-wedges the client cleanly — the recreated
server loads persisted state, the 11 queued RPCs complete, and
postgres carries on (live proof of the persistence/recovery path
under mid-workload server death).

**F24 follow-through — the class, not the bug (u11.17).** The fd
cache moved behind `FdCache` (fd_cache.rs): private maps, guard-free
API (owned clones only), and a `by_path` index that turns every
by-path consumer (OPEN seeding, COMMIT fsync reuse, F17b fallbacks)
from an O(n) scan into a point lookup — the scan was both the guard
holder AND a latent perf cliff under postgres's
many-backends-open-one-file pattern. Mechanized discipline per the
identity.rs precedent: a grep-lint test
(`no_iter_guards_in_scrutinees`) fails the build on any if/while-let
scrutinee iterating a map in the nfs/pnfs trees (it immediately
caught two benign Vec sites in kerberos.rs — annotated), and
clippy.toml denies holding any dashmap guard type across `.await`.
Notably the DS-side fd cache (pnfs/ds/io.rs) had already dodged this
exact trap with a hand-written comment — knowledge that never
transferred to the NFS side, which is the case for mechanizing it.

**Bring-up gates — two GREEN runs (2026-07-19), with a hygiene
correction.** Two consecutive fully clean RWX bring-ups: pg-0 Ready,
`pgbench -i -s 200` over NFS, witness up, ledger acking; quiescence
textbook (xprt sends==recvs, zero outstanding, idle 2s, ZERO STALE
lines — F23 confirmed live, no recovery-op churn). CORRECTION: these
were first attributed to u11.16/u11.17, but both helm rolls had
silently failed (wrong release name — `flint`, actual `flint-csi` —
with helm's stderr piped into a grep; AND this session's images were
pushed as `spdk-csi-driver:*` while the chart pulls `flint-driver:*`).
Both green gates ran on **u11.15** — which has F22+F23 but NOT F24.
So there is no bring-up A/B for F24: u11.15 clears bring-up when the
shard dice roll right (the deadlock needs a same-shard hash between a
seeded stateid and the matched entry, ~P(1/shards) per shared-path
OPEN — the first u11.15 run hit it during initdb's shutdown
checkpoint; the next two runs didn't). The F24 root cause needs no
A/B — the gdb capture (both workers futex-parked in
handle_open→_insert / dispatch→_remove with the shard-guard chain
visible) is direct evidence. Hygiene rules adopted: never filter helm
output in roll scripts, and every roll ends with a pod-image
assertion before the gate counts. Residual observation (P3,
release-gate item): 64× `CLOSE: Invalid stateid: StateId not found`
+1 WRITE across bring-up (~3/6717 client-visible) — suspected retried
CLOSE missing a session reply-cache path; client already freed the
state, no recovery churn; investigate with pynfs at release.

**F25 (P2, teardown robustness — one wedged teardown, four defects).**
Tearing down the harness while the u11.15 nfs server sat in the F24
freeze exposed a chain of teardown-path weaknesses, each individually
survivable, jointly a 40-minute tarpit:
(1) **kubelet skips NodeUnstage after pod force-delete** — TWICE in
one teardown (the pg pod's RWX mount, then the nfs pod's companion):
`node.status.volumesInUse` never clears, the A/D controller never
even initiates detach (no deletionTimestamp on the VA — this is NOT
the 6-min force-detach case, which only fires for volumes absent from
volumesInUse). Unstick: restart kubelet (phase-1 recipe, confirmed
again).
(2) **flint DeleteVolume is not idempotent-fast**: each attempt runs
serial all-node disk scans (5 nodes incl. CP + cordoned builder)
which alone exceed the csi-provisioner sidecar's 10s gRPC deadline —
11+ DeadlineExceeded retries against a volume whose lvol was ALREADY
gone. Fix direction: fast-path return when the lvol/infra are absent;
bound or parallelize the scans; respect the gRPC context so
abandoned work doesn't pile up.
(3) **"Pod delete issued" that never landed**: the controller logged
the nfs pod delete but the pod never got a deletionTimestamp (still
`Error`, 55m old, 8 min later) — needed a manual force-delete.
Suspect silent error swallowing in delete_nfs_server_pod.
(4) **MNT_FORCE clears the mount, not the kernel client**: after the
forced unmount succeeded, /proc/fs/nfsfs/servers still showed the
nfs_client at USE=8 pinned to the (deleted) service ClusterIP —
the F22 poisoning precondition with ZERO visible mounts. F22's
"never lazy-unmount" rule is necessary but not sufficient: any
forced unmount that aborts a frozen-server window can leave the
pinned client. Reboot remains the only cure (aws-4 rebooted).
Ops sequence that unstuck everything, in order: kubelet restart →
controller bounce (reset provisioner backoff) → manual nfs pod
force-delete → second kubelet restart → verify lvol/infra gone →
manual PV delete → node reboot.

### Drill 3.1 (graceful cross-node migration) — 3 attempts FAIL on
the db write-probe ONLY; F26 opened. **RESOLVED: attempt 6 on u12.3
PASSED all 7 checks (2026-07-20 — see "Drill 3.1 PASS" analysis in
the C6 section below)**

Mechanics PASS every time (u11.17 attempt: Ready 32s, max ledger
stall 27s, exactly one nfs pod with same uid throughout, witness
clean, VAs consistent, data path clean, no orphaned mounts, no driver
errors, ledger 13/13 acked present, pg_amcheck clean). The FAIL is
the 2/7 writability probe: post-migration INSERTs time out.

**F26 (P1, OPEN — post-migration NFS server degradation to
~50–200ms/op).** Evidence chain on the u11.17 attempt:
- Client transport HEALTHY: sends==recvs (zero outstanding), queue 0,
  no retrans, no recovery-op churn (TEST/FREE_STATEID +0). OPENs do
  complete — at ~2 per 12s. Not a wedge: a crawl.
- Per-op latency uniform: WRITE 292ms / CLOSE 271ms / GETATTR 218ms
  (rtt≈exec, so all server-side). tcpdump on the server node shows
  request-in → reply-out gaps of 50–200ms, strictly serialized,
  ~7–20 ops/s total across both clients (pg + witness).
- Exonerated: backing device (dsync 0.3–0.8ms in-pod), SQLite state
  db (148KB + 4MB WAL, no bloat), CPU throttling (11 periods),
  Nagle (nodelay set on accept), F24-freeze (runtime healthy, epoll
  driver live), network path (fresh server on same topology is fast).
- Suspicious: pod cgroup shows 84% SYSTEM-time CPU; hot threads
  caught in allocator page-alloc hooks; gdb stack samples show
  crossbeam channel churn (tracing-appender queue) and a
  per-compound `Vec::clone` inside `dispatch_compound_inner`.
- DECISIVE split: deleting the nfs pod (state reloads from SQLite:
  2 clients, 2 sessions, 208 stateids) with pg STILL cross-node →
  0.3–0.6ms/op instantly. The degradation is ACCUMULATED IN-PROCESS
  STATE, not topology; trigger window is around the old connection's
  death at migration (F18-adjacent aftermath?).
- Related suspect, upgraded from P3: `CLOSE: Invalid stateid:
  StateId not found` runs at ~7% of CLOSEs continuously (41 per
  2min even on the fresh, fast server) — hypothesis: pg's
  rename-over re-mints the fh (F17), and the client's eventual
  CLOSE presents state the server has re-keyed/dropped → possible
  per-CLOSE leak feeding the degradation, or an independent
  state-machine discrepancy. Needs code-level root-cause either way.
Repro recipe: RWX harness up (fast) → drill 3.1 migration →
writability probe times out; restart nfs pod → instantly fast.
Next steps written down in the session plan: rerun 3.1 with a
latency monitor armed at the migration moment, thread census +
per-op timing on the degraded instance BEFORE restarting it, then
code-audit the connection-death path (per-compound Vec::clone, the
tracing-appender channel, dispatch serialization) and the CLOSE
not-found key mismatch.

Drills 3.1b–3.9: NOT RUN (session paused mid-investigation; cluster
torn down — resume on a fresh cluster with the F26 repro recipe).

**F26 root cause CONFIRMED by code audit (2026-07-19, static —
cluster already torn down).** Not the dispatcher/SQLite/back-channel.
`note_fs_rename` (filehandle.rs:687, called from RENAME
fileops.rs:3486) and `note_fs_remove` (filehandle.rs:762, from REMOVE
fileops.rs:3304) do a full `.keys()`/`.iter()` scan of the filehandle
caches — O(N) iteration + O(N) `PathBuf` allocation — while holding
**write** locks that every filehandle-resolving op (GETATTR/READ/
WRITE/CLOSE/LOOKUP, both connections) takes as **read**. `path_to_
handle` grows unboundedly (one entry per distinct path ever handled;
in-memory only, never persisted, pruned only per-subtree), so each
rename holds the global lock longer and allocates more as the run
proceeds — postgres renames constantly. This explains every live
observation: uniform cross-connection latency (single shared RwLock;
writer blocks all readers), growth over runtime (N climbs), 84 %
system time + page-alloc churn (the O(N) `.cloned().collect()`), and
"fresh pod instantly fast" (the cache is not persisted — `attach_
backend` reloads only the v2 id↔path table — so a restart resets
N→0). The `Vec::clone` the gdb samples flagged in
`dispatch_compound_inner` is a red herring: the bounded reply-cache
measurement clone, constant per op. Key architectural finding:
`path_to_handle`/`handle_to_path` are **pure performance caches** —
v3 handles are deterministic (`SHA256(path‖instance‖ino)`), self-
describing (path embedded, recovered by `parse_handle`), and self-
verifying (inode re-checked at resolve) — so their eviction scans are
defensive, not required for correctness. Only the v2 id↔path table
(long paths) and `rename_aliases` (F23) are authoritative.

**Re-architecture design → [`f26-filehandle-cache-redesign.md`](f26-filehandle-cache-redesign.md).**
Recommends: (1) delete `handle_to_path` (v1/v3 self-describe;
`parse_handle` needs no map) — removes a global lock from the hot
read path; (2) make `path_to_handle` a bounded sharded cache with
O(1) point-eviction instead of the subtree scan (this is the fix that
kills F26); (3) back the v2 table with a `BTreeMap` for O(log M + k)
subtree re-key; (4) reverse-index `rename_aliases` for O(1) chain-
collapse. Net: hot read path takes no global lock for v1/v3;
rename/remove drop from O(N)-under-lock to O(1)+O(log M); memory
bounded. Includes a perf-regression test (50 k entries, time 1 k leaf
renames under a p99 budget) that would fail on today's O(N) code —
the mechanized guard for this class, per the F24 lint precedent.
Design is incremental (steps 1–2 alone resolve the degradation) and
preserves F17/F23/v2-persistence/STALE-vs-BADHANDLE invariants.
**Literature review added (§11–§13 of that doc): the current
path-based handle design is itself the problem.** Production userspace
NFS servers (Linux knfsd, NFS-Ganesha FSAL_VFS) don't put paths in
handles — they encode the kernel's inode+generation handle via
`name_to_handle_at(2)`/`open_by_handle_at(2)`, which is rename-stable
by construction (F23 free) and stales replaced files via the
generation number (F17 free), deleting the entire cache/alias/scan
machinery rather than optimizing it (F26 cannot occur). flint's export
is ext4 (supports it) and the handle fits the 128B budget; the one
gate is `CAP_DAC_READ_SEARCH` in the (already-privileged) NFS pod.
**Decision (2026-07-19): go straight to the inode-handle architecture
(§12); do not build the interim path-based fix.** It is a
net-negative diff that retires the design smell behind the whole
F17/F23/F24/F26 family, and there is no production fire forcing an
interim patch (cluster torn down). Plan: (1) a ~1h capability spike —
`name_to_handle_at`/`open_by_handle_at` inside the real flint-nfs pod
securityContext (the sole hard dependency; Ganesha flags it as tricky
in containers); (2) implement mint/resolve against the ext4 export,
fds into the existing FdCache; (3) re-validate the restart/reclaim
path (kernel handles survive restart — inverts today's
instance_id STALE-on-restart) with pynfs + a phase-3 drill re-run.
Fallback ONLY if the cap is ungrantable: per-directory generation
counters + lock-free reads (Linux dcache RCU / SOSP'15), not
point-eviction.

**F27 (P2, BACKLOG — NFSv4 state persistence: throughput ceiling +
put/delete ordering bug).** Surfaced while root-causing F26
(exonerated as F26's cause but real on its own); mechanism corrected
by the 2026-07-19 design review. The NFS server persists volatile
state (clients, sessions, **stateids**) through a single
`Arc<Mutex<Connection>>` SQLite handle (`state_backend/sqlite.rs:87`).
Persists are **not** synchronous on the op path — every OPEN/CLOSE
fires a detached `spawn_persist` task (`state_backend/mod.rs:62`) and
returns immediately. The real ceiling is three-fold: (i) all persist
tasks serialize on the one connection mutex, and production opens
`synchronous=FULL` (`server_v4.rs:118`) — one fsync per row, serially;
(ii) each queued persist parks a `spawn_blocking` slot while waiting
on that mutex, so a burst can exhaust tokio's blocking pool and starve
unrelated blocking work (an indirect hot-path coupling); (iii) the
backlog is unbounded, so under load persisted state lags memory
arbitrarily far behind — the failover loss window silently widens. On
the fresh u11.17 pod the reload was **208 stateids** — a row per open,
one lock, one fsync at a time.

**The same code has a live ordering bug (correctness, arguably P1,
exists today).** Stateid put and delete are independent unordered
tasks (`stateid.rs:445-460`); `put_stateid` is INSERT OR REPLACE and
`delete_stateid` deletes by `other` (`sqlite.rs:550/602`). An
OPEN→CLOSE in quick succession can execute delete-then-put: the late
put **resurrects a closed stateid** in the DB, and after a failover
`load_records` restores it — for a lock stateid that is a phantom
persisted lock that can block another client's conflicting lock.
Out-of-order puts likewise persist a stale seqid. Clients already
solved exactly this with an ordered mpsc worker (`client.rs:412-421`);
stateids never got the same treatment.

**How other userspace NFS servers avoid this — persist almost
nothing, rebuild via grace+reclaim.** Both mainstream implementations
persist only a *small per-client recovery record*, never per-stateid:

- **Linux knfsd** (`nfsdcltrack`/`nfsdcld`): "the server must track a
  small amount of **per-client** information on stable storage" — one
  row per client (`nfs_client_id4` + boot epoch + `reclaim_complete`
  timestamp), written on client create/confirm and RECLAIM_COMPLETE,
  **not** on OPEN/CLOSE. (Notably it *also* uses SQLite — so SQLite
  isn't the problem; the per-op, single-mutex usage is.)
- **NFS-Ganesha**: pluggable recovery backends (`fs`, `fs_ng`,
  `rados_kv`, `rados_ng`, `rados_cluster`) store **client** recovery
  records only. Ephemeral state (opens/locks/delegations/layouts) is
  *not* persisted — it is rebuilt by clients after a restart.

The volatile per-stateid state is reconstructed by the **NFSv4
grace-period + reclaim protocol**: on restart the server enters a
grace period, bars new state, and clients re-establish their opens/
locks via CLAIM_PREVIOUS + RECLAIM_COMPLETE. The server only needed
the client list (and an epoch) to police reclaims safely. For HA /
a server that moves nodes (flint's exact case), the recovery DB lives
on shared, **epoch-tagged** storage (Ganesha's RADOS objects) so the
failover instance enforces a coordinated grace period; `rados_ng`/
`rados_cluster` additionally handle crash-*during*-grace (a hole in
the simpler `rados_kv`).

**Why flint diverges, and the fix (design settled 2026-07-19).** flint
deliberately persists *full* stateid state and reloads it so a
rescheduled NFS pod resumes exactly where the old one stopped and the
client's in-flight RPCs just complete — **seamless failover with no
grace-period stall** (observed live: "recreated server loads persisted
state, 11 queued RPCs complete"). That intent stays. The fix is one
structure, not a menu:

- **Single ordered coalescing writer.** One dedicated task owns the
  SQLite connection (the `Arc<Mutex<_>>` is deleted), fed by an
  ordered mpsc channel of typed ops (put/delete stateid, client,
  session, lock…). Per-key coalescing while queued: put-then-delete
  collapses to delete, put-then-put keeps the last — so the queue is
  bounded by live-key count, not op rate. Group commit every ≤5 ms or
  256 ops, whichever first; explicit flush on graceful shutdown. This
  one design (1) fixes the ordering bug — channel order is apply
  order; (2) removes the mutex and the spawn_blocking pressure; (3)
  **bounds** the loss window — a ≤5 ms flush interval strictly beats
  today's unbounded backlog, so seamless failover gets *stronger*, not
  weaker; (4) amortizes fsync so `synchronous=FULL` durability holds
  at ≥10k persisted ops/s — orders of magnitude above any OPEN/CLOSE
  workload.
- **Rejected: sharding the connection.** SQLite in WAL mode has a
  single writer lock *per database file*; extra connections buy zero
  write parallelism unless the DB splits into separate files —
  complexity the coalescing writer makes unnecessary.
- **Fallback: the mainstream model** (client-recovery records only +
  grace/reclaim) remains available if a bounded reclaim stall on
  reschedule ever becomes acceptable — lighter and battle-tested, but
  it surrenders the seamless-failover property on purpose.

**Is SQLite the right store at all? Yes — keep it.** The workload is
tiny rows (hundreds live), point writes, full-scan-on-boot, one
process; behind the coalescing writer the engine sees at most a few
hundred group commits/s, which SQLite sustains at FULL with a huge
margin. It is the most crash-tested embedded store available, and
knfsd uses it for this exact job. Alternatives considered and
rejected: LMDB/redb/sled (embedded KV — no gain at this size, younger
crash pedigree, migration risk), RocksDB (background compaction and
tuning burden for hundreds of rows), and a hand-rolled append-only
log + snapshot (fastest on paper, but hand-rolled durable recovery is
exactly the bug class that produced F5 — flint's own patched blob
recovery durably emptying stores). The bottleneck was never the
engine; it was one-row-per-fsync through one mutex. Revisit only if
state ever needs multi-writer/multi-node access — that is an
architecture change (Ganesha's epoch-tagged shared RADOS model), not
an engine swap.

Sequencing: the writer is independent of the F26 §12 handle work and
can land first (small blast radius). Note the §12 FH-format cutover
invalidates persisted stateid records (they embed wire-FH bytes,
`state_backend/mod.rs:199`); the migration is specced in the F26 doc
§12.1(d) and ships with §12, not with this fix.
Sources: `nfsdcltrack(8)`/`nfsdcld`; NFS-Ganesha
`ganesha-rados-cluster-design(8)` + recovery-backend docs; RFC 8881
§8.4.2 (grace/reclaim), §9 (locking recovery); SQLite WAL docs
(single-writer, group commit).

**STATUS: IMPLEMENTED 2026-07-19 (commit 896e702), lab-validated.**
Writer thread owns the connection; mutex + `spawn_persist` deleted;
every mutation site converted to the ordered `enqueue_write`. Bench:
20k awaited durable puts (synchronous=FULL, on-disk) in 159 ms =
**125k ops/s** (gate ≥10k/s); 721/721 lib tests green with new
ordering/coalescing, read-your-writes, and drop-flush regression
tests. Bench numbers are macOS-fsync; re-confirm on Linux during the
C6 gate, and live validation rides the phase-3 re-run.

### C6 live gates on runx (2026-07-19/20) — F28 found+fixed; F29/F30 opened during recovery

The 3.1 re-run on u12.0 (v4 handles + F27 writer, non-root pod)
failed on the write-probe again — but with a NEW signature, not
F26's uniform crawl.

**F28 (P1, FIXED d8c4502 in u12.2, live-validated 2026-07-20):
O(live-opens) scan on every CLOSE melts the server under connection
churn.** Signature on u12.1 (verbose instrumentation build): CLOSE
rate 240/min vs OPEN 2/min, growing "CLOSE: Invalid stateid …
StateId not found" storm, FREE_STATEID=0, server CPU idle with
worker threads futex-parked, per-op latency 100ms–5s. Root cause by
elimination (network/disk/fsync/CPU all exonerated via mountstats,
wchan sampling, socket states, stateid-correlated logs):
`close_open_state` located the map key with
`open_states.iter().find()` — a full scan of live opens per CLOSE,
under churn that allocated ~28.6k stateids in 13 min (sequential
counters confirm). Once drain lagged allocation, CLOSE replies
slipped past the client RTO; retransmitted CLOSEs re-executed as
not-found (BAD_STATEID) and fed back into the churn. Fix:
`open_state_keys` reverse index (stateid `other[12]` → open key),
populated at record_open, consumed at close — O(1) CLOSE.
u12.2 soak verdict: the MELT is gone — server-side CLOSE rtt <1ms,
steady open/close throughput (~250/min, tracking ~1:1) for the full
20 min, no monotonic ratchet, server CPU healthy. But overall
workload throughput was still collapsed (0.086 TPS) — that is F31
below, a distinct bug whose damage F28's slowness had been
amplifying. (An earlier draft claimed F28 also explained the
historic ~7% CLOSE not-found residual — REFUTED live 2026-07-20: a
completely fresh client mount against the F28-fixed server still
showed ~9.5% CLOSE not-found.)

**F29 (P1, product gap, OPEN): a force-deleted NFS pod rescheduled
onto the same node bind-mounts a dead staging mount.** Observed live
2026-07-20 as a 4-step chain: (1) the u12.2 helm roll restarted the
csi-node DS; aws-4's spdk-tgt restart at 07:05:00 orphaned
/dev/ublkb0 (ublk has no user-recovery flag configured — queue I/O
hangs forever instead of erroring), freezing the NFS server mid-I/O
at 07:05:01.745 (threads D-state in folio_wait_bit; pod Running,
0 restarts, log silent for 6.5h). (2) v1.15 graceful-recovery
re-created the lvol's ublk under a NEW device id, but nothing
remounts the staged filesystem — the staging mount still referenced
the corpse. (3) Force-delete skips NodeUnstage; kubelet's
volume-manager cache still says "staged", so the replacement pod on
the same node skips NodeStage (where the v1.10 self-heal lives) and
NodePublish blind bind-mounts the dead superblock. (4) The
replacement "boots": the v4 probe passes from page cache, then the
first real disk I/O (SQLite state-DB open touching the ext4 journal)
parks in D (do_get_write_access) — wedged-at-init, silent, and
SIGKILL-proof; the zombie's D-state siblings pin the old netns so
the client's ESTABLISHED TCP never breaks either. Fix shape:
NodePublish must verify the staging path is a live mountpoint on the
current device epoch (statfs/liveness probe) and trigger re-stage
instead of bind-mounting; evaluate UBLK user-recovery so orphaned
queues error out rather than hang. Runbook rule (landmine addendum):
NEVER bounce NFS consumers while a csi-node DS roll is in flight —
consumers restart AFTER the roll settles.

**F30 (P0-class product gap, OPEN): flint-nfs-server happily exports
an empty directory as if it were the volume.** During F29 recovery,
a lazy out-of-band umount of the dead staging mount (without a
kubelet restart) left kubelet's cache saying "staged"; the next
NodePublish bind-mounted the now-bare mountpoint directory on the
node's 8GB root disk. The server booted on it without complaint:
created a fresh `.flint-nfs/`, a NEW fh.key, an empty state.db — and
served. Every client handle failed HMAC ("v4 filehandle
authentication failed" storm); from the client the volume's data
simply vanished. No refusal, no warning. Fix shape: stamp a
volume-identity marker at first NodeStage (e.g.
`.flint-nfs/volume-id` = volume uuid) and verify it at server
startup and/or NodePublish; on mismatch or absence-where-expected,
crash loudly instead of serving an empty export. (fh.key mtime was
the forensic tell: junk key 13:55, real key 05:51.)

Recovery recipe that worked (in order): `umount -l` the stale pod
binds + globalmount → graceful pod delete → remove junk `.flint-nfs`
from the bare dir → `systemctl restart kubelet` (resyncs the
volume-manager cache — REQUIRED after any out-of-band umount) → pod
recreate runs a real NodeStage → fresh ublk device mounts, original
fh.key returns, old client handles validate again; pg-0 recovered
without a bounce (postgres WAL crash-recovery, 20M-row table
intact).

Instrumentation lesson: the u12.1 `--verbose` NFS build multiplies
data-path latency ~300× (client-side stat 358ms → 1ms after turning
it off; pg_isready 1.6–2.2s → 31ms). ~40 DEBUG lines/RPC through the
containerd stdout pipe (10MB log rotation every ~10s) backpressures
the reply path. Verbose is for correctness forensics only; never
measure latency or run gates with it on.

**F31 (P1, FIXED 5e3d348 — root-caused via reproducer tests
2026-07-20): stateid lifecycle races under the shared open-owner
destroy live opens; under connection churn the recovery tax
collapses throughput.** The u12.2 baseline soak (20 min pgbench -C
over the local socket) is the measurement: **0.086 TPS, 78s average
transaction latency** (zero failed transactions — durability held),
975 seqid=1 "CLOSE not found" warns, 175 client TEST_STATEID
recovery rounds, 2.6–4.1s whole-session stat stalls. The earlier
merge-race framing was incomplete; the reproducer tests (8-thread
open/close churn on shared (owner, fh) keys — the exact pgbench -C
shape, since the Linux client keys open-owners by uid so every
process shares one) pinned FOUR cooperating defects:
1. `record_open` returned an existing stateid WITHOUT bumping the
   seqid when the share-mask was unchanged (a misreading of RFC 8881
   §18.16.4, encoded in two unit tests as intended behavior). The
   bump is the protocol's ONLY defense against a reordered in-flight
   CLOSE from the same owner: with it, a stale CLOSE(seq=k) after a
   re-OPEN advanced the state to k+1 fails OLD_STATEID (benign);
   without it, the stale CLOSE validates and DESTROYS the state the
   new opener holds. knfsd bumps on every OPEN; I/O ops are immune
   (the client sends them seqid=0 "current stateid" form).
2. Fresh-open double-allocation: two concurrent OPENs for the same
   key could both take the vacant path; the last insert won and the
   loser's stateid was silently orphaned (570/4000 lost in the
   stress test). Fixed by running decide-and-mutate under the
   `open_states` entry guard (one-way nesting
   open_states→{states,indexes}, same as every other path).
3. CLOSE was validate-then-remove (TOCTOU) and seqid-blind. Now an
   atomic seqid-checked `remove_if` under the entry shard lock,
   returning a typed outcome; stale→NFS4ERR_OLD_STATEID instead of
   BAD_STATEID (which detonates a TEST_STATEID recovery round that
   stalls the whole session — the error-code discrimination is
   load-bearing, see the "how mainstream servers handle this"
   discussion in the session log).
4. A bounded tombstone ring of recently-closed stateids, pushed
   BEFORE the removal commits, classifies racing duplicate closes
   and replays as OLD_STATEID (the last 15/4000 stress failures were
   racers reading the maps in the instruction window between
   remove_if and the old post-removal tombstone). The stale-guard
   also now cleans the reverse index — a leaked entry let a late
   CLOSE of a dead stateid tear down its successor's open.
Tests: 3 new (deterministic reorder, successor-kill, 8-thread
stress: 570→15→0 lost stateids), 2 rewritten. 727/727 green.
Also explains the historic ~7% CLOSE not-found residual in every
prior 3.1 attempt.
**LIVE-VALIDATED on u12.3 (2026-07-20 A/B, identical 20-min
pgbench -C soak): 21.27 TPS vs 0.086 = 247× (25,524 tx vs 108);
latency avg 297ms vs 78s; connection time 71ms vs 10.9s; zero
failed transactions both sides; ZERO TEST_STATEID recovery rounds
(was 175); ZERO server warns (was ~30/min); ~30k client opens/min
sustained flat for 20 min with stat probes 12–39ms.** Client-side
CLOSE "errors" (~14% of closes) are now benign OLD_STATEID replies
for reordered closes — the designed knfsd-style answer; the client
absorbs them with no recovery activity. Mild TPS drift in the last
interval (24→15) has checkpoint/autovacuum signature, not a
server-side ratchet. Related wart, backlog: EXCHANGE_ID trunking
probe mints a duplicate clientid (RFC 8881 §18.35 casework) —
harmless, unfixed.

### Drill 3.1 PASS — the C6/F26 acceptance (2026-07-20 16:30Z, u12.3)

Sixth 3.1 attempt overall, first FULL PASS (results.csv:
ready=27s, all seven checks green). What each check proves:
- **Writability probe PASS** — the original F26 symptom that failed
  attempts 1–3 (u11.13→u11.17, post-migration 50–200ms/op crawl)
  and attempt 4 (u12.0/u12.1, F28 CLOSE-storm collapse). The full
  stack it validates: v4 kernel filehandles (F26, path maps empty),
  non-root pod + file caps, F27 ordered coalescing writer, F28
  reverse-index CLOSE, F31 stateid lifecycle.
- **Ledger + amcheck PASS**: zero lost acked writes across the
  cross-node migration; pg_amcheck --heapallindexed -j2 clean
  within budget. Pre-drill standalone amcheck also clean —
  corruption attribution unambiguous.
- Exactly one NFS pod throughout, SAME uid (server untouched by a
  client migration — the isolation invariant); witness on a third
  node: 0 mismatches, writes fresh throughout; VAs cover exactly
  the consumer nodes; ublk data path clean; no orphaned mounts; no
  driver errors.
- Post-migration client op profile (fresh mount, ~15 min window):
  READ 18,641 + WRITE 106,142 with **ZERO errors**; 4,616 opens /
  3,608 closes with 621 benign OLD_STATEID replies (~17% — the
  natural reorder rate under churn, the designed answer);
  TEST_STATEID=0, FREE_STATEID=0 — zero client recovery activity.
  Server log total warns for the window: 4 (startup keytab note,
  2 probes of a pre-roll ghost session, 1 DESTROY_SESSION cleanup).

**The reported "stall=637s" is a harness artifact, dissected**: the
pg-load pgbench timeline reads 776 TPS pre-drill → 0.2–0.9 TPS for
exactly the ~11-minute verify-amcheck window → **993 TPS within
seconds of amcheck completing**. pg_amcheck --heapallindexed -j2
saturates postgres+NFS; pg_isready goes "no response" even at the
new 5s budget; NotReady empties the headless DNS and the
per-connection ledger writer starves. Max inter-ack gap OUTSIDE
the amcheck window: **1 second**. Attempt 5 (16:03) failed ONLY on
amcheck timing out at the old 600s single-stream budget — same
artifact class; harness patched in 880f3f7 (amcheck -j2 + 1200s +
timeout≠corruption discrimination; probe budget 5s). Harness
backlog: exclude the verify window from the stall metric (or run
the availability window before amcheck) so the drill stops
measuring its own integrity scan.

Phase-3 acceptance state (as of the runx run): 3.1 DONE. 3.1b–3.9
deferred by user directive 2026-07-20 (runx torn down after this
run); they need a fresh cluster. **→ completed 2026-07-20 on
testflnt2 — see "Full Phase-3 matrix" below.**

### Full Phase-3 matrix — testflnt2 (2026-07-20, u12.3 ublk)

Fresh cluster (trove-style provision, project `testflnt2`): 4×
i4i.xlarge workers (4 vCPU, 937 GB instance-store NVMe) + 1 CP,
**Ubuntu 22.04 / kernel 6.8.0-1051-aws**, Cilium, EBS+EFS CSI
pre-installed. Stack under test: `flint-driver:1.17.0-u12.3` +
`spdk-tgt:1.6.0-f5fix.1`, `blockDevice.backend=ublk`,
`ublk.numQueues=4`. Harness driven remotely over the cluster's
kubeconfig endpoint (no in-cluster runner, no SSM).

**Bring-up deltas required on this cluster (all environment, not
flint defects — record for the trove/fleet backlog):**
- **ublk_drv absent on 6.8-aws.** `modprobe ublk_drv` → not found;
  `/dev/ublk-control` missing; spdk-tgt logged `UBLK control dev …
  can't be opened` → `Can't create ublk target`. Fix: install the
  kernel-matched `linux-modules-extra-$(uname -r)` (available from
  Ubuntu jammy-updates; nodes have apt/network), `modprobe
  ublk_drv`, roll the csi-node DS so spdk-tgt re-creates the UBLK
  target (`UBLK target created successfully`). Fleet note: 6.8-aws,
  unlike the mainline 6.18.29 the campaign validated ublk on, ships
  ublk only in modules-extra (AL2023 6.1 has none at all — see the
  "ublk for the local hop" follow-up). **ublk is a HARD node
  prerequisite; provision must ensure the module before install.**
- **hostPort 9809 collision.** The EFS-CSI node DS (hostNetwork)
  already binds 9809 on every node; flint's csi-node healthz wanted
  the same → all DS pods Pending on ports. Fix:
  `healthCheck.csiDriverPort=9810` (node-agent API 9081 is
  conflict-free and unchanged, so the harness/agent RPCs are
  untouched).
- **hugepages-2Mi=0.** spdk-tgt requests 8 Gi of 2 Mi hugepages;
  nodes booted with none. Allocated 4096 pages/worker
  (`/proc/sys/vm/nr_hugepages`) + **kubelet restart** (kubelet reads
  hugepage capacity only at startup, so runtime allocation isn't
  reflected until a restart).
- **DS scheduled onto the control-plane** (which has 0 hugepages →
  Pending, and would hang 3.9's `rollout status`). Fixed with a
  nodeAffinity excluding `node-role.kubernetes.io/control-plane`.
- **Disk-init (the standing gate).** All four workers came up with
  zero lvstores (the trove disk-init gap, reproduced again). Init'd
  the non-system 937 GB NVMe (`0000:00:1f.0`, `is_system_disk=false`)
  per worker via the node-agent `/api/disks/initialize_blobstore`;
  verified `blobstore_initialized=true` + ~933 GB free before any
  drill.

**3.1 REPRODUCED — PASS** (fresh cluster, independent of runx):
ready 25s, all 7 checks green, cross-node migration .149→.146, one
nfs pod same-uid throughout, witness clean, amcheck clean, ublk data
path clean. **stall=23s** here (not the amcheck artifact — pg stayed
Ready through this run's verify), confirming the u12.3 stack (v4
kernel filehandles/F26 + F27 writer + F28 O(1) CLOSE + F31 stateid
lifecycle) on a second cluster and a different kernel.

| # | Kill vector | Verdict | ready/stall | Notes |
|---|---|---|---|---|
| 3.1 | graceful cross-node migration | **PASS** | 25s / 23s | headline reproduction; 1 nfs pod same-uid, amcheck clean |
| 3.1b | force-delete + in-container pkill -9 | **PASS** | 14s / 26s | dirty postmaster over NFS; WAL replay; 0 loss (RWX is not node-scoped, so no RWO-style two-postmaster corruption) |
| 3.2 | flint-nfs pod delete | **FAIL** (real) | 170s / 168s io-resume | reconciler recreated the nfs pod in **39s**; **0 acked-write loss, amcheck clean**, writable at end — but postgres fsync-PANIC'd (`could not fdatasync … Input/output error` on a WAL seg) during the ~40s server-outage window and crash-recovered. FAIL is the strict log-scan; durability held. Arguably expected for a total server outage |
| 3.3b | csi-node POD delete on the nfs node | **FAIL** (real, F29) | 3s / 2s* | spdk-tgt restart under the running nfs pod re-created the ublk device under a **new id (638946)** while the nfs pod's mount still referenced the old (id 0) → orphaned ublk + broken export. "self-recovered 2s" was only the page-cache window; the data path then degraded → **F25 teardown tarpit** (pg-0 wedged D-state on the dead NFS; recovered by force-deleting stuck pods + kubelet restart on the node). **0 acked-write loss.** (*db verdict also caught a `kubectl exec`→apiserver stream timeout during amcheck — a remote-harness artifact, not corruption) |
| 3.4 | csi-node POD delete on the client node | **PASS** | 108s / 20s | 18s client stall, self-recovered in-place (pg+nfs co-located here, so this also restarted spdk under the nfs pod — recovered cleanly this time, mount followed the new ublk id) |
| 3.5 | controller kill mid-RWX ControllerPublish | **PASS** | 22s / 24s | cross-node migration .149→.151, **no duplicate nfs pods** through controller death |
| 3.8 | client churn ×10 | **PASS** | 111s / 86s | nfs pod survived all 10 cycles (same uid); per-cycle 7–13s |
| 3.9 | ☠ full csi-node DS roll | **PASS** | 235s / 19s | **I/O rode through in-place**, 0 restarts, amcheck clean — the graceful rolling restart + f5fix dirty-restart recovery let each ublk device quiesce/recover in place, so the abrupt-delete F29 (3.3b) did NOT reproduce. Matches the phase-1 ublk DS-roll ride-through |
| 3.3a | spdk-tgt PROCESS kill on the nfs node | **SKIPPED** | — | needs SSM; AWS creds expired on this workstation |
| 3.6 | nfs-server NODE kill (r2) | **SKIPPED** | — | needs SSM+EC2 and an r2 harness; not run |
| 3.7 | client NODE kill | **SKIPPED** | — | needs SSM to restore kubelet; unsafe without it |

Net: **6 PASS, 2 FAIL (both real, neither data loss), 3 skipped
(AWS-gated).** Ledger reconciliation was clean on EVERY drill —
**zero lost acknowledged writes across the whole matrix**, including
the two FAILs.

**Findings this run:**
- **F32 (P1, OPEN — F29 confirmed live on ublk/6.8): abrupt
  spdk-tgt restart under a running flint-nfs pod orphans the ublk
  device.** A csi-node POD delete on the nfs node (3.3b) re-creates
  the lvol's ublk under a NEW device id; the nfs pod's existing
  `/mnt/volume` mount points at the dead device and cannot follow,
  so the export breaks and teardown wedges (F25 tarpit). This is the
  ublk analog of the phase-1 U4 "fresh start mints a new device the
  old mount cannot follow" — UBLK_F_USER_RECOVERY is not in effect
  on this stack/kernel, so the device is re-minted rather than
  recovered in place. The **graceful** DS roll (3.9) avoids it
  (rolling, one node at a time, f5fix quiesce/recover), so the
  trigger is specifically the abrupt single-pod delete under the nfs
  pod. Fix shape (same as F29): NodePublish must verify the staging
  mount is a live mountpoint on the current device epoch and
  re-stage instead of serving a dead one; evaluate configuring UBLK
  user-recovery so the queue recovers the existing gendisk. Runbook:
  NEVER delete/bounce a csi-node pod on the nfs-server's node while
  that volume is live — roll gracefully (3.9-style) instead.
- **3.2 log-scan vs durability:** deleting the sole NFS server pod is
  a genuine data-path outage; postgres's fsync-PANIC is correct
  fail-safe behavior and durability survived (0 loss, amcheck
  clean). The strict `verify-db` log grep (`PANIC|…`) flags it FAIL;
  the drill's intent (reconciler recreates ≤~45s, I/O resumes with
  no data loss) was met. Backlog: decide whether an fsync-PANIC that
  crash-recovers with 0 loss should count as PASS for the pod-delete
  drill, or whether the client should ride the outage via hard-mount
  blocking rather than surfacing EIO.
- **Harness/remote-driving artifact (not flint):** running the
  harness over the cluster's kubeconfig endpoint, the long
  `kubectl exec` amcheck stream to the API server timed out
  intermittently (`read tcp …:6443: operation timed out`), producing
  spurious `amcheck`/`write-probe` sub-failures (seen on 3.3b). The
  authoritative durability signal — ledger reconciliation over the
  acked.log — completed on every drill and is what the "0 lost acked
  writes" verdict rests on. For future remote runs: run amcheck from
  an in-cluster job/pod, or QUICK=1 the exec-heavy check and rely on
  ledger + pg-log + a short write probe.
- **F25/F30 recovery recipe reconfirmed:** the 3.3b tarpit cleared
  with force-delete of the stuck pods + `systemctl restart kubelet`
  on the affected node (via a privileged hostPID pod — no SSM
  needed for a *restart*), which resyncs the volume-manager cache;
  the flint controller then deleted the volume and PVs cleanly.

Artifacts: `tests/chaos/artifacts/3-3.{1,1b,2,3b,4,5,8,9}-*/`
(driver logs, db-verdict, ublk/mount/VA dumps); verdict rows in
`tests/chaos/results.csv`.

### nvmeof backend A/B — testflnt2 (2026-07-20, same u12.3 images)

Re-ran the matrix with `blockDevice.backend=nvmeof` (kernel NVMe-oF
loopback instead of ublk) on the same cluster/images, primarily to
test whether **F32 (the 3.3b ublk-orphan) is ublk-specific**. Backend
switch = `helm upgrade --set blockDevice.backend=nvmeof` + DS roll;
**the SPDK LVS/blobstore is backend-agnostic and was NOT re-initialized**
(ublk vs nvmeof only changes the kernel-facing exposure, not the
on-disk store — reinit would needlessly destroy it).

**Bring-up delta (environment, parallels the ublk_drv gap):** the RWX
volume attach failed at first with
`nvme connect failed: Failed to open /dev/nvme-fabrics: No such file
or directory` — the `nvme_tcp`/`nvme_fabrics` initiator modules are
not auto-loaded on 6.8-aws (contrast the campaign's AL2023 6.1 where
nvme was built-in). Fix: `modprobe nvme_tcp` on every worker
(modules already on disk from the earlier `linux-modules-extra`
install); the nfs-pod NodeStage then succeeds and pg-0 mounts. **The
backend's initiator kernel module is a hard node prerequisite —
ublk_drv for ublk, nvme_tcp/nvme_fabrics for nvmeof.**

| # | nvmeof | ublk | Read |
|---|---|---|---|
| 3.1 | **PASS** (ready 24s) | PASS 25s | migration clean on both backends |
| 3.1b | **PASS** (ready 180s, cross-node) | PASS | force-delete+pkill, 0 acked loss both |
| 3.2 | **FAIL** | FAIL | **same fsync-PANIC** on the sole-nfs-server outage (0 acked loss, amcheck clean) → **backend-independent**. Plus an nvme-leak (see below) |
| 3.3b | **FAIL (soft) — F32 does NOT reproduce** | FAIL (hard, F32) | **the headline result.** nvmeof self-heals: io_resume **1s**, pg Ready **2s in-place**, postgres log clean (no PANIC), **all 912 acked writes present**, NO orphaned device, NO teardown tarpit. The kernel NVMe-oF initiator reconnects to the re-created export — exactly what ublk lacks (ublk mints a new device id the mount can't follow → orphan + F25 tarpit needing a manual kubelet restart). nvmeof's FAIL is only amcheck-timeout (artifact) + the backing loopback session still `connecting` at verify + the deleted-volume orphan-leak |
| 3.4 | **FAIL (artifact)** | PASS | verify hit a transient `cluster unreachable` (workstation↔API-server blip), not flint |
| 3.5 / 3.8 / 3.9 | run in progress; verify connectivity-limited | PASS | client-migration / churn / DS-roll — data-plane classes already green on ublk this session and on nvmeof in phase-1; this run's verdicts are dominated by the remote-harness connectivity artifact below |

**Findings:**
- **F32/F29 is ublk-specific (confirmed).** The abrupt spdk-tgt restart
  under a running nfs pod orphans only the ublk device (new-id
  re-mint); the nvmeof loopback initiator reconnects to the
  re-created subsystem (ctrl_loss_tmo) and the mount survives.
  nvmeof rode 3.3b through with zero data loss and no manual
  recovery; even the post-3.3b harness reset was clean (no F25
  tarpit).
- **3.2 is backend-independent.** Deleting the sole NFS server pod is
  a genuine data-path outage on either backend; postgres fsync-PANICs
  (fail-safe) and crash-recovers with zero acked-write loss and clean
  amcheck. Not a driver defect in either mode.
- **nvmeof-only wart — orphaned loopback NVMe-oF sessions.** After a
  volume delete (the mandatory 3.1b reset), the kernel initiator for
  the gone volume lingered `connecting` (reconnect loop, ctrl_loss_tmo)
  and flagged the verify's leak check on every subsequent drill; and
  post-spdk-restart the live volume's session took time to return from
  `connecting` to `live`. This is the phase-1 "orphaned NVMe session
  on rapid delete" class — a real nvmeof teardown-cleanup gap (ublk
  has no kernel sessions, so it never shows this). Backlog: NodeUnstage
  / controller reaper should tear down the loopback initiator
  controller on volume delete.
- **Harness/remote-driving caveat (dominates 3.4–3.9 here).** Running
  the harness over the cluster's kubeconfig endpoint, the long
  `kubectl exec` amcheck streams repeatedly hit the 1200s timeout and
  a transient `cluster unreachable` blip failed 3.4 outright — these
  are workstation↔API-server connectivity artifacts, not flint. The
  authoritative durability signal (ledger reconciliation) was clean
  wherever it could be measured. Run the harness from an in-cluster
  job/pod for clean nvmeof verdicts on the remaining drills.

**Net (nvmeof, this session):** the data-plane conclusions match ublk
where measurable, with two backend-specific differences — nvmeof
**avoids F32** (its big advantage) but **leaks orphaned loopback nvme
sessions** on delete (its cost); 3.2 fails identically on both.

### Root-cause addendum — 2026-07-20, code+artifact forensics (no cluster)

Post-hoc analysis of the two FAILs and the nvmeof leak, from the
committed artifacts and the source tree alone.

**F32 root cause CONFIRMED: a backing-PV annotation identity bug —
not a ublk kernel limitation.**
- `store_block_device_info` patches the PV **named by the CSI volume
  handle**. RWX backing volumes have handle `nfs-server-<id>` but PV
  name `flint-nfs-pv-<id>`, so the patch 404s on every stage and is
  swallowed as "Non-fatal" (hard evidence:
  `tests/chaos/artifacts/3-3.2-1784577349/driver-logs.txt:2414`,
  `persistentvolumes "nfs-server-pvc-e13acd1b-…" not found`). The
  `flint.io/ublk-id` annotation therefore NEVER persists for RWX
  backing volumes. (User RWO PVs are unaffected: PV name ==
  volumeHandle — which is why phases 1–2 never saw this.)
- On the abrupt csi-node restart (3.3b), rehydration's
  `resolve_ublk_id` falls back to the 20-bit volume-id hash → 638946,
  which ≠ the agent-allocated serving id 0 (`ublk.txt`: same lvol
  `0ed42595…`, id 0 pre-kill vs 638946 post). `/dev/ublkb638946`
  doesn't exist, so `ensure_ublk_disk` skips its recovery-first arm
  (`ublk_recover_disk`) and fresh-starts under the wrong id
  (driver-logs: `[REHYDRATE] rebuilt local ublk disk from ground
  truth ublk_id=638946`) — while the nfs pod's mount stays pinned to
  the dead `/dev/ublkb0`. Orphan + broken export + F25 tarpit.
- **The 3.4/3.9 PASSes rode on accidental id alignment, not on a
  working design.** The post-3.3b reset created the replacement
  volume's disk AT its hash id (461552 — hash-shaped, not
  smallest-free; visible in the 3.4/3.5/3.8/3.9 ublk dumps). 3.4's
  abrupt csi-node kill then re-resolved the SAME hash id, found
  `/dev/ublkb461552`, and `ublk_recover_disk` preserved the mount.
  Two corollaries: (a) ublk user-recovery on 6.8-aws +
  spdk-tgt 1.6.0-f5fix.1 is demonstrably functional — the ONLY
  defect is id resolution; (b) a fresh-cluster RWX DS roll (3.9)
  with an agent-allocated id would re-mint just like 3.3b, so 3.9's
  green here does not clear the class.
- Same-bug corollary, latent leak: NodeUnstage's
  `get_block_device_info` 404s identically and falls back to
  "legacy ublk cleanup" of the HASH id — a no-op against the real
  disk. Any genuine cross-node move of an RWX backing volume leaks
  the source node's ublk disk (holding the lvol open). No drill this
  run unstaged a non-aligned backing volume, so it hasn't been
  observed yet.
- Fix shape: an identity helper mapping backing handle → backing PV
  name (`flint-nfs-pv-<storage_id>`), used by both store and get;
  unstage + rehydrate should resolve by live/kernel bdev match
  first, annotation second, and never fall through to the bare hash.
  F29's NodePublish staging-liveness check stays as defense-in-depth.

**3.2 (fsync-PANIC) RECLASSIFIED: not "expected outage behavior" — a
real zero-grace-resume gap.** The consumer mount is hard
(`vers=4.2,noresvport,sec=sys`): a pure server outage can only
block, never EIO. An EIO from fdatasync requires a definitive
server ERROR on writeback — and `estale=1` on BOTH backends says the
recreated server answered at least one pre-restart filehandle/state
with a STALE-class error during the client's dirty-page recovery;
the kernel flags the mapping (AS_EIO) and postgres correctly PANICs.
Crash recovery then re-opens everything fresh and succeeds — which
is exactly why durability held while the drill's actual design
intent (transparent zero-grace resume) did not. Verdict stays FAIL.
- Cleared suspects (code review): the EXCHANGE_ID §18.35 table is
  correct for reloaded CONFIRMED records (case 1 returns the
  persisted clientid; client records preload before the listener
  accepts); the sqlite persistence writer flushes its queue tail on
  Drop, so a graceful SIGTERM does not lose enqueued state; the
  deliberate BADSESSION→EXCHANGE_ID→CREATE_SESSION restart flow is
  sound per its design comment.
- Live suspects (each code-confirmed, one server-log capture needed
  to pick): (1) **pseudo-fs `instance_id` is `SystemTime::now()` at
  boot** (`pseudo.rs:102`) and is baked into pseudo/root handles —
  every restart invalidates the client's cached mount-root handle;
  the real-fs KernelFh layer uses `stable_nfs_instance_id(volume_id)`
  and the pseudo layer was simply never converted. Unambiguously
  wrong; fix-first regardless of attribution. (2) handles resolvable
  only via the in-memory open-files view (F17b/c fallback:
  unlinked / renamed-over inodes) are unrecoverable across ANY
  restart — fundamental (knfsd shares it) — and pg WAL recycling is
  rename-heavy. (3) an in-flight-compound state tail at SIGTERM
  (processed-but-unreplied ops) replaying against reloaded state.
- Harness gap blocking final attribution: phase-3 verify captures
  csi driver-logs but NOT the flint-nfs pod log (the runx 3.1
  harness captured `nfs-server-final.log`). One 3.2 rerun with
  server-log capture — or an integration test restarting the server
  under a live kernel-client mount — pins the exact op.

**nvmeof orphan-session leak root cause (code-confirmed):** the only
initiator disconnects live in NodeUnstage
(`disconnect_from_nvmeof_target`) and NodeStage's stale-controller
reuse guard (nvme_recovery #3). A volume deleted while its consumer
was force-deleted (the mandatory 3.1b reset flow) never runs
NodeUnstage, and no controller/reaper path disconnects node-side
initiators for gone volumes — the kernel then reconnect-loops
`connecting` forever (artifact: `pvc-ca414215…` still `connecting`
two drills later). Fix shape: an orphan reaper that diffs node
initiator sessions against live PVs — the exact analog of the ublk
loss-detector's diff — disconnecting sessions whose volume is gone.

**Fix status (b0427ca, same day, unit-tested 727→741 — needs one
cluster session to live-validate):**
- **F32 FIXED:** `identity::pv_name_of_handle` (+`backing_pv_name`/
  `backing_pvc_name`) is now THE handle→PV-name resolver;
  `store/get_block_device_info`, rwx_nfs, and cutover route through
  it. The rehydrate walk **backfills** the `flint.io/ublk-id`
  annotation with the actual serving id (startup + every monitor
  tick), healing volumes staged before the fix. NodeUnstage's
  annotation-less fallback now stops the disk **by backing bdev**
  (the agent resolves the serving id from live SPDK state and
  refuses to guess when nothing serves it) — closing the latent
  cross-node unstage leak; the hash id survives only as a loud last
  resort for pre-attrs PVs. Regression tests pin the name mapping,
  the delete-id resolution matrix (bdev beats stale hash;
  no-serving→no-stop), and the mint shapes against rwx_nfs.
- **3.2 fix-first LANDED:** `PseudoFilesystem` now takes the
  manager's stable per-volume instance id (was `SystemTime::now()`
  per boot); root handle bytes and create_time are restart-invariant
  and old-incarnation root handles remain recognized (tested both
  ways). Final attribution of the fsync-PANIC still needs one 3.2
  rerun with the new server-log capture.
- **nvmeof leak FIXED:** `reap_orphan_initiator_sessions` on the 60s
  monitor tick — fabrics-only sysfs scan, `classify_subsystem_nqn`
  ownership, PV existence via `pv_name_of_handle` (backing-handle
  NQNs judged by the synthetic PV), non-`live` state gate so
  deletion-in-flight and spdk-restart reconnect windows are never
  touched, kernel `nvme disconnect` per orphan. Decision matrix
  unit-tested including the exact `pvc-ca414215` leak shape.
- **Harness gap CLOSED:** verify-drill now captures the flint-nfs
  pod log (and `--previous` when present) into
  `nfs-server{,-previous}.log` per drill.

Related wart (same session): flint answers a trunking-probe
EXCHANGE_ID (same co_ownerid+verifier, unconfirmed) by minting a
SECOND clientid instead of returning the existing record (RFC 8881
§18.35 casuistry). Harmless today — the client confirms one and the
other ages out — but worth folding into the F31 fix pass.

### runy2 live-validation of the fix wave — 2026-07-20/21 (u12.4)

Cluster runy2 (trove project 43, DELETED 2026-07-21 with verified
zero residue): spot-only INCLUDING the CP (5× i4i.xlarge us-west-1 +
cordoned c5d spot builder), workers kernel-swapped to mainline
6.18.29, blobstores initialized+verified before any drill. Stack:
`flint-driver:1.17.0-u12.4` (= b0427ca fix wave) + `spdk-tgt:1.6.0`.

**Every fix in the wave validated live:**
- **F32 DEAD.** Stage-time `flint.io/ublk-id` annotation present on
  the backing PV from the first attach (never existed pre-fix; zero
  store errors). 3.3b: abrupt csi-node kill → rehydrate resolved id
  0 from the annotation → `ublk_recover_disk` → "recovered quiesced
  kernel device (mount preserved)", ready 6s, no orphan, no tarpit.
  3.9 DS roll rode through on the same path with a fresh-staged id —
  no hash-alignment luck involved. 1.9b (RWO regression) recovered
  the HYBRID chain (remote nvme bdev + ublk) in 24s.
- **3.2 defect GONE, both backends.** ublk: estale=0, no
  fsync-PANIC, 804/804 acked, server log (new capture) shows the
  clean zero-grace resume — clients re-CREATE_SESSION on persisted
  clientids, 41 stateids reloaded, zero STALE replies. nvmeof:
  estale=0, no PANIC, 642/642, io_resume 96s. Residual flags were
  harness budgets (fixed below) plus an UNATTRIBUTED observation:
  pg-0 was recreated cross-node mid-outage on both u12.4 3.2 runs
  (not seen on u12.3/testflnt2; db clean both times; k8s 1.34.9 is
  the lead suspect — next session).
- **nvmeof leak CLOSED at the source.** The 3.1b→reset leak flow
  left ZERO `connecting` sessions on any node — the F32 identity fix
  also repaired NodeUnstage's disconnect (the same 404 ate the
  nvmeof cleanup annotations), so no orphan ever forms; the reaper
  is the tested backstop.

**Full matrix:** ublk 3.1/3.1b/3.3a/3.3b/3.4/3.5/3.8/3.9 PASS (3.3a
first-ever run — SSM available this time), 3.2 db-clean;
3.6/3.7 FAIL → **F33**; nvmeof 3.1b PASS, 3.2 db-clean, 3.3b
self-heal 1s + amcheck-timeout artifact + stale-duplicate wart.
Zero lost acked writes across every drill, both backends.

**F33 (P1, NEW — found by 3.6, reproduced by 3.7): no NFS-server
self-fencing.** kubelet-stop node kill leaves the server process
ALIVE on the isolated node (observed 93 minutes, actively burning
CPU) while the reconciler resurrects a replacement on a surviving
replica node. Client failover is a race: pg-0 escaped because its
TCP broke; the witness's established flow stayed anchored to the
orphan and hung the entire time — and released INSTANTLY when the
orphan died (kubelet restore), proving process death is the cure.
3.7 reproduced it from the client side: disk-follows-pod co-locates
the server with the consumer, so a client-node kill is also a
server-node kill. Data: ZERO loss both times — the r2 fence
protected the authoritative leg (db verdicts clean, 2495/2495).
Collateral: the double kubelet-stop/orphan-reap cycles wedged
containerd (zombie reactor, StopContainer DeadlineExceeded) —
runtime restart + pod force-delete needed.
**FIXED (7a80e3a): backing-store self-fencing watchdog** in the
server (`nfs/fence.rs`): heartbeat write+fsync on a prober thread,
wall-clock staleness monitor (catches D-state hangs, EIO loops,
device death uniformly), process exit past the deadline (default
90s; `FLINT_FENCE_DEADLINE_SECS`, 0 disables). Unit-tested incl.
the hanging-probe reproduction and a healthy-probe
never-false-positive guard. Needs one cluster session to
live-validate (rerun 3.6: witness should recover ≤ ~deadline+RTT).

**ublk DEAD-device edge (F29-family, found via 1.9b forensics):** a
device whose daemon is SIGKILLed under a wedged containerd lands
DEAD (not quiesced): `ublk_recover_disk` → ENODEV AND
`ublk_start_disk` on the id → ENODEV (the corpse occupies it) —
unreclaimable without UBLK_CMD_DEL_DEV, which the agent lacks; node
reboot required (instance-store survives reboot). FIXED-partially
(7a80e3a): the state is now classified and escalated with the
runbook instead of warn-looping (tests pin the live error shape);
the DEL_DEV escape hatch (io_uring ctrl cmd) stays backlog.

**nvmeof stale-duplicate controllers (3.3b wart):** post-tgt-restart
the initiator reconnects on a fresh controller while the old one
lingers `connecting` for the SAME subsystem forever. FIXED
(7a80e3a): the reaper disconnects non-live controllers that have a
LIVE sibling (per-device `nvme disconnect -d` — the NQN form would
cut the live path); lone non-live controllers stay untouched.
Unit-tested incl. the multipath and cross-subsystem no-touch cases.

**Harness fixes (7a80e3a):** all ledger/witness exec reads
timeout-wrapped (an unwrapped `tail` on a dead NFS mount hung 3.6
for 87 minutes); 3.2 READY_TIMEOUT 120→300 (the reconnect tail is
~180-220s in known-good runs); 3.7 surfaces `nfs_colocated=`;
WITNESS=1 ignored with a note on RWO (the witness needs the shared
mount — waiting on it failed every RWO deploy).

Cluster ops notes (recorded in the runy2 memory): trove
`controlPlaneNodeType:"aws_spot"` must be explicit or the CP silently
launches on-demand; AL2023 has NO ublk_drv on any kernel stream —
mainline 6.18.29 deb swap required, and the mainline debs ship a
plain `data.tar` (not .zst); teardown verified zero
instances/EBS/spot via tag filter `trove/runy2/*`.

## Findings

### F1 — RECLASSIFIED 2026-07-13: concurrent postmasters, not a storage bug

**Verdict: NOT a flint storage defect.** Reproduced first-try on the fresh
`runt` cluster with the replacement pod's attempt-0 log intact, which the
first incident was missing. The log proves temporal overlap: attempt-0 read
`pg_control` at 13:42:01.625 whose "last known up" stamp was **13:42:01** —
written by the *old* postmaster, still alive after the force delete. Then
attempt-0's recovery failed with `xlog flush request 1/12FFF7A0 is not
satisfied --- flushed only to 1/12FFF6F0` — a heap page whose LSN advanced
**past attempt-0's view of end-of-WAL, while it ran**, because the old
postmaster was still writing through the same node-shared mount.

**Mechanism (documented k8s foot-gun, not flint):** `kubectl delete pod
--grace-period=0 --force` removes the API object immediately; the STS
controller creates the replacement at once; the same node satisfies WFFC; and
RWO is **node-scoped**, so kubelet happily NodePublishes the same staged
volume to the new pod while the old containers still run. Two postmasters,
one PGDATA, one shared page cache. `postmaster.pid` cannot protect across
containers (separate PID namespaces). Every anomaly in the original F1
analysis — the "reverted" `pg_control` (8 KB read-modify-write clobber by the
stale-read instance), the missing/recycled WAL segments (the old postmaster's
checkpoint legally recycled them), the zeroed `6A` shell — is explained with
zero storage-layer misbehavior.

On `runt` the picture was further muddied by an independent failure hit 6 s
later: kubelet DiskPressure evicted the csi-node pod (see **F2**), killing
spdk-tgt under the wedged mount and producing the reconnect-loop/EIO tail.

**What this changes:**
- Drill 1.3's original PASS bar ("ledger clean after WAL replay") was
  mis-specified for RWO: Kubernetes explicitly documents that force-deleting
  StatefulSet pods can violate at-most-one semantics. With RWO the DB *can*
  legally corrupt itself. The drill's flint-scoped bar is: CSI hygiene stays
  clean (exactly one VA, healthy session, no orphaned mounts, clean
  detach/reattach) — which it did, in both incidents.
- flint advertises `SINGLE_NODE_SINGLE_WRITER` (RWOP). A **1.3b RWOP drill**
  is added: with `ReadWriteOncePod`, kubelet must refuse the second pod's
  mount until the first is fully unpublished — force-delete must then be
  corruption-free end-to-end.
- The open durability question (does a *hard spdk-tgt kill* lose fsync-acked
  writes?) is exactly drill **1.9**'s ledger check — the durability leg the
  v1.15.0 grace3/grace4 drills (which validated liveness/resumption) never
  exercised.

Original (now superseded) analysis kept below for the record.

#### Original F1 writeup (superseded)

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

### F2 (real flint chart bug, FIXED) — csi-node evictable under DiskPressure

The chart set **no `priorityClassName`** on the csi-node DaemonSet (or the
controller). On `runt`, the 8 GB root EBS crossed the kubelet
ephemeral-storage eviction threshold at 13:42:08 (images + churn) and kubelet
chose the csi-node pod for eviction — **killing spdk-tgt under every mounted
flint PVC on the node** (the csi-node-roll landmine, self-inflicted), then
kept evicting each DS replacement until pressure cleared (6 evictions,
13:42–13:50). NVMe sessions reconnect-looped (`ctrl_loss_tmo=1800`), and the
pre-existing mount was wedged until manual controller delete via sysfs.

**Fix (shipped in-repo, applied live to runt):** chart now sets
`priorityClassName: system-node-critical` on the csi-node DS and
`system-cluster-critical` on the controller (values-overridable:
`node.priorityClassName` / `controller.priorityClassName`). Kubelet never
selects system-node-critical pods for resource eviction.

Unstick recipe recorded: dead controller in reconnect loop blocks unmount →
`echo 1 > /sys/class/nvme/<ctrl>/delete_controller` (host has no nvme CLI),
then pod teardown proceeds and the PV deletes cleanly through the driver.

### F3 (environment/trove) — 8 GB worker root is too small

Base images + flint images + one workload image (~4.8 GB) leave <2.5 GB
headroom; pod churn crosses the 85% eviction threshold within minutes of
harness deploy. Kubelet's reclaim also deletes just-pulled images (the
re-pull hit a Docker Hub 502 mid-recovery). Trove backlog: bigger root
volume (or dedicated imagefs on the instance store). Campaign mitigation:
F2's priority fix + accepting workload-pod evictions as legitimate chaos.

### F5 (**P0 — data loss**) — hard spdk-tgt death loses fsync-acked data; a young lvol vanishes entirely

**Experiment D (drill 1.9p), 2026-07-13 on runt — 100% reproduction, first try:**

1. Fresh volume (thin lvol, flint default `thinProvision=true`), pgbench +
   ledger load for ~7 min (`lvols: 1, free: 884636MB` in the driver's LVS
   view). All postgres commits fsync-acked through NVMe FLUSH.
2. `pkill -9 -f spdk_tgt` on the lvol's node (the exact kill vector the
   v1.15.0 graceful-recovery feature targets; sidecar restarted cleanly,
   consumer pod untouched).
3. spdk-tgt gen-3 startup: `blobstore bs_recover: Performing recovery on
   blobstore` (unclean-shutdown path) → `Lvol store found … examination done`
   → **`lvols: 0, free: 890101MB`** — the lvol is GONE from the recovered
   metadata; its ~5.5 GB returned to free space. Every fsync-acked byte lost.
4. Aftermath (**F6**): reconcile-on-loss (#1) re-creates the subsystem but
   `nvmf_subsystem_add_ns` fails forever (`bdev … cannot be opened,
   error=-19` — the bdev no longer exists) retrying every 10 s; the
   initiator reconnect-loops against a listener that exports nothing; the
   consumer's disk I/O hangs indefinitely (35+ min observed) while the pod
   stays **Ready** (its probe, `pg_isready`, touches no disk).

**Why v1.15.0's grace3/grace4 validation missed it:** those drills used aged
volumes (metadata long since synced by a prior clean unload) and verified
*liveness* — held-open fds, I/O resumption — through the still-warm page
cache. They never verified *durability* of recent writes, and never killed
the target while the blobstore held unsynced metadata.

**Mechanism (two candidates, isolation = follow-up):** flint never issues any
blobstore/blob md sync (`grep -r sync_md` over the driver: zero hits;
`thin_provision` defaults true at `main.rs:1135`). SPDK persists thin-lvol
cluster allocations — and evidently, on this v26.05 build, even blob
creation — only at clean unload / explicit `spdk_blob_sync_md`. Alternative
or compounding: `bdev_uring` buffered-vs-O_DIRECT semantics (gen-N buffered
writes in host page cache never reaching media, gen-N+1 reading the device
directly). Either way the contract is broken: **NVMe FLUSH is acked for data
whose metadata (or content) does not survive target process death.**

This retro-explains **incident 2** (the 14:10 DS roll under load → mixed
old/new on-disk state: a clean SIGTERM unload racing its 30 s grace under
active connections, then partial metadata persistence), and plausibly the
original runs incident (eviction storm = repeated hard kills).

**Experiment T (same kill, thick lvol — `thinProvision: "false"`): NO
mitigation.** Pre-kill `lvols: 1, free: 869621MB` (the full 20 GiB truly
allocated at create); post-kill `lvols: 0, free: 890101MB` — the thick lvol
vanished identically, I/O never resumed (300 s+). So blob *existence* is
only persisted at clean unload, independent of provisioning mode.

**Refined mechanism:** blobstore metadata (blob existence, cluster maps) is
written to media only on clean unload (SIGTERM path) or explicit
`spdk_blob_sync_md` — which flint never issues. Data-cluster writes for
*previously-synced* blobs go to media directly and survive process death —
which is exactly why the aged volumes in grace3/grace4 survived hard kills
(their md was synced by earlier clean restarts) while any volume created
since the last clean shutdown is silently **un-created** by the next hard
death, and thin allocations/resizes on older volumes roll back.

**ROOT CAUSE PINNED (Experiment R, 2026-07-13): flint's own
`blob-recovery-optimized.patch` drops valid on-media blobs during recovery.**

Discriminating experiment on a fresh volume (`pvc-9b07e1d5…`, lvol-local on
runt-aws-1):

1. Pre-kill raw scan of `/dev/nvme1n1`: the lvol's creation md page IS on
   media (name xattr at device offset 356414 → device page 87). Creation
   `blob_persist` works; O_DIRECT confirmed on the spdk_tgt fds
   (`flags=01140002`).
2. `pkill -9 spdk_tgt` → sidecar gen+1 → `bs_recover` runs the **patched**
   path (`Recovery: Using batched reads (64 pages/batch)` NOTICE — patch
   confirmed active in the deployed `spdk-tgt:1.5.0`) → `lvols: 0`.
3. Post-kill scan: **the md page is still on media, byte-identical offset**,
   and decodes perfectly by upstream validity rules — `id=0x1_00000002`
   matches its md-region position (page_index 2 with `md_start=85` per the
   superblock), `sequence_num=0`, `next=0xffffffff` (single page), CRC set.
   Upstream `bs_load_replay_md` would recover this blob; the batched
   replacement skipped it and then durably rewrote the store as empty
   (used-masks flushed by `bs_load_write_used_md`).

Additional defects visible in the patch by inspection, independent of the
exact skip bug: it never follows blob md page chains (`in_page_chain` can
never become true — multi-page blobs lose all pages after the first); it
never calls `bs_load_replay_extent_pages` (extent-table cluster allocations
are never replayed — silent data truncation even where the blob survives);
and at end-of-scan `bs_load_replay_md_chain_cpl` calls `spdk_free(ctx->page)`
on a pointer into the already-freed batch buffer (invalid free / UAF).

**Companion bug — `lvol-flush.patch`:** makes lvol advertise FLUSH and
completes every flush as an immediate no-op success ("blobstore handles
persistence" — it does not; "the underlying base bdev handles actual flush" —
it is never forwarded, and `bdev_uring` supports only READ/WRITE). Every
fsync through the stack is acked without flushing anything. In practice
O_DIRECT completion has been covering data writes, but the FLUSH contract is
void — device volatile-cache loss on power failure is unhandled, and nothing
ever persists blobstore md at runtime.

**Fix plan:**
- **Revert `blob-recovery-optimized.patch`** — take upstream recovery's
  correctness over scan speed (the batched scan of this 893,592-page md
  region took ~4.5 s; upstream's serial scan is slower but this is a
  crash-recovery path). If the optimization is wanted later, it must be
  rebuilt with chain-following, extent-page replay, per-page parity with
  upstream, and an A/B recovery test against a store with multi-page +
  extent-table + freshly-created blobs.
- Rework `lvol-flush.patch` to forward FLUSH to the base bdev (bdev_uring
  needs real flush support) and/or sync blob md on flush; at minimum stop
  acking flushes as no-ops.
- Rebuild spdk-tgt, and gate on Experiments D/T/R as regression drills
  (create → write → `kill -9` → recover → ledger + amcheck + cold-reader
  verify).
- Until fixed: treat any hard spdk-tgt death as data-loss-capable; the only
  safe restart is clean SIGTERM with generous grace (the 30 s DS default
  under load is itself suspect — incident 2).
- F6 independently: reconcile-on-loss must escalate when the bdev is gone
  (surface VolumeCondition, mark the volume failed) instead of silent
  infinite retry under a Ready pod.

Evidence: `tests/chaos/artifacts/expD-1783953943/` (spdk-tgt gen-2/gen-3
logs incl. the `bs_recover` line, csi-driver NVME-RECOVERY loop, LVS views).

### F5 fix + validation (2026-07-13, spdk-tgt:1.6.0-f5fix.0)

Patches reworked (`blob-recovery-batched.patch` — batched reads with
upstream-identical processing; `lvol-flush-sync.patch` — FLUSH →
`spdk_blob_sync_md` on the lvolstore md thread). Unit gate on the builder
node: **blob_ut 500/500** (206,448 asserts; every `blob_dirty_shutdown`
recovery sub-case through the batched path), lvol_ut 37/37, vbdev_lvol_ut
23/23. Image `dilipdalton/spdk-tgt:1.6.0-f5fix.0` (digest 62664caf) deployed
to runt's DS (roll performed with zero PVs — landmine-safe).

First live gate run recovered the blobs (`Recover: blob 0x0 / 0x1` NOTICEs)
— but surfaced **F7** (below). After F7 remediation (clean stores),
**D-redux-2 PASSED the full gate**:

- kill: `pkill -9 spdk_tgt` under pgbench on the lvol-local node →
  `bs_recover` → batched scan of all 893,592 md pages in **4.6 s** →
  `Recover: blob 0x0 / 0x1` → `Lvol store found — begin parsing` →
  reconcile re-export → initiator reconnect → **I/O resumed at +45 s**,
  consumer pod untouched (same UID, restarts 0→0)
- **WARM verify: PASS** — all 687 acked writes present, `pg_amcheck
  --heapallindexed` clean, writable
- **COLD verify: PASS** — cordon + graceful delete → cross-node reschedule
  (fresh session, cold cache) — all 956 acked writes present, amcheck clean
- **kill-2: PASS, and harder than designed** — during the second kill on the
  same (already-recovered) store, kubelet evicted pg-0 off its node
  (XFS-dynamic "inode" pressure = F3 space pressure in disguise; the
  csi-node itself survived — the F2 priority fix held), and the STS
  replacement landed on the lvol host **while its spdk-tgt was
  mid-recovery** — NodeStage retried until the bdev appeared, pod Ready
  ~70 s after the kill. Ledger: **all 1,423 acked writes present**, amcheck
  clean. Recovery idempotency + eviction + cross-node move mid-recovery,
  zero loss.

Compare the identical drill on the broken bits: lvol vanished entirely,
I/O wedged forever. **F5 is fixed.** Follow-ups that remain open on the
flint side: F6 (reconcile escalation + VolumeCondition), F7 fleet
remediation (or tolerant-recovery mode), packaging the fixed spdk-tgt into
the next release (this campaign ran `1.6.0-f5fix.0`).

### F7 — stores that ran the broken recovery are poisoned for strict recovery

The old broken recovery "deleted" lost blobs by rewriting empty used-masks
while leaving their (valid, CRC-intact) md pages on media. Normal blobstore
deletes zero md pages (`blob_persist_zero_pages`), so healthy stores never
contain valid orphan pages — but stores that ever ran the broken recovery
do. The corrected (upstream-semantics) recovery then finds the stale blob,
replays its extent table — whose extent pages have since been reused by
newer blobs — hits an id mismatch (`bs_load_cur_extent_page_valid`) and
fails the whole store load with `-EILSEQ` (identical to what vanilla
upstream recovery would do). Observed live: D-redux-1 recovered stale blob
`0x1` (`lvol_pvc-fa92d8e6…`, deleted in Experiment D) and the LVS load
failed; the consumer stayed wedged.

**Remediation (applied to runt):** wipe super+masks+md region (`dd` first
4.4 GB) + agent re-initialize on all three workers; controller scale-cycle.
**Fleet implication:** any deployed store that experienced a hard spdk-tgt
death on the broken-recovery images carries latent orphan pages; before
relying on the fixed recovery, stores must be rebuilt — or recovery needs an
opt-in tolerant mode (skip-and-WARN on blobs with dangling extent pages
instead of failing the store). Recorded as follow-up work.

### F8 — csi-node pod restart is amnesiac: exports never re-created, health checks lie (drill 1.9b, 2026-07-17)

Drill 1.9b (delete the whole csi-node pod on pg's node, so node-agent +
csi-driver + spdk-tgt all restart) on **v1.15.0 + f5fix.1** reproduced the
documented landmine and exposed its mechanism:

- spdk-tgt restarts and re-loads the LVS from disk, but the **NVMe-oF
  subsystem/listener/namespace exports are runtime state** that only
  reconcile-on-loss re-creates — and the reconciler's staged-volume records
  are **in-memory in the csi-driver container**. After a pod-level restart
  the reconcile loop runs happily with `success=0 skip=0 error=0`: it has
  nothing to reconcile. (This is why a bare spdk-tgt kill recovers in
  ~45–67s — the surviving csi-driver still knows the volume — but a pod
  delete never recovers.)
- The host initiator survives on ctrl-loss-tmo and reconnect-loops against
  the missing export **forever** (ECONNREFUSED ×253 observed, 5s cadence).
- The node's volume health check reported `healthy=true` and the consumer
  pod stayed Ready throughout — 20+ minutes of dead I/O, zero signals
  (same silent-hang family as F6).
- **Recovery validated:** force-delete the consumer pod. The replacement's
  ControllerUnpublish/Publish cycle rebuilt the export (replacement landed
  cross-node and attached remotely); ~7 min total, **zero VA surgery**.
- Evidence: `tests/chaos/artifacts/1-1.9b-landmine-1784250607/`.

Fix directions (flint work, with F6): reconcile from persistent ground truth
(this node's VolumeAttachments / kubelet staging dir), not an in-memory set;
make the DS/volume health probe actually touch the export path; surface
VolumeCondition so consumers aren't silently dead.

**F8b addendum (drill 1.15, full DS roll, 2026-07-17): the validated
"consumer bounce" recovery only works cross-node.** The 1.15 bounce
rescheduled pg-0 onto the **same node** (runu-aws-2) and never recovered:

- Same-node replacement reuses the already-staged volume — kubelet issues no
  NodeStage, so NodeStage self-heal never runs and the amnesiac reconciler
  (F8) never rebuilds the export. Post-roll the tgt is so bare that even the
  discovery listener refuses connections (ECONNREFUSED on 127.0.0.1:4420) —
  there is nothing target-side for the initiator's reconnect loop to find.
- The doomed postmaster sits in D-state on the dead session; kubelet cannot
  complete the kill (`FailedKillPod: KillContainer ... DeadlineExceeded`),
  so the old sandbox pins the mount while the new pod's postgres fails
  readiness against the same dead filesystem. Wedged 20+ min until manual
  intervention (would self-clear only at ctrl_loss_tmo=1800s).
- **Working manual recipe (validated live):** cordon the node → force the
  dead initiator session down (`echo 1 > /sys/class/nvme/<ctrl>/
  delete_controller`, D-state clears instantly) → delete the consumer pod.
  Cross-node republish then rebuilt the export on the bare tgt
  (`volumeType:"remote"`, listener on the node IP) and pg-0 was Ready in
  **44s** — versus ~7 min in 1.9b, where the stuck unstage had to wait out
  the 6-min force-detach window. Ledger reconciliation: **zero lost acked
  writes** (db PASS).
- StatefulSet consumers have no scale-cycle escape hatch equivalent to
  Deployments: a bare pod delete can land same-node any time the node has
  capacity. Until F8 is fixed, treat the landmine recipe for STS as
  cordon-first, then bounce.

Environmental note from the same verify: the orphaned-mounts check flagged
kubelet-leaked tmpfs/hugetlbfs mounts for deleted pods on **all four** nodes
(including ones the drill never touched) — residue of the F9 eviction storm
(~2.2k Evicted dashboard pods, since deleted; leaked mounts unmounted).
Zero flint volume mounts were orphaned; not a flint defect.

Harness hardening from the same incident (both fixed): `wait_acks_fresh`
raced (the final pre-kill ack looks "fresh" — now requires an ack newer than
T0), and verify-db had no timeouts (a dead volume wedged the whole batch
inside pg_amcheck — every check is now timeout-wrapped).

### F9 (**P1 — cross-node data-plane kill**) — stale NodeUnstage deletes a live subsystem (2026-07-17)

Between drills, a revived node's deferred cleanup destroyed the volume it no
longer owned. Timeline (all UTC, volume pvc-c15f47dd, single replica **on
runu-aws-2**, evidence `tests/chaos/artifacts/1-1.13pre-rofs-1784255332/`):

- 02:20 drill 1.12-rerun: kubelet stopped on aws-2 (pg's node) + oos taint.
  Pod force-deleted by GC — but aws-2's kubelet is dead, so its containers
  and mounts survive untouched.
- 02:21:35 ControllerUnpublish(aws-2) — **fencing works**: aws-2's host NQN
  removed from the subsystem. 02:25:14 ControllerPublish(aws-1) repeats the
  defensive `nvmf_subsystem_remove_host` (#3 disconnect-before-reuse). The
  replacement pod on aws-1 attaches cross-node to aws-2's target; Ready
  02:25:33; verify PASSES (writes flowing).
- ~02:26 the drill's cleanup restarts aws-2's kubelet. It finds the stale
  pod dir → NodeUnpublish (02:26:33.4) → **NodeUnstage (02:26:33.5) →
  `delete_nvmeof_block_device()` (driver.rs ~1175) → agent
  `/api/blockdev/delete_nvmeof` → `nvmf_delete_subsystem(<volume NQN>)` on
  aws-2's spdk-tgt — the subsystem actively serving aws-1's live
  attachment.**
- 02:26:37 aws-1 dmesg: `Buffer I/O error on dev nvme2n1 … lost async page
  write` → ext4 remounts RO → postgres FATAL "Read-only file system" →
  error spam fills aws-1's 8 GB root → kubelet evicts pg-0 for
  ephemeral-storage (02:29:44) → F3-style disk-pressure taints on two nodes.
- The harness's post-drill health gate caught it (1.13's preflight refused
  to start), and teardown after the incident was clean (ns + PVs deleted,
  no finalizer hang).

**The bug:** NodeUnstage's contract is initiator-side cleanup (unmount +
disconnect the local session). `nvmf_delete_subsystem` is target-lifecycle
work — correct only while the unstaging node is the sole consumer. After a
force-detach + cross-node re-attach, the stale node's late unstage deletes
the export under the live consumer. Host-level fencing (F/#3) doesn't
protect the subsystem object itself.

**Fix directions (post-campaign, with F8/F6):** NodeUnstage must not delete
a subsystem that (a) it didn't stage, or (b) has any other live host/VA —
guard by checking the subsystem's host list / this node's VA ground truth;
target teardown belongs to ControllerUnpublish-of-last-attachment or volume
deletion. Durability note: acked writes were WAL-fsynced to the (intact)
lvol before the kill — this is an availability P1, not a lost-acked-write
P0; the post-reset ledger reconciliation will confirm.

### Follow-up: ublk for the local hop (post-campaign evaluation)

The kernel-facing hop is today a loopback NVMe-oF session (kernel initiator
→ TCP over lo → local spdk-tgt) — double TCP traversal plus
subsystem/listener/fencing state for a same-host handoff. The
SPDK-recommended shape is ublk for local exposure, NVMe-oF only cross-node.
Resilience-wise it can match the post-F8 path: SPDK v26.05 ships
`ublk_recover_disk` (+ `test/ublk/ublk_recovery.sh`) for daemon-death
reattach via `UBLK_F_USER_RECOVERY`, and the F8 ground-truth rehydrator
would drive the ublk last hop the same way it drives `ensure_export`.
**Blocked on the fleet kernel:** AL2023 6.1 does not build
`CONFIG_BLK_DEV_UBLK` (`modprobe ublk_drv` → not found, verified on
runu-aws-3 2026-07-17), so loopback NVMe-oF is the only working local path
on these nodes. **And squeezed from the other end:** the Sept-2025 upstream
ublk rework (explicit queue/tag ids, split `nr_io_ready`/`nr_queues_ready`)
broke SPDK's ublk target on kernels ≥6.14 (Ubuntu 6.14.0-33 / 6.17
confirmed; spdk/spdk#3758, filed against v25.05) — and Longhorn's
SPDK-based v2 engine hit a 100%-reproducible kernel NULL-deref panic in
`ublk_init_queues` (node reboot) on Ubuntu 24.04 / 6.17.0-1017-aws
(longhorn/longhorn#13509) — i.e. an API mismatch can take down the NODE,
not just the volume. **RESOLVED for our stack on kernel 6.18.29:**
validated 2026-07-17 — the SHIPPED `spdk-tgt:1.6.0-f5fix.1` (SPDK v26.05
d519b163c) runs ublk cleanly on 6.18.29-061829-generic (mainline), no
panic, clean start/stop.

**Measured A/B (2026-07-17):** one x86 spot i4i.large (2 vCPU), Ubuntu
24.04 + mainline 6.18.29, the shipped spdk-tgt image (`spdk_tgt -m 0x1`),
same 1 GiB malloc bdev (RAM-backed ⇒ pure exposure-path measurement),
identical fio suites (io_uring, direct=1, 20s/5s ramp). ublk: 1 queue,
QD 128. NVMe-oF loopback: kernel initiator over 127.0.0.1:4420.

| case | ublk | NVMe-oF loopback | delta |
|---|---|---|---|
| 4k randread QD1 | 41.0k IOPS, 17.8µs avg, p99 27.5µs | 33.4k IOPS, 20.0µs avg, p99 35.6µs | **ublk +23% IOPS, −2.2µs** |
| 4k randread QD32 | 123.9k IOPS | 59.3k IOPS | **ublk +109%** |
| 4k randwrite QD32 | 117.3k IOPS | 58.2k IOPS | **ublk +101%** |
| 128k seq read QD8 | 2388 MiB/s, p99 2.0ms | 2869 MiB/s, p99 473µs | **loopback +20% BW, far better tail** |

Read: for small-block DB-style I/O the loopback TCP double-traversal
costs ~half the achievable IOPS — ublk is ~2× at QD32 and modestly better
at QD1. For large sequential transfers loopback nvme-tcp WINS (+20% BW)
and has a much tighter tail; ublk's per-op copy path and single server
queue dominate there (untuned — more ublk queues may close it). Caveats:
2-vCPU box (fio and the reactor contend), malloc backend, local hop only.

**Conclusion:** the hybrid (ublk local hop + NVMe-oF cross-node) is a
real win for the IOPS-bound path and is viable on kernel 6.18.29+ with
our shipped image — but the fleet runs AL2023 6.1 with no ublk_drv, so
adoption is gated on a fleet kernel change (or a custom kernel/AMI in
trove, whose cloud-init is AL2023/dnf-specific today). Sequential-heavy
workloads would keep loopback. Follow-ups if pursued: driver ublk path
needs F8-rehydrator coverage (ublk daemon state dies with the pod too;
SPDK v26.05 has `ublk_recover_disk` for the reattach) and per-workload
backend selection.

### Other findings

- **P0-a / P0-b** (trove provisioning) — see Phase 0. Not flint bugs; recorded
  for the trove backlog.
- Pre-existing orphaned NVMe session observed after kuttl create/delete churn
  (a controller stuck `connecting` for a deleted PV, 1800s ctrl-loss-tmo).
  Cleaned before drills; flagged to reproduce deliberately in the churn drill
  (1.10) to determine whether flint leaks NVMe sessions on rapid volume delete.

## Patch hygiene (2026-07-16)

The F5 fix was developed on a throwaway `f5fix` branch in the local spdk
checkout, then exported to the repo's patch files. Per project policy the
patch files are the only artifact: applying `nvmf-hostlog` + `ublk-debug` +
`blob-recovery-batched` + `lvol-flush-sync` onto pristine **v26.05**
(d519b163c, the latest SPDK release; `Dockerfile.spdk` pins `git checkout
v26.05`) was verified byte-identical to the branch on every touched file, and
the branch was deleted. `spdk-tgt` is rebuilt from the committed Dockerfile +
patches alone (tag `1.6.0-f5fix.1`) to prove the patch-only path end-to-end.

## Teardown

2026-07-13, on user request (pause + stop costs): `flint-chaos` ns deleted —
PV released and deleted through the driver in ~30 s **from a CrashLoopBackOff
consumer** with no finalizer hang, zero VAs left (a clean-detach datapoint in
itself). Trove project 36 deleted; all 4 EC2 terminated; spot/EBS/EIP orphan
sweep clean; kubeconfig removed. Campaign resumes on a fresh cluster at
drill 1.3 (repro of F1).

### Phase 2 nvmeof (loopback + kernel initiator), 2026-07-18 — runv, ublk.13 driver

**r2 matrix ALL PASS** (orchestrators on, PHASE_LABEL=2): 2.1
remote-leg csi kill — worst ack 14s, raid re-joins; 2.2a RAID-host tgt
SIGKILL — repair_data_path 139s; 2.2b RAID-host pod delete — ~208s
actual outage, self-healed (monitor path; the ublk detector fast-path
has no nvmeof equivalent yet — candidate follow-up); 2.3 remote NODE
kill — rode through, worst ack 0s; 2.5 migration 24s; 2.6 churn ×10
(10-22s cycles). All db PASS. nvmeof recovery on RAID-host vectors is
slower than ublk (139-208s vs 31-33s) but fully automatic.

**2.4 (☠ REAL node terminate) PASS on r3**: terminated pg's node
(aws-5, holding pg + one of three legs) — pg Ready 310s cross-node,
db PASS, independent heap probe MISSING=0. Clean-run timing for the
node-loss recovery: ~5min (NotReady detection + taint + reschedule +
stage backoff), served from the surviving legs.

**2.7 nvmeof: N/R** — first attempt aborted on an over-applied oracle
relocation (fixed: csi-pod kills never harm pg-load), rerun aborted on
replica discovery, and the 2.4 finale then consumed the third storage
node. Mechanism coverage stands via ublk 2.7 (triple simultaneous leg
bounce, 21s) + nvmeof 2.1 (same SPDK-initiator reconnect machinery).

**F11 (OPEN, store-durability)**: aws-6's blobstore — created THIS
NIGHT, f5fix-only lifetime — went terminally unloadable after the
dirty-kill barrage (`blob_parse: Blobid (0x100000000) doesn't match
metadata (0x100000001)` → super blob unopenable). f5fix does NOT fully
protect store metadata under repeated dirty kills; remediation is
`bdev_lvol_create_lvstore` straight over the corrupt store (no wipefs
needed — improves on the F7-era recipe). The drills through those
kills all PASSED: replicas absorbed the store loss, which is the
r2/r3 value proposition working as designed. Fleet guidance: replicated
SCs make single-store death a survivable node-class failure.

**Phase-2 verdict:** r2/r3 chaos matrix green on BOTH backends after
the fix wave (U9 ublk r2 repair, F8-amnesia seed, degraded-assembly
floor, single-survivor direct serve, orchestrators default-on).
Post-v1.16.0 commits — next release carries them. Open backlog: U11
replica re-placement, F11 store-md hardening, nvmeof detector-tick
repair parity, drill 1.14/2.4 AD-timer budget, nvmeof 2.7 on a
restored 3-storage-node fleet.

## Phase 3 continuation (runz, 2026-07-21) — F33 acceptance run

Cluster runz (trove 44): spot-only incl CP, k8s **1.34.9**, workers on
mainline 6.18.29, **kube-apiserver audit log enabled on the CP**
(RequestResponse for pods/eviction/binding in flint-chaos) — armed
specifically to attribute the pg-0 cross-node recreation seen twice on
u12.4. Stack u12.5 = 9c0ce9b (F29 staging-liveness, F30 volume-identity
marker, ublk DEL_DEV escape hatch) over spdk-tgt 1.6.0.

### Drill 3.6 (first run, u12.5): FAIL — and the most productive drill of the campaign

Timeline: kubelet-stop on the server node → resurrect on the surviving
replica node at **69s** → fence armed at boot (90s/10s) fired at
**~87s after the resurrect fenced the store** ("backing store
unresponsive past deadline") — **F33 detection validated**.

**F33b (P1, FIXED 29b3071): the fence's exit never completed.** Worker
threads sat in D-state on the fenced ublk raid; `exit_group` cannot
reap uninterruptible threads. The corpse held its TCP sockets 40+ min
(finally became a zombie only after kubelet restore let the I/O error
out) — no FIN/RST ever reached the clients, so witness AND pg-0 hung
exactly as pre-F33. Fix: `fence::fence_exit` — `shutdown(SHUT_RDWR)`
every socket fd (from a /proc/self/fd census) BEFORE exiting; socket
shutdown cannot block on the dead filesystem, so clients get EOF and
re-resolve through the per-volume Service even if the exit wedges
forever. Verified by unit tests (peer-EOF delivery); live acceptance =
3.6 rerun on u12.6 (witness_recovery metric now recorded by the
harness).

**pg-0 cross-node recreation ATTRIBUTED (the u12.4 3.2 mystery).**
Caught live mid-hang by the audit log: kubelet evicted pg-0 —
`phase=Failed, reason=Evicted, "node was low on resource:
ephemeral-storage"` (6.8MB free on the 8GB root, filled by the
error-flooding stalled workload) — then the **StatefulSet controller
deletes Failed pods and recreates them** (audit: statefulset-controller
DELETE + CREATE in the same second; scheduler bound the replacement
cross-node). Not a k8s-1.34 bug: standard Failed-pod replacement. It
reads as "unattributed" post-hoc because events expire (1h TTL) and
the eviction reason lives in the deleted pod object. Root fix is the
F3 trove backlog (8GB worker roots); the audit-log recipe is the
diagnostic tool of record.

**Contaminated db verdict**: "666 lost acked writes" was the ledger
comm running against an unreachable postgres (empty seq list → comm
counts every acked write missing). Harness now skips the comm when
pg_isready fails (loss = UNKNOWN, drill already failed on isready).
Witness check similarly reported a vacuous "mismatches=0
last-write-age=<raw epoch>" on a timed-out mount read — now reports
UNRESPONSIVE, and 3.6 gained `wait_witness_fresh` (witness_recovery=Ns
is THE F33 acceptance metric; drill FAILs if it never recovers).

**F34 (P2, OPEN, driver)**: after the drill's kubelet restore, the
csi-node driver's gRPC UDS listener on that node was dead while the
container stayed Running/ready (socket file present, no listener —
kubelet mount retries got EOF then connection-refused; pg-0's
replacement blocked ~10 min). No driver restarts recorded → the accept
loop died silently inside a live process, and liveness never caught
it. Unstick: delete the csi-node pod (safe — node hosted no active
flint volumes). Needs: liveness that actually dials csi.sock +
listener-death → process-exit coupling.

### Ops recipes added (runz)

- **Hung-client unstick without waiting out TCP timeouts**: the kernel
  NFS client's socket lives in the netns where mount(2) ran — NOT the
  workload pod's netns (conntrack showed the flows under the host
  netns with the csi-node pod's source IP on this Cilium cluster).
  Recipe: sweep every distinct netns via /proc/*/ns/net, `ss -tn
  '( dport = :2049 )'` in each, `ss -K` the stale flows; the client
  reconnects through the Service to the live backend instantly.
- **Audit-log deletion-actor extraction**: RequestResponse policy on
  pods in the chaos ns; the DELETE event's responseObject carries the
  final pod status (phase/reason/eviction message) — the smoking gun
  survives pod deletion, unlike events.
- Trove wart: create-commit FLATTENS heterogeneous server rows to the
  cluster default instance type — add the c5d builder via scale-out
  (servers/create + commit) AFTER initial provisioning; this build of
  trove tags instances `trove:*` (not `trove/<name>/*`) — teardown
  audits must filter accordingly.

### Drill 3.6 run 2 (u12.7): FAIL — F36 opened; F30 pays for itself live

Run shape: server on aws-1 → kubelet-stop + OOS taint → resurrect on
aws-3 at 94s. Fence fired at 93s stale (detection ✓ again). The stale
witness flow DIED this run and the orphan zombied within minutes (vs
run 1's 40-min D-state corpse) — consistent with F33b's socket FINs
working — but the witness still never recovered, because the
resurrected server itself went down:

**F36 (P1, OPEN): the resurrect's own fencing can kill the new
assembly.** Forensic timeline (full logs in
artifacts/3-3.6-1784654616/forensics/):
- 17:24:41 OOS force-detach → ControllerUnpublish(backing vol, aws-1)
  — which itself FAILED early on an F32-family metadata lookup
  ("volume metadata not found in PV"), a separate wart.
- 17:25:04 stage on aws-3: leg-0 export ensure on aws-1 FAILED —
  `bdev ... already claimed: type exclusive_write by module raid` (the
  old node's raid was never unstaged; kubelet down; loopback claims
  are invisible to nvmf fence-out, which only fences REMOTE
  consumers). Assembly proceeded on leg-1 (aws-2) alone:
  add_host(aws-3) + remove_host(aws-1) ✓, attach with
  ctrlr_loss_timeout=-1 ✓, ublk0 up, ext4 mounted, data present.
- 17:25:49 the leg-1 qpair dropped; subsequent reconnects were DENIED
  (`nvmf_qpair_access_allowed: does not allow host` loop on aws-2) —
  the ACL/fence state ended up excluding the LIVE consumer. With
  reconnects denied, EIO reached ext4 → journal abort → fs shutdown →
  /dev/ublkb0 later gone (clean SPDK stop on bdev loss).
- Server restarted → **F30 refused the dead export (exit 57) — the
  loud-refusal design working exactly as intended** (pre-F30 this
  identical state silently served garbage on runx). CrashLoop → no
  ready endpoint → witness/pg-0 had nothing to fail over TO.
- No DEL_DEV activity anywhere (escape-hatch gating held). The agent
  never detached the controller (reaper innocent).

Open attribution residual: whether remove_host raced add_host on the
same subsystem or a second fence pass removed the new consumer —
narrows inside the resurrect's export-ensure sequencing
(nvmeof_export). Fix direction: fence-out must be ordered/idempotent
against the incoming consumer's ACL (never remove a host that a
concurrent ensure just admitted), and a denied-reconnect loop on a
live consumer must surface as a health event, not silent EIO.

**Also fixed from this run: exit-path observability** (4bf8d74) — the
F30 refusal reason and fence_exit's sockets_shutdown line were LOST
because process::exit skips the non-blocking appender flush; critical
exits now eprintln! first. The one-line diagnosis this enables was
worth an hour of SSM forensics today.

db verdict run 2: honest FAIL (isready; ledger SKIPPED by the new
isready gate — no fabricated loss number), plus one orphaned initiator
session on aws-3 (the fenced-out controller, connecting forever —
cleaned by re-assembly). Zero acked-write loss confirmed after
recovery, run 2 included.

### F36 attribution COMPLETE (dmesg + tgt cross-timeline)

Kernel log (forensics/dmesg-aws-3.txt): 17:25:48 mass WRITE I/O errors
on ublkb0 → `EXT4-fs: shut down requested (2)` → journal abort — 24s
after aws-2's tgt logged `Snapshotting blob` + `Lvol f3fe782b deleted`
(17:25:24). The ACL theory is DEAD (exactly one add_host(aws-3) + one
remove_host(aws-1) in the whole window; the 2s denial loop was aws-1's
fenced initiator). The real chain:

1. Resurrect stage on aws-3: leg-0 (aws-1, in_sync) blocked by the old
   raid's exclusive_write claim → degraded assembly served from
   **leg-1, which replica-sync still recorded as STALE**.
2. aws-2's reconciler correctly skipped the stale leg ("not in_sync —
   export owned by the catch-up orchestrator", skip_count=1).
3. The catch-up orchestrator did its normal stale-head
   snapshot+delete — **deleting the lvol under the live export**.
   Target-side rejection completes I/O with error (U7's loss_tmo=-1
   protects connection loss, NOT invalid-namespace status) → ext4
   shutdown → F30 refusal loop.

F36 = two missing guards, one on each side of the volume_claims
contract: (a) degraded assembly from a not-in-sync leg must TAKE the
per-volume claim (or be refused — serving a stale leg silently is its
own data-integrity question); (b) the catch-up orchestrator must check
for live consumers (claim + nvmf controllers) before deleting a head.
Fix next session; the claim plumbing (volume_claims) already exists.

Collateral data damage + repair: pg_xact/0000 lost its unfsynced tail
(81920 bytes, needed 98304) when the ext4 died mid-write — WAL
(fsynced) survived, so redo FATALed on a short SLRU read
("could not access status of transaction"). Repair: zero-extend the
file to the needed page boundary (truncate -s) — redo re-derives the
commit bits from WAL; zero acked loss expected (U8-class pg_xact
finding, now with the exact repair recipe).

### F36 UPGRADED TO P0: real acked-write loss + lineage fork (post-recovery audit)

Post-recovery ledger audit: **752 of 3161 acked writes missing** — all
acked 17:18-17:19Z (PRE-incident, healthy harness, leg-0 serving).
Mechanism: the crashloop recovery reassembled from the STALE leg-1
lineage a second time (leg-0 still claim-blocked on aws-1), so the
recovered DB is leg-1's past; post-recovery writes then forked onto
that stale lineage. Neither leg now holds the full history —
split-brain materialized end-to-end from one node-kill drill:
- leg-0 (aws-1): true pre-incident state incl. the 752 seqs;
  claim-frozen; preserved as lvol snapshot
  `f36-forensic-leg0-1784656900` (uuid e1d81ef2) + epoch snapshots
  103-105.
- leg-1 lineage (current serving fs): missing 25351-26103, owns
  everything after.

This is the campaign's FIRST real durability failure, and it is
entirely F36's two missing guards (stale-leg assembly without the
volume claim; stale-head delete without a consumer check) plus a third
now visible: **degraded assembly must prefer / require the in_sync
leg, and must be able to BREAK a dead node's stale claim safely**
(leg-0 was the right choice both times and was skipped both times for
the same mechanical reason). Verdict rows for 3.6 runs 1-2 stand;
run-2 db verdict now reads: REAL LOSS 752 acked (F36), not artifact.

### Drill 3.6 run 3 (u12.8, full guard stack): the F33 acceptance LANDS; residual = F36c

**witness_recovery=94s** (vs NEVER in runs 1-2 and 93min on runy2) —
resurrect 93s, pg-0 Ready 95s with 0 restarts, no manual intervention
anywhere. F33 detection + F33b socket-FIN + F35 + both F36 guards all
executed live (the run also incidentally validated the whole self-heal
chain when a spot reclaim + a botched ghost-cleanup force-delete
landed mid-roll: fence eprintln line captured, 6 sockets FIN'd, exit
wedged harmlessly in D-state, guard-2 freed leg-0 after the node
reboot).

Residual (drill FAIL components, honestly): **6 acked writes lost**
(consecutive tail 44026-44031) + heap/index tears (amcheck rc=2, page
checksums) — the resurrect assembled from leg-0 before catch-up had
fully equalized it with leg-1's freshest writes. This is **F36c**, the
deliberately-deferred third guard: assembly-side in_sync requirement.
Design tension to resolve before implementing: phase-2's
single-survivor direct serve (drill 2.4's zero-loss headline) WANTS
serve-anything under node loss; F36c wants freshest-or-refuse. The
answer is likely "serve the most-current REACHABLE leg; refuse only
when a fresher leg is known to exist and reachable" + surface
acked-tail-risk as a VolumeCondition. 752 lost (run 2, no guards) →
6 lost (run 3, guards a+b) — the residual is now bounded to the
catch-up delta at kill time. nvme-leak component was transient
(connecting-to-dead-node during the drill window; live again after
node restore). Harness reset required post-run (torn pages make the
bench DB an invalid oracle; forensic snapshot e1d81ef2 released with
it — attribution + logs preserved in artifacts/3-3.6-1784663315/).

### Drill 3.2 (u12.8) + operational findings

3.2 substance PASS: server recreated 29s (reconciler), **zero ESTALE**
(the original 3.2 defect class confirmed dead), witness clean, **db
PASS** (ledger + amcheck fully clean), pg-0 Ready 134s / stall 129s
(reconnect tail). Recorded FAIL components were environment:
attribution=rescheduled was ANOTHER ephemeral eviction (see below);
nvme-leak was **F37 (P2, NEW)**: the 29s same-node recreate races
NodeUnstage — old ublk id 0 + leg controller linger next to the new id
1 on the SAME raid bdev (unmounted leak, not split-brain; reapers
protect it because the PV exists), and the ublk-id annotation went
stale (0) until the rehydrate backfill self-corrected it to 1 within a
tick. Manual clean: ublk_stop_disk on the non-serving id. Fix shape:
unstage-vs-restage ordering (stage should reap same-bdev strangers).

Eviction epidemic root-caused: THREE pg-0/server evictions today all
showed victim usage=24Ki — the pods were sacrificial (request:0 ranks
first), the node pressure came from containerd image-pull UNPACK
spikes (~2x image size transient) on 8GB roots (F3). ephemeral-storage
requests (43f2ad6) armor the ranking but also tighten allocatable (the
threshold message grew 851MB→1.27GB) — the real trigger-kill is
PRE-PULLING all in-use images on every worker (done; 2.0-3.6GB steady
headroom). F3 (bigger roots) remains the structural fix.

### F29 incident replay (u12.8, k8s 1.34.9): vector closed upstream

Replayed the runx shape both ways on a live RWX volume: out-of-band
`umount -l` of the backing globalmount + (a) graceful server delete,
(b) FORCE delete (the original incident used force-delete to skip
NodeUnstage). Both healed: kubelet 1.34's volume reconstruction ran a
**fresh NodeStage before NodePublish** (agent log: Stage CALLED →
Publish CALLED), the F29 probe saw a live staging, and the server came
up with the correct F30 marker + real data. The 1.33-era
skip-restage-after-force-delete vector no longer exists on 1.34; F29's
NotMounted/Dead refusal arms remain as unit-tested defense-in-depth
(the mid-life store-death shape — run 2 — bypasses publish entirely:
the fs died under an already-published mount, where the fence/F30/F35
chain owns recovery instead).

### DEL_DEV live gate (386027a): PASS on real 6.18 kernel

c6gates test pod (privileged, hostPath /dev) on w5: del_dev_live
deleted scratch /dev/ublkb42 via UBLK_U_CMD_DEL_DEV io_uring SQE128
uring_cmd in <100ms. The escape hatch's submission path is proven
end-to-end; residual = the EPERM-recover classifier variant (dead
device answering EPERM instead of double-ENODEV — seen once after an
in-flight-roll force delete).

### 2.2a r2 spot-check (u12.8): PASS — ready=16s, stall=10s, db clean
(beats the phase-2 31s baseline).

## Post-campaign backlog (v1.18.0 cut, 2026-07-21)

v1.18.0 = the runz-validated wave (F29/F30/F32-fix/F33/F33b/F35/F36ab/
DEL_DEV), multi-arch amd64+arm64. Shipped to main + release commit
e1b1288. Backlog worked after the cut (unvalidated → v1.19.0 material):

- **F34 (FIXED, 84c3c8c)**: bounded NFS consumer mount — see commit.
- **DEL_DEV EPERM variant (FIXED, 84c3c8c)**: classifier widened.
- **F36c (IMPLEMENTED 2026-07-21, UNVALIDATED —
  docs/f36c-assembly-freshness-gate.md)**: the last-writer-set assembly
  gate (rev 1's epoch rank ties in the run-3 scenario — both legs read
  the same `last_epoch`; the post-cut tail is recorded nowhere).
  Shipped shape: `WriterSet` on the sync record (wholesale stamp
  record-before-writes at assembly; shrink only via mark_stale; grow on
  admission), pure gate logic in `freshness_gate.rs` (unit-tested incl.
  the run-3 defer and the 2.4 immediate-serve shapes), wired
  gate-before-forced-stale in `create_raid_from_replicas`; claim-block
  counts as writer-set membership; defer bound = wall-clock
  `flint.io/f36c-defer` (180s default, re-armed on missing-set change);
  permanent branch serves + `flint.io/acked-tail-risk` + AckedTailRisk
  event; kill switch FLINT_F36C_GATE=disabled (chart default enabled).
  RELEASE GATE: drills 3.6c AND a 2.4 re-run (gate enabled, first-tick
  survivor serve, marker raise/clear) on a live cluster. The remaining
  P1 until validated.
- **F37 (IMPLEMENTED 2026-07-21, UNVALIDATED)**: the ~29s same-node
  ublk recreate races NodeUnstage — a stale ublk id lingers on the
  SAME raid bdev as the new id (unmounted leak; only the ublk-id
  ANNOTATION self-heals, via rehydrate backfill). NO reaper owned the
  device — earlier "reapers clear it / PV-gated pass owns it" wording
  was wrong: the rehydrate reap loop skips any live id whose bdev is
  in the desired set, and raid bdevs are non-attributable to it
  anyway; pre-fix the leak persisted until a manual ublk_stop_disk
  and burned toward ublks_max (64/node). Shipped shape:
  `ensure_ublk_disk` sweeps same-bdev strangers on every ensure path
  (stage, rehydrate, detector) BEFORE serving its own id, gated on
  the opener-free probe — O_EXCL open of /dev/ublkb<id> (EBUSY while
  any fs/holder has it; catches lazy `umount -l`, the F29 replay's
  own move) + /sys/block/ublkbN/holders. Opener'd stranger + our id
  not yet serving → refuse the duplicate start, retry next tick;
  opener'd stranger + our id already serving → split consumer, never
  EIO either side, log the runbook line; stop races the in-flight
  unstage benignly (ENODEV tolerated both sides); stop failure
  degrades to the pre-F37 leak, loudly. Unit-tested
  (node_agent::f37_tests). RELEASE GATE: the drill — force-delete a
  same-node RWX consumer mid-unstage, assert the reap fires ONLY on
  the opener-free device, including the lazy-unmount negative case
  (mounted-invisible stranger NOT reaped, no consumer EIO).
- **F3 (trove/infra)**: 8GB worker roots + image-pull unpack spikes =
  eviction roulette. Mitigations landed in the harness (ephemeral
  requests + fleet pre-pull); structural fix = bigger roots at
  provision time.

## v1.19.0-rc1 validation day (runaa, 2026-07-22)

Cluster: **runaa** (trove project 45) — 6× i4i.xlarge spot workers +
on-demand CP + cordoned c5d.4xlarge spot builder (DS-excluded via
`flint.io/builder` nodeAffinity — the chart's top-level affinity value;
cordon alone does not stop a DaemonSet and the tgt crashloops on a
node whose NVMe belongs to Docker, wedging the roll). Driver
`dilipdalton/flint-driver:1.19.0-rc1` (a33246c) rolled over the trove
1.17.0 install; FLINT_F36C_GATE=enabled verified in-pod. The trove
disk-init gap reproduced AGAIN — `/api/disks/initialize_blobstore` on
`0000:00:1f.0` per worker (the GET /api/disks route hangs; use the
POST RPC-style routes). Harness: SC=flint-r2 MODE=RWX WITNESS=1,
fleet pre-pull done.

**Writer-set first-light:** the very first NodeStage assembly stamped
`writer_set` = both legs on the sync record (since=02:44:07Z), legs
aws-1 (server-local, disk-follows-pod) + aws-2.

### Drill 3.6c (F36c TRANSIENT) — GATE PASS; db verdict pending recovery

Degrade phase finding: spdk-tgt kill on the remote leg did NOT shrink
the writer set — v1.15 survivable-reconnect healed the leg before the
staleness detector fired (1 tgt restart, leg back in_sync). The set
correctly kept both legs; the drill proceeded on the both-writers arm.

Server-node kill (kubelet stop + OOS taint, old tgt keeps the claims —
the adversarial variant): **the gate deferred assembly 9 times**
(driver logs + AssemblyDeferred events) while the fresh leg was
claim-blocked on the dead node, **StaleReplicaAdmitted=0** (the
pre-F36c forced-stale path never fired), no acked-tail-risk raised
(transient branch held), resurrect on aws-2 at 561s, witness recovered
575s, max ledger stall 38s. The 561s (vs 3.6's 94s) is the variant's
cost: the claim only clears once fencing/reboot catches up — and it
exposed an operational corollary: **an outage stretched past the NFSv4
lease horizon leaves clients with unreclaimable handles** (pg-0 ESTALE
loop on `global/1262`; witness on another node healed fine at 575s).
Recovery = client bounce (fresh mount + WAL replay) — same recipe as
the DS-roll landmine. Recorded FAIL components ready+db are this
recovery, NOT loss: the db check was SKIPPED-unreachable at drill end;
definitive ledger+amcheck verdict re-run post-bounce (below).

### 3.6c definitive db verdict: PASS — the F36c transient bar LANDS

Post-recovery ledger + amcheck (pg bounced for a fresh mount after the
lease-horizon ESTALE): **all 1423 acked writes present, amcheck clean,
writable** — ZERO acked-write loss through a server-node kill with a
mid-rebuild trailing leg and a claim-stranded fresh leg. Run-3 on runz
lost 6 writes on this shape; rc1's gate loses none. The recorded
FAIL row (ready+db) is the recovery mechanics, superseded by this
verdict.

rc1 residuals found by the drill, FIXED in rc2 (74c6a1c): risk-marker
amnesia (ServeWithRisk stamps the survivor as sole writer; the next
tick's "all writers attached" cleared the annotation ~90s after raising
it — now clears only on flagged-leg rejoin/replacement + evented as
AckedTailResolved) and the claim classifier missing the live shape
(zombie-raid claim = nvmf_subsystem_add_ns SPDK -32602 "Invalid
parameters", no "claim" in the message).

### F38 (P1, NEW): re-entry export-drop livelock + its real enabler

The 3.6c aftermath surfaced a destructive loop: consumer-blindness
monitor (60s tick, 3-strike) → repair_data_path →
create_raid_from_replicas → drop_stale_local_exports severs the
volume's OWN live consumer export (direct-serve broke the "exports the
raid bdev, never the lvol" assumption written at the drop site), fs
EIOs under the export, F30 refusal crashloops, repeat. Recovery = nfs
pod recreate (fresh stage), ~30s. Design doc:
docs/f38-reentry-export-drop.md. THE ENABLER (F32-class, one line,
FIXED rc3 71d7330): the degraded-direct exemption was written to the
USER PV while the monitor reads the BACKING PV for RWX — so every RWX
direct serve entered the loop within 3 ticks, outage or not. The full
layered fix (re-entry idempotence, self-chain drop exemption via local
hostnqn qpairs, F30-crashloop → pod-recreate escalation, 3.6e drill)
is specced for the next wave. Operational corollary recorded: a
transient-branch defer that stretches an outage past the NFSv4 lease
horizon leaves clients with unreclaimable handles — bounce the
consumer (documented recipe; F38's fix-d automates it).

### Drill 3.10 (F37): PASS — dup-free under 3× same-node recreate race

Force-delete of the nfs server pod ×3, every recreate landing on the
SAME node (the F37 window): **dups_max=0** (26s/22s/38s settle), F37
reap lines 0 / busy-refusals 0 — the unstage won every race on
k8s 1.34 + rc2, so the stage-side reap validated as "defect class
absent under provocation" while the reap trigger itself stayed cold
live (unit-suite coverage only — honest status). Definitive db verdict
post-recovery: **PASS, all 385 acked writes present, amcheck clean**
(the recorded ready/db FAIL components were WAL-recovery + probe
timeouts under amcheck load, no space/pressure involved). Witness 0
mismatches.

### 3.6c rc3 re-run: PASS — rc2/rc3 fixes validated live; heal-tail observation

Gate: 9 defers, StaleReplicaAdmitted=0, resurrect 529s, witness 543s,
pg-0 rescheduled 760s (server/client CO-LOCATED this run — the
attribution "FAIL" is that topology, not a defect). db: ledger PASS at
drill time (973/973), clean re-verify PASS (1104/1104 + amcheck).
**rc2 fix live**: flint.io/acked-tail-risk persisted across assembly
ticks and named the stranded leg exactly. **rc3 fix live**:
degraded-direct landed on the BACKING PV; ONE single-survivor assembly
total vs rc1's one-per-90s livelock.

Heal-tail (teardown cut it short, recorded honestly): the stale leg
parked at `stale` with retention pin epoch-3 and **zero catch-up
activity logged in 30 min** — catch-up appears not to trigger for a
direct-serve RWX volume's stale leg (no raid to hot-rejoin, no
restage without rejoin-bounce which is default-off, and no visible
chase). OPEN (v1.19 gate): direct-serve reintegration path — likely
the same F38 fix-d surface; investigate the catch-up trigger for
raid-less serving entities. AckedTailResolved lifecycle therefore
NOT yet observed live (unit-tested only).

### runaa teardown (2026-07-22, user-directed cost stop)

Remaining for the release gate, to run on the next cluster: **3.6d**
(permanent/terminate variant — rc3 makes it honest), **2.4 re-run**
(RWO r2, gate enabled), **AckedTailResolved live observation**, and
the F38 layered fix + 3.6e. Driver image for resume:
`dilipdalton/flint-driver:1.19.0-rc3` (43c1b24 / 71d7330).
