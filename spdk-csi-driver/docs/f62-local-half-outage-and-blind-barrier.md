# F62 — the local half destroys the serving raid, and the barrier does not notice

Status: **FOUND LIVE 2026-07-30 on runao (driver `1.22.0-rc5`), immediately
after fixing F61.** Not fixed. **`maintenance.drainRoll.enabled` must stay
OFF — and F61's fix ALONE makes a roll strictly more dangerous, not less.**

## The uncomfortable headline

F61 was a livelock: a node whose drain marked nothing could never be rolled,
so the campaign spun forever. Safe, but the DaemonSet never converged.

Fixing F61 let the pod delete through — and the very first time it did, on a
node hosting a serving raid, it took the volume down permanently.

**F61's bug was load-bearing.** The wedge was the only thing preventing the
un-implemented local half from being exercised. Removing the wedge without
implementing the local half converts a silent stall into a silent outage.

## What was measured

Sequence on runao, one RWX volume (`flint-r2`, pg-0 writing continuously,
NFS server on `runao-aws-4` together with one of the two legs):

```
01:41:21  [MAINT] node drained — deleting csi-node pod (RollStart) node=runao-aws-4
          (drained=0 — consumer == node, so the drain was SKIPPED by design;
           MaintenanceLocalConsumer had already been emitted)
01:41:51  new csi-node pod created on runao-aws-4
01:41:56  its containers start — spdk-tgt is a FRESH process
01:41:45  pg-0's last postgres log line ("checkpoint starting"), then silence
01:42:21  [MAINT] node drained — deleting csi-node pod ... node=runao-cp-1
          ^ the roller ADVANCED, one tick later, barrier NOT blocking
```

State afterwards, stable for >5 minutes with no self-healing:

| probe | result |
|---|---|
| `bdev_raid_get_bdevs` on the server's node | `{"result":[]}` — **raid gone** |
| same on the other node | `{"result":[]}` — not relocated, destroyed |
| `bdev_get_bdevs` on the server's node | 9 bdevs — the lvols survived (on disk) |
| nfs server pod | Running, `restarts=0`, alive with a dead backing device |
| pg-0 | `1/2 Running`, `restarts=0` — **hung**, not crashed |
| acked ledger | stalled 315s and counting — writes gone |
| sync record | **both legs still `in_sync`** |

pg-0 never restarted and never PANICked because the NFS mount is `hard`: the
client blocks indefinitely rather than erroring. That makes this failure
mode *quieter* than an EIO, not milder — `kubectl get pod` shows Running,
and only the readiness probe and the ledger reveal it.

## Two distinct defects

### F62a — nothing re-runs the raid assembly after a tgt restart

Verified against the SPDK source (`~/github/spdk` @ `bb2b757ac`,
v26.05.1-pre) rather than assumed, because the first framing of this —
"SPDK cannot persist a raid" — is **wrong**.

SPDK *can*: `module/bdev/raid/bdev_raid.c:3411` has `raid_bdev_examine_sb`,
an examine-based auto-assembly path that reconstructs a raid from
superblocks on its member bdevs when they reappear.

flint **opts out on purpose** — `driver.rs:3159` passes
`"superblock": false`, and the comment there gives two hard reasons:

* the superblock is "the root of the §3 phantom-assembly hazard class"
  (auto-assembly resurrecting a stale raid that then squats on the lvol —
  the F47/F49 EPERM family), and
* it "shifted the filesystem 1 MiB into every base lvol, which made
  snapshots/clones unmountable raw and silently formatted volumes restored
  from multi-replica snapshots" — a live regression on 2026-06-12.

So enabling superblocks is NOT the fix; it trades this outage for a
data-corruption class that has already bitten.

flint's design is that **raids are ephemeral and re-created at every
NodeStage from the PV replica record**. The actual gap is that nothing
re-runs that assembly when the tgt restarts *underneath an already-staged
volume*: kubelet does not re-NodeStage a volume it believes is staged, and
here the NFS server never lost its mount, so nothing triggered a restage.
The lvols survived (9 bdevs present); only the raid — the runtime
composition — was gone, and its one creator was never called again.

That points at the fix the v1.10.0 note already sketched as option (1):
a node-agent **reconcile-on-boot** that re-creates the raids for volumes
staged on that node, from the records it already reads, with identical
naming. Not a superblock.

### F62b — the barrier is blind to the damage it just caused

The barrier is documented as "raid-aware, not pod-ready" and the model
carries `BarrierRaidAware = TRUE`. But in code the barrier's evidence is the
**sync record** (`insync_by_node` + obstruction), and the record still said
both legs `in_sync` — because nothing had stale-marked anything. No leg
failed; the *raid* was destroyed out from under a healthy record.

So the barrier permitted the next node one tick later. On a larger fleet
this composes exactly like the unfenced roll TLC rejected: each node's roll
destroys one more serving raid, and the barrier waves the campaign through
because every record still looks perfect.

This is the same lesson as the RecordBarrier hardening pass (2026-07-28),
which found silent loss because "every record-level check passes on the
lying record" — and the fix there was to probe ground truth before the
record round. The barrier kept the record-level evidence.

## Disposition

1. **`maintenance.drainRoll.enabled` stays OFF.** Both F61 and F62 must be
   closed before it can default on. F61 alone is not enough — it is worse
   than nothing.
2. **F61's fix should NOT be reverted** — the livelock is a real defect and
   its TLC tooth (`FlintReplicationRollWedge.cfg`) is now in the gate. But
   the pod delete for a local-consumer node must be **refused, not
   performed**, until staged-device continuity exists: skip the node, keep
   the campaign converging for every other node, and surface the skipped
   set as an operator-actionable condition ("N nodes host serving raids and
   need manual handling"). That is strictly better than both the silent
   wedge and the silent outage.
3. **The barrier must probe the raid on the CONSUMER node**, not just the
   record — `base_bdevs_list` configured count on the node serving each
   volume. Then a destroyed raid blocks the campaign instead of being
   waved through.
4. The model needs the local half to be *representable* well enough to
   express F62b: a `LocalLegs` roll that destroys the serving raid while
   the record still reads in_sync. Today `MaintDrainSkip` leaves `serving`
   untouched, which is why TLC blessed a fix that breaks live. That is the
   honest limit of the current abstraction and the next tranche's work.

## Why neither the model nor the drills caught it earlier

- The model **cannot** see it: the local half is explicitly out of scope,
  and `MaintDrainSkip` (added for F61) leaves `serving` unchanged, so TLC
  believes rolling a local leg is harmless bookkeeping.
- The drills **could not reach** it: F61's wedge meant the pod delete never
  happened, so no drill had ever restarted a tgt under a live serving raid.
  Fixing the livelock was the precondition for measuring the gap.
