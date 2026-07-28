# runal campaign — the P1/P3/P4 live-validation gate (2026-07-28)

**Cluster:** runal (trove project 56), 4× i4i.xlarge workers + spot CP, all
spot, us-west-1, k8s v1.34.10. **Image:** `dilipdalton/flint-driver:1.22.0-rc1`
(amd64, `sha256:237c80c1…`) built from main @ `91e8865` — the P1 leader
election (`e64c9a7`) + P2 newtype tranche 1 (`3e84e22`) + P3 probe-not-parse
(`a232ac4`) + kubelet-capture harness prep (`91e8865`) stack. Chart 1.21.0,
driver image overridden. Deleted same day, zero residue verified.

**Headline: first campaign with NO new F-number.** Every prior release gate
found at least one.

## P3 canary — 2.9 run A (clean config): PASS, best 2.9 on record

Re-init 203s, **in_sync 325s, 0 window flips, 0 zombie severs, 0 E_f
duplicate errors**, stall 1s, db PASS. The attach-converge/probe conversions
did not perturb the hot-rejoin path; no admission retry was lost to
error-shape misreads. (Run B: in_sync 262s; run C: 384s — the 264s-class is
now the norm, not the exception.)

## P1 live gate — kube-Lease leader election: ALL THREE HALVES PROVEN

1. **Mis-granted candidate stands by** (the F50/F53 config-drift shape,
   re-created deliberately): `FLINT_ORCHESTRATORS=enabled` set on the
   dashboard → it campaigns (`[ORCH_LEASE] campaigning`) and NEVER acquires
   across many lease periods; lease stays with the controller,
   transitions=0; its orchestrator loops emit startup banners only, every
   tick skips on `is_leader()`.
2. **Dead-holder takeover via CAS**: controller pod killed mid-2.9 (run B,
   T0+249s) → new holder within ~45s (termination grace + 15s observation
   window), transitions 0→1. Run B PASS through TWO lease transitions
   (stall stayed 1s — handovers invisible to I/O). Then scale-to-0 → the
   dashboard, as sole candidate, ACQUIRED (transitions=2).
3. **The usurper actually orchestrates** (run C): with the dashboard
   holding the lease and the restored controller standing by, 2.9 rebuilt
   end-to-end under dashboard orchestration — its log shows
   `[HOT_REJOIN] Window committed … window_ms=236` (the same raid
   admission F53 caught it doing ILLEGALLY on runaj — now done legally as
   elected leader); the controller logged ZERO orchestrator work lines.
   PASS, in_sync 384s. Reverse handover: mis-grant removed → dashboard pod
   dies → controller re-acquires ~45s (transitions=3).

Deviation from the state-doc sketch: the 3.6e-with-mis-grant variant was
deliberately NOT run — P4's stall number needed the shipped topology
(single-variable discipline), and the standby/takeover/usurper invariants
were already proven three ways on 2.9.

## 3.6f co-location rerun: PASS — the runaj mystery did NOT reproduce

The exact runaj shape arose naturally (server moved aws-1 → aws-2 = pg-0's
own node). Server Ready 46s, **pg-0 zero restarts**, zero ESTALE / zero
PANIC (F52 held), F49/F47 invariants held, stall 40s (runaj: 457s + pg-0
killed at T0+8s). New check (i) captured kubelet's journal on the consumer
node: **zero kill-signature lines** (`36f-kubelet-runal-aws-2.log`).
**Attribution hypothesis, now evidence-backed:** the runaj pg-0 kill was
downstream of the pre-fix F52 ESTALE/PANIC crash loop (kubelet killing a
crash-looping pod wedged on the dead NFS mount → D-state →
FailedKillPod) — with F52 fixed the entire causal chain vanishes. The
capture stays armed in the harness should it ever recur.

## 3.6e — F43 acceptance re-proven + the P4 measurement

Mechanics ALL green: leg node terminated (aws-1), identity swap 213s,
standby 245s, **cutover took the claim** (nfs bounce 277s), replacement
in_sync 308s, settle window HELD (2/2, writer_set=2, head_in_use=0),
latent-pin sweep clean, cutover events 1/1/0, **yields=0 seizures=0**,
witness clean, no acked-tail risk.

**P4 number: ledger stall 177s** on the leg-loss path — fast_io_fail is
20s, so detection latency dominates by ~157s (consistent with the
runai/runak ~150-159s class). The ≤60s target means closing the detection
gap, not the transport gap. This is the next availability workstream.

**The db FAIL is environmental and fully attributed:** runal-aws-2
(pg-0 + relocated server + leg_1) was SPOT-RECLAIMED mid-verify —
`i-0d46406d63a472b92` shutting-down; amcheck died rc=143 (SIGTERM when
kubelet vanished), writability probe unreachable. The ledger sub-check
had already completed: **all 2351 acked writes present**.

## Bonus (unplanned): real double node loss, zero acked loss

After the drill consumed aws-1, the reclaim took aws-2 — server + client +
leg simultaneously. Recovery (one manual step: `kubectl delete node
runal-aws-2`, the runak recipe): server resurrected on aws-4 in ~20s onto
its surviving leg's node (the F49-fixed consumer-local assembly, in
anger), pg-0 rescheduled and 2/2 Running in ~5min15s. Post-recovery
verify-db: **PASS — all 2636 acked writes present** (writes continued
through recovery), amcheck clean, writable. pglog honestly UNPROVEN (prior
instance's log died with the node).

## Files

- `2-2.9-1785253152/` — run A (P3 canary)
- `2-2.9-1785253748/` — run B (controller-kill mid-drill)
- `2-2.9-1785254209/` — run C (rebuild under dashboard leadership)
- `3-3.6f-1785255315/` — co-location rerun
- `3-3.6e-1785255529/` — F43 re-proof + P4 measurement (db FAIL = reclaim)
- `36f-kubelet-runal-aws-2.log` — the consumer-node kubelet journal
- `../results.csv` — one row per drill
