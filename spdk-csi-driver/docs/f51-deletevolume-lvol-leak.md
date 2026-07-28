# F51 — DeleteVolume skips rebuilt replica lvols and every epoch snapshot, then reports success

**Status:** OPEN, found live 2026-07-27 on runai (driver `1.21.0-rc2`), while
tearing the chaos harness down between drills. Not a drill result — found by
sweeping SPDK state after the PV object was gone.

**NOT a regression from the F47/F48/F49 wave.** The wave only touched export
(`nvmf_*`) teardown, and export teardown is the half that worked perfectly
here — every subsystem on every node was swept clean. The defect is in the
lvol (`bdev_lvol_*`) half of `DeleteVolume`, which the wave did not touch.

**Severity: LOW — deferred cleanup, not a permanent leak.** The orphan sweep
(`node_agent::orphan_sweep`, on the 60s monitor tick, enabled unless
`FLINT_ORPHAN_SWEEP=disabled`) condemns exactly this shape: `classify_lvol`
maps both `vol_<pv>_replica_<i>` and `epoch-<pv>-<n>` to `Owner::Pv(<pv>)`, and
branch (1) of `plan_sweep` condemns a PV-owned lvol *on PV absence alone*, so
these should be reaped about three strikes (~3 min) after the PV disappears.

> **Correction to my first read of this.** I initially wrote this up as a
> silent permanent leak. That was wrong: I sampled the residue ~2 minutes after
> the PV went away and manually deleted it, which is *inside* the sweep's
> 3-strike window — so I destroyed the evidence before the backstop could act.
> The direct confirmation is still outstanding; see §4.

What remains a genuine bug regardless of the backstop: the primary path is
supposed to do this work, it doesn't, and it prints
`✅ [CONTROLLER] Multi-replica volume deleted` while not doing it. Defence in
depth should not be load-bearing, and an operator reading that line has no
signal that a 3-minute reconciliation is still owed.

## 1. Observed

One `numReplicas=2` RWO volume (`pvc-98d84f1f…`, 21Gi), deleted normally by
namespace teardown. `DeleteVolume` returned success and the PV object went
away. SPDK state ~2 minutes later:

| node | lvols left behind |
|---|---|
| runai-aws-1 | `vol_pvc-98d84f1f…_replica_0` (the **whole replica**, ~19 GiB allocated) + `epoch-…-3`, `-4`, `-6`, `-7` |
| runai-aws-2 | `epoch-…-2`, `-3`, `-4`, `-6`, `-7` |
| runai-aws-4 | none (never hosted a leg) |

Subsystems: **zero** on all three nodes. Raids: **zero**. All leaked lvols had
`clones=0` — nothing was blocking their deletion; they were simply never asked
to go away. A manual `bdev_lvol_delete` of each succeeded immediately.

## 2. Two independent causes

### F51a — the replica delete is keyed on a UUID that a rebuilt leg no longer has

`main.rs:1839-1875` iterates the PV's replica list and, per replica, calls
`check_backing_storage_exists(node, replica.lvol_uuid)` before deleting. That
helper (`driver.rs:834`) asks the node agent `/api/volumes/check_exists` **by
UUID**.

This volume had been through drill 2.9 (destroy a leg's lvstore in place).
Catch-up rebuilt the aws-1 leg, and the rebuilt lvol has a **new** UUID; the
PV's replica record still carries the original (`17fd6f0c-1dfc-…`, the UUID
the expansion-refusal messages were still naming minutes earlier). So the
check returned `false`, the loop took the

```
ℹ️ [CONTROLLER] Replica N backing storage already gone (UUID: …)
```

branch, and skipped the delete — for an lvol sitting right there under its
deterministic name.

The node-agent trace proves the loop *did* iterate aws-1's replica: the two
`nvmf_delete_subsystem` calls at the bottom of the same loop body
(`main.rs:1871-1874`) landed on aws-1 at 22:24:09, but no `bdev_lvol_delete`
ever did. Compare aws-2, whose UUID was still current — one
`bdev_lvol_delete c2829f6c…` at 22:24:09.517, success.

