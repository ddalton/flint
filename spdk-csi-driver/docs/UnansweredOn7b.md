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
