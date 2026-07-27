# F44 — the cutover bounce leaks the old NFS node's raid-leg connection

**Status:** FOUND 2026-07-27, live, on cluster `runae` (drill 3.6e / r2-perm).
Driver `dilipdalton/flint-driver:1.20.0-rc1` (commit `4adf986`).
**Severity: P1.** An RWX numReplicas≥2 volume that loses a backing-leg node
permanently ends up stuck at one leg with the client wedged. There is no
self-heal path out of it — the deadlock is stable, not transient.

**Found by the F43 acceptance drill.** F43's own gate passed; this is the
defect immediately downstream of it, and it was unreachable until F43 was
fixed (see "Why this was never seen before").

---

## 1. What F43's drill actually proved

Drill 3.6e permanently terminates a backing-**leg** node that is *not* the
nfs-server's node, so the NFS server stays alive and writing and the
replacement can only be admitted by cutover. Result on `runae`:

| stage | t (s) | outcome |
| --- | --- | --- |
| leg node `runae-aws-2` terminated | 0 | — |
| Node object deleted | 57 | re-placement armed |
| I/O never stalled | — | **F42 held** |
| identity swapped → `runae-aws-4` | 169 | **F40 dispatched** |
| replacement reached `standby` | 179 | the pre-fix park point |
| **nfs pod BOUNCED** | **298** | **F43 fix: cutover won the claim** |
| replacement `in_sync`, raid 2/2 | 330 | redundancy restored |
| `CutoverSucceeded` event | 05:19:02 | "Data path restored after the bounce (restage rebuilt the raid)" |

So the F43 mechanism works: cutover reserved the claim against catch-up's
30s re-claim and landed. Pre-fix this bounce never happens and the standby
parks forever. **That specific fix is confirmed on the vector it was written
for.**

F43 is nonetheless **NOT closed** — the acceptance criteria were "raid 2/2
with zero acked loss", and the volume did not *stay* at 2/2.

## 2. What went wrong after

Within ~3 minutes of the successful cutover the volume collapsed back to a
single leg and never recovered. Steady state at the time of writing
(30+ minutes in, still looping):

```
replicas: runae-aws-1 in_sync, runae-aws-4 stale     writer_set = 1
DegradedDirectServe: single replica served DIRECT, no raid layer
ReplicaHeadInUse: re-fires every 60s, indefinitely
CutoverStarted: 3 bounces (aws-1 → aws-4 → aws-3)
pg-0: wedged — `cannot exec in a stopped state`, Ready only at t=1112s
witness: UNRESPONSIVE (mount read timed out)
db verdict: FAIL (pg unreachable → acked-loss UNKNOWN, not measured)
```

## 3. Root cause

**REFINED after log analysis (same day):** the detach hygiene *exists* and
NodeUnstage *did* run on the outgoing node — it is silently a no-op for RWX
because of an identity mismatch.

`teardown_volume_spdk_state` (driver.rs, phase-0 code from 2026-06-10) step 3
sweeps per-replica initiator controllers by name prefix, derived from the
**staged** volume id:

```
prefix  = nvme_<volume_nqn(staged_id) with :/. → _>_
```

But an RWX server stages under the **backing wrapper** identity
(`nfs-server-<pv>`), while the per-replica head subsystems — and therefore
the leg controller names — use the **inner** storage id:

```
sweep prefix   nvme_nqn_2024-11_com_flint_volume_nfs-server-pvc-737e63a3..._
leg controller nvme_nqn_2024-11_com_flint_volume_pvc-737e63a3..._1   ← never matches
```

aws-1's log confirms the shape: `NodeUnstageVolume called` → loopback
subsystem deleted (wrapper id) → `RAID deleted at unstage` (wrapper id,
`raid_nfs-server-pvc-…`) → **zero "Detached replica controller" lines** →
`returning success`. Unstage reports success while leaking every leg
controller. On RWO the staged id IS the PV name, so the prefix matches and
the sweep works — **RWO is unaffected** (consistent with drill 2.5 and the
churn drills never showing this).

This is the identity-aliasing bug class the v1.6.0 identity unification was
built against; the CI lint catches literal drift, not a wrong-id-fed-to-the-
right-helper.

The observable consequence — the old node keeps a live `bdev_nvme`
controller pointed at the replacement leg's head. Confirmed directly at both
ends:

```
# aws-1 — no longer hosts the NFS server, still holds the controller:
$ rpc.py bdev_nvme_get_controllers
nvme_nqn_2024-11_com_flint_volume_pvc-737e63a3-..._1

# aws-4 — the replacement leg's subsystem sees exactly that one consumer:
$ rpc.py nvmf_subsystem_get_controllers nqn...:volume:pvc-737e63a3-..._1
cntlid=1 host=nqn.2024-11.com.flint:node:runae-aws-1

# meanwhile the NFS server has moved on:
nfs pod → runae-aws-3
```

That orphaned controller makes the head "in use", so the **F36 guard
correctly refuses to rebuild it**:

> `ReplicaHeadInUse`: Catch-up of stale replica 676c2928… on runae-aws-4
> deferred: subsystem `nqn…:volume:pvc-…_1` exports this head to 1 live
> controller(s): `nqn…:node:runae-aws-1` — rebuilding would delete the head
> under a live consumer (F36)

