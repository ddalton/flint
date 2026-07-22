# F36c — degraded-assembly freshness gate (design)

**Status:** designed, not implemented (deferred from the v1.18.0 wave —
it touches the most durability-critical path and needs its own drill).
**Rev 2 (2026-07-21):** the freshness trigger is now the **last-writer
set**, not epoch rank — review showed rev 1's epoch rank ties in the
exact run-3 scenario and would never fire (see "Why epoch rank alone
cannot fire" below).

## The finding (runz drill 3.6 run 3, 2026-07-21)

After a server-node kill, the resurrect assembled the r2 volume from
the **lower-epoch leg** because the higher-epoch leg's lvol was still
claim-blocked by the dead node's phantom raid. Result: **6 client-acked
writes lost** + heap/index tears — the delta between the two legs at
kill time. F36 guards a+b (no-rebuild-under-live-consumer;
phantom-raid hygiene before the sync-state skip) cut the loss from 752
(run 2) to 6 (run 3) but did not eliminate it: guard-b frees the
blocked leg's claim, but the FIRST assembly tick can race ahead on the
reachable-but-staler leg before the fresher leg's claim clears.

## Root cause

`driver.rs` degraded assembly (the `total.min(2)→1` floor, commit
ea948ed) assembles from **whatever in-sync legs attached this tick**
with `min_required = 1`. It excludes *explicitly-Stale* legs (epoch
history), but it does **not compare freshness across the legs that did
attach** — so a leg that is "in_sync" per its sync-record but trails a
temporarily-unreachable sibling is served as authoritative.

For a synchronous raid1 both legs should hold every acked write, so in
steady state there is no delta. The 6-write window exists because a
just-rejoined / catching-up leg can be marked in_sync at an epoch
boundary while the peer has acked writes past that boundary that have
not yet propagated — assembling from the trailing leg drops them.

## The tension (why this isn't a one-liner)

The `total.min(2)→1` floor and single-survivor direct serve
(819262c) exist to deliver drill 2.4's headline: **permanent node loss
→ serve the lone survivor, zero manufactured outage.** A naive
"require the freshest leg" gate would reintroduce the 2.4 outage — the
freshest leg may be the one that was terminated.

The distinction the gate must draw:
- **Permanent loss** (freshest leg's node is Gone/terminated): serve
  the reachable survivor, accept the bounded acked-tail risk, and
  surface it as a `VolumeCondition` — never hang.
- **Transient unavailability** (freshest leg's node is Ready, or its
  lvol is merely claim-blocked / mid-attach): the fresher data is
  coming back — **wait (bounded) or break the stale claim** before
  serving the trailing leg.

## Why epoch rank alone cannot fire (rev-2 correction)

Rev 1 proposed ranking legs "by epoch position (the epoch/
write-position already tracked in the sync record + `bdev_lvol_get`
generation)". Two things are wrong with that:

- **Both legs tie in the motivating case.** The only per-leg
  freshness the record holds is `last_epoch`, stamped at epoch cuts
  (`apply_epoch_cut`) and at catch-up admission (`mark_in_sync`). In
  run 3 the trailing leg was admitted AT E_f and the fresh leg
  participated in the E_f cut — both legs read `last_epoch = E_f`.
  The 6 lost writes landed after the cut and are recorded nowhere
  (the cut cadence is 300s default, so the invisible intra-epoch
  tail is up to one full interval of acked writes, not 6). A
  "strictly-higher-epoch leg is known" trigger evaluates FALSE and
  the gate never fires. Epoch rank only separates legs ≥1 whole
  epoch apart — the explicitly-Stale class that the assembly's Stale
  exclusion and guard-b already police.
- **`bdev_lvol_get` generation does not exist.** No lvol
  write-generation is tracked anywhere in the driver or exposed by
  SPDK's lvol RPCs. (The raid1 superblock sequence number IS real
  on-device evidence of last-array membership and can corroborate
  later; it is not the primary key.)

