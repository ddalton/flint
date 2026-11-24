-------------------------- MODULE FlintCatchupSLiveW --------------------------
(***************************************************************************)
(* Liveness controls on FlintCatchupS's fairness (the gate, 2026-10-31):  *)
(* SpecNoReturn is Spec with WF_vars(Return(r)) dropped and every other   *)
(* conjunct of Fairness kept: does a barrier end without a failed leg's   *)
(* path coming back?                                                       *)
(***************************************************************************)
EXTENDS FlintCatchupS

FairnessNoReturn ==
  /\ \A r \in R : /\ WF_vars(MarkStale(r))
                  /\ WF_vars(SelectBase(r)) /\ WF_vars(RevertD(r))
                  /\ WF_vars(BulkCopy(r)) /\ WF_vars(SessionFail(r))
                  /\ WF_vars(CutOne(r))
  /\ WF_vars(CutRecord) /\ WF_vars(CutAbort)
  /\ \A e \in Epochs : WF_vars(Observe(e))
  /\ WF_vars(StampLand) /\ WF_vars(Release) /\ WF_vars(RecordConfirm)

SpecNoReturn == Init /\ [][Next]_vars /\ FairnessNoReturn
=============================================================================
