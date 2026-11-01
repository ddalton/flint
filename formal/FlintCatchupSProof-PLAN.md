# FlintCatchupS: the TLAPS proof of the stamp rule (gate item 2)

The user's word (via flint-cb, 2026-10-31): "go ahead with the full stamp proof".

## Claim
THEOREM Safety == Spec => [](Inv_NoNewLoss /\ Inv_MembersHold /\ Inv_RaidHolds /\ Inv_AckStamped)
on FlintCatchupS rev 12 (8e54566d; rev 13 41fa0652 is the same model with ids that may be model values),
in c27's setting, crash AND zombie move ON:
  Stamp, MajorityRead "count", RelView, Decree, DecreeGated, DecreeSparesLast, EvenVouch "decree",
  TornExcluded, ZombiePreStampLegal, TornNeverSole, RecordUp (free?), DeferExpires FALSE,
  MonitorSkipsHold FALSE, Policy "trust", CutStampsAll FALSE, Capture FALSE, AckAfterMark FALSE,
  AssemblyMark TRUE, StageDefer TRUE, LocalLegGuard TRUE, MarkGate TRUE, CoverageWalk TRUE,
  RaidFailsAtDrop TRUE, StageOracle FALSE, WriteQuorum = AssemblyQuorum = 1, ZombieWhere "colocated",
  BaseInclusive, PinRetention, Backoff TRUE; Crash TRUE, ZombieMoves TRUE.
Any number of blocks, writes, epochs, failures, assemblies, retained epochs and replicas.

## Plan
1. IndInv (FlintCatchupSInd.tla), model-checked part by part at two and three replicas (tla-mc, the c28/c27 cfgs).
2. TLAPS: the type layer, then per action, as for FlintCatchupT (reuse its lineage/copy lemmas where the
   stamp leaves them intact).  Cached passes on the box; the fresh-cache record is a separate plan + cost.

## Why T's proof does not carry over
T's ASSUME Clean has Capture TRUE, no crash, no zombie: its I_Hold says every record-in_sync leg holds every
acked write once the raid is gone.  Here Capture is FALSE and a majority release does not mark the lost legs
stale: an in_sync leg can lack acked writes.  The stamp (and the stage's read) is what keeps them out.

## The stamp layer (first candidates, v0)
- due => hold; hold => attached; attached /\ ~hold => rgenA = agen /\ rmemA = members
- attached /\ ~due => every member's Core(stamp) = Core(CurStamp)  (then ~badAck: Complete needs ~hold)
- zx # 0 => zx \notin members
- every record-in_sync leg carries the live seq (nseq) and its size (asize): an assembly marks the rest stale
- every record-in_sync leg in rmemA (the last released generation) holds every acked write not in flight
