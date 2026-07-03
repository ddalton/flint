# Tier-2 phase 7b — open decisions to resolve before implementation

**Status:** reviewed 2026-07-01 (Claude Fable 5, against the design/eval/spike
docs, the v2 patch, and the SPDK v26.05 tree). Recommendations recorded under
each decision below; code-verified findings, design items, and an
implementation plan appended. **Both decisions confirmed by the operator 2026-07-01, adopting the
recommendations as written**: Decision 1 = (B) with the per-PV
`hot-rejoin: "disabled"` opt-out and the synthetic-RWX exclusion;
Decision 2 = (C) then (A). Nothing gates implementation; the plan below is
active. Original framing preserved unchanged.

Phase 7b (hot-rejoin orchestration) got a GO from the 7a spike
(`tier2-spike-2026-06-12.md`); the two knobs the design/eval leave open are
the operator/product call, not derivable from the code.

## What 7b signs up for (context for the decisions)

From `tier2-spike-2026-06-12.md` "What 7b orchestration signs up for" +
`incremental-replica-rebuild.md` §7:

- The window sequence + unwind ladder in the controller (leased quiesce →
  final snapshot `E_f` on a survivor → esnap-clone head on R_dst →
  `bdev_raid_add_base_bdev --skip-rebuild` → unquiesce; unwind on any
  failure: unquiesce, delete the clone, promote-or-delete `E_f`).
- Attach pre-staging / concurrency to hold the ~2 s window (pre-create the
  subsystem+listener skeletons outside the window; run the two NVMe-oF
  attaches concurrently — only snapshot→clone→add must be serial).
- Record flip after the add, then the esnap **backfill + `set_parent`
  localization**; no full-redundancy report until localization completes;
  R_src death mid-backfill → revert R_dst to `stale`.
- Dead-controller reaping (node-agent reconcile reaps controllers whose
  subsystem now rejects them — the reconnect-flood operational finding).
- The §9-8 adversarial set re-run per SPDK bump.

The whole loop is dark by default behind a global `FLINT_HOT_REJOIN=enabled`
gate (mirroring `FLINT_CATCHUP` / `FLINT_CUTOVER`), and it only ever acts on
a **ready standby** (lag ≤ threshold) of an **attached** multi-replica
volume. The two decisions below sit *inside* that gate.

---

## Decision 1 — Trigger policy (per-volume activation)

**The original question (verbatim, from the design review):** the doc/eval
prescribe the *target class* (no-opt-in restart-intolerant RWO), but are
silent on the *mechanism*. Does hot-rejoin act automatically on that whole
class, or require its own positive opt-in annotation the way the Tier-1 RWO
bounce does (`rejoin-bounce: enabled`)? A genuine knob the doc leaves open.

**The tension.** Hot-rejoin is *non-disruptive* (no pod restart), which
argues for making it automatic — unlike the `rejoin-bounce` restart, there
is no availability cost to weigh. But it rides the *correctness-critical*
`skip_rebuild` patch: a wrong in-sync admission serves stale reads — silent
corruption, the §0 cardinal sin — which argues for an explicit, surgical
per-PV opt-in. The global `FLINT_HOT_REJOIN` flag already keeps it dark
until an operator turns the mechanism on cluster-wide.

**Options.**

- **(A) Per-PV opt-in annotation.** Require
  `flint.csi.storage.io/hot-rejoin: "enabled"` on the PV in addition to the
  global gate. Parity with `rejoin-bounce`; the correctness-critical path
  fires only where an operator explicitly asked. Most conservative. Cost:
  the restart-intolerant class the feature exists for gets nothing until
  someone annotates each volume — the "unbounded residual" the eval flags
  persists for un-annotated volumes.
- **(B) Auto for attached RWO that did *not* opt into `rejoin-bounce`.**
  Exactly the eval's named target class. RWX (Tier-1 bounce is already
  near-transparent) and volumes that opted into the disruptive bounce stay
  on Tier-1; everything else — the databases that can't restart and won't
  reschedule — gets synchronous redundancy back with zero per-volume config.
  Note this deliberately *inverts* `rejoin-bounce`: the annotation opts into
  the disruptive path, its absence opts into the non-disruptive one. Also
  note it steps into the exact case `cutover.rs` currently answers with
  "attached; rejoin-bounce not enabled — waiting for a natural reassembly"
  (`plan_cutover`), turning that indefinite wait into a hot rejoin.
- **(C) Auto for any attached multi-replica RWO with a ready standby.**
  Broadest; annotation-independent. `rejoin-bounce` then only governs the
  now-secondary bounce path. Simplest gate, widest blast radius for a wrong
  admission.

**Leaning (not decided):** (A) for a correctness-critical carried patch —
opt-in parity with `rejoin-bounce`, an operator accepts the patch per
volume — unless the product goal is "the restart-intolerant class heals with
no per-volume config," in which case (B) targets exactly that class while
leaving RWX and bounce-opted volumes untouched. (C) only if the global gate
is considered sufficient blast-radius control on its own.

**Recommendation (2026-07-01 review): (B), plus a per-PV opt-out.** The
eval's GO case rests entirely on the no-opt-in class ("Tier 2 is not an
optimization here; it is the only mechanism") — so the leaning's "unless
the product goal is…" clause is already answered by the eval itself.
Option (A)'s double opt-in (default-off global gate AND per-volume
annotation) recreates the documented-unbounded residual for exactly the
volumes the patch was justified by. Nor does the annotation reduce the
risk it is meant to guard: a wrong `skip_rebuild` admission corrupts
annotated volumes just as silently — what actually bounds the risk is the
global gate (turning on `FLINT_HOT_REJOIN` is the operator's deliberate
acceptance of the patch), the validation campaign, and staged rollout.
Add `flint.csi.storage.io/hot-rejoin: "disabled"` as a surgical per-PV
opt-out — a lever none of the three options as written provides. Side
benefit: under (B) the cutover and hot-rejoin planners operate on disjoint
classes (bounce-opted vs. not), so they cannot race on one volume. If
maximum first-release conservatism is wanted, shipping (A) semantics is
defensible only with the planner structured so the flip to (B) is a config
default change, and a committed flip after the first live campaign.

**Sub-decision (B) forces — synthetic RWX backing PVs must be excluded
explicitly.** The `nfs-server-<vol>` backing PVC is itself an attached
multi-replica RWO volume that never opts into `rejoin-bounce`, so a
literal (B) would hot-rejoin it — contradicting "RWX stays on Tier-1" and
racing `plan_cutover`, which owns those bounces. Classify via the existing
`record_pv_name` resolution. (Hot-rejoining the backing PV is strictly
less disruptive than the NFS bounce and could subsume it later — a
follow-up, not 7b scope.)

**Implementation touch-point:** the `plan_hot_rejoin(view, cfg)` planner
(new, mirroring `cutover::plan_cutover`) — the decision is a branch on
`view.consumer.is_some()`, `view.rwo_bounce_enabled`, and/or a new
`view.hot_rejoin_enabled` annotation read.

---

## Decision 2 — Validation scope (where 7b's deliverable line sits)

