\* NON-VACUITY PROBES for the FlintExtents tranche-1 runs.  Each probe is an
\* invariant TLC must VIOLATE, and the violation is what licenses citing the
\* corresponding green run.  Per the A2Probe standing rule, a probe names the
\* ACTION, never the situation: every ghost below has exactly one writer, so
\* a violation is a witness that the action really executed somewhere in the
\* reachable state space — not merely that its guard could have been
\* satisfied (the mistake PROBE 2 in FlintA2Probe made, kept there as the
\* record of how a probe that existed for exactly this purpose got past me).
---------------------------- MODULE FlintExtentsProbe ----------------------------
EXTENDS FlintExtents

\* For FlintExtents.cfg's green on Inv_RecallCompletesBeforeReuse: worthless
\* unless a block is ever actually RE-CYCLED to a new owner.  reuseFired's
\* only writer is GrantInsert, on a free->provisional edge of a block with
\* gen > 0.
ProbeReuseFires == ~reuseFired

\* For the same green: the fence path must actually execute (a state space
\* where every holder politely returns never exercises the machinery the
\* module exists for).  fenceFired's only writer is Fence.
ProbeFenceFires == ~fenceFired

\* For FlintExtentsTgtAmnesia.cfg's contrast with the shipped PTPL belt:
\* worthless unless the target actually restarts in the shipped world too.
\* tgtRestarted's only writer is TgtRestart.
ProbeTgtRestarts == ~tgtRestarted

\* For FreeRevalidates' green specifically: the belt refuses frees exactly
\* when a live grant the snapshot missed covers a reclaimed block, so its
\* green is vacuous unless that world is reachable.  resnapshotGrew's only
\* writer is ReclaimResnapshot, and it fires only when the re-read finds
\* holders where the snapshot recorded none — the precise situation the
\* stale-snapshot-free mutation exploits, witnessed through an action.
ProbeResnapshotGrows == ~resnapshotGrew

==============================================================================
