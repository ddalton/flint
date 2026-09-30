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