The signal that actually separates the legs is **membership in the
last writer set**. For a synchronous raid1, every acked write lives
on every leg of the serving assembly — so the legs of the LAST
serving assembly hold everything, and a leg outside it (standby,
mid-catch-up, just-admitted) may trail by an unrecorded tail. Run 3
is exactly this: leg-1 was serving (hence claim-blocked by the dead
node's raid — the claim itself is last-writer evidence), leg-0 was
the just-admitted chaser.

## Proposed resolution (rev 2)

1. **Record the writer set durably.** Add `writer_set` (lvol uuids +
   stamped-at) to `VolumeSyncRecord`, written by the assembling node
   whenever a serving assembly is created or changes membership
   (assembly, standby admission at stage, leg drop seen by the fast
   detector). Same resourceVersion-guarded PV-annotation write path
   as the rest of the record — one small control-plane write per
   membership change, zero data-path cost. Crash-ordering rule:
   the record is written BEFORE the new membership takes writes. A
   too-LARGE recorded set only defers more than needed (safe); a
   too-SMALL set is the loss vector and is never permitted.
2. **Rank at assembly time:** a leg in the recorded writer set
   outranks a leg outside it; `last_epoch` breaks ties (it still
   separates the ≥1-epoch stale cases); equal-rank legs assemble as
   today. Independent corroboration, immune to record staleness: a
   leg whose lvol is **claim-blocked by the previous assembly's
   raid** was in the last writer set — claim-block counts as
   writer-set membership even if the record disagrees.
3. **If every writer-set leg attached** → assemble as today; by the
   sync-raid1 invariant the assembly holds every acked write.
4. **If a writer-set leg is missing**, branch on evidence about THAT
   LEG's availability — not k8s Node readiness alone (F33: Ready
   nodes run dead tgts):
   - **Transient** (its lvol answers a direct probe, or is
     claim-blocked / mid-attach, or its node+agent are live):
     **defer** this assembly. Guard-b's phantom-raid hygiene clears
     the claim meanwhile. The defer bound is a wall-clock deadline
     stamped in the record — NOT a tick count: NodeStage retry
     cadence belongs to kubelet backoff, and run 3 showed claim
     clearing can require a node reboot (~90-120s). Default 180s
     (env `FLINT_F36C_DEFER_SECS`), re-armed while progress evidence
     exists (claim holder count shrinking, node mid-boot).
   - **Permanent** (node object gone / terminated / NotReady past
     the AD-forced-detach horizon, AND the lvol probe is dead):
     serve the reachable leg + set `flint.io/acked-tail-risk`
     VolumeCondition naming the writer-set gap. Never hang.
   - **Deadline expiry without progress** → fall through to the
     permanent branch (serve + condition). The gate bounds the
     outage; it never manufactures an indefinite one (the 2.4
     regression).
5. **Ordering vs forced-stale admission:** the gate evaluates BEFORE
   the below-2-base forced-stale block in
   `create_raid_from_replicas` — forced stale admission is legal
   only in the permanent branch, otherwise a Stale leg rides in
   underneath the gate while it defers.
6. **Prereqs to verify at implementation:** node-plugin RBAC for
   get/list Nodes (permanent-branch check; today the agent reads
   PVs/VolumeAttachments), and that the fast detector's leg-drop
   path can reach the record writer (it already writes sync-state
   transitions).
7. **Never** silently serve a trailing leg while a writer-set leg is
   transiently unavailable — that is the exact 6-write loss.

## Acceptance

A repeat of 3.6 run 3 (server-node kill with a catch-up delta between
legs) must show **witness_recovery ≤ deadline+RTT AND zero acked-write
loss** — the run-3 metric plus the run-2 durability bar, together.

Drill 3.6c pins the trigger to the writer set, not epoch rank: with
BOTH legs at the SAME `last_epoch`, trail the non-serving leg by N
post-cut writes, then kill the node holding the fresher (serving)
leg. Assert:
- (a) the trailing leg is NOT served while the fresher leg is
  transiently unavailable (claim-blocked / node rebooting);
- (b) zero acked-write loss once the fresher leg rejoins;
- (c) permanent variant (terminate the node instead): the trailing
  leg IS served within the deadline, with the
  `flint.io/acked-tail-risk` VolumeCondition raised naming the gap.

## Scope note

This is the assembly-time (`driver.rs` NodeStage raid-create) half of
the F36 family. The catch-up-time half (guard-a, never delete a
live-consumed head) shipped in v1.18.0 (commit 03bf1ff). F36c is the
remaining P1. Rev 2 additionally touches `replica_sync.rs`
(`VolumeSyncRecord.writer_set` + stamp helpers) and the fast
detector's leg-drop path (writer-set shrink write); the epoch
scheduler is unchanged (`last_epoch` remains the tiebreak).
