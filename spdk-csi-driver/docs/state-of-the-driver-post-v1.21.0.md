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
> candidacy, Lease = activity; 907 tests). **LIVE GATE PASSED same day on
> runal** (`tests/chaos/artifacts/runal-p1p3p4-gate/`): mis-granted
> dashboard stood by across many lease periods; dead-holder CAS takeover
> ~45s; the dashboard-as-usurper drove a full 2.9 rebuild
> (`Window committed` in ITS log, zero work lines in the standing-by
> controller's); reverse handover clean. Two lease transitions mid-drill
> were invisible to I/O (stall 1s). **P1 is DONE.**
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
> **IMPLEMENTED 2026-07-28** (`docs/spdk-error-classification-audit.md`
> — every site enumerated with a verdict, every shape claim verified
> against SPDK v26.05.1-pre source). Converted: all five hot_rejoin
> `bdev_nvme_attach_controller` sites → `attach_converged()` (three
> previously hard-failed behind a best-effort detach), the F49
> `drop_local` delete-of-absent (was an unmatchable `-32602`), and the
> dead `driver.rs::create_nvmeof_target` fossil deleted (3 trap sites,
> zero callers). Mock now refuses duplicate attaches with the real
> v26.05 shape. Family-A (errno-mapped lvol/raid) textual classifiers
> audited and kept. **Live canary PASSED same day on runal: 2.9 in_sync
> 325s, 0 window flips, 0 severs, 0 E_f duplicate errors — the
> 264-380s class is now the norm across three runs. P3 is DONE.**
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
> **MEASURED on runal 3.6e (2026-07-28): stall 177s** on the shipped
> topology, clean config. fast_io_fail=20s → the detection gap is ~157s.
> Confirmed the 150-177s class across three clusters; the ≤60s target
> means closing DETECTION, not transport.
>
> **ATTRIBUTED + FIX IMPLEMENTED same day**
> (`docs/p4-dead-target-detection.md`): the gap is the TCP blackhole — a
> terminated instance sends no RST, the qpair never errors, and
> fast_io_fail only counts from the reset path, so the raid kept the dead
> base configured 116–176s (runak's complete 3.6e logs; stale-mark +10s
> and swap +10s after that — everything downstream was already fast). RWO
> 2.5 passed on RST luck, not a better path. Fix = `DeadTargetTimeouts`
> (nvme_recovery.rs): global `bdev_nvme_set_options` at tgt bring-up —
> transport_ack_timeout=13 (TCP_USER_TIMEOUT ≈8s, kernel-enforced),
> timeout_us=30s + action_on_timeout=reset, tcp_connect_timeout_ms=10s;
> applied at agent startup before the first attach and re-applied on
> baseline-collapse (tgt restart), -EPERM tolerated. Expected stall ≈30s.
> 3.6e now records `degrade=<s>` and gates on P4_STALL_BUDGET=60.
>
> **LIVE GATE PASSED same day on runam 3.6e: stall 36s ≤ 60s** (degrade
> 76s from the terminate call incl. instance shutdown; swap 129s, in_sync
> 298s — the whole chain sped up). **P4 is DONE.** The same drill found
> **F55** (`docs/f55-bounce-truncated-reply-eio.md`): the cutover bounce
> truncates in-flight RPC replies → client EIO → pg PANIC — deterministic
> mid-checkpoint repro, quiesced bounces clean; fixed same day
> (`DrainGate` frame-atomic shutdown, `a4902ef`); **F55's own live gate
> PASSED 2026-07-29 on runan as new drill 3.13** — the checkpoint forced
> into flight, then the kill: drained=1, deadline never expired, panic=0,
> eio=0, pg restarts 0. P4 also reproduced on a second cluster: kill
> stall 37s (runam 36s) vs the 150–177s class before the fix. Evidence:
> `tests/chaos/artifacts/runam-p4-f55-gate/` and `runan-s2-gate/`.
- **Evidence:** ledger stall 159s on runak's clean 3.6e, ~150s measured on
  runai — vs RWO 2.5 where writes never paused. fast_io_fail (20s) is not
  the bottleneck; *detection* dominates on the RWX path. Also S2's ~237s
  admission bounce stall (reframed as a feature: RWX admission without the
  bounce).

> **S2 DESIGNED (model-first) 2026-07-28** —
> `docs/s2-bounce-free-rwx-admission.md`: in-place admission via the
> RWO-proven hot-rejoin window on the live serving raid; no NFS bounce
> (F55 exposure and the F48 two-head phase removed structurally). The R2
> claim arbitration is now formally verified (`formal/`,
> `AdmissionNotStarved`; the F43 mutation rediscovers the starvation
> lasso and proves the fix had to be priority, not fairness).
> **IMPLEMENTED same day** (StagedDomain in identity.rs; window +
> reconcile family domain-routed; cutover admission arm deferred to the
> window, bounce = relocation only; RWX consumer = backing PV's VA; 926
> lib tests incl. the RWX-domain crash sweep; musl clean). Kill switch
> `FLINT_RWX_INPLACE_ADMISSION` (default ON).
> **LIVE GATE PASSED 2026-07-29 on runan (drill 3.12, first run):
> window_ms=228 (228 ms of quiesce), admit_stall 1s vs the bounce path's
> 59s measured the same day on the same cluster by 3.6e with the kill
> switch OFF — a ~59× win on the admission's guest cost. NOTE the doc's
> older "~237s bounce" is the runag-era figure and overstates today's
> alternative by ~4×: P4, F52 prewarm and DrainGate each shortened the
> bounce. Also nfs pod uid + restarts unmoved, zero CutoverStarted,
> zero ESTALE/PANIC, pg-0 restarts 0, settle held 2/2, db PASS. The
> claims log shows catch-up yielding (`held_by="hot-rejoin"`) — the F43
> mutation's priority prediction observed on the wire. S2 is DONE; the
> RWX availability headline is now the P4 detection number (37s), not
> the admission. Evidence `tests/chaos/artifacts/runan-s2-gate/`.**
> **F55's live gate PASSED the same day** (new drill 3.13,
> bounce-mid-checkpoint forced rather than lucky: drained=1, zero
> PANICs) — the last item owed out of runam.
- **This is the availability headline number** for any database-on-RWX
  story; durability is already there.
