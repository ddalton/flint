# Tier-2 (`skip_rebuild` hot rejoin) — evaluation and decision

**Date:** 2026-06-12 (follows `phase6-residual-2026-06-12.md`; design in
`incremental-replica-rebuild.md` §7)
**Question:** carry the ~250-line `skip_rebuild` SPDK patch and build the
esnap-clone hot-rejoin orchestration (§7), or stop at Tier 1?
**Deciding metric (§6):** time spent degraded with a ready standby and no
reassembly opportunity, weighed against one workload restart per heal.

> **Spike executed same session** — all four deliverables pass; verdict
> upgraded to **GO for 7b** with two named design items (window attach
> pre-staging, dead-controller reaping). Results, the v2 patch revision
> the spike forced, and measured numbers: `tier2-spike-2026-06-12.md`.

## Verdict

**GO — as a bounded spike first.** Write the patch, build the image, drive
one manual hot rejoin on the test cluster, and measure the quiesce window
and crash behavior before committing to orchestration. The case for Tier 2
has *strengthened* on the cost side since the design was written (the patch
now targets exactly the shipped SPDK version, and rev 5's superblock-less
raids delete the riskiest parts of the patch) while the benefit has
*narrowed* to one class — RWO workloads that cannot tolerate restarts —
which happens to be the canonical block-storage consumer (databases).
Full orchestration (phase 7b) is conditional on the spike's numbers.

## What phase 6 measured (the data this decision was waiting for)

- **Tier 1, effective bounce: ~3 min residual**, almost entirely detection
  latency (three independent 60 s ticks), plus **one workload restart**.
  Data movement is seconds at test volume sizes.
- **Tier 1, ineffective bounce: unbounded** (same-node reschedule reuses
  the staged raid; `CutoverIneffective` retries at 900 s cooldowns with the
  same odds each round). The residual distribution is bimodal.
- Since measurement, v1.3.0 shipped the scheduling-escalation taints and
  the RWX rounds validated taint steering live — the ineffective mode is
  now mitigated, not eliminated (it remains a verify-and-retry loop, not a
  guarantee).

## What changed since the design was written

Three facts move the cost/benefit, all verified 2026-06-12:

1. **The patch now targets the shipped tree exactly.** §7's patch shape was
   traced on v26.05 with a port to "shipped v26.01" assumed; rev 5 moved
   the build to v26.05 (`Dockerfile.spdk`, for `bdev_raid_delete
   clear_sb`). No port. The pipeline already carries five local patches
   and has survived two SPDK bumps (v25.09→v26.01→v26.05), so the
   *mechanism* of carrying a sixth is proven — see "honest costs" for why
   this one is still different in kind.

2. **Superblock-less raids (rev 5) simplify the patch materially.** §7 was
   written when raids carried superblocks. Three consequences:
   - The sb-flip completion sequence (`raid_bdev_process_finish_write_sb`)
     is moot — for a no-sb raid the completion is quiesce → install
     `base_channel[slot]` on every channel → unquiesce. Less new code on
     the critical path.
   - The examine-divert hazard (the flag having to survive
     `raid_bdev_examine_sb` when the added bdev carries a stale matching
     sb) cannot occur: the esnap-clone head is a fresh lvol with no raid
     sb, and the array does no sb examine.
   - Crash safety shifts from on-disk slot state to the control-plane
     record — which rev 5 already made the sole assembly authority. A
     crash anywhere in the window leaves the record at `standby`; the next
     assembly excludes the replica and the chase resumes. Same fail-safe
     outcome as §7's sb argument (redundant catch-up, never corruption),
     reached with less machinery. The §9-8 crash test ("between channel
     install and sb write") becomes "orchestrator/target death between add
     completion and record flip to `in_sync`" — a pure control-plane test.

3. **Nothing upstream changed.** spdk/spdk#3349 (the RAID1 slot-expansion
   request this would partially serve) is still open/Todo with no linked
   PRs (re-checked today); upstream still has no grow/assume-clean/quiesce
   RPC through v26.05. The Longhorn fork still maintains exactly this
   primitive on `longhorn-v25.09` (branch active as of this week) — the
   reference implementation remains available and maintained, and nobody
   is going to ship this for us.

## Where the benefit actually lands, post-1.3.0

Per volume class, what Tier 2 buys over the Tier 1 we now have:

- **RWX volumes: marginal.** The NFSv4 persistence round made the Tier-1
  bounce nearly transparent — server pod replaced in seconds, state roams
  with the PVC, dirty-state clients retransmit with no grace stall, zero
  app restarts. Tier 2 would shave the server-restart blip and the restage
  window. Not worth the patch on its own.
