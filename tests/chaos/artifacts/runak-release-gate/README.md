# runak — the v1.21.0 release-gate campaign (2026-07-28, trove project 55)

4× i4i.xlarge workers + 1 CP, all spot, us-west-1, k8s v1.34.10.
Images: `1.21.0-rc4` (= F47–F53 waves) then `1.21.0-rc5` (= rc4 + F54).
Chart: local tree (operator removed, `FLINT_ORCHESTRATORS` grants, Recreate).
`orchestrator-decisions.txt`: every driver-image pod's logged role decision —
exactly one ENABLED (the controller), the F53 invariant.

## Verdicts (the release gate)

| drill | image | verdict | headline |
|---|---|---|---|
| 2.10 | rc4 | PASS | 20→21Gi in 70s under writes, legs byte-exact, stall 1s |
| 2.1 | rc4 | PASS | degraded serve, I/O uninterrupted, stall 17s |
| 2.9 | rc4 | drill PASS / F48-gate FAIL | **found F54** — see below |
| degraded-refusal | rc4 | refusal PASS / completion outside window | refused=1, no partial fan-out; completed +162s after rejoin (observed past the 15-min watcher) |
| 2.10 | rc5 | PASS | 21Gi in 33s, stall 1s — fastest yet |
| 2.9 | rc5 | **PASS** | in_sync 357s, **0 window flips, 0 zombie severs**, stall 1s — the F54 gate |
| degraded-refusal | rc5 | **PASS** | refused while stale, capacity never moved, completed 403s after rejoin |
| 3.11 | rc5 | PASS | RWX 21Gi in 32s, witness clean, db PASS, stall 3s |
| 3.6e | rc5 | **PASS — CLEAN** | all sub-checks green incl. attribution + db; postgres restarts **0** |

## F54 (found by 2.9 on rc4, fixed same day)

Hot-rejoin retry over its own unwound residue lost the E_f host fence to
SPDK's bare `-32603 Internal error` (duplicate host — no text to match).
Same classification bug as F48, one RPC later. Cost on rc4: two ~7-min
backoff cycles, in_sync at 1266s vs the 264s class. The concurrent expand
was exonerated by the claims log (every `wanted_op=expand` line is a skip —
it never held the claim). Fix: converge probes for every nvmf builder in
`prestage` (`docs/f54-ef-hostfence-duplicate-classification.md`). rc5 rerun:
first-try admission, in_sync 357s.

The rc4 run's OTHER backoff cycle: window #1 lost to the F48 zombie
consumer racing its own sever (severable only once the head went stale) —
recorded in the F54 doc §3, deliberately unfixed on release eve.

## Clean 3.6e (rc5) — the F43 acceptance, finally uncontaminated

Terminate leg node (aws-2) under RWX writes → identity swap 187s → standby
208s → cutover bounce 324s (claim taken on the reservation; yields=0,
seizures=0) → in_sync 356s → settle window HELD → latent-pin sweep clean →
witness clean → db PASS (verify reads --previous) → **pg-0 never restarted**.
Ledger stall 159s = the documented RWX detection-latency residual (fast_io_fail
is 20s; detection dominates), same magnitude as the runai measurement.

## Environmental events (not the product)

- `runak-aws-1` spot-reclaimed (`instance-terminated-no-capacity`) mid
  rc4→rc5 helm roll. rc5 controller self-healed the affected RWO volume
  live: `[REPLACE] Replica identity swapped off lost node — full build
  queued` (leg 0 → aws-4). Recovery: delete Node object, add replacement
  worker via trove (`runak-aws-1785210466`), DS roll completed, disk
  init (data disk only), harness reset.
