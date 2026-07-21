# F36c — degraded-assembly freshness gate (design)

**Status:** designed, not implemented (deferred from the v1.18.0 wave —
it touches the most durability-critical path and needs its own drill).

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

## Proposed resolution

1. **Rank attached + known legs by epoch position** at assembly time
   (the epoch/write-position already tracked in the sync record +
   `bdev_lvol_get` generation). Cheap: metadata already on hand.
2. **If the highest-epoch leg attached** → assemble as today.
3. **If a strictly-higher-epoch leg is KNOWN but unattached**, branch
   on its node's k8s condition:
   - node Ready / lvol claim-blocked → **defer** this assembly tick
     (bounded, e.g. 5 ticks); guard-b's phantom-raid hygiene clears the
     claim in the meantime so the fresh leg attaches next tick.
   - node NotReady past the AD-forced-detach horizon, or terminated →
     **serve the reachable leg + set a `flint.io/acked-tail-risk`
     VolumeCondition** naming the epoch gap.
4. **Never** silently serve a trailing leg while a fresher one is
   one tick from reachable — that is the exact 6-write loss.

## Acceptance

A repeat of 3.6 run 3 (server-node kill with an epoch delta between
legs) must show **witness_recovery ≤ deadline+RTT AND zero acked-write
loss** — the run-3 metric plus the run-2 durability bar, together. Add
an explicit drill 3.6c: kill the node holding the fresher leg while the
other leg trails by N writes; assert the trailing leg is NOT served
until the fresher leg rejoins (transient) or a VolumeCondition is
raised (permanent).

## Scope note

This is the assembly-time (`driver.rs` NodeStage raid-create) half of
the F36 family. The catch-up-time half (guard-a, never delete a
live-consumed head) shipped in v1.18.0 (commit 03bf1ff). F36c is the
remaining P1.
