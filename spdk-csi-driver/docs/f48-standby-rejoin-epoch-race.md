# F48 — a standby leg demoted mid-admission wedges at 1/2 behind a zombie consumer

**Status:** FIXED in-tree 2026-07-27 (three fixes, see §Fixes), drill-gated.
Found live on runah during the v1.21.0 expansion campaign, on the FIRST-EVER
live run of drill 2.9 (F11 in-place lvstore loss). Pre-existing; **not**
introduced by the v1.21.0 expansion wave — see "Expansion is not implicated".

**Severity:** a numReplicas=2 RWO volume can be left permanently degraded 1/2
with no self-heal. Data is safe throughout (the surviving leg serves; the
oracle recorded zero acked loss) but redundancy is silently gone and nothing
converges it.

**Vector:** drill 2.9 — destroy the remote leg's lvstore in place, node alive.

## Corrected mechanism (this doc's first version got it wrong)

The first write-up blamed "the epoch tick re-marks the standby stale."
Deeper log + code archaeology shows the epoch recorder never demotes anyone
(`apply_epoch_cut` only stamps `last_epoch`; the health monitor's
`replicas_missing_from_raid` explicitly protects standby/stale legs). The
actual cascade in run A, each step now code-attributed:

1. **18:38:23** — catch-up parks the rebuilt leg at warm standby
   (`through=epoch-…-6`).
2. **18:38:52.7** — an admission attempt begins. Its INTENT write demotes
   the standby to stale **by design** (`record_hot_rejoin_intent`,
   hot_rejoin.rs: "a standby target is demoted to stale in the same
   write" — the marker makes every crash point recoverable). Concurrently
   the epoch scheduler cuts and records `epoch-…-7 replicas=1` — it only
   defers cuts while a *hot-rejoin op holds the claim*, and the claim
   holder at that instant was catch-up.
3. **~18:38:53** — the admission window fails: the E_f skeleton create
   returned SPDK's duplicate shape (`-32603 Unable to create subsystem`,
   which the textual `is_already_exists` matcher does NOT recognize), and
   the strict-fresh E_f snapshot cut hit `File exists` (the scheduler's
   concurrent epoch-7 cut; EEXIST unwinds by design). The unwind clears
   the marker and leaves the leg **stale** — from here the designed
   recovery is catch-up's bulk rebuild.
4. **18:38:52.76** — hot-rejoin's planner tick had bounced off catch-up's
   claim and posted a RESERVATION. Its next ticks see no standby → Wait →
   the reservation is never consumed and never released. Catch-up AND
   expansion yield to it until the 180s idle-TTL lapse: **~4 minutes of
   starvation** (`reserved_secs` climbed 60 → 119 → 149 → 179 in the log).
5. **18:42:52 onward** — catch-up finally runs the bulk rebuild and defers
   every tick, forever:

   ```
   WARN [CATCHUP] stale head is LIVE-CONSUMED — rebuild deferred (F36)
     consumer=subsystem nqn.…:volume:<pv>_0 exports this head
     to 1 live controller(s): nqn.…:node:<consumer-node>
   ```

   The "live controller" is the consumer node's nvme connection to the
   leg's `_0` export. Its raid slot is EMPTY (`is_configured: false`) —
   SPDK nulls a failed slot's name/uuid but never detaches the initiator
   controller — yet `head_live_consumer` counts any live controller as a
   consumer. A **zombie connection pins the head forever**; bouncing the
   consumer pod does not clear it (NodeStage re-attaches during assembly).

Step 5 is the permanence bug; steps 2-4 are one way (of several) to arrive
at "stale + zombie-consumed". Run B took the same drill on the same build
and healed at 274s because its admission landed before the epoch tick.

## Evidence (runah, 2026-07-27)

Two runs of drill 2.9 back to back, same cluster, same driver
(`1.21.0-rc1`):

| | standby at | epoch tick | outcome |
|---|---|---|---|
| run A (`pvc-570e8c1e…`) | 18:38:23 | **18:38:52 (+29s)** — collides with admission | intent demotes → window unwinds (EEXIST) → reservation leak (~4min) → F36 zombie defer, wedged 1/2 forever |
| run B (`pvc-2106980d…`) | 19:05:59 | 19:07:52 (+113s) — after admission | `in_sync` at 274s, raid 2/2, **drill PASS** |