- **Gate:** 3.6e stall budget. Set an explicit target (e.g. ≤60s) rather
  than "record + investigate".

## Open investigations (carry into the next campaign's harness work)

1. **3.6f's unattributed pg-0 kill** (runaj): **RESOLVED-BY-ABSENCE on
   runal (2026-07-28).** The exact co-location shape re-ran on rc
   `1.22.0-rc1` (server moved onto pg-0's own node): pg-0 zero restarts,
   zero ESTALE/PANIC, stall 40s (vs 457s), and the new kubelet capture
   (check (i)) recorded ZERO kill-signature lines. Evidence-backed
   attribution: the runaj kill was downstream of the pre-fix F52
   ESTALE/PANIC crash loop; F52's fix removed the whole causal chain.
   The capture stays armed in the harness should it ever recur.
   Server-on-client co-location is no longer suspect.
2. **The csi-node roll landmine** (standing since v1.12): a DS roll
   restarts spdk-tgt under mounted PVCs → EIO. Graceful recovery (v1.15)
   covers single-node events; a full roll still needs the
   reset choreography. An upgrade story that survives a plain
   `helm upgrade` remains unshipped.
   > **DESIGN + FORMAL GATE 2026-07-28** (implementation owed):
   > `docs/maintenance-drain-csi-node-roll.md` — the maintenance
   > tranche of `FlintReplication.tla`. Three guards, each proven
   > separately necessary by mutation: drain-before-restart, a
   > readmission barrier (pod-ready is NOT it — TLC downs the volume
   > in 5 steps with zero failures on today's semantics), and a leased
   > suppression mark. P4 detection stays always-on by design. The
   > un-modelable half (local ublk continuity) is the ublk-recovery
   > spike + drill 3.14, both owed with the implementation.
3. **F54 doc §3 residual:** prestage trusts consumer-side bdev *presence*
   over path liveness, so hot-rejoin's first window can lose one ~7-min
   backoff cycle to the F48 zombie racing its own sever (severable only
   once the head goes stale). Bounded, but it's the difference between
   357s and 264s-class recovery. Options: liveness-probe the controller in
   prestage, or extend the sever to standby heads.
   > **CLOSED 2026-07-28:** prestage's consumer pre-connect is now
   > identity-verified (prestage_inline's uuid check, ported); the zombie
   > is detached and re-attached fresh before the window. Regression test
   > with a frozen-AER mock, verified red pre-fix. 918 lib tests.

## Deferred features (tracked, not campaign-blocking)

ublk online resize (kernel ≥6.16), dashboard expand UI, raw-block
NodeExpand, S1 (mitigated by clear_sb ≥v26.05), S4 (fold into next
campaign). Known pre-existing: the pnfs doctest fails on main — gate on
`cargo test --lib` (893 green at v1.21.0).

## Evidence trail

Release gates: `tests/chaos/artifacts/runak-release-gate/README.md` (runak),
plus the runaj/runai/runah/runag bundles beside it. Finding docs:
`docs/f43…` through `docs/f54…`. Release record: tag `v1.21.0` @ `092881e`.
