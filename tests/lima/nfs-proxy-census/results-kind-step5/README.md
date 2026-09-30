# step 5 on kind: the fleet idle default, HIB-1, and the csi-node roll restart

Box, kind (1 control plane + 1 worker), 2026-09-30. `../step5-kind.sh`.
RustFS in-cluster for the bucket, the host kernel mounting the proxy's
NodePort (`actimeo=3600`), and a stand-in `flint-csi-node` DaemonSet whose
`spdk-tgt` container is `sleep`. Ladder: `nfsProxy.idleDefaults` suspend
60 s, hibernate 120 s.

| Share | Setup | Expected |
|---|---|---|
| ws-lock | bucket, no `spec.idle`, a process holds a write lock through the proxy | suspended, NOT hibernated: deferred, disk kept, lock kept |
| ws-lease | bucket, no `spec.idle`, a file written and closed (the mount stays) | hibernated (a lease is not state); wakes from the bucket |
| ws-nobucket | no bucket, no `spec.idle` | suspended only |
| ws-optout | bucket, `spec.idle: {}` | never leaves Ready |
| ws-other | no bucket, `spec.idle: {}`, class `other` | not restarted by the roll |

**Run 1** (`run-1-address-withheld.txt`), 12/17. It found a proxy defect.
The lock holder's write after the wake got **ESTALE**, and a read of
the hibernated-then-woken ws-lease got **ENOENT**, both within 2 s of the
proxy's wake request, instead of DELAY. The operator withholds
`status.address` during every ladder transition, and the proxy's kube
mode dropped a share with no address from its table. For the length of
a wake the workspace was absent. The client then released the file's
state, so ws-lock later hibernated correctly with no state held. The
other failures follow from that, plus one rig fault: the ladder was
still running during the roll leg.

**Run 2** (`run-2.txt`), 16/17, after the proxy fix (rows keep the last
address) and the rig fix (the ladder frozen before the roll):
- ws-lock deferred at 316 s: "1 client(s) with a live lease hold opens,
  locks or delegations; back to suspended". It kept its PVC, and the
  holder's write after the verify wake and the proxy's wake **succeeded**.
- ws-lease hibernated at 357 s (PVC gone), and a read through the proxy
  woke it and returned `lease-bytes` from the bucket.
- ws-nobucket suspended and kept its PVC; ws-optout stayed Ready.
- The rollout restarted ws-lock, ws-lease and ws-optout (new pod,
  `HubRestarted`, Ready again) and not ws-other (class `other`).
- **The one FAIL was the rig's convergence check.** It summed the core
  events' `count`, which these events do not carry, and read 0 then 0.
  Judged from the operator's log instead (`run-2-operator-marks.txt`):
  one mark per restarted hub, all at 18:34:41, and none after the
  restarts completed at 18:34:57, through the rest of the run. The
  check now counts those log lines. **That fixed check has not been run.**

**Run 3** (`run-3.txt`), 18/19 after CR-only hibernation was added: the
convergence check (now counting the operator's mark lines) passed, and
the wake through a RESTARTED proxy (only the CR-implied address) passed.
The CR-only check failed: after `DiskReclaimed` the next pass came only
at `REQUEUE_SETTLED` (300 s). Fixed: `Deleted` requeues at
`REQUEUE_PROGRESS`.

**Run 4** (`run-4.txt`), same FAIL, with a different cause in the
operator's log: `configmaps ... forbidden: cannot delete`. The chart
granted no `delete` on ConfigMaps (or Deployments). The park stopped
after the Service, and the Deployment-last order left the Deployment in
place, so the next pass retried instead of taking the share as parked.
Fixed: the grants, a test pinning them
(`the_chart_lets_the_operator_delete_what_the_park_deletes`, fails
without the ConfigMap grant), and deletes limited to objects the share
controls (`owned_by`, with a uid precondition).

**Run 5** (`run-5.txt`), **19/19**: ws-lease has 0 objects after
hibernating and wakes through the restarted proxy; everything else as
in run 2.

## Operator and proxy memory at 20,000 shares (`../step5-scale-kind.sh`)

`scale-20000.txt` / `scale-20000-mem.txt`: 20,000 shares created
hibernated (no disk), all parked as their CR alone in 905 s, no
restarts, no Deployment/Service/ConfigMap in the namespace.

| | empty | peak while parking | settled | per share |
|---|---|---|---|---|
| operator | 28 MiB | 465 MiB | 416 MiB | ~19 KiB |
| proxy | 32 MiB | 361 MiB | 354 MiB | ~16 KiB |

`scale-200-mem.txt`: a 200-share run for the one-share breakdown. A parked
share is 2,782 bytes of JSON, 1,235 of them `managedFields` (44%). The
20,000 run printed 0% because `kubectl -o json` hides managedFields
without `--show-managed-fields`, since fixed in the rig.