Run A end state: raid `1/2` (`base_bdevs_list[0]` nulled,
`is_configured:false`), a healthy fully-sized fresh leg sitting unused on
the leg node, and the F36 defer repeating once a minute indefinitely.

Artifacts: `tests/chaos/artifacts/2-2.9-1785177282/` (run A, wedged) and
`tests/chaos/artifacts/2-2.9-1785178963/` (run B, PASS).

## Expansion is not implicated

Run A also had a pending PVC expansion (deliberately: the degraded-refusal
variant of drill 2.10). `expand` is a maintainer: every claim attempt
either refused on the sync belt in microseconds or yielded
(`[CLAIMS] yielding to a reserved resolver operation … wanted_op="expand"`).
It never held the claim while a resolver wanted it, and run B reproduced
the identical claim sequence with no pending expand.

## Fixes (in-tree 2026-07-27)

1. **F36 defer: sever zombies, protect only real consumers**
   (`catchup.rs`). `head_live_consumer` now returns the subsystem + host
   NQNs; a new `zombie_head_consumers` probe checks, per host node,
   whether ANY raid there has a configured base backed by this head (by
   the deterministic remote-base bdev name or any head id). All-zombie →
   detach the dead controllers (`ReplicaHeadZombieConsumerSevered` event),
   still defer THIS tick (fail closed against a same-tick attach race),
   and the next tick rebuilds. Any probe error, non-flint host, or
   matching configured base keeps the plain F36 defer. This kills the
   permanence for every trigger path, not just run A's.
2. **Reservation release on no-work** (`volume_claims.rs::release_reservation`,
   wired into hot-rejoin's and cutover's `Wait` arms). A resolver that
   finds its work gone hands the queue back immediately instead of
   starving maintainers for the idle-TTL.
3. **E_f skeleton create: probe, don't parse** (`hot_rejoin.rs::prestage`).
   On a create error that the textual already-exists matcher doesn't
   recognize, probe `nvmf_get_subsystems`; present ⇒ converged, absent ⇒
   real error. Removes the `-32603` duplicate-shape admission failure.

Deliberately NOT changed:

- The intent write still demotes standby → stale (marker-recoverability is
  load-bearing), and the EEXIST unwind still leaves the leg stale. With
  fix 1 the bulk path converges in minutes, which is the designed
  recovery. A possible refinement — restore standby when the window
  failed before any destructive step — is noted as follow-up, not taken:
  it needs a per-step failure classification the unwind doesn't carry.
- The epoch scheduler's cut-deferral consult still keys on hot-rejoin
  claim ops only. Deferring cuts under catch-up's claim would break the
  multi-hour-chase design ("cuts must keep flowing").
- `ReplicaHeadInUse` already fires per defer tick (the first write-up's
  "add an event" recommendation was already satisfied); with fix 1 the
  remaining defers are genuine and the event says why.

## Open question for the drill gate

The exact issuer of the 18:38:52.8 admission attempt (E_f RPCs fired while
hot-rejoin's tick had bounced) is not conclusively identified from the
captured logs — candidates are catch-up's `admit_one_standby` inline
admission and the marked-dispatch reconciler. The re-run of drill 2.9 with
these fixes should capture it; if an admission path can run without the
volume claim, that is its own finding.

## Drill gate

Drill 2.9 must pass repeatedly on the fixed build — including a variant
that *forces* the losing order (park at standby, then step the epoch
scheduler before admission) so fix 1 is proven against the actual race
rather than against luck. Also assert: no `reserved_secs` > ~65 in the
claim logs (fix 2), and zero `ReplicaHeadInUse` for volumes whose raid
shows the slot unconfigured (fix 1).

Related: [F36 defer / live-consumed head], [F43 claim arbitration]
(`f43-rwx-replacement-admission.md`), [F11 store re-init], C6 in
`catchup.rs` (the copy-path cousin of the zombie-controller family).