**The original question (verbatim):** §9 makes "Tests" its own phase (8);
every prior phase (1–5b) landed on `main` unit-tested with e2e deferred to a
campaign, and 7a was the live drill for the patch. So where 7b's deliverable
line sits — unit tests + runbook vs. carry through the live adversarial run —
is a process choice the doc doesn't dictate.

**Options.**

- **(A) Unit tests + runbook now; live deferred to a campaign.** Implement
  the orchestrator + window + unwind + backfill/localize with unit tests
  against a fake transport (mirroring `catchup.rs` / `cutover.rs`), an
  operator runbook, and the §9-8 adversarial set documented as the campaign
  checklist. Matches how phases 1–5b landed ("unit-tested on `main`, e2e
  deferred"). Live drills (orchestrator kill in-window, R_src kill during
  backfill, crash between add and record flip) run in the next cluster
  campaign.
- **(B) Also drive the §9-8 live adversarial run now.** Everything in (A),
  then execute the adversarial drills on a live patched (`:tier2-spike`)
  cluster before calling 7b done. Requires a reachable patched cluster this
  session; highest confidence, largest session.
- **(C) Mechanism-only library first.** Land just the window sequence +
  unwind ladder as a unit-tested library (no controller trigger loop, no
  Decision 1 needed yet), so the smallest correctness-critical piece merges
  first; wire the orchestrator loop + trigger policy in a follow-up.

**Leaning (not decided):** (A) — matches the established phase cadence and
does not block on cluster availability; (B) only if a patched cluster is up
and the campaign is happening in the same session; (C) if the goal is to
de-risk the merge by landing the correctness-critical core ahead of the
trigger policy debate.

