---------------------------- MODULE LeanP1ProofL1 ----------------------------
(* L1: LSpec => Prop_UISaveCompletes (G1: every save the gateway starts is
   answered), for every path count, writer count and bound.  The gate's
   world for it, LeanP1LiveHolds, has LeanP1Holds's bounds (1.66B distinct)
   and has never been decided: TLC ran out at 103M (2026-09-28), and tla-mc
   on 371 GB was past 640 bytes a state at 116M (2026-10-14).

   The argument is WF1, for each path p:
     P == gw[p] # Nil      (a save in flight),
     Q == gw[p] = Nil      (it was answered: landed or 412),
     A == the fair action of LSpec, GCas(p) and the step's frame.
   (1) P /\ [Next]_vars => P' \/ Q'   -- Q is ~P;
   (2) P /\ <<A>>_vars => Q'          -- GCas clears gw[p];
   (3) P => ENABLED <<A>>_vars        -- GCas's guard is gw[p] # Nil and
       LeaseGuard, and LeaseGuard is TRUE in the shipped shape
       (GatewayIgnoresLease, G1: the gateway does not wait for the lease);
       every variable A writes is a function of the state, so the witness
       is A's own right-hand sides.
   (2) and (3) need gw \in [Paths -> Opt(Handles)], so the EXCEPT clears
   p: M0's TypeInvariant.  The tie to LeanP1.tla is M0's: LeanP1Anc has
   exactly LeanP1's states, and `anc` is a history variable written by a
   deterministic update, so a LeanP1 step is A's exactly when the LeanP1Anc
   step with AncUpdate is, and each is enabled exactly when the other is.

   Control: with GatewayIgnoresLease dropped from Shipped, (3) must fail --
   a stalled holder keeps GCas disabled (LeanP1LiveWaitsOnLeaseSmall, the
   gate's TLC control, fires).                                             *)
EXTENDS LeanP1ProofM0

\* LSpec's fair action for path p, and its fairness conjunct.
GA(p) == GCas(p) /\ TookUpdate /\ RetUpdate /\ AncUpdate /\ UNCHANGED <<aged, ages, rdoc, rlag>>
Fair == \A q \in Paths : WF_vars(GA(q))

LEMMA L1_Step ==
  ASSUME NEW p \in Paths, TypeOK, gw[p] # Nil, <<GA(p)>>_vars
  PROVE  gw'[p] = Nil
BY DEF GA, GCas, TypeOK

LEMMA L1_Enabled ==
  ASSUME NEW p \in Paths, TypeOK, gw[p] # Nil
  PROVE  ENABLED <<GA(p)>>_vars
<1>1. LeaseGuard BY ShippedShape DEF Shipped, LeaseGuard
<1>2. [gw EXCEPT ![p] = Nil] # gw BY DEF TypeOK
<1>. QED BY <1>1, <1>2, ExpandENABLED
         DEF GA, GCas, TookUpdate, RetUpdate, AncUpdate, AncOf, Gen, vars, aux, ret

THEOREM UISaveCompletes == LSpec => Prop_UISaveCompletes
<1>. SUFFICES ASSUME NEW p \in Paths
              PROVE  LSpec => ((gw[p] # Nil) ~> (gw[p] = Nil))
     BY DEF Prop_UISaveCompletes
<1>. DEFINE P == gw[p] # Nil
            Q == gw[p] = Nil
<1>1. TypeOK /\ P /\ [Next]_vars => P' \/ Q'
  OBVIOUS
<1>2. TypeOK /\ P /\ <<GA(p)>>_vars => Q'
  BY L1_Step
<1>3. TypeOK /\ P => ENABLED <<GA(p)>>_vars
  BY L1_Enabled
<1>4. LSpec => Spec /\ Fair BY DEF LSpec, GA, Fair
<1>5. Fair => WF_vars(GA(p)) BY DEF Fair
<1>6. Spec => []TypeOK /\ [][Next]_vars BY TypeInvariant, PTL DEF Spec
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, PTL

==============================================================================