**The guard is not the bug — the leaked consumer is.** F36 is doing exactly
its job; it is being fed a ghost.

### The deadlock cycle

1. Cutover bounces the NFS pod off `aws-1`; `aws-1`'s leg controller survives.
2. The replacement leg on `aws-4` is the last writer, but its head is pinned
   by the ghost on `aws-1`.
3. Catch-up defers on `ReplicaHeadInUse` (every 60s, forever).
4. The leg is marked `stale`; F36c then defers assembly because the *last
   writer* is unavailable → `AssemblyDeferred` storms.
5. Fallback: `AckedTailRisk` + `DegradedDirectServe` — one leg, no raid layer.
6. Cutover fires again → the NFS pod moves again → a *new* node inherits the
   same unsatisfiable state. Repeat.

Note the first `AckedTailRisk` names the cause outright:
`serving without last-writer leg(s) … on runae-aws-4 (claim-blocked)`.

## 4. Why this was never seen before

Every prior 3.6* variant kills the **nfs-server's own node**. That path
resurrects the server elsewhere and re-admits legs through a *fresh stage*,
which builds connections from scratch — there is no surviving old node to
leak from. Killing a **remote leg** leaves the server alive, so admission
must go through cutover, and cutover's bounce is the only code path that
relocates a *live* NFS server off a node that still holds leg connections.

Before the F43 fix that bounce was starved and never ran, so the leak had no
way to occur. **Fixing F43 made this path reachable for the first time.**

## 5. Fix direction (not yet implemented)

Given the refined root cause, the primary fix is small and surgical:

- **Fix the sweep's identity**: derive the per-replica prefix in
  `teardown_volume_spdk_state` from the *inner* storage id
  (`identity::storage_id_of_handle(staged_id)`), not the raw staged id —
  or sweep both prefixes. One line of intent; the RWO behavior is unchanged
  because for RWO the two ids are identical.
- **Regression test that is now writable at unit level**: for a
  `nfs-server-<pv>` staged id, assert the sweep's prefix matches
  identity.rs's leg-controller naming for `<pv>`. The original tests
  couldn't catch this because they derived the expected prefix from the
  same wrong id.
- Keep as hardening (secondary, from the original analysis):
  - a bounce-loop brake — N cutovers with no improvement escalates
    (R4 ladder) instead of relocating the server forever;
  - do NOT widen the F36 guard's definition of "live" — the guard behaved
    correctly; detaching at the source is the honest fix.

## 6. Reproduction

```sh
# 4-worker all-spot cluster, driver ≥1.20.0-rc1, disks initialised
SC=flint-r2 MODE=RWX WITNESS=1 ./deploy-harness.sh up
# ensure the target leg node hosts NEITHER the nfs pod NOR pg-0
./drills/phase3.sh 3.6e
```

Drill 3.6e is committed in `tests/chaos/drills/phase3.sh`. Its own gate
(`in_sync` reached) **passes** — the failure surfaces in `verify-drill.sh`'s
db + nvme checks and in the PV event stream. Suggested hardening: extend
3.6e to re-assert 2/2 after a settle window, so the regression is caught by
the drill's own verdict rather than by the post-hoc checks.

## 7. Collateral observation (separate, minor)

Deleting the wedged volume left its **epoch snapshot lvols** (epoch-…-5
through -10, 6× 20Gi thin) orphaned on the surviving leg node; the primary
lvol was removed. Probably specific to deletion-under-wedge (epoch GC rides
the catch-up ticks that were deadlocked), but DeleteVolume arguably should
cascade the epoch chain unconditionally. Cleaned manually (descending epoch
order deletes fine). Watch for recurrence on clean deletions before filing
it as its own F-number.

## 8. Live confirmation + fix (2026-07-27, same day)

- Manually detaching the ghost controller on the old server node
  (`bdev_nvme_detach_controller`) instantly unblocked catch-up:
  `ReplicaCatchupStarted` within seconds, **self-healed to 2/2 `in_sync`
  180 s later with no other intervention** — the leak was the sole blocker,
  and the whole F43 admission chain works once it is gone.
- **Fix implemented:** `per_replica_controller_prefixes()` in driver.rs —
  teardown step 3 now sweeps by prefixes derived from BOTH the inner
  storage id (`storage_id_of_handle`) and the staged handle (belt; the two
  are identical for RWO, so RWO behavior is unchanged). Regression tests
  `f44_teardown_prefix_tests` pin backing-handle → inner-leg-name matching
  and the id-boundary underscore. 857 lib tests green.
- Shipped as `dilipdalton/flint-driver:1.20.0-rc2`
  (sha256:dd2d8024…, amd64). Drill 3.6e gained a **settle assertion**:
  in_sync must SURVIVE +360 s (the first run collapsed ~3 min after
  in_sync); collapse now fails the drill with the F44 signature.

## 9. Artifacts

`tests/chaos/artifacts/3-3.6e-1785128835/` (db-verdict, driver-logs, VA
dumps, nvme state) — cluster `runae`, trove project 49.
