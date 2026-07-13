# flint CSI attach/detach chaos campaign

Postgres-under-load chaos drills for the CSI attach/detach path, progressing
RWO r1 → RWO r2(/r3) → RWX. Closes the node-failure / VA-cleanup /
pod-lifecycle gaps flagged HIGH-PRIORITY in
`tests/system/MISSING_CRITICAL_TESTS.md`. Campaign writeup:
`spdk-csi-driver/docs/attach-detach-campaign-2026-07.md`.

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

## Cluster assumptions

- trove-provisioned k8s with flint chart (SPDK three-container mode) in
  `flint-system`; campaign SCs `flint`/`flint-r2`/`flint-r3` (all
  WaitForFirstConsumer — do NOT reuse the kuttl multi-replica SC, it binds
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