**Recommendation (2026-07-01 review): (C) then (A).** They are not really
alternatives — (C) is (A) split into two merges — and (C)'s decisive
advantage is that the mechanism library does not depend on Decision 1 at
all, so implementation starts before the trigger-policy call is made and
the correctness-critical core gets reviewed in isolation, ahead of the
policy debate. One scope correction: (C) as written above ("just the
window sequence + unwind ladder") is not a shippable mechanism — a hot
rejoin without backfill + `set_parent` localization leaves a permanent
esnap dependency (R_src's node a SPOF for the not-yet-local clusters, the
failure mode the eval flags as new in kind). Backfill/localization belongs
in the library; only the trigger loop defers. Deferring live drills is
defensible despite the correctness-critical patch because the risk split
is favorable: 7a already live-drilled the patch mechanism itself (window,
scrub, all three crash drills); what 7b adds is control-plane choreography
whose failure mode is fail-safe by the rev-5 contract, and the genuinely
dangerous piece — the flip to `in_sync` gated on localization — is exactly
what fake-transport unit tests exercise well.

---

## Note on prerequisites (not open questions — just flagged)

Neither decision changes these, but 7b implementation also carries:
- the esnap-localizing record marker (a live-but-not-yet-local raid member
  must be excluded from the `catchup.rs` chase — shallow-copy write-claims
  the destination and would fight raid writes — and must not report full
  redundancy; `mark_in_sync` flips it only after `set_parent` localization);
- crash safety per the rev-5 contract (a crash anywhere in the window/backfill
  leaves the record at `standby`; the next assembly excludes the replica and
  the chase resumes — so the marker rides *on top of* `standby`, not a new
  terminal state).

---

## Review findings (2026-07-01) — verified against code

### SPDK primitives 7b leans on: both confirmed fit for purpose

- **Pre-staged attach works end-to-end — the ~2 s window is realistic.**
  The nvmf target raises the namespace-change async event on `add_ns`
  (`nvmf_subsystem_ns_changed`, `lib/nvmf/subsystem.c:2703` →
  `nvmf_ctrlr_async_event_ns_notice`), and the `bdev_nvme` initiator
  registers the ns-changed callback at controller attach
  (`bdev_nvme.c:5940` → `nvme_ctrlr_populate_namespaces`), creating the
  bdev when the namespace appears. So 7b pre-creates subsystem/listener
  skeletons and pre-connects both controllers *outside* the window; inside
  it, each "export+attach" collapses to one `add_ns` plus AER latency.
  **Correction to the spike's design item:** "run the two attaches
  concurrently" is not actually available — the dependency chain
  snapshot → attach-`E_f` → clone → attach-head → add is strictly serial.
  Pre-staging is the design, not one of two options.
- **`set_parent` localization is a first-class path.** `bs_set_parent_refs`
  (`lib/blob/blobstore.c`) explicitly handles esnap clones: it strips
  `SPDK_BLOB_EXTERNAL_SNAPSHOT` and the external-snapshot xattr when
  re-parenting onto a local snapshot. Orchestrator-facing constraints:
  parent must be a snapshot; exact cluster-count match; `-EBUSY` on a
  concurrent locked op (serialize against shallow copies — the existing
  §10-3 discipline); `-EEXIST` when already the parent (idempotent-re-run
  friendly).
- **`E_f` source-independence (the localization correctness argument).**
  `set_parent` moves no data, so the local parent must be **bit-identical**
  to the external `E_f` in every cluster the head has not locally COWed.
  Satisfiable precisely because `E_f` is cut under the quiesce with io
  drained — survivor `E_f` images are bit-identical (the spike's md5 scrub
  demonstrated exactly this) — so any §5-correct base-inclusive replay to
  `E_f` is a valid localization target regardless of which survivor sources
  it. Localization thereby inherits `catchup.rs`'s existing machinery; no
  new correctness proof is needed. This argument belongs in the
  localization design paragraph (design item 1 below).
- **The tiers stay structurally separated at the RPC layer:** the v2 patch
  rejects a `skip_rebuild` add with `-EINVAL` unless the raid is ONLINE, so
  the flag cannot leak into the CONFIGURING/assembly path where Tier-1
  admission owns membership — matching the spike's "compose without
  coordination" observation.

### Two v2 patch hardening items (→ patch v3, plan phase 7b-0)

1. **Lease expiry can race an in-flight add.** The `-EPERM` lease check
   runs at RPC entry (`rpc_bdev_raid_add_base_bdev`), but channel
   installation proceeds asynchronously via `spdk_for_each_channel` across
   threads. If the lease expires mid-iteration, the auto-unquiesce fires
   and writes resume while some channels still lack the new slot — a write
   submitted on an unpopulated channel never reaches the new base: the
   exact silent divergence the gate exists to prevent. Closed operationally
   by the pre-add renew (10 s lease vs. a sub-second add — the spike script
   already does this), but v3 should pin the lease while an add is in
   flight (expiry defers rather than fires), and the orchestrator must
   treat *renew-immediately-before-add* as a hard invariant either way.
2. **Unquiesce failure after lease-free leaves a permanent quiesce.**
   `rpc_bdev_raid_unquiesce` frees the lease *before* calling
   `raid_bdev_unquiesce`; a nonzero return leaves the bdev quiesced with no
   lease and no expiry poller — guest io hung with no auto-release, the
   precise incident the lease was built to prevent. The expiry path has the
   same shape (logs the failure, lease already freed). v3: release the
   lease only after unquiesce initiates successfully, or re-arm a short
   expiry on failure.

## Design items to fold into implementation (2026-07-01)

1. **The localization choreography needs its own design paragraph** — it is
   one sentence in §7 and roughly as intricate as the window. Pieces to pin
   down: the shallow-copy landing pad (the old standby head), the
   base-inclusive replay to `E_f` (valid from any in-sync source per the
   source-independence finding above), snapshotting the pad as local `E_f`,
   `set_parent` of the esnap head onto it, disposal of the leftover pad
   clone, and the crash point at every step (each must resolve to
   "record still `standby`+marker → revert-and-resume").
2. **The esnap-localizing marker needs an explicit lifecycle.** Set at the
   record flip (with `active_lvol_uuid` → the esnap-clone head); cleared
   either by localization → `mark_in_sync` or by the revert when a
   post-crash chase resumes. While the raid holding the member is online:
   excluded from the chase (shallow-copy write-claim vs. raid writes) and
   from redundancy reporting. At the next assembly: excluded from
   membership and treated as **revert-first** — never admitted directly
   (its local chain does not reach `E_f`; its parent is an external bdev
   that may be gone). The health monitor must tolerate a base in the raid
   whose record says `standby` (the phase-4 fixes handled only the inverse:
   a standby *missing* from the raid).
3. **Crash-time cleanup must be reconciler-shaped, not just the in-line
   unwind ladder.** Orchestrator death mid-window strands: the `E_f` export
   subsystem on R_src, the esnap clone on R_dst, and an unrecorded
   epoch-named `E_f`. The clone is reaped by the resumed chase's revert;
   the unrecorded epoch converges when the scheduler's next cut hits EEXIST
   at the same seq. But the **`E_f` export subsystem has no owner** — the
   orphan sweep only reaps objects keyed by *absent* PVs and this PV is
   alive. Name an owner (the catch-up hygiene pass is the natural home).
4. **Per-volume mutual exclusion across chase / cutover / hot-rejoin.** The
   rev-5 contract makes races *safe*, but safe-and-wasteful still burns a
   quiesce window and an unwind (e.g. a cutover bounce restaging
   mid-window). Generalize the catch-up in-flight set to a per-volume
   single-operation claim shared by all three planners.
5. **Observability + the eval's fallback.** Window-duration metric with the
   2 s target as an alert; `HotRejoinSucceeded/Unwound/Failed` events; a
   localization-lag metric (the backfill window is the new SPOF exposure —
   it should be observable, not just "not reported fully redundant").
   Carry the eval's fallback forward: if in-controller p99 cannot hold
   ~2 s, reconsider the atomic-add variant (§7 option b).
6. **Dead-controller reaping is separable.** The reconnect flood is
   triggered by replica-node recreation plus re-fencing, which Tier-1 flows
   already produce today — it is really a Tier-1 operational bug the spike
   found, and can land ahead of the rest of 7b.

## Implementation plan (recorded 2026-07-01)

**Phase 7b-0 — patch v3 + separable fixes** (no decisions needed):
- v3 of `raid-skip-rebuild.patch`: pin the lease during an in-flight
  `skip_rebuild` add; fix the unquiesce-vs-lease-free ordering in both the
  RPC and expiry paths. Rebuild `spdk-tgt` on the remote build node and
  re-run the `scripts/tier2-spike.sh` behavioral validation (the
  per-revision discipline the eval priced in).
- Dead-controller reaping in the node-agent reconcile (design item 6).

**Phase 7b-1 — mechanism library** (Decision 2 (C) core; independent of
Decision 1):
- `hot_rejoin.rs` in the controller process beside `catchup.rs` /
  `cutover.rs`, reusing its transport and record-store abstractions:
  pre-staging (skeletons + controller connects outside the window), the
  window sequence (leased quiesce → `E_f` cut via `execute_cut` on
  survivors → `add_ns` `E_f` → esnap-clone head on R_dst → `add_ns` head →
  renew → add `--skip-rebuild` → unquiesce), the unwind ladder, the record
  flip (+marker, `active_lvol_uuid`), backfill + `set_parent`
  localization, `mark_in_sync` clearing the marker, R_src-death →
  revert to `stale`.
- Marker semantics wired into `catchup.rs` (chase exclusion, revert-first
  admission) and the health monitor (design item 2).
- The reconciler for the stranded-`E_f`-export crash case (design item 3).
- Unit tests against the fake transport: full choreography, every unwind
  rung, crash-resume at each step, the EBUSY/EEXIST paths.

**Phase 7b-2 — trigger loop** (Decision 1 confirmed 2026-07-01: (B)):
- `plan_hot_rejoin(view, cfg)` mirroring `plan_cutover`: per the (B)
  recommendation — attached multi-replica RWO, no `rejoin-bounce` opt-in,
  not a synthetic RWX backing PV, no `hot-rejoin: "disabled"` opt-out,
  ready standby (lag ≤ threshold), per-volume claim free — behind
  `FLINT_HOT_REJOIN=enabled`.
- The shared per-volume claim (design item 4); events + metrics (item 5).

**Phase 7b-3 — validation tail** (Decision 2 (A)):
- Operator runbook; the §9-8 adversarial set documented as the next
  campaign's checklist: orchestrator kill in-window, R_src kill
  mid-backfill, crash between add and record flip, in-controller window
  p99 vs. the 2 s target.

## Status (2026-07-01, end of session — pickup point)

Phase 7b-0 is code-complete on `main`; its behavioral validation is
blocked on cluster availability (June's spot build node is gone; cluster
being re-provisioned).

- **Patch v3 committed** (`da74444`, `raid-skip-rebuild.patch`): both
  hardening items above, plus the strengthened held-lease check (the add
  now requires an ARMED lease — not mid-acquire, not mid-release, poller
  registered — closing a third window where an add could slip in while
  the initial quiesce RPC was still in flight). `bdev_raid_quiesce_list`
  gains `pin_count` / `expired_while_pinned` / `releasing`. Validated
  locally: applies clean on v26.05 + the five other carried patches in
  Dockerfile order; `genrpc.py` schema/CLI/C lints green;
  `clang -fsyntax-only -Wall` clean on both patched C files. **Pending
  items done same day — see "7b-0 validation complete" below.**
- **Dead-controller reaping committed** (`72e2731`):
  `src/controller_reap.rs` pure planner (strict flint-shape prefix,
  raid-base guard, positively-dead states only, 3 strikes) + the node
  agent's 60 s monitor-tick pass. Gates: `FLINT_CONTROLLER_REAP=disabled`,
  `FLINT_CONTROLLER_REAP_STRIKES` (default 3). 9 unit tests; full suite
  green (476).
- **Orchestrator contract notes for 7b-1, discovered writing v3:** treat
  `-ENOENT` from `bdev_raid_unquiesce` as already-released (an expired
  lease's auto-release may have won); `-EBUSY` during a pinned add means
  retry after the add's response; renew-immediately-before-add stays a
  hard invariant (the pin closes the race, the renew keeps the window
  honest); a failed unquiesce no longer strands the quiesce — the lease
  survives and its expiry poller retries.
- **Next up:** finish 7b-0 validation once the cluster is back, then
  phase 7b-1 (`hot_rejoin.rs` mechanism library per the plan above).

## 7b-0 validation complete (2026-07-01 evening, cluster `runj`)

**Cluster.** trove-provisioned `runj`, us-west-1: 3× `i4i.large` SPDK
workers (435 GiB lvstores, **1 MiB clusters**), 1× `c5d.4xlarge`
build/consumer node (docker data-root on its NVMe ⇒ SPDK skips the disk,
so both raid legs stay on i4i workers — June topology), `t3.medium` CP.
Two provisioning gotchas, both worked around by hand:
trove keys Flint's install mode off the **control-plane** instance type
(`provider.rs is_spdk_eligible(cp_instance_type)`) — a t3.medium CP gets
the NFS-only chart even with all-NVMe workers; re-installed in SPDK mode
manually and added a control-plane-excluding affinity to the node DS
(no hugepages on the CP ⇒ its pod otherwise pends and wedges the DS
rolling update). trove's `MultiProvider::add_node` also aborted AWS
scale-out when local docker was absent (kind probe used `?`); fixed in
the trove repo (`multi.rs`, tolerant dispatch like `delete_cluster`).

**Image.** `dilipdalton/spdk-tgt:tier2-spike-v3` (digest `5e6e0e57…`),
all six patches apply clean, built natively on the c5d and rolled to all
4 nodes. Portability finding: the published `spdk-tgt:1.1.1` (built on
an Ice Lake i4i) **crashes DPDK EAL on Skylake** (`c5d`) with
"unsupported cpu type: VPCLMULQDQ" — build on the oldest-µarch node;
noted in `remote-x86-build-node.md`.

**Gate.** kuttl standard suite 8/8 PASS + clean-shutdown PASS
(clean-shutdown needed one rerun: first attempt scheduled its writer on
the build node while spdk-tgt was still crash-looping on 1.1.1).

**Drills — all PASS (v2 parity + v3 additions):**

| drill | result |
|---|---|
| skip_rebuild add, no lease | -EPERM, "requires a held bdev_raid_quiesce lease" |
| lease auto-release, never renewed | released at **8.001 s / 8.000 s target** (in-container socket timing, ~8 ms poll) |
| unquiesce after auto-release | **-ENOENT** "no quiesce lease held" — matches the 7b-1 contract note |
| renewal extends | renew at t+3.007 s → release at t+8.015 s (= renew + 5 s lease) |
| full rejoin window | **10.421 s** total vs June 10.28 s (same kubectl-exec harness; same ~3 s irreducible: 2 export+attach steps ≈ 3.0 s each); both legs `configured=true`, writer uninterrupted, renew-gate honored |
| **pin observed (v3)** | `bdev_raid_quiesce_list` sample during the add: `pin_count: 1, poller_armed: true, releasing: false` (1 of 1205 lease-present samples) |

**Scrub (both-leg snapshots cut under one lease).** All
filesystem-referenced blocks bit-identical across legs. Full-device md5
**diverges by design on reused lvstores**: exactly one 1 MiB cluster
differed — survivor exposes stale bytes (prior kuttl-test data) in the
never-written remainder of a freshly allocated thin cluster, while the
esnap-clone leg materializes zeros for the same region (COW from
E_f-unallocated). June's full-device md5 parity was an artifact of
pristine lvstores. Not a data-path defect; future scrubs must compare
fs-allocated blocks only (or pre-zero the lvstore).

**Crash contract (spike-specific assertions).** Writer-pod bounce:
record never flipped ✓; restage discarded the spike leg ✓ (the
non-flint-named `spike_head` controller correctly survives the
ownership-filtered teardown and is left to the spike cleanup script).
Deviation from June: with `FLINT_CATCHUP=disabled` the stale replica was
never healed to standby, so restage hit 1.3.0's **forced stale
admission** fallback ("below the 2-base minimum with stale replicas
excluded — divergence hazard, evented") and the volume served mixed
stale/current reads until teardown — the documented pre-Tier-1 hazard
path observed live. June's "data intact" ran through standby admission
(catch-up enabled). Two takeaways for 7b: (1) strongest live motivation
yet for the 7b-2 trigger loop; (2) 7b-2 should consider a per-volume
policy preferring unavailability over stale admission once hot rejoin
owns the heal path.

**Cluster left standing for 7b-1.** kubeconfig `/tmp/trove-aws-kc-runj`
(copied to `tests/system/config/kubeconfig`); controller env pinned:
`FLINT_EPOCH_SCHEDULER=enabled`, `FLINT_EPOCH_INTERVAL_SECS=30`,
`FLINT_CATCHUP=disabled`, `FLINT_CUTOVER=disabled` (drill config — reset
before Tier-1-dependent work); SCs `flint` (clone), `flint-spdk`,
`flint-2r` (2-replica thin); build node has docker + socat proxy pod
(`kubectl port-forward pod/docker-build-proxy 23750:2375`). Spike and
scrub artifacts cleaned; fixture PVC deleted. **Next: 7b-1
(`hot_rejoin.rs`).**

## 7b-1 implemented (2026-07-01, same session) — `hot_rejoin.rs`

The mechanism library is code-complete on `main`, unit-tested against a
fake transport (suite 495 green). `src/hot_rejoin.rs` beside
`catchup.rs`/`cutover.rs`, reusing `CatchupRpc`/`CatchupStore`,
`copy_chain_to` (new target-explicit variant of `copy_chain_and_align`),
`revert_head`/`revert_head_to_empty`, `lineage_chain` and the §5 record
discipline.

**Shape.** Pre-stage (E_f export skeleton + host fence + listener on the
source, namespace-less controller pre-connect on the target, replica
export convergence + consumer pre-connect) → window (leased quiesce →
strict-fresh E_f cut on every survivor → `add_ns` E_f, AER-surfaced on
the pre-connected controller → esnap-clone head → replica-export ns swap
pad→head with two AER waits → renew → `add --skip-rebuild` with bounded
EBUSY retry → unquiesce, `-ENOENT` = already-released) → record flip →
localization (pad revert to a conservative base → base-inclusive replay
to exactly E_f → pad snapshotted as the LOCAL E_f → `set_parent` →
pad + transport disposal → `mark_in_sync`). Full unwind ladder on any
window failure, deepest rung tested.

**Design deltas vs the plan above — each strengthens the crash story:**

1. **The marker is written as INTENT before the window opens** (not only
   at the flip). This closes a hole found during implementation: a crash
   between the raid add and the record flip would leave a live head leg
   with a `stale` record, and the next catch-up tick's
   `ensure_dst_attached` would swap the replica-export namespace out
   from under the live leg. With intent-first, every crash point is
   marker-resolvable: stale+marker+leg-live → adopt (re-flip);
   stale+marker+leg-dead → scrub; standby+marker+leg-live → resume
   localization; standby+marker+leg-dead → promote if localized, else
   demote. This also gives the stranded-E_f export its owner (design
   item 3) as a *targeted* scrub — no blind per-tick sweeps.
2. **The window's E_f cut is strict-fresh** — EEXIST aborts and unwinds.
   `execute_cut`'s EEXIST-tolerance is correct for the scheduler but
   wrong here: adopting a snapshot cut at some other instant as E_f is
   precisely the divergent-leg failure the lease exists to prevent
   (scheduler racing the same seq). Full scheduler/chase/cutover mutual
   exclusion stays 7b-2's shared per-volume claim (design item 4).
3. **The E_f export NQN is `nqn.2024-11.com.flint:hotrejoin:<vol>`** —
   deliberately outside the `:volume:` prefix so 7b-0's dead-controller
   reaper can never condemn the esnap parent's controller while the
   source is merely restarting (tested against
   `controller_reap::flint_controller_prefix`).
4. **Legless marked standby resolves promote-vs-demote by inspecting the
   head's parent**: already re-rooted onto the local E_f → plain standby
   (claim released, phase-4 admission owns it — localization work kept);
   still esnap → head deleted, demoted to stale for ordinary catch-up.

**Wiring.** `ReplicaSyncRecord.hot_rejoin: Option<String>` (serde-
optional; pre-7b records parse) + `mark_hot_rejoin_intent` /
`mark_hot_rejoined` / `clear_hot_rejoin`, cleared by `mark_in_sync` and
by the chase's `record_revert`. `CatchupStore` grew the three
transitions (KubeStore impls; trait defaults refuse loudly).
`run_catchup_for_volume` dispatches marked replicas to
`hot_rejoin::reconcile_marked` and excludes them from the chase and the
bulk catch-up. `create_raid_from_replicas` excludes marked replicas from
assembly whatever their state (revert-first). The health monitor reports
`replica-health: localizing` instead of clearing the annotation while a
marker is set (design item 2's redundancy exclusion; the lag metric
proper stays 7b-2).

**Deliberate conservatisms, noted for later:** the localization backfill
base is the oldest recorded epoch present on the destination (the
original stale-since back-off is lost at the flip — over-copies, never
under-copies; carrying stale-since through the marker is the
optimization). The pad is re-reverted on localization resume (loses
partial copy progress, never correctness).

**Not yet done:** a live behavioral run of the library on `runj` — the
mechanism has no caller until 7b-2's trigger loop (or a small manual
harness); the in-controller window p99 vs the 2 s target is therefore
unmeasured (pre-staging removes both attach handshakes from the window —
the spike's ~1.2 s each becomes two AER waits). **Next: 7b-2 (trigger
loop + shared claim + events/metrics), then the 7b-3 campaign.**

## 7b-2 implemented (2026-07-01, same session) — trigger loop + shared claim

The trigger loop is code-complete on `main` (suite 503 green): the
mechanism now has its production caller. Three pieces, per the plan:

**The shared per-volume claim (design item 4)** — `src/volume_claims.rs`,
a process-global registry (single-controller assumption, same as
CreateVolume placement): `try_claim(volume, op)` returns an RAII guard, at
most one long-running operation per volume across catch-up / cutover /
hot-rejoin. The catch-up orchestrator's private in-flight set is replaced
by it (same skip-if-busy behavior, now cross-planner); the cutover tick
claims around the bounce itself (verification stays passive); the
hot-rejoin tick claims around the window+localization and around marker
reconciles. The epoch scheduler deliberately does NOT claim — its cuts
are the chase's designed input and must keep flowing during multi-hour
catch-ups — it only *consults* the registry and defers a volume's cycle
while a `hot-rejoin` claim is held (a scheduler cut racing the window's
strict-fresh E_f cut would abort the window).

**Standby targets (mechanism delta the plan forced into the open).** The
plan's trigger gate — "ready standby (lag ≤ threshold)" — was written
before 7b-1, whose `resolve()` only took STALE replicas. The gate is
right: the eval's motivating limbo is precisely the *converged standby*
on an attached no-bounce RWO volume (`plan_cutover`'s "waiting for a
natural reassembly"), and a raw stale rejoined directly would serve most
reads through the remote E_f export for the entire backfill — a live
read-latency regression the fenced chase avoids for free. So Tier-1
stays the bulk-copy engine and hot rejoin is the admission step. The
mechanism therefore accepts standby targets via **demote-at-intent**:
`mark_hot_rejoin_intent` moves standby → stale + marker in ONE record
write, preserving the 7b-1 crash-decode table exactly (stale+marker is
always "window not yet flipped"; standby+marker always "flipped,
localizing"). The demote keeps `last_epoch`/`reverted_to`, so an unwound
window re-heals to standby cheaply. Two more mechanism hardenings found
writing this: `resolve()` now refuses the whole volume while ANY replica
carries a marker (the E_f export NQN is per-volume — two concurrent
windows would share a transport), and the intent write is verified to
have landed after the CAS (the update closure no-ops silently if a
restage admitted the target in_sync between load and write — previously
the window could open on a stranger).

**The planner + loop (Decision 1 (B), verbatim).**
`plan_hot_rejoin(view, cfg)` is pure and mirrors `plan_cutover`: Wait on
synthetic-RWX-backing PV → RWX → `hot-rejoin: "disabled"` opt-out →
`rejoin-bounce` opt-in (disjoint classes; the planners cannot race on
one volume) → marker present (the reconciler owns it) → not attached →
no epoch history; then Rejoin iff the most-converged standby (the same
target `resolve()` picks) trails by ≤ `FLINT_HOT_REJOIN_MAX_LAG`
(default 1). A raw stale gets an explicit "awaits the Tier-1 catch-up"
Wait, making the division of labor visible in operator logs. The loop
(`run_hot_rejoin_orchestrator`, 60 s tick, controller role, spawned in
`main.rs` behind `FLINT_HOT_REJOIN=enabled`) builds views from
PV/VolumeAttachment listings, dispatches marker reconciles under the
claim — so a crashed rejoin converges even with `FLINT_CATCHUP` off —
and runs each volume's rejoin as its own task (one volume's localization
must not stall another's two-second window). Failed (unwound) windows
back off `FLINT_HOT_REJOIN_RETRY_SECS` (default 300) per volume — every
attempt costs the consumer a quiesce.

**Observability (design item 5)**, event-based (no metrics registry in
the codebase — a real registry is future work): `HotRejoinSucceeded`
carries per-step window timings; a committed window slower than
`FLINT_HOT_REJOIN_WINDOW_TARGET_MS` (default 2000) additionally emits a
Warning `HotRejoinWindowSlow` naming the eval's fallback (atomic-add
variant, §7 option b); `HotRejoinLocalized` now reports the esnap
exposure duration (flip → localized, from the record's `since` stamp) —
the localization-lag signal for the new-in-kind SPOF window.

**Noted trade-offs:** epoch cuts for a volume pause while its hot-rejoin
claim is held — bounded and benign for the target class (a converged
standby's localization replays little), but a cold-stale manual rejoin
would pause them for the whole backfill (splitting the claim into
window-scoped vs localization-scoped is the refinement if it ever
matters). The stale-since→marker carry (smaller localization base) and
pad copy-progress preservation remain deferred from 7b-1. The 7b-0
forced-stale-admission takeaway (a policy knob preferring unavailability
over stale admission once hot rejoin owns the heal path) is NOT in
7b-2 — it changes 1.3.0 assembly semantics and belongs with the 7b-3
campaign evidence.

**Not yet done:** the live run on `runj` — enable `FLINT_EPOCH_SCHEDULER`
+ `FLINT_CATCHUP` + `FLINT_HOT_REJOIN` on the controller, fail a leg,
watch chase → standby → window → localization end-to-end; measure the
in-controller window p99 vs the 2 s target. That is the first item of
the 7b-3 validation tail (runbook + §9-8 adversarial campaign).

## 7b-3 first live session (2026-07-02, cluster `runj`) — E2E validated, one P0 bug found & fixed

**Setup.** `flint-driver:tier2-7b3` (e05b650) built on the c5d, rolled to
the controller and node DS; controller env `FLINT_EPOCH_SCHEDULER=enabled`
(30 s), `FLINT_CATCHUP=enabled`, `FLINT_HOT_REJOIN=enabled`,
`FLINT_CUTOVER=disabled`. Fixture: 2 Gi `flint-2r` PVC, busybox writer
(5 fsync'd appends/s) on the c5d consumer; replicas on `runj-aws-1`
(idx 0) and `runj-aws-2` (idx 1). Leg kills = `kill 1` in the node pod's
`spdk-tgt` sidecar.

**Drill 1 — the full pipeline, PASS.** Kill 06:19:42 → `ReplicaStale`
+33 s → catch-up (revert to ep-1, replay to ep-3) → standby +53 s →
chase advance (catch-up won the 06:21:29 claim race; the trigger won the
next) → 06:22:29 **intent observed as `stale + MARKER:6`**
(demote-at-intent) with the scheduler's cut visibly deferred under the
claim → **window 148 ms** (quiesce 19, cut E_f 10, export+AER E_f 25,
esnap clone 11, export+AER head 41, renew 11, add 19, unquiesce 12) →
flip → localized after 4 s of esnap exposure → both `in_sync` at
06:22:34. Kill→full-redundancy **2 m 52 s**, dominated by detection
(33 s) and 60 s tick cadences; the mechanism itself is ~5 s of it. The
writer never restarted and shows a fully contiguous sequence with **zero
gaps > 1 s** — the window is invisible at this write rate. The spike's
~10.4 s manual window collapsed exactly as designed: both ~1.2–3 s
attach handshakes became 25/41 ms AER waits.

**P0 bug found (the reason live runs exist): the orphan sweep reaped the
serving `_hr` head.** At 06:24:50 — three 60 s strikes after the head's
creation — `runj-aws-2`'s node agent deleted
`vol_<pv>_replica_1_hr` as an absent-PV orphan:
`strip_replica_suffix` rejects the `_hr` tail (digits check), so the
whole remainder was read as a PV name that doesn't exist. Overlapping
with drill 2's kill of the other leg it took the raid to total loss
(writer EIO; `VolumeDataPathLost` correctly flagged). Inspection found
the same hole for the localization pad export id `<vol>_hrpad<i>` in
`classify_subsystem_nqn` (would reap the pad export mid-backfill). Both
fixed in `5feb602` (+ classification pins; the E_f `:hotrejoin:` NQN is
confirmed invisible to the sweep by prefix); image `tier2-7b3.1` rolled.
Note the bomb was armed from drill 1 alone — every hot-rejoined volume
would have died ~3 minutes after localization, no second failure needed.

Two sub-findings from the blast radius, both worth runbook entries:
1. **Recovery deadlock by design:** with the record claiming `in_sync`
   but the head deleted out-of-band, stage refuses assembly ("in_sync
   peer unavailable, below minimum" — correct, the record is authority)
   and the health monitor cannot re-label without an online raid.
   Remediation is an operator record patch: mark the replica stale, drop
   `active_lvol_uuid`. Documented procedure now exists (this session).
2. **Transparent absorption:** drill 2's own kill (`runj-aws-1`,
   06:24:41) never even degraded the record — spdk-tgt restarted in 2 s,
   the node agent re-exported, and nvme-tcp reconnect resumed queued io
   before any write failed. No stale, no divergence. Tier-0 at its best.

**Recovery doubled as organic validation.** After the record patch and a
writer bounce: degraded assembly from `runj-aws-1` → catch-up reverted
the replacement head onto `runj-aws-2`'s OWN surviving chain at ep-8 —
the chain drill 1's localization built, consumed as a §5 revert base for
the first time — and the concurrent restage's **phase-4 admission won
the race against the trigger** (correct: hot rejoin exists for volumes
with no reassembly in sight; one was in flight). Both `in_sync` again
without a second window.

**Drill 3 — E2E on the fixed image, PASS, sweep survival proven.** Kill
06:51:13 → stale +30 s → standby → `MARKER:15` (scheduler deferral
observed again) → **window 159 ms** → localized after 3 s → both
`in_sync` at +3 m 42 s. The new `_hr` head then **survived 7+ minutes of
sweep ticks** (pre-fix TTL was exactly 3), served as a full epoch-stream
member (its parent advanced with every cut, ep-15 → ep-48 over the
55-minute steady-state hold), raid `online 2/2`, zero sweep activity on
the node, writer contiguous end-to-end.

**Scorecard vs targets:** two windows, 148/159 ms — **13× under the 2 s
bar**; no `HotRejoinWindowSlow` events; localization lag 4 s / 3 s on a
near-empty volume with the conservative oldest-present base (the
multi-GB lag measurement is still owed); intent/flip/reconcile record
choreography behaved exactly per the 7b-1 state table under live races
(catch-up vs trigger claim alternation adds ≤ ~2 min worst case — fine).

**Still owed for 7b-3:** the §9-8 adversarial set (orchestrator kill
in-window, R_src kill mid-backfill, crash between add and flip),
localization lag at data scale, the two-leg scrub with the
fs-allocated-only methodology, the operator runbook (must include the
record-repair procedure above), and window p99 over more than two
samples. Cluster `runj` stands with the fixture running
(`tier2-7b3.1` everywhere, all three orchestrators on).

## 7b-3 adversarial drill session (2026-07-02, cluster runj, image tier2-7b3.1)

The §9-8 set, run against the standing fixture (`hr-e2e`/`hr-writer`,
5 fsync/s ledger writer, consumer on `runj-aws-3` after the c5d spot
node was reclaimed overnight; its stale Node object deleted — see the
starvation finding). 1.5 GiB of bulk data written first so localization
had a wide target. Bottom line up front: **all three drills produced
their answers, the writer ledger is contiguous across the entire
session (zero acked-write loss through 7 windows, 4 controller kills,
5 leg kills, one full raid collapse), and the session surfaced two P1s,
one availability gap intrinsic to replicas=2, and four smaller
findings** — none of which invalidate the 7b mechanism; two of them
point at a design simplification that could remove the exposure-window
failure class entirely.

### Window-latency dataset (n=9 committed windows, both sessions)

148, 148, 147, 148, 156, 157, 158, 159, 176 ms — worst observed 176 ms,
**11× under the 2 s bar**, phase breakdown stable (biggest component is
always the two export+AER steps at ~65-80 ms combined). No
`HotRejoinWindowSlow` events all day.

### Finding 1 (P1): hot rejoin is non-repeatable per replica — head-name
### EEXIST, protected by the sweep fix

The head lvol name is deterministic (`vol_<vol>_replica_<n>_hr`). After
a successful rejoin the promoted head keeps that name; when the leg
next dies, the catch-up chase builds a fresh standby clone under the
canonical name and **abandons the old head without deleting it** (the
window's ns-swap path deletes the lvol it replaces; the chase path does
not). The next window's esnap-clone then fails EEXIST, unwinds, and
backs off 300 s — forever, because the P0 sweep fix (5feb602) maps
`_hr` shapes to `Owner::Pv`, so the stray is sweep-protected. Observed
live at 14:41:13; every subsequent drill cycle required a manual
`bdev_lvol_delete` of the abandoned head (safe once it is neither the
record's `active_lvol_uuid` nor a raid member — the guarded delete is
in the runbook section below). Yesterday's drill 3 didn't hit this only because the
P0 bug had (destructively) deleted drill 1's head in between.
**FIXED same day** (both belts): the §5 revert now reaps the exact
lvol it supersedes when that lvol holds the `_hr` alias (uuid-matched,
`catchup.rs`), and the window build scrubs a stranded `_hr` namesake
pre-intent — refusing with NotEligible when the holder IS the record's
live lvol (a raw-stale target still serving the previous rejoin's
data, `hot_rejoin.rs`). Suite 509. **LIVE-VALIDATED (tier2-7b3.2,
2026-07-02 evening)**: two consecutive hands-off leg-kill→rejoin
cycles on the same replica — cycle 1 promoted a head under the `_hr`
name; cycle 2's chase logged "Reaped the superseded hot-rejoin head
(revert replaced it)" and its window committed clean in 156 ms
(pre-fix this exact sequence EEXISTed and needed a manual delete per
cycle). Windows on the new image: 160/156 ms.

### Finding 2 (P2): unwind-ladder E_f-export leak, self-healing on the
### second pass

Attempt #1's unwind (EEXIST at the clone step) left the E_f export
subsystem `nqn…:hotrejoin:<vol>` alive on the source; attempt #2 then
failed at `nvmf_create_subsystem` on that leftover. Attempt #2's unwind
*did* remove it (the ladder is idempotent against artifacts it didn't
create — good), so the leak self-heals after one wasted 300 s cycle.
Fix: unwind should treat the E_f export as unconditionally-owned.

### Finding 3 (P2): an unwound window's E_f cut leaves the record's
### epoch stream pointing at a deleted snapshot

The strict-fresh E_f cut advances `current_epoch`; on unwind the source
snapshot is deleted but the record still names it, so catch-up fails
("epoch-N not found in the source lineage") until the next scheduler
cut moves past it — two wasted cycles observed. Fix: keep the E_f
snapshot on unwind (it is a valid epoch; GC reaps it normally) rather
than trying to roll back the record.

### Finding 4 (P1): trigger starvation under adverse tick-phase bias

All orchestrator loops tick at boot+60k with sub-second spawn offsets,
so each minute is a three-way race (scheduler cut vs catch-up claim vs
trigger evaluation). The trigger fires only when it reads lag ≤ 1 with
the claim free — i.e. when it beats both. **With the dead spot node
still present as a Node object, its 3 s node-agent TCP timeouts skewed
the race enough that the trigger lost 8/8 eligible ticks (~9 min
starvation, unbounded)**; within seconds of `kubectl delete node` it
won the very next tick, and post-cleanup it won 8 of 12 eligible races
(7 of 8 first-eligible-after-standby). Convergence must not depend on
incidental API latency: give the trigger a deterministic slot (e.g.
evaluate immediately after a chase completes within the same catch-up
tick, or offset its phase by +30 s from the scheduler), or let it
tolerate lag ≤ max_lag+1 while a chase holds the claim.

### Drill B1 — controller death mid-localization: recovers, but by luck
### (resume walks the source lineage; GC races it)

Setup: leg kill → standby → window 157 ms (E_f 496) → forced pod delete
timed off the commit log line; SIGKILL landed mid-localization.
Observed: **raid stayed 2/2 and the writer never gapped through the
decapitation** (a committed window needs no orchestrator to keep
serving); replacement pod up in ~5 s; reconciler fired immediately and
chose the resume arm. But resume attempts #1/#2 failed wanting source
epochs 490/491 — **already GC-reaped** (the source retains ~5-6). It
succeeded on attempt #3 only because local GC drift moved the walk base
into the retained window. Kill→fully-recovered 2 m 11 s; esnap exposure
133 s (vs 3-4 s undisturbed). Two defects: (a) the resume path walks
the **source** lineage although the strict-fresh chase guarantees E_f
exists in the **local** chain — the original localization uses the
local chain (that's why it is O(delta), see below) and the resume
should too; (b) while wedged, the head serves raid writes with no epoch
credit (record standby@496 vs current 497+) — an outage longer than the
GC horizon wedges the marker permanently (operator remediation below).

### Drill B2 — controller death in-window: scrub arm validated, 30 s to
### clean standby

Landing a kill inside a ~550 ms intent→commit span took precision
work: `kubectl delete --force` SIGKILL latency is 1.5-6 s+ and jittery
(three misses mapped the neighborhood: post-commit+31 ms → in-place
container restart 1.3 s + resume → localized 13 s, writer clean;
mid-scheduler-tick and pre-tick kills → benign). The hit came from
writing `1` to the container's **cgroup v2 `cgroup.kill`** from the
privileged spdk-tgt container on the same node (host cgroup fs is rw
there; ±2 ms timing, node-local clock): kill landed between intent-CAS
and quiesce. The record froze at `stale + marker` ("hot-rejoin window
opening"); the restarted container's reconciler decoded it and ran the
**scrub arm** — artifacts cleaned, marker cleared, catch-up re-chased —
**crash → clean standby in 30 s**, writer unaffected. Combined with B1
and drill C, three of the four decode arms are now live-validated
(resume, scrub, demote; adopt was exercised organically yesterday).
Residual: a kill inside the ~150 ms quiesced sub-span (orphaned-lease
writer stall, 8 s bound) — still harness-validated only (7b-0), the
span is too narrow to hit reliably even at ±2 ms because tick-phase
drift is ±100 ms. The cgroup.kill/cgroup.freeze technique is now the
standard drill primitive (documented in the runbook).

### Drill C — source death mid-localization: fail-stop, zero acked-write
### loss; availability gap at replicas=2

Setup: leg kill → standby → window 158 ms (E_f 528) → `kill 1` on the
**source** spdk-tgt 511 ms after commit (mid-localization). The
acked-write-loss hypothesis (raid keeps acking on the esnap-only head;
demote later discards those acks) is **refuted — the failure is
fail-stop**: the head's esnap parent is an nvme attachment to the
source's E_f export, so source death killed the head bdev too; with the
source's own leg also dead the raid dropped to 0/2 and **unregistered
entirely** — hard EIO to the consumer, nothing acked after the collapse
second. Ledger proof: the drill generation ends at idx 20459,
timestamp = the collapse second, with zero discontinuities before it.
The in-flight localization deferred gracefully ("Rejoin complete
localized=false" — committed windows don't unwind), and the next
reconcile correctly ran the **demote arm** ("lost its leg before
localization — demoted to stale").

Recovery was two-layered: the node agent **reassembled the raid
autonomously ~2 min after the collapse** and admitted the re-chased
standby via fenced-delta equalization (`ReplicaAdmitted`, "in_sync, no
rebuild"), but the **filesystem layer stayed dead** (the mount points
at the unregistered device instance) until the consumer pod was
bounced — total writer outage ~4 min. Two follow-on findings:

- **(P1) The health monitor is silent on raid-ABSENT**: no
  `VolumeDataPathLost` fired during the 2-minute total outage, and the
  record kept claiming `in_sync` + cutting epochs on the source while
  zero consumer writes could flow (consumer-blindness again). Root
  cause: the event only fired when layer-3 FLAGGING ran (3 strikes AND
  in-place repair failed) — layer-2 repair winning that race made the
  whole outage invisible. **FIXED same day**: the node agent remembers
  which raids it has observed present; a previously-seen raid vanishing
  under a live attachment emits `VolumeDataPathLost` on the FIRST
  strike (a never-seen raid is an in-flight NodeStage and stays on the
  3-strike cadence), paired with `VolumeDataPathRestored` when the
  episode closes early (`raid_collapse_verdict` in `cutover.rs`,
  wiring in `node_agent.rs`). Repair/flag cadence unchanged. The
  consumer-blind epoch cutting remains the open phase-6 item.
  **LIVE-VALIDATED (tier2-7b3.2)**: `bdev_raid_delete` under the live
  attachment → `VolumeDataPathLost` at **+18 s** (first tick; was
  never emitted at all pre-fix) → `VolumeDataPathRepaired` at
  +2 m 19 s (strike-3 in-place repair, raid back online 2/2).
  Operational nuance re-confirmed: a workload actively writing during
  the outage still needs a bounce after Repaired — ext4 gives up on
  the vanished device before the kernel initiator's reconnect window
  (the event text says exactly this). One cosmetic follow-up landed
  post-validation: repair-success now also closes the warned episode,
  suppressing a redundant `VolumeDataPathRestored` one tick later.

### Build path replaced (the c5d spot node is gone)

The remote-x86 build node (c5d + socat proxy) died with the spot
reclaim. New path, proven this session: a **`docker:27-dind`
privileged pod on a runj worker** + `kubectl port-forward 23750:2375`
+ `DOCKER_HOST=tcp://127.0.0.1:23750` — then the exact runbook build
commands work unchanged (built and pushed `tier2-7b3.2` in ~17 min
cold on an i4i.large). CAVEAT: these workers have only ~8 GiB
ephemeral root — the build cache trips node disk-pressure EVICTIONS
(it evicted the node DS pod, the controller, and the writer
mid-roll). **Delete the dind pod immediately after pushing**; treat
the builder as per-session disposable.
- **(availability, structural) At replicas=2, the source is a single
  failure domain for the whole volume during esnap exposure** — it
  hosts both the surviving leg and the head's esnap parent. With 3+
  replicas an independent leg survives. Options: document as a
  replicas=2 caveat; gate the trigger on source health; or eliminate
  the exposure (next paragraph).

### The design question the session converged on — DECIDED (2026-07-02)

As posed during the drills: if the head were cloned from a **local E_f
snapshot** instead of esnap-cloned to the remote E_f export, there
would be no remote parent, no localization phase, no exposure window —
B1's wedge and drill C's collapse both become unreachable. Why doesn't
the window do that?

**The literal local-E_f clone is unsound — rejected.** Code review
killed the premise: the window does NOT chase the standby to E_f (the
record's `last_epoch = E_f` mid-window is flip bookkeeping, not a local
snapshot — the 8-step timings have no chase step). At window time the
local chain tops out at the last chased epoch E_l; the gap E_l→E_f is
the un-chased delta, up to ~2 epochs of writes. A local E_f can only
exist if that delta is copied INSIDE the quiesced window — O(delta)
with writes frozen — which is June's design and its 10 s windows. The
remote esnap is not incidental; it is what makes the window O(1)
(~150 ms at any write rate) by deferring the data movement to the
post-commit localization. Cloning from local E_l without the delta
would hand `--skip-rebuild` a diverged leg: silent corruption.

**Decision: adaptive dual-path (7b-4).** The un-chased delta is
cheaply estimable at trigger time (epoch snapshots carry
`num_allocated_clusters`; the current-epoch sliver is bounded by
write-rate × cut interval). Pick the path per rejoin:

- **Small delta** (est. ≤ `FLINT_HOT_REJOIN_INLINE_DELTA_MAX`, default
  64 MiB): **quiesced fenced-final-delta live admission** — quiesce
  (the lease IS the fence: no writer exists), cut E_f, chase the
  standby lvol itself through E_f with the existing §5 delta machinery
  against the frozen source, `clear_head_sb`, add the STANDBY with
  `--skip-rebuild`, unquiesce. This is phase-4 admission recombined
  with 7a's live add — the phase-4 comment ("a base cannot join an
  ONLINE raid without the stock blind rebuild") predates
  `--skip-rebuild` and the two mechanisms were simply never rejoined.
  No head clone, no `_hr` names, no E_f export, no pad, no
  localization, no exposure window at all: the entire drill-C failure
  class and the B1 resume/GC-wedge class are unreachable, and the
  crash-decode table collapses to adopt/scrub. Window = O(delta),
  bounded by an abort budget (lease/2): overrun → unquiesce-and-abort
  (nothing target-side mutated beyond an idempotent chase) → esnap
  path next attempt. Content-identity rests on already-validated
  ground (phase-4 fenced delta + the 7b-0 scrub).
- **Large delta**: today's esnap path unchanged — O(1) ~150 ms window,
  short source-dependent exposure (O(delta) localization), now
  hardened by the P1 fixes. Residual: the resume arm must prefer the
  local chain over the source lineage (B1 finding, still owed), and
  the replicas=2 source-death caveat stands, bounded to the exposure
  seconds.

With the trigger's lag ≤ 1 gate and 30-60 s cut cadence, the small
path is the overwhelmingly common case in practice; the esnap path
remains the guarantee that the window never stretches with write
rate. Implementation is scoped as **7b-4** (trigger estimator + the
inline-delta window variant + tests + a drill session).

### Also observed / operational notes

- Consumer-blindness race, live: an epoch cut recorded `in_sync:N` for
  a dead leg (spdk-tgt restarted between kill and health tick) before
  stale-marking corrected it — harmless under revert-first, but it is
  the phase-6 bug surfacing again.
- Thin-aware full rebuild, live: after drill hygiene eroded the shared
  local history, the chase correctly fell back to
  `ReplicaFullBuildStarted` ("head recreated empty, replaying the full
  source lineage, holes skipped") — ~2 min for 1.5 GiB.
- spdk-tgt leg kills accumulate kubelet CrashLoop backoff (8 restarts →
  ~2.5 min revival); drill pacing, not a product issue.
- Runbook additions: (1) guarded delete of an abandoned `_hr` head
  (must not be `active_lvol_uuid`, must not be a raid member); (2)
  post-collapse recovery = wait for autonomous raid reassembly, then
  bounce the consumer pod to remount; (3) wedged `standby+marker` after
  a long controller outage = the B1 GC-horizon wedge — either wait for
  local-GC drift or patch the record (clear marker, mark stale) and let
  Tier-1 re-chase; (4) `cgroup.kill`/`cgroup.freeze` via a privileged
  same-node container is the precision crash primitive for drills.

**Remaining for 7b-3 after this session**: the quiesced-span orphaned-
lease live measurement (narrow; consider a fault-injection env knob
instead of timing luck), the two-leg fs-allocated-only scrub, the
operator runbook as a standalone doc, and the local-E_f design decision
above. Cluster `runj` stands healthy: fixture running, `in_sync×2`,
all orchestrators on, epoch stream at ~533.