**Second vector in the same helper:** `check_backing_storage_exists` also
returns `Ok(false)` when the node agent is simply *unreachable*
(`driver.rs:855-865`, "treat as storage gone"). A leg on a node that is down at
delete time is skipped too. That branch is right about *not failing the
delete* — it is wrong as a reason to *skip* it and then claim success.

### F51b — nothing in DeleteVolume ever deletes `epoch-<pv>-N` snapshots

The multi-replica arm has no epoch handling at all. Catch-up mints
`epoch-<pv>-N` snapshots (`bdev_lvol_snapshot`) on each end as it works, and
they outlive the volume on every node that ever ran catch-up for it — including
aws-2, where the replica lvol itself *was* deleted correctly. Five snapshots
survived there with no parent.

This only appears on volumes that have had a leg rebuild, which is why
clean-volume teardowns have never shown it.

## 3. Fix directions (not implemented)

1. **Delete by deterministic name, not by recorded UUID.** Simplest form that
   fixes both halves at once: per replica node, list lvols, keep those whose
   `identity::classify_lvol(name) == Owner::Pv(<this volume>)`, delete each by
   the UUID the listing returns. That reuses `delete_lvol(node, uuid)`
   unchanged, needs no new node-agent route, and catches the rebuilt replica
   (whatever its UUID) and every epoch snapshot in one pass — the same
   `classify_lvol` authority the orphan sweep already trusts.
2. **Never let "cannot verify" mean "already deleted."** Distinguish *node said
   no such lvol* from *could not reach node*, and report the difference.
3. **Stop logging unconditional success** — `Multi-replica volume deleted`
   should say how many replicas were destroyed, skipped, and deferred to the
   sweep.

## 4. Confirmation — DONE, severity LOW stands

Run at the runai teardown 2026-07-27 on a **second, independent volume**
(`pvc-ed1543d4…`, the RWX volume from 3.6e). Namespace deleted, then SPDK left
strictly untouched:

```
baseline  aws-2: vol_…_replica_1 + epoch-…-7..12      aws-4: vol_…_replica_0 + epoch-…-7..12
+1s    residue=13 | aws-2=6 aws-4=7      ← DeleteVolume already finished
+107s  residue=13 | aws-2=6 aws-4=7
+129s  residue=7  | aws-2=0 aws-4=7      ← aws-2 swept
+171s  residue=0  | aws-2=0 aws-4=0      ← aws-4 swept
```

with the reap itself in the node-agent log:

```
23:24:38 [ORPHAN_SWEEP] reaping orphans of absent PVs subsystems=0 lvols=6
23:24:38 [ORPHAN_SWEEP] deleted orphan lvol lvs_runai-aws-2_…/epoch-pvc-… (×6)
```

**All residue gone 171 s after PV absence** — right at the predicted 3 strikes
× 60 s. The backstop covers this shape, so LOW is correct.

### What this run added: F51a is broader than §2a says

The skipped replica on aws-4 was **`vol_…_replica_0`, the replacement leg
created by 3.6e's node-loss re-placement** — not a 2.9-style in-place lvstore
rebuild. Its node received the two `nvmf_delete_subsystem` calls (so the loop
iterated it) and **zero `bdev_lvol_delete`**. aws-2, whose UUID was current,
got its replica deleted by UUID normally.

So the stale-UUID window opens on **ordinary node loss**, which is the common
production path, not just on the synthetic 2.9 vector. That raises how often
this fires, without changing the severity — the sweep still cleans up.

## 5. Drill gate

Add a post-delete residue assertion to the harness teardown (it already sweeps
subsystems; extend it to lvols), with the sweep's window built in: after the
last PV is gone, every worker must report zero `vol_*` and `epoch-*` lvols
**within 5 minutes**. Run it after a drill that forces a leg rebuild (2.9), not
just a clean one — a clean volume never mints epochs and never re-UUIDs a leg,
so it cannot catch either half of this.

Evidence: `tests/chaos/artifacts/f51-delete-lvol-leak-runai/` — controller log,
all three node-agent logs, and the per-node lvol/subsystem/raid state captured
before the manual sweep.

Related: [F47](f47-loopback-export-teardown.md) (the export half of teardown,
working correctly here), [F50](f50-hotrejoin-window-concurrency.md) (the churn
on this same volume that produced the epoch pile and the leg rebuild).
