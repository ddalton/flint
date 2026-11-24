--------------------------- MODULE FlintCatchupTLive ---------------------------
(***************************************************************************)
(* FlintCatchupT's liveness arms with the data plane fair: a write in      *)
(* flight eventually lands or drops on each leg and completes (a real raid *)
(* resolves every I/O within its timeout).  Without it a stalled write     *)
(* blocks an epoch's observation forever and every progress property fails *)
(* for a reason that is not the catch-up's.                                *)
(***************************************************************************)
EXTENDS FlintCatchupT

DataFairness ==
  /\ WF_vars(\E w \in pend : Complete(w))
  /\ WF_vars(\E w \in pend : \E r \in R : Land(w, r) \/ Drop(w, r))

LiveSpec == Spec /\ DataFairness

\* Option (b) holds an ack until every base the raid failed is marked stale.
\* With the monitor fair that hold is temporary: a write in flight
\* eventually completes, acked or failed.
Prop_AckProgress ==
  \A v \in 1..MaxVer : (\E w \in pend : w.v = v) ~> ~(\E w \in pend : w.v = v)

=============================================================================
