# State of the driver — post-v1.21.0 assessment and next-campaign priorities

**Written 2026-07-28, immediately after the v1.21.0 cut.** This is the input
doc for planning the next campaign: the bottom-line evaluation, the
prioritized structural work, and the open investigations, each with its
evidence and its acceptance gate.

## Bottom line

**Late-beta, production-candidate for durability-critical /
availability-tolerant workloads.** RWO r2 is close to ready today. RWX with
live databases became *viable* with F52 (postgres rode through cutover with
zero restarts on runak's clean 3.6e) but needs the detection-latency work
and the 3.6f attribution mystery resolved first.

What's proven: **zero acked-write loss across every drill on every cluster
to date** — durability is measured, not asserted (ledger oracle + witness +
amcheck). Self-healing is live-proven, including against a real unplanned
spot reclaim (runak: `[REPLACE]` dispatched autonomously mid-diagnosis).

What isn't converged: the orchestration layer. F47–F54 = eight findings in
four days of campaigning, and the *release gate itself* found F54. Each
campaign still pays for itself. Three defect classes recur; two have known
structural fixes that keep getting deferred (below).

**The convergence signal to aim for: the first release-gate campaign that
finds nothing new.** No release to date has cleared its own gate without at
least one new F-number.

## Priorities for the next campaign (in order)

### P1 — kube-Lease leader election for the orchestrator block
> **IMPLEMENTED 2026-07-28** (`orchestrator_lease.rs`; role grant =
> candidacy, Lease = activity; 907 tests). **Live gate still owed** — run
> the acceptance drill below in the next campaign before calling P1 done.
- **Class it kills:** singleton-by-configuration (F50: operator pod as a
  second hot-rejoin orchestrator; F53: dashboard backend as a third —
  two instances of the family in one day).
- **Today's state:** `orchestrator_role.rs` + `FLINT_ORCHESTRATORS` grants
  is an env-var honor system, mechanism-free. It fixed the shipped
  configurations, not the class: any future process that sets the wrong env
  (or a chart merge that drops the `disabled` line) re-opens it silently.
  The single-orchestrator harness check only catches it on clusters we
  drill.
- **Seam:** `orchestrator_role.rs` is where the decision already lives;
  gate the same five orchestrators (epoch scheduler, catch-up, cutover,
  hot-rejoin, NFS reconciler) on holding the Lease.
- **Gate:** 2.9 + 3.6e with a deliberately mis-granted second process
  (FLINT_ORCHESTRATORS=enabled on the dashboard) — the Lease must keep it
  inert; plus a controller-kill mid-drill to prove failover.

### P2 — the newtype tranche (StagedHandle vs StorageId)
> **TRANCHE 1 IMPLEMENTED 2026-07-28** (`identity.rs`: `StorageId` —
> normalized at construction — and `StagedHandle` — verbatim, no `Deref`
> to str; the helpers where the domains cross are retyped:
> `replica_export_nqn`/`legacy_…`/`replica_alias_nqn`, `raid_name`,
> `loopback_teardown_nqns`, `lvol_belongs_to`, the replica-family mints,
> `hotrejoin_export_nqn`; 911 tests, all-targets + musl clean;
> behavior-preserving by construction). Remaining tranches: hoist the
> types upward through fn signatures/record structs so boundary
> `StorageId::of_handle` constructions migrate to the RPC entry points
> (each construction site is greppable — that IS the tranche-2 work
> list), then type the derived-id namespaces (`lvol_name`, `volume_nqn`,
> epoch/snapshot naming — zero field bugs to date, needs an ExportId-style
> domain analysis first).
- **Class it kills:** identity-domain confusion — F44, F45/B1/B2, F46,
  F47, F51 all landed as "right helper, wrong id domain". The CI lint
  cannot catch a wrong id passed to a right helper; the type system can.
- **Today's state:** deferred twice (runae triage, runag triage). The F47
  fix was the stated prerequisite for the last tranche — it shipped in
  1.21.0, so the prerequisite is gone.
- **Gate:** compile-time only (no live drill needed); the win is `cargo
  test --lib` staying green while the signatures change. Belt: the
  existing identity CI lint stays.

### P3 — SPDK error-classification audit (probe, never parse)
- **Class it kills:** string-matched RPC errors — F48
  (`nvmf_create_subsystem` duplicate = "-32603 Unable to create…"), then
  F54 seven days later (`nvmf_subsystem_add_host` duplicate = bare
  "-32603 Internal error", no text at all). Two instances, one week, one
  RPC apart. Each cost a live campaign to find.
- **Work:** enumerate every call site classifying SPDK errors textually
  (`is_already_exists` / `is_missing` / ad-hoc `contains`) and convert the
  convergence-critical ones to state probes (the F54 pattern:
  `get_subsystem` / `subsystem_has_host` / `subsystem_has_listener` in
  hot_rejoin.rs). Make the test mocks faithful to SPDK's real duplicate
  shapes as each site converts — the F54 regression test only exists
  because the mock was taught to lie the way SPDK lies.
- **Gate:** unit-level per site; one 2.9 run as the end-to-end canary.

### P4 — RWX node-loss detection latency
- **Evidence:** ledger stall 159s on runak's clean 3.6e, ~150s measured on
  runai — vs RWO 2.5 where writes never paused. fast_io_fail (20s) is not
  the bottleneck; *detection* dominates on the RWX path. Also S2's ~237s
  admission bounce stall (reframed as a feature: RWX admission without the
  bounce).
- **This is the availability headline number** for any database-on-RWX
  story; durability is already there.
- **Gate:** 3.6e stall budget. Set an explicit target (e.g. ≤60s) rather
  than "record + investigate".

## Open investigations (carry into the next campaign's harness work)

1. **3.6f's unattributed pg-0 kill** (runaj): when the relocated server
   lands on its own client's node, pg-0 was killed at T0+8s with
   `FailedKillPod … DeadlineExceeded` (D-state on the dead NFS mount) and
   a 457s stall — no cutover, no eviction, no STS delete in evidence.
   runai's passing 3.6f lacked the co-location. **Harness gap: no drill
   captures kubelet's log on the consumer node — add it before the next
   RWX run.** Until attributed, treat server-on-client co-location as
   suspect.
2. **The csi-node roll landmine** (standing since v1.12): a DS roll
   restarts spdk-tgt under mounted PVCs → EIO. Graceful recovery (v1.15)
   covers single-node events; a full roll still needs the
   reset choreography. An upgrade story that survives a plain
   `helm upgrade` remains unshipped.
3. **F54 doc §3 residual:** prestage trusts consumer-side bdev *presence*
   over path liveness, so hot-rejoin's first window can lose one ~7-min
   backoff cycle to the F48 zombie racing its own sever (severable only
   once the head goes stale). Bounded, but it's the difference between
   357s and 264s-class recovery. Options: liveness-probe the controller in
   prestage, or extend the sever to standby heads.

## Deferred features (tracked, not campaign-blocking)

ublk online resize (kernel ≥6.16), dashboard expand UI, raw-block
NodeExpand, S1 (mitigated by clear_sb ≥v26.05), S4 (fold into next
campaign). Known pre-existing: the pnfs doctest fails on main — gate on
`cargo test --lib` (893 green at v1.21.0).

## Evidence trail

Release gates: `tests/chaos/artifacts/runak-release-gate/README.md` (runak),
plus the runaj/runai/runah/runag bundles beside it. Finding docs:
`docs/f43…` through `docs/f54…`. Release record: tag `v1.21.0` @ `092881e`.
