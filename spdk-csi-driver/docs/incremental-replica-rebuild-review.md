# Review: incremental-replica-rebuild.md

**Subject:** `incremental-replica-rebuild.md` rev 4 — snapshot-epoch delta resync
**Reviewer:** Claude (Opus 4.8), simplified 2026-06-11

## Bottom Line

The design is directionally sound. Keeping sync state in the lvols and PV
annotations, rather than in the roaming raid bdev or raid superblock, is the
right architectural choice. The two-tier split also makes sense: Tier 1 avoids
SPDK changes by catching replicas up out of band and admitting them only at
reassembly; Tier 2 is correctly left as an optional hot-rejoin optimization.

The main issue is narrower: **§5's correctness proof overstates what
`back-off + revert` proves.** The procedure can still be correct, but the proof
needs to name the other mechanisms it relies on.

## Core Findings

1. **The load-bearing SPDK assumptions check out.**
   `shallow_copy` copies only clusters allocated in the source blob itself, so an
   epoch snapshot can act as a delta source. SPDK raid1 can also acknowledge a
   write through a surviving leg while another leg is failing, which is the
   failure-window case the design accounts for.

2. **Copying `E_b` itself is part of correctness.**
   §5 says the catch-up copies `E_b -> ... -> E_latest`, but the proof talks
   mostly about post-`E_b` deltas. That misses an important point: copying
   R_src's `E_b` snapshot is what handles ordinary skew between the replicas'
   base-epoch cuts.

3. **The final delta is the real in-sync boundary.**
   A caught-up replica is only a `standby`; it can still trail writes made after
   the latest copied epoch. Tier 1 is safe only if the replica is not admitted
   into the raid until the fenced final delta at reassembly completes. The doc
   already describes that step, but §5 should explicitly connect correctness to
   it.

4. **Do not claim `back-off + revert` alone is sufficient.**
   That phrase is too strong. A better statement is: `back-off + revert` handles
   the failure-transition window; copying `E_b` handles base-epoch skew; and the
   final delta before admission makes the standby safe to mark `in_sync`.

## Recommended Doc Change

Tighten §5's correctness note to say:

> The catch-up proof depends on three pieces: selecting a backed-off base epoch,
> reverting R_dst to its own `E_b`, and shallow-copying R_src's `E_b` snapshot
> before subsequent epoch deltas. This makes the standby consistent up to the
> latest copied epoch. The standby must not be admitted as `in_sync` until the
> fenced final delta at reassembly has completed.

Also keep the regression test recommendation: Tier 1 depends on
`bdev_raid_create` admitting equalized bases without starting a rebuild, so that
invariant should be pinned by a test.

## Verdict

Keep the design. Revise the proof. The review should not frame the narrow
`E_latest = E_b` case as permanent data loss under the full Tier 1 state machine,
because a standby is not supposed to be admitted until the final delta runs. It
is best treated as a documentation/proof gap and as a test case for the
standby-to-`in_sync` admission boundary.
