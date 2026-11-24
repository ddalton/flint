-------------------------- MODULE FlintCatchupSLiveW2 -------------------------
(***************************************************************************)
(* rev 17 liveness, restated (10-31, c31 lv1): without the premise that   *)
(* the last healthy member never fails (LastFails, the user's in-write    *)
(* rule), a raid can lose every member under a held loss (the drain takes *)
(* the healthy leg, the failing one fails the stamp: StampLand with no    *)
(* survivor), and no stamp can release it.  Prop_BarrierEndsM: a barrier *)
(* ends, or the raid has no member left (the volume is dead until it is   *)
(* unstaged).  FlintCatchupSLiveW (752b1a56) is unchanged.                *)
(***************************************************************************)
EXTENDS FlintCatchupSLiveW

Prop_BarrierEndsM == [](attached /\ hold => <>(~hold \/ ~attached \/ members = {}))
\* where the dead raid comes from: no healthy member left while held
Prop_BarrierEndsH == [](attached /\ hold => <>(~hold \/ ~attached \/ Healthy = {}))
=============================================================================
