------------------------- MODULE FlintCatchupSReach -------------------------
(* Vacuity probes for FlintCatchupS: each Reach_* must be VIOLATED (the     *)
(* state it names is reachable), or the stamp arm proves nothing.           *)
EXTENDS FlintCatchupS
Reach_Hold      == ~hold                                \* a barrier is raised
Reach_MajRel    == ~(attached /\ ~hold /\ agen > 0 /\ Majority(CurStamp))   \* a majority released
Reach_Sole      == sole = {}                            \* a minority continuation confirmed
Reach_ZStamp    == ~zst                                 \* the zombie stamped its leg
Reach_Excluded  == ~(attached /\ aseq > 1 /\ \E q \in R : dState[q] = "stale" /\ stamp[q].seq < aseq /\ q \in stamp[q].mem)
Reach_Seq3      == nseq < 3                              \* three assemblies
Reach_ZombieAck == ~zAcked
Reach_ServeOnZ  == ~(attached /\ postMove /\ zLeg # 0 /\ members = {zLeg})   \* the spared z alone serves after the move
Reach_Risk      == ~riskTail                            \* a serve-with-risk stage (DeferExpires)
Reach_Vouch     == ~vouched                             \* a stage served on the even-size vouch alone (rev 9)
Reach_VouchSpot == ~(~attached /\ VAtt # {} /\ VouchCond /\ ~PlainConclusive)   \* the vouch's situation, the knob aside
Reach_TornMarked == ~tornMarked                         \* a stage left a torn leg out (rev 10)
Reach_TornServed == ~tornServed                         \* a stage served a torn leg (TornExcluded: the sole survivor)
Reach_JunkNoStamp == ~(zx = 0 /\ zLeg # 0 /\ ~zst /\ zjv # {})   \* the zombie's write landed, the zombie gone before its stamp
=============================================================================
