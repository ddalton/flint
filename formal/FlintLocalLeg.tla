---------------------------- MODULE FlintLocalLeg ----------------------------
(***************************************************************************)
(* A stale leg on the CONSUMER'S node: clear_head_sb's refusal, the F36   *)
(* guard, and the order catchup_stale runs them in (spdk-csi-driver/src/  *)
(* catchup.rs; driver.rs create_raid_from_replicas; hot_rejoin.rs).       *)
(*                                                                         *)
(* FlintReplication's consumer-node arm (batch 3, 2026-10-27) left a      *)
(* residual: a consumer that MOVES onto a stale leg's node.  clear_head_sb *)
(* (catchup.rs:1455) refuses any catch-up while ANY raid of the volume's  *)
(* name is ONLINE on the destination's node, so that leg is never rebuilt *)
(* while the pod stays.  The proposed precision: refuse only when the     *)
(* online raid holds THIS head.  Here:                                     *)
(*                                                                         *)
(*   - legs, one node each, plus a leg-less node; the consumer stages a   *)
(*     raid on one node and may move (Unstage, Stage elsewhere);          *)
(*   - the raid is the live consumer raid: its configured bases are       *)
(*     `members`; a write lands on every member and is acked;             *)
(*   - stage assembles the record-in_sync legs, admits standbys (final    *)
(*     delta), and the forced-stale fallback (driver.rs:2587) fills to two *)
(*     bases with record-STALE legs, which stay Stale;                    *)
(*   - a member's path can fail: the raid drops it, the record still says *)
(*     in_sync until the health monitor marks it (the detection lag).  The *)
(*     record converges before the next assembly (Unstage waits for it):  *)
(*     a record still lagging at a REassembly is FlintReplication's F36c  *)
(*     writer-set gate's territory, which this module does not model;     *)
(*   - catchup_stale in CODE ORDER: the F36 guard (head_live_consumer,    *)
(*     catchup.rs:2010, nvmf exports only), the revert (bdev_lvol_delete: *)
(*     v26.05 _vbdev_lvol_destroy unregisters with no claim check, so a   *)
(*     configured local base is hot-removed from the live raid), then     *)
(*     ensure_dst_attached -> clear_head_sb (2218), then the copy;        *)
(*   - the chase (clear_head_sb again) and admission: hot rejoin into the *)
(*     live raid (no clear_head_sb) or the final delta at the next stage. *)
(*                                                                         *)
(* SPDK's examine is modelled as a no-op on the live raid, because flint  *)
(* creates every consumer raid with superblock:false (driver.rs:3412):    *)
(* v26.05 raid_bdev_examine_sb answers -EINVAL for an sb naming the live  *)
(* raid (its sb is NULL) and -EEXIST from create_from_sb for any other sb *)
(* under the same name.                                                    *)
(*                                                                         *)
(* KNOBS (the shipped values first):                                       *)
(*   ClearSbPrecise = FALSE   any ONLINE raid on the leg's node refuses;  *)
(*                            TRUE = only one that holds THIS head        *)
(*   F36SeesLocal = FALSE     F36 sees only heads exported over nvmf;     *)
(*                            TRUE = it also reads the replica node's     *)
(*                            raid bases (the membership check moved      *)
(*                            BEFORE the revert)                          *)
(*   DeleteSkipsOnline = TRUE the precise path never deletes an online    *)
(*                            raid; FALSE = its phantom-delete loop does  *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANTS
  Legs,              \* e.g. {a, b} or {a, b, c}; leg l lives on node l
  Spare,             \* a node with no leg (the consumer may stage there)
  MaxWrites, MaxFaults, MaxStages,
  ClearSbPrecise, F36SeesLocal, DeleteSkipsOnline

ASSUME Spare \notin Legs

Nodes == Legs \cup {Spare}
Writes == 1..MaxWrites

VARIABLES
  state,     \* [Legs -> {"insync", "stale", "standby"}] the record
  content,   \* [Legs -> SUBSET Writes] what each leg's head holds
  members,   \* SUBSET Legs: the live raid's configured bases
  staged,    \* a raid is assembled (the consumer is staged)
  cnode,     \* the consumer's node (meaningful while staged)
  session,   \* [Legs -> {"idle", "reverted"}] catchup_stale progress
  acked,     \* SUBSET Writes acked to the consumer
  nextW, faults, stages,
  lost       \* ghost: acked writes whose LAST copy a catch-up step destroyed

vars == <<state, content, members, staged, cnode, session, acked, nextW,
          faults, stages, lost>>

TypeOK ==
  /\ state \in [Legs -> {"insync", "stale", "standby"}]
  /\ content \in [Legs -> SUBSET Writes]
  /\ members \subseteq Legs
  /\ staged \in BOOLEAN
  /\ cnode \in Nodes
  /\ session \in [Legs -> {"idle", "reverted"}]
  /\ acked \subseteq Writes
  /\ nextW \in 0..MaxWrites
  /\ faults \in 0..MaxFaults
  /\ stages \in 0..MaxStages
  /\ lost \subseteq Writes

\* Any record with at least one in_sync leg; nothing written yet.
Init ==
  /\ state \in {f \in [Legs -> {"insync", "stale"}] : \E l \in Legs : f[l] = "insync"}
  /\ content = [l \in Legs |-> {}]
  /\ members = {}
  /\ staged = FALSE
  /\ cnode = Spare
  /\ session = [l \in Legs |-> "idle"]
  /\ acked = {}
  /\ nextW = 0 /\ faults = 0 /\ stages = 0
  /\ lost = {}

InSync == {l \in Legs : state[l] = "insync"}
\* The acked writes only leg l holds: overwriting l's head destroys them.
OnlyOn(l) == {w \in acked \cap content[l] : \A m \in Legs \ {l} : w \notin content[m]}
Local(l) == staged /\ cnode = l          \* the live raid sits on l's node
HasSource(l) == \E s \in InSync : s # l

(***************************************************************************)
(* The consumer                                                            *)
(***************************************************************************)

\* A write lands on every configured base and is acked (raid1, one good leg
\* suffices).  The raid always keeps at least one base: SPDK raid1 never
\* drops its last one (a path loss fails the I/O, it does not shrink).
Write ==
  /\ staged /\ members # {}
  /\ nextW < MaxWrites
  /\ nextW' = nextW + 1
  /\ content' = [l \in Legs |-> IF l \in members THEN content[l] \cup {nextW + 1}
                                                ELSE content[l]]
  /\ acked' = acked \cup {nextW + 1}
  /\ UNCHANGED <<state, members, staged, cnode, session, faults, stages, lost>>

\* A member's path fails: the raid drops the base.  The record does not
\* change here (the detection lag).
Fault(l) ==
  /\ staged /\ l \in members /\ members # {l}
  /\ faults < MaxFaults
  /\ faults' = faults + 1
  /\ members' = members \ {l}
  /\ UNCHANGED <<state, content, staged, cnode, session, acked, nextW, stages, lost>>

\* monitor_raid_health: an in_sync leg the raid no longer holds is stale.
MarkStale(l) ==
  /\ staged /\ state[l] = "insync" /\ l \notin members
  /\ state' = [state EXCEPT ![l] = "stale"]
  /\ UNCHANGED <<content, members, staged, cnode, session, acked, nextW, faults, stages, lost>>

\* NodeUnstage, once the health monitor has caught up (see the header).
Unstage ==
  /\ staged
  /\ InSync \subseteq members
  /\ staged' = FALSE
  /\ members' = {}
  /\ UNCHANGED <<state, content, cnode, session, acked, nextW, faults, stages, lost>>

\* NodeStage on node n (the consumer may have moved): in_sync legs attach
\* (one may fail to attach: `drop`), standbys are admitted with the final
\* delta, and if fewer than two bases result the forced-stale fallback adds
\* record-Stale legs with a built head (never an empty one, 4dfd3878) — they
\* stay Stale.  The record marks a left-out in_sync leg stale
\* (record_assembly_sync_state).  Admission needs a source.
Stage(n) ==
  /\ ~staged /\ stages < MaxStages
  /\ \E drop \in SUBSET InSync, forced \in SUBSET {l \in Legs : state[l] = "stale"} :
       LET attach == InSync \ drop
           stand  == IF attach # {} THEN {l \in Legs : state[l] = "standby"} ELSE {}
           base   == attach \cup stand
       IN /\ Cardinality(drop) <= 1
          /\ base \cup forced # {}
          /\ \A l \in forced : content[l] # {} /\ session[l] = "idle"
          /\ forced # {} => Cardinality(base) < 2
          /\ Cardinality(base \cup forced) <= 2 \/ forced = {}
          /\ members' = base \cup forced
          /\ content' = [l \in Legs |-> IF l \in stand
                                          THEN content[CHOOSE s \in attach : TRUE]
                                          ELSE content[l]]
          /\ lost' = lost \cup UNION {OnlyOn(l) \ content[CHOOSE s \in attach : TRUE] : l \in stand}
          /\ state' = [l \in Legs |->
                         IF l \in stand THEN "insync"
                         ELSE IF l \in drop THEN "stale"
                         ELSE state[l]]
  /\ staged' = TRUE
  /\ cnode' = n
  /\ stages' = stages + 1
  /\ UNCHANGED <<session, acked, nextW, faults>>

(***************************************************************************)
(* catchup_stale, in code order                                            *)
(***************************************************************************)

\* F36 (head_live_consumer): defers while a consumer reads the head.  As
\* shipped it finds only a subsystem exporting the head with live
\* controllers — a REMOTE consumer.  A configured base of the raid on the
\* leg's own node is not exported, so it passes.
F36Defers(l) ==
  /\ staged /\ l \in members
  /\ (F36SeesLocal \/ cnode # l)

\* Revert (revert_head / revert_head_to_empty): bdev_lvol_delete unregisters
\* the head; a live raid that holds it loses that base.  The new head is a
\* clone of an older epoch: model it as empty (it holds none of the writes
\* the copy must bring back).
Revert(l) ==
  /\ state[l] = "stale" /\ session[l] = "idle"
  /\ HasSource(l)
  /\ ~F36Defers(l)
  /\ session' = [session EXCEPT ![l] = "reverted"]
  /\ content' = [content EXCEPT ![l] = {}]
  /\ lost' = lost \cup OnlyOn(l)
  /\ members' = members \ {l}
  /\ UNCHANGED <<state, staged, cnode, acked, nextW, faults, stages>>

\* clear_head_sb on the destination's node (ensure_dst_attached).
ClearSbRefuses(l) ==
  IF ClearSbPrecise THEN Local(l) /\ l \in members
                    ELSE Local(l) /\ members # {}
\* The precise path's phantom-delete loop meets the live raid under the
\* volume's name: DeleteSkipsOnline = FALSE deletes it.
ClearSbDeletesLive(l) ==
  ClearSbPrecise /\ ~DeleteSkipsOnline /\ Local(l) /\ members # {} /\ l \notin members

\* The copy from an in_sync source; the session records standby.
Copy(l) ==
  /\ state[l] = "stale" /\ session[l] = "reverted"
  /\ ~ClearSbRefuses(l)
  /\ \E s \in InSync \ {l} :
       /\ content' = [content EXCEPT ![l] = content[s]]
       /\ lost' = lost \cup (OnlyOn(l) \ content[s])
  /\ state' = [state EXCEPT ![l] = "standby"]
  /\ session' = [session EXCEPT ![l] = "idle"]
  \* The consumer stays staged under a deleted raid: an outage.
  /\ members' = IF ClearSbDeletesLive(l) THEN {} ELSE members
  /\ UNCHANGED staged
  /\ UNCHANGED <<cnode, acked, nextW, faults, stages>>

\* chase_standby: re-copy from an in_sync source, through clear_head_sb.
Chase(l) ==
  /\ state[l] = "standby"
  /\ ~ClearSbRefuses(l)
  /\ \E s \in InSync \ {l} :
       /\ content[s] # content[l]
       /\ content' = [content EXCEPT ![l] = content[s]]
       /\ lost' = lost \cup (OnlyOn(l) \ content[s])
  \* The consumer stays staged under a deleted raid: an outage.
  /\ members' = IF ClearSbDeletesLive(l) THEN {} ELSE members
  /\ UNCHANGED staged
  /\ UNCHANGED <<state, cnode, session, acked, nextW, faults, stages>>

\* Hot rejoin into the live raid (hot_rejoin.rs): the head is cut from a
\* survivor that is a configured base, under the quiesce window; no
\* clear_head_sb on this path.
HotRejoin(l) ==
  /\ state[l] = "standby" /\ staged /\ l \notin members
  /\ \E s \in members \cap InSync :
       /\ content' = [content EXCEPT ![l] = content[s]]
       /\ lost' = lost \cup (OnlyOn(l) \ content[s])
  /\ state' = [state EXCEPT ![l] = "insync"]
  /\ members' = members \cup {l}
  /\ UNCHANGED <<staged, cnode, session, acked, nextW, faults, stages>>

Next ==
  \/ Write \/ Unstage
  \/ \E l \in Legs : Fault(l) \/ MarkStale(l) \/ Revert(l) \/ Copy(l) \/ Chase(l) \/ HotRejoin(l)
  \/ \E n \in Nodes : Stage(n)

Fairness ==
  /\ \A l \in Legs : WF_vars(MarkStale(l)) /\ WF_vars(Revert(l))
                     /\ WF_vars(Copy(l)) /\ WF_vars(HotRejoin(l))

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* Properties                                                              *)
(***************************************************************************)

\* No catch-up step destroys the last copy of an acked write: the revert,
\* the copy, the chase or an admission overwrites a head only where some
\* other leg still holds what it held.  (The forced-stale fallback's own
\* divergence hazard -- a Stale base missing writes others hold -- is
\* evented and out of scope; this is the catch-up's.)
Inv_NoCatchupLoss == lost = {}

\* A staged volume keeps a raid with a base: no catch-up step takes the
\* consumer's last base or deletes its raid.
Inv_RaidKept == staged => members # {}

Inv == TypeOK /\ Inv_NoCatchupLoss /\ Inv_RaidKept

\* A stale leg the raid does not hold, with an in_sync source, leaves Stale
\* (the consumer may stay put: Unstage/Stage carry no fairness).
Prop_StaleRebuilt ==
  \A l \in Legs :
    (state[l] = "stale" /\ l \notin members /\ HasSource(l))
      ~> (state[l] # "stale" \/ l \in members \/ ~HasSource(l))

=============================================================================
