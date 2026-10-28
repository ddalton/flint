-------------------------- MODULE FlintCatchupNTick --------------------------
(***************************************************************************)
(* The catch-up orchestrator's TICK on FlintCatchupN (catchup.rs          *)
(* run_catchup_for_volume, ~3118-3160).  Every standby is chased each      *)
(* tick, but the rebuild runs on ONE stale leg: the FIRST in record order  *)
(* that is Stale with no hot-rejoin marker and no live maintenance drain   *)
(* -- with no reachability condition.  FlintCatchupN gives every stale     *)
(* leg its own weak fairness, so it cannot see a first leg that never      *)
(* completes starving the rest (flint-91's control arm, 2026-10-28: A      *)
(* stale and unreachable from the only in-sync source, every tick fails    *)
(* at ensure_dst_attached, B -- stale and rebuildable -- never gets a      *)
(* turn).                                                                  *)
(*                                                                         *)
(* att[r]: the copy path to r ("down": ensure_dst_attached fails -- a      *)
(* partition between r and the sources, the node Ready), at most one       *)
(* fault per leg, optionally healed.  FirstMatch = TRUE is the code;       *)
(* FALSE is the fix (a failed catchup_stale moves on to the next stale     *)
(* leg in the same tick -- every stale leg gets a turn).                   *)
(***************************************************************************)
EXTENDS FlintCatchupN

CONSTANTS FirstMatch, Heals

VARIABLE att        \* [R -> {"ok", "down", "healed"}]

tvars == <<vars, att>>

TTypeOK == TypeOK /\ att \in [R -> {"ok", "down", "healed"}]

TInit == Init /\ att = [r \in R |-> "ok"]

TickCands == {q \in R : dState[q] = "stale" /\ hot[q] = 0}
TickLeg == IF TickCands = {} THEN 0 ELSE Min(TickCands)
TickOK(r) == ~FirstMatch \/ r = TickLeg

SelectBaseT(r)  == TickOK(r) /\ SelectBase(r) /\ UNCHANGED att
RevertDT(r)     == TickOK(r) /\ RevertD(r) /\ UNCHANGED att
BulkCopyT(r)    == TickOK(r) /\ att[r] # "down" /\ BulkCopy(r) /\ UNCHANGED att
SessionFailT(r) == TickOK(r) /\ SessionFail(r) /\ UNCHANGED att
ChaseT(r)       == att[r] # "down" /\ Chase(r) /\ UNCHANGED att

\* catchup_stale reverted (or resumed) and then failed at ensure_dst_attached:
\* the session ends, reverted_to stands, the next tick selects again.
AttachFail(r) ==
  /\ TickOK(r)
  /\ att[r] = "down" /\ dState[r] = "stale" /\ sb[r] # -1 /\ reverted[r] = sb[r]
  /\ sb' = [sb EXCEPT ![r] = -1]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq,
                 cutK, cutTgt, cutDone, members, attached, path, dState, mark,
                 reverted, pin, riskRec, pend, acked, fails, badAdmit, used,
                 tainted, floor, restamp, mustRev, hot, hotSrc, padTop, att>>

AttachDown(r) ==
  /\ dState[r] = "stale" /\ att[r] = "ok"
  /\ att' = [att EXCEPT ![r] = "down"]
  /\ UNCHANGED vars

AttachUp(r) ==
  /\ Heals /\ att[r] = "down"
  /\ att' = [att EXCEPT ![r] = "healed"]
  /\ UNCHANGED vars

TNext ==
  \/ /\ \/ \E x \in Blocks : Submit(x)
        \/ \E w \in pend : Complete(w) \/ \E r \in R : Land(w, r) \/ Drop(w, r)
        \/ \E r \in R : \E x \in Blocks : Zombie(r, x)
        \/ \E r \in R : FailBegin(r) \/ FaultOut(r) \/ MarkStale(r) \/ Return(r)
        \/ CutBegin \/ CutRecord \/ CutAbort \/ Retire
        \/ \E r \in R : CutOne(r) \/ \E e \in Epochs : GC(r, e)
        \/ Unstage \/ Stage \/ \E r \in R : StageAdmit(r) \/ AdmitRefuse(r)
        \/ \E r \in R : HotWindow(r) \/ HotLocalize(r) \/ HotRefuse(r) \/ HotParentLost(r)
                        \/ HotInline(r) \/ HotPromote(r)
     /\ UNCHANGED att
  \/ \E r \in R : \/ SelectBaseT(r) \/ RevertDT(r) \/ BulkCopyT(r) \/ SessionFailT(r)
                  \/ ChaseT(r) \/ AttachFail(r) \/ AttachDown(r) \/ AttachUp(r)

TFairness ==
  /\ \A r \in R : /\ WF_tvars(MarkStale(r) /\ UNCHANGED att) /\ WF_tvars(Return(r) /\ UNCHANGED att)
                  /\ WF_tvars(SelectBaseT(r)) /\ WF_tvars(RevertDT(r))
                  /\ WF_tvars(BulkCopyT(r)) /\ WF_tvars(SessionFailT(r))
                  /\ WF_tvars(AttachFail(r))
                  /\ WF_tvars(CutOne(r) /\ UNCHANGED att)
                  /\ WF_tvars(AttachUp(r))          \* vacuous unless Heals (its guard)
  /\ WF_tvars(CutRecord /\ UNCHANGED att) /\ WF_tvars(CutAbort /\ UNCHANGED att)

TSpec == TInit /\ [][TNext]_tvars /\ TFairness

\* A stale leg that is back, with a source, a recorded epoch and a WORKING
\* copy path leaves Stale (a leg whose own path is down is excused).
Prop_AttachableProgress ==
  \A r \in R : (dState[r] = "stale" /\ path[r] = "back" /\ Sources(r) # {}
                  /\ recorded # {} /\ att[r] # "down")
                 ~> (dState[r] # "stale" \/ path[r] # "back" \/ Sources(r) = {} \/ att[r] = "down")

=============================================================================
