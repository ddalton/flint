---- MODULE CtxRec3 ----
(* The record obligations the proof needs, under the v3 copy's full context. *)
EXTENDS LeanP1Anc, TLAPS
THEOREM C7 == WriterInit \in Writer
BY DEF WriterInit, Writer, Opt
THEOREM C6 == ASSUME NEW W \in Writer PROVE W.memo \in Nat /\ W.sHeld \in [Paths -> Opt(Handles)]
BY DEF Writer
THEOREM C5 ==
  ASSUME NEW W \in Writer
  PROVE  /\ W.local \in [Paths -> Opt(Handles)] /\ W.baseline \in [Paths -> Opt(Handles)]
         /\ W.integrated \subseteq Gens /\ W.uploads \subseteq Paths /\ W.snap \in [Paths -> Opt(Handles)]
         /\ W.inst \in [Paths -> Opt(Handles)] /\ W.retire \subseteq Handles /\ W.synced \in Nat
BY DEF Writer
\* A constructor's membership from what it reads.
THEOREM M1 ==
  ASSUME NEW s \in Writers, w \in [Writers -> Writer], doc \in [Paths -> Opt(Handles)], seq \in Nat,
         NEW fail \in SUBSET Paths, ConsumeTaken(s, fail) \subseteq Paths,
         {Gen(doc[p]) : p \in {q \in ConsumeTaken(s, fail) : doc[q] # Nil}} \subseteq Gens
  PROVE  ConsumeW(s, fail) \in Writer
BY DEF ConsumeW, Writer, Opt
THEOREM M2 ==
  ASSUME NEW s \in Writers, w \in [Writers -> Writer], doc \in [Paths -> Opt(Handles)], seq \in Nat
  PROVE  FinishW(s) \in Writer
BY DEF FinishW, Writer, Opt
THEOREM M3 ==
  ASSUME NEW s \in Writers, w \in [Writers -> Writer], NEW fail \in SUBSET Paths
  PROVE  ConsumeW(s, fail).uploads = w[s].uploads /\ ConsumeW(s, fail).snap = w[s].snap /\ FinishW(s).uploads = {}
BY DEF ConsumeW, FinishW
====