- **RWO with `rejoin-bounce` opt-in: modest.** Tier 1 costs one tolerated
  restart and ~3 min; Tier 2 removes the restart and the restage minute
  but not the detection ticks that dominate the residual. The ticks are
  tunable in both tiers.
- **RWO without the opt-in: this is the case.** Tier 1 has *no* path to
  reassembly other than waiting for a natural reschedule — for a workload
  that doesn't churn, the deciding metric is unbounded by design, with the
  volume degraded and the standby chasing indefinitely. Restart-intolerant
  stateful workloads (databases) are simultaneously the least likely to
  reschedule naturally, the least able to opt into bounces, and the most
  important block-storage consumers. Tier 2 is not an optimization here;
  it is the only mechanism that restores synchronous redundancy without an
  availability event.
- **Spot fleets** (the motivating environment): natural churn does the
  reassembly for free and Tier 1 covers most volumes most of the time —
  but spot fleets running databases on flint are exactly where "the node
  that died was the replica, the consumer kept running, and nothing will
  reschedule it" recurs.

## Honest costs (what GO signs up for)

- **A correctness-critical carried patch.** The existing five patches are
  logging/debug/flush-hook material; a wrong `skip_rebuild` admits a stale
  base as a read source — silent data corruption, the §0 cardinal sin.
  Every future SPDK bump re-validates this patch *behaviorally* (the §9-8
  adversarial tests must run per bump), not just "applies cleanly".
- **The leased-quiesce RPC pair** (~30–50 lines of the patch) and its
  control-plane discipline: the window spans several RPCs across three
  nodes; the lease (timeout + renewal) is what turns orchestrator death
  into "rejoin failed" instead of "guest IO hung". The §7 unwind ladder
  (add fails → unquiesce, delete clone, promote-or-delete `E_f`) must be
  implemented with the same convergent rigor as the rest of the control
  plane.
- **The esnap backfill dependency window is a new failure mode Tier 1
  doesn't have.** Until `set_parent` localizes the chain, R_src's node is
  a single point of failure for the not-yet-local clusters and guest reads
  routed to R_dst double-hop over NVMe-oF. Tier 1's restage has no such
  window. The §7 mitigations (priority backfill, no full-redundancy report
  until localization, revert R_dst to `stale` on R_src death) are
  control-plane work that lands with phase 7b.
- **Estimated scope:** ~200–250 lines C in the raid module (less than the
  §7 estimate, per the rev-5 simplifications) + schema/CLI; spike-able in
  isolation. Orchestration (7b) is the larger half: quiesce lease client,
  the snapshot→export→clone→add window sequence, unwind, backfill
  priority, localization tracking, and the adversarial test set.

## Spike definition (phase 7a — the committed step)

Deliverable: a patched `spdk-tgt` image and one scripted manual hot rejoin
on a live 2-replica volume, producing:

1. **Quiesce-window measurement** under a writing workload. Target to
   beat: p99 < 2 s with lease timeout 10 s (all in-window steps are
   metadata ops + cross-node RPC latency). If the window can't hold ~2 s,
   the guest-visible stall approaches initiator timeout territory and the
   design needs the atomic-add variant (§7 option b) reconsidered.
2. **Correctness check:** writer running throughout; post-rejoin scrub
   comparing both legs (read-verify through each base) — zero divergence.
3. **Crash drill:** kill the orchestrating script inside the window (lease
   must auto-unquiesce; guest IO resumes; unwind leaves a consistent
   record) and kill spdk-tgt between add completion and the record flip
   (next assembly must treat the replica as stale).
4. **Patch-maintenance dry run:** the patch staged as
   `raid-skip-rebuild.patch` in the existing `Dockerfile.spdk` pipeline.

GO/NO-GO for phase 7b on those four results. Nothing in 7a touches
production paths: the patch ships dark (flag defaults absent, plain add
unchanged) and can ride `:latest` without affecting Tier-1 behavior.

## Recommendation recorded

Tier 2 enters the roadmap as phase 7a (spike) now; 7b (orchestration)
gated on the spike. The alternative — declaring Tier 1 sufficient — would
mean documenting "restart-intolerant RWO workloads run degraded until
their pod next reschedules" as a permanent product property. Phase 6's own
data says that residual is unbounded; with the standby machinery already
keeping a converged copy one metadata-op away from membership, the
remaining distance is the smallest piece of the whole design.
