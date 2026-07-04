# Incremental Replica Rebuild Test

Validates the Tier-1/Tier-2 self-healing pipeline end to end as part of the
baseline gate: a replica leg is killed under active writes and must return to
full redundancy **without touching the workload**.

## What it exercises

1. A 2-replica volume serves a continuously fsync-ing writer.
2. The spdk-tgt on a **non-consumer** replica node is killed (`kill 1` in the
   node DS sidecar) — the raid degrades to 1/2 but keeps serving.
3. The consumer-side health monitor marks the replica `stale`
   (`ReplicaStale`, ~60s tick).
4. Catch-up rebuilds it incrementally from epoch snapshots
   (`ReplicaCatchupStarted`, or `ReplicaFullBuildStarted` when the volume is
   younger than the T_back safety margin — both are valid here), chases to
   `standby`, and hot rejoin re-admits the leg into the live raid
   (`HotRejoinSucceeded`, `--skip-rebuild`).
5. Asserts: replica back to `in_sync` with an admission event, bulk-data
   md5 intact, append ledger contiguous (zero acked-write loss), writer
   container restart count 0 and still making progress.

## Requirements

- ≥ 2 workers with initialized flint storage (the 2 replicas must land on
  different nodes so a non-consumer leg exists).
- `jq` on the test runner.
- spdk-tgt image with the raid `skip_rebuild` patch set (any release ≥ 1.4.0).

## Cluster impact — run isolated

- Killing spdk-tgt degrades **every** volume with a leg or consumer on the
  target node. This suite runs with `parallel: 1` in its own suite file
  (`kuttl-testsuite-replica-rebuild.yaml`) and must not run against a cluster
  carrying unrelated live volumes — same isolation contract as
  clean-shutdown.
- Step 00 enables `FLINT_EPOCH_SCHEDULER` / `FLINT_CATCHUP` /
  `FLINT_HOT_REJOIN` (and pins `FLINT_EPOCH_INTERVAL_SECS=30` when the
  configured interval is absent or > 60s) on the controller if they are not
  already on; step 07 restores the exact prior values (including removing
  vars that were absent). The controller rolls twice as a result — expected.

## Run

```sh
kubectl kuttl test --config kuttl-testsuite-replica-rebuild.yaml
```

First live validation: 2026-07-04 on cluster `runk` (see
`spdk-csi-driver/docs/` cluster-restart drill record).
