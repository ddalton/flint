# flint CSI attach/detach chaos campaign

Postgres-under-load chaos drills for the CSI attach/detach path, progressing
RWO r1 → RWO r2(/r3) → RWX. Closes the node-failure / VA-cleanup /
pod-lifecycle gaps flagged HIGH-PRIORITY in
`tests/system/MISSING_CRITICAL_TESTS.md`. Campaign writeup:
`spdk-csi-driver/docs/attach-detach-campaign-2026-07.md`.

## Release gate: `restart-gate.sh`

A flint-csi-driver chart is not pushed without a restart-gate record:
`scripts/release.sh` refuses unless
`gate-records/flint-csi-driver-chart-<version>.txt` starts with
`RESTART-GATE PASS flint-csi-driver-chart <version>`, or
`RESTART_GATE_WAIVER="<why>"` is set (printed into the release log).

```sh
# Flint <version> installed with the candidate images, chaos SCs applied
KUBECONFIG=~/.kube/<cluster>.yaml AWS_REGION=<region> AWS_PROFILE=rolesanywhere \
  tests/chaos/restart-gate.sh <version>      # ~45 min; writes the record on PASS
```

It runs `kernel-deadline-test.sh` (a same-node kernel controller waits out a
target outage; the `ctrl_loss_tmo` backstop fails it; `delete_controller`
releases a blocked unmount), then RWO r3 2.2a, 1.9 twice, 2.7, 2.11 and RWX
3.1, 3.2, 3.3a twice, 3.8 — every drill that restarts or kills spdk-tgt or a
csi-node pod under a mounted volume. Needs 3 storage nodes (the control plane
may be one) and SSM access for the kill drills.

**Why it is a gate.** These drills last ran on 2026-07-21. On 2026-08-02 the
consumer's kernel initiator got a 20 s fail-fast bound; it passed every unit
test and every check, and made every slower restart end in a read-only
filesystem until the drills ran again on 2026-10-22 (fixed in 1.60.2). Run it
for any release that touches the node agent, the kernel connect/reconnect
policy, NodeStage/Unstage/Publish, the raid or export recovery, the spdk-tgt
image, or the csi-node DaemonSet's chart values; waive it only for a release
that changes none of those.

## Oracle

- `pg` StatefulSet (postgres:16, `--data-checksums`, `synchronous_commit=on`,
  no liveness probe, chaos sidecar in a shared PID namespace).
- `pg-load` Deployment (anti-affinity to pg): pgbench pressure + a **ledger
  loop** appending only ACKed commit seqs to `/acked/acked.log`.
- Verdict (`verify-db.sh`): any acked seq missing from the `ledger` table =
  **lost acked write** = FAIL. Plus `pg_amcheck --heapallindexed`, log-corruption
  grep, writability probe.

## Usage

```sh
export KUBECONFIG=~/.kube/runs.yaml AWS_PROFILE=rolesanywhere
cd tests/chaos

SC=flint MODE=RWO ./deploy-harness.sh up   # or SC=flint-r2 / MODE=RWX WITNESS=1
./baseline.sh capture                       # clean-state snapshot per node
./drills/phase1.sh 1.2                      # one drill per invocation
tail -1 results.csv                         # verdict row appended per drill
```

Per-drill artifacts land in `artifacts/<phase>-<drill>-<t0>/` (driver logs,
VA dumps, nvme state, db verdict).

Those captures are evidence of what a cluster actually held at a moment,
so they are never rewritten. Runs older than the `chert.us` rename show
the identifiers of their day — `flint.io/*`, `nfs.flint.io/*` — and that
is correct: a capture edited to agree with today's keys would be a
capture claiming a cluster printed something it never printed. Read them
as dated, not as current.

## Cluster assumptions

- trove-provisioned k8s with flint chart (SPDK three-container mode) in
  `flint-system`; campaign SCs `flint`/`flint-r2`/`flint-r3` (all
  WaitForFirstConsumer — do NOT reuse the system suite's multi-replica SC, it binds
  Immediate).
- Workers with instance-store NVMe (i4i.*): **r1 data dies with the node** —
  drills 1.13/1.14 are clean-failure drills by design.
- SSM access to nodes (`AWS_PROFILE=rolesanywhere`) for kubelet/spdk-tgt
  kills and node restore; EC2 stop/start/terminate for the ☠ drills.
- NVMe reconnect defaults under test (v1.15.0):
  `ctrl_loss_tmo=1800s`, `reconnect_delay=5s` (spdk-csi-driver/src/nvme_recovery.rs).

## Drill index

Phase 1 (RWO r1): 1.1 pkill -9 in container · 1.2 graceful delete · 1.3 force
delete · 1.4 cordon+delete cross-node · 1.5 drain · 1.6/1.7 controller kill
mid-attach/mid-detach · 1.8 controller absent through migration · 1.9 spdk-tgt
hard kill (v1.15.0 graceful recovery) · 1.9b csi-node pod delete (landmine
probe) · 1.10 churn ×20 · 1.11 kubelet stop no-taint (slow path) · 1.12
kubelet stop + out-of-service taint · 1.13 ☠ instance shutdown · 1.14 ☠ EC2
terminate · 1.15 ☠ DS roll (expected-fail + recovery).

Phases 2 (r2 RAID1) and 3 (RWX/NFS) drivers are added when those phases
start; see the campaign writeup for the full matrix.

## Safety rails

- Never force-delete a Terminating pod without the out-of-service taint —
  that manufactures the stuck-VA state drill 1.11 measures organically.
- All csi-node DaemonSet env changes happen before workloads exist (a DS
  roll under mounts is the landmine — drill 1.15 exercises it deliberately,
  LAST in phase).
- Controller is replicas=1: idempotency drills run strictly serially.
