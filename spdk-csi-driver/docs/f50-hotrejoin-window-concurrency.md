# F50 — the hot-rejoin admission window races a concurrent catch-up on the same volume, and never lands

**Status:** OPEN, found live 2026-07-27 on runai (drill 2.9, driver
`1.21.0-rc2`). This is the finding the [F48](f48-standby-rejoin-epoch-race.md)
write-up flagged as its open question — *"if an admission path can run without
the volume claim, that is its own finding."* It is.

**NOT a regression from the F47/F48/F49 wave.** Proven by ordering, §2.

**Severity:** a numReplicas=2 RWO volume stays at **1/2** indefinitely after a
leg rebuild. Materially better than the F48 wedge it replaces — the leg sits at
`standby` and catch-up keeps it chasing, data is safe, and the F48 zombie-sever
prevents the permanent pin — but redundancy is never restored without
intervention. The pending expand behind it also never completes (the
degraded-refusal belt correctly holds it).

**Vector:** drill 2.9 (destroy the remote leg's lvstore in place). Presumably
any vector that drives catch-up and hot-rejoin at the same volume
concurrently.

## 1. What happens

Catch-up rebuilds the leg and parks it at warm standby. Hot-rejoin then opens
its quiesce window to admit it — but **catch-up is still active on the same
volume**, and the two collide. Every window unwinds; catch-up re-parks the leg
at standby; the cycle repeats indefinitely:

```
standby → (hot-rejoin intent) stale → window unwinds → standby → …
```

Three consecutive attempts on runai, each failing differently:

| attempt | E_f create | outcome |
|---|---|---|
| 22:03:59 | **succeeds** | two `nvmf_delete_subsystem` land on the E_f NQN at 22:04:00.019/.020, then `add_ns` at 22:04:00.191 fails **-32602** — the subsystem was deleted out from under the in-flight window |
| 22:04:59 | succeeds | `add_ns` succeeds; unwinds later on `ns swap (old ns still visible on consumer): bdev nvme_…_0n1 did not become absent on runai-aws-3 within the AER budget` |
| 22:08:59 | fails **-32603** (residue from the above) | adopted by the F48 fix-3 probe, proceeds past the create as designed |

The concurrency is visible directly in the source node's RPC trace: catch-up's
`bdev_lvol_start_shallow_copy` / `bdev_lvol_check_shallow_copy` calls interleave
with the window's E_f `nvmf_*` calls on the same node in the same seconds. And
the controller logged, 48ms *after* the window's E_f create had already gone
out on the wire:

```
22:03:59.836  (aws-2) nvmf_create_subsystem  …:hotrejoin:<pv>      ← window is live
22:03:59.884  [CLAIMS] volume claimed by another operation — skipping this tick
              wanted_op="hot-rejoin-reconcile" held_by="catch-up"
```

So the claim registry was doing its job for `hot-rejoin-reconcile` while some
path was *already* running window mechanics against a volume catch-up held.
Two `HotRejoinScrubbed` events ("Scrubbed the stranded artifacts of an
uncommitted hot rejoin") land in the same window — the scrub is the most
likely issuer of the two deletes, and the scrub is dispatched by
`reconcile_marked`, which **catch-up itself dispatches** under its own claim.

That is the shape of the bug: the per-volume claim makes *orchestrators*
mutually exclusive, but catch-up's marked-dispatch performs hot-rejoin
maintenance (resume/adopt/**scrub**) under the *catch-up* claim, so it can
destroy E_f state belonging to a window that the hot-rejoin orchestrator is
running. Mutual exclusion between claim holders is not mutual exclusion
between the *operations* that touch E_f.

## 2. Why this is not a regression from the F47/F48/F49 wave

The wave's only change on this path is fix 3 in `hot_rejoin::prestage`: when
`nvmf_create_subsystem` returns an error the textual matcher doesn't
recognise, probe `nvmf_get_subsystems` and adopt an existing subsystem instead
of failing. **That code runs only when the create fails.**

- Attempt 1 — the one that produced the `-32602` — had a **successful**
  create. The new code never executed, and the failure still occurred.
- Attempt 3 is the only one where the adopt path ran, and it behaved as
  designed (continued past a residual subsystem to `add_host`).
- Pre-wave, runah run A hit `-32603` at the create and unwound *there*, which
  masked these later failures. The wave removed the earlier failure and
  exposed the next one in the chain — the same "each fix exposes the next bug"
  progression as F44 → F45 → F46 on runae.

Confirmed live in the same run: the F48 zombie-sever fired
(`ReplicaHeadZombieConsumerSevered … pinned by 1 dead controller
connection(s) backing no raid slot; severed`), and claim contention stayed at
`held_secs=0/4` with **zero** reservations recorded — F48 fixes 1 and 2 both
working, run A's ~4-minute starvation gone.

## 3. Fix directions (not implemented — needs the RCA below finished)

1. **Make E_f state ownership follow the claim, not the module.** The scrub in
   `reconcile_marked` must not delete E_f artifacts for a volume whose marker
   is *live* (an intent written seconds ago is not "stranded"). Gate the scrub
   on marker age / a window-in-progress flag, not merely on marker presence.
2. **Serialize the window against catch-up's copy traffic properly.** Either
   hot-rejoin takes the claim for the whole prestage+window (it may already
   intend to — verify), or catch-up must yield before the window opens rather
   than continuing shallow copies on the source.
3. **Re-examine the AER budget for the ns swap** (attempt 2's failure) under
   concurrent copy load on the consumer — it may simply be too tight when the
   node is busy, which would make this a tuning issue layered on top of (1).
4. Emit an event when N consecutive windows unwind, so "leg parked at standby
   forever" is visible without reading controller logs (the F48 fix-1 event
   only covers the zombie case).

## 4. RCA still to do (offline, from the captured bundle)

Evidence: `tests/chaos/artifacts/f50-hotrejoin-window-runai/` — timestamped
controller log, all four node-agent logs, the PV sync record, PV events, and
live subsystem state per node.

Open questions:
- **Which code path issues the two `nvmf_delete_subsystem` at 22:04:00?**
  Scrub is the hypothesis; confirm against `reconcile_marked` and the prestage
  unwind path.
- **Does the hot-rejoin window hold `OP_HOT_REJOIN` for its whole duration?**
  If it releases between prestage and window, that alone explains the race.
- **Is the ns-swap AER budget failure independent**, or a consequence of the
  source node being saturated by catch-up's shallow copies?

## 5. Drill gate

Drill 2.9 must reach `in_sync` repeatedly. Add an assertion for the new shape:
if the leg cycles `standby → stale → standby` more than twice without reaching
`in_sync`, fail loudly as F50 rather than timing out generically — a silent
15-minute timeout is what let this hide behind F48.

Related: [F48](f48-standby-rejoin-epoch-race.md) (the wedge this replaces; its
fixes are proven working in the same run), [F43 claim arbitration]
(f43-rwx-replacement-admission.md), C6 in `catchup.rs`.
