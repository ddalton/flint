------------------------------ MODULE FlintClaims ------------------------------
(***************************************************************************)
(* The multi-process claims/window layer — the F50/F53 axis, which the     *)
(* replica-lifecycle module deliberately assumes away (its single `claim`  *)
(* variable IS the single-process assumption).                             *)
(*                                                                         *)
(* The facts this module encodes, all confirmed live:                      *)
(*                                                                         *)
(*   - volume_claims.rs is IN-PROCESS.  Two controller-shaped processes    *)
(*     (a helm rolling-upgrade overlap, the vestigial operator pod — F50;  *)
(*     the dashboard backend — F53) each have their own registry; each     *)
(*     believes its claims serialize everything; neither can see the       *)
(*     other's in-flight work.                                             *)
(*   - The destructive collision (F50): process A's hot-rejoin window      *)
(*     writes intent (standby -> stale + marker) and prestages E_f;        *)
(*     process B's catch-up marked-dispatch decodes stale+marker-with-     *)
(*     no-head — which is EXACTLY what a live pre-flip window looks like   *)
(*     — and scrubs the E_f artifacts out from under the in-flight         *)
(*     window.  "Mutual exclusion between claim holders is not mutual      *)
(*     exclusion between the operations that touch E_f."                   *)
(*   - The fix stack is LAYERED: the marker grace (hot_rejoin_at; a young  *)
(*     marker is left completely alone), the P1 kube-Lease (gates          *)
(*     STARTING ops at tick granularity — an in-flight op of a deposed     *)
(*     leader is never interrupted), strategy: Recreate, and the F53 role  *)
(*     grant.  Until now the stack was prose-argued; the F50 doc itself    *)
(*     retracts its own completeness claim ("They closed every window I    *)
(*     had looked for").  This module machine-checks the two layers that   *)
(*     are PROTOCOL (grace, lease) and states precisely which failure      *)
(*     each one carries.                                                   *)
(*                                                                         *)
(* THE MODEL: one volume, one rejoining leg (survivors and content are     *)
(* abstracted to a single warmth bit — FlintReplication owns the content   *)
(* machine), two processes with private claim registries.  The window is  *)
(* decomposed into open/commit precisely because an atomic Admit cannot    *)
(* be raced: the F50 loss lives BETWEEN intent and flip.  The record CAS   *)
(* is the TLA action atomicity itself; the marker carries NO owner field   *)
(* (faithful: a young stale+marker is indistinguishable from a live        *)
(* window by record state alone), and only the owner's own in-memory task  *)
(* state (winOwner) lets it continue its window.                           *)
(*                                                                         *)
(* THE GRACE AS AN ORDERING AXIOM: TLA cannot say "300s"; the grace's      *)
(* quantitative content is the assumption that a marker outlives the       *)
(* grace only if its window is no longer live (real windows span ~250ms;   *)
(* the grace is 300s).  MarkerAge therefore fires only when winOwner is    *)
(* none — the window committed, died, or was abandoned.  Dropping          *)
(* MarkerGrace lets Scrub ignore youth entirely: the pre-F50 world, and    *)
(* the mutation run must rediscover the loss.                              *)
(*                                                                         *)
(* THE LEASE AS TICK-GRANULARITY: LeaderGate guards ACQUIRE/OPEN only;     *)
(* CatchUp, Scrub and Commit run ungated under an already-held claim (an   *)
(* in-flight op is never interrupted).  Leadership moves freely off a      *)
(* dead holder (the TTL — weakly fair, unbudgeted) and can also move       *)
(* SPURIOUSLY off a live one (a renewal hiccup — budgeted): the deposed    *)
(* leader's in-flight dispatch keeps running.  That overlap is exactly     *)
(* what the grace exists to survive, which is why the NoGrace mutation     *)
(* finds its loss WITH the lease on: the layers are complementary, not     *)
(* redundant (the F48 gate-and-fence shape, one layer up).                 *)
(*                                                                         *)
(* Out of scope: window unwind on RPC failure and adopt-vs-scrub          *)
(* discrimination (the crash-sweep sim harness's job); the F53 role        *)
(* grant itself (configuration, not protocol — CSI_MODE conflation is     *)
(* killed by orchestrator_role.rs, not by a model); content/lineage        *)
(* (FlintReplication).                                                     *)
(*                                                                         *)
(* 2026-07-29 audit census: the leader-gated actors number SIX             *)
(* (hot_rejoin, catchup, epoch_scheduler, cutover, maint_roll, rwx_nfs)   *)
(* — this module's two processes model the claims/window layer only.  The *)
(* epoch scheduler's cut-deferral consults the process-LOCAL registry     *)
(* (volume_claims OP_HOT_REJOIN visibility), so a foreign process's       *)
(* scheduler can drop a cut inside a live window — churn-only (the EEXIST *)
(* unwind is the safe direction), fenced by the Lease alone.  For the     *)
(* maintenance ROLLER the Lease is SAFETY-load-bearing (read-then-act on  *)
(* shared record state): the NoLeader run's "operability, not safety"     *)
(* verdict is scoped to the actions modeled HERE, not to those actors.    *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Procs,       \* controller-shaped processes, e.g. {p1, p2}
  MaxCrashes,  \* budget for process deaths + spurious leadership moves
  LeaderGate,  \* TRUE = the P1 kube-Lease gates acquiring/opening (tick granularity)
  MarkerGrace  \* TRUE = the F50 marker grace (scrub only OLD markers)

VARIABLES
  alive,       \* [Procs -> BOOLEAN]
  claim,       \* [Procs -> {"none","catchup","admission"}] — PRIVATE registries
  leader,      \* Procs — the Lease holder
  legState,    \* {"stale","standby","insync"} — the rejoining leg's record state
  legWarm,     \* the leg's payload is caught up / usable (scrub wipes it)
  serving,     \* the leg was admitted into the serving raid
  window,      \* {"none","young","old"} — the marker + its age; NO owner field
  winOwner,    \* Procs \cup {"none"} — the owner's IN-MEMORY task state only
  crashes

vars == <<alive, claim, leader, legState, legWarm, serving, window, winOwner, crashes>>

NoneOr(S) == S \cup {"none"}

TypeOK ==
  /\ alive \in [Procs -> BOOLEAN]
  /\ claim \in [Procs -> {"none", "catchup", "admission"}]
  /\ leader \in Procs
  /\ legState \in {"stale", "standby", "insync"}
  /\ legWarm \in BOOLEAN
  /\ serving \in BOOLEAN
  /\ window \in {"none", "young", "old"}
  /\ winOwner \in NoneOr(Procs)
  /\ crashes \in 0..MaxCrashes

AllDead == \A p \in Procs : ~alive[p]

Init ==
  /\ alive = [p \in Procs |-> TRUE]
  /\ claim = [p \in Procs |-> "none"]
  /\ leader \in Procs
  /\ legState = "stale"
  /\ legWarm = FALSE
  /\ serving = FALSE
  /\ window = "none"
  /\ winOwner = "none"
  /\ crashes = 0

(***************************************************************************)
(* Claims — per-process, mutually invisible.  Only ACQUISITION is leader-  *)
(* gated (tick granularity).                                               *)
(***************************************************************************)

\* Work the catch-up dispatch would perform this tick: a cold stale leg
\* to warm, or a scrubbable marker.  The tick claims only when it has
\* work and the RAII guard drops only when the work is done — modeled
\* explicitly, because without it TLC finds the honest WF trap: a
\* claim acquire/release ping-pong keeps WindowOpen (which needs
\* claim = "none") only intermittently enabled, and weak fairness never
\* obligates an intermittently-enabled action.
CatchupWork ==
  \/ (~legWarm /\ legState = "stale" /\ window = "none")
  \/ (window # "none" /\ (MarkerGrace => window = "old"))

AcquireCatchup(p) ==
  /\ alive[p]
  /\ (LeaderGate => p = leader)
  /\ claim[p] = "none"
  /\ CatchupWork
  /\ claim' = [claim EXCEPT ![p] = "catchup"]
  /\ UNCHANGED <<alive, leader, legState, legWarm, serving, window, winOwner, crashes>>

\* Never releases the admission claim while its window is in flight (the
\* claim guard is RAII around the window task), and never drops a
\* catch-up claim with its dispatch's work still pending.
ReleaseClaim(p) ==
  /\ alive[p]
  /\ claim[p] # "none"
  /\ winOwner # p
  /\ (claim[p] = "catchup" => ~CatchupWork)
  /\ claim' = [claim EXCEPT ![p] = "none"]
  /\ UNCHANGED <<alive, leader, legState, legWarm, serving, window, winOwner, crashes>>

(***************************************************************************)
(* The work.  CatchUp warms the leg (Tier-1 chase).  WindowOpen is the     *)
(* intent write: acquire the admission claim, stamp the marker (young),    *)
(* demote standby -> stale (the F48 intent semantics) — one CAS.           *)
(* WindowCommit is the flip: the owner trusts its in-memory state and      *)
(* does NOT re-verify warmth or the marker (that verification happened at  *)
(* open; the flip re-checking it is not the shipped code) — which is       *)
(* exactly why a scrub landing in between is a silent loss.                *)
(***************************************************************************)

CatchUp(p) ==
  /\ alive[p]
  /\ claim[p] = "catchup"                  \* in-flight: ungated
  /\ ~legWarm
  /\ legState = "stale"
  /\ window = "none"
  /\ legWarm' = TRUE
  /\ legState' = "standby"
  /\ UNCHANGED <<alive, claim, leader, serving, window, winOwner, crashes>>

WindowOpen(p) ==
  /\ alive[p]
  /\ (LeaderGate => p = leader)
  /\ claim[p] = "none"
  /\ legWarm
  /\ legState = "standby"
  \* The record's one-window rule (CAS).  NOTE: at this module's tick
  \* granularity the guard is REDUNDANT, machine-checked (deleting it
  \* leaves all three gate verdicts identical): an open requires
  \* legState = "standby" and the first open demotes to "stale", so a
  \* second open is structurally impossible whatever this conjunct
  \* says.  The CAS's unique contribution appears only under
  \* read/write decomposition of the open (two processes both reading
  \* "standby" before either writes) — deliberately out of scope here
  \* (every action is one CAS round by construction).  Kept because the
  \* code performs the check; there is no OneWindowCAS mutation run
  \* because a mutation that cannot lose proves nothing — see
  \* formal/README.md (the dropped run 5q).
  /\ window = "none"
  /\ claim' = [claim EXCEPT ![p] = "admission"]
  /\ window' = "young"
  /\ winOwner' = p
  /\ legState' = "stale"
  /\ UNCHANGED <<alive, leader, legWarm, serving, crashes>>

WindowCommit(p) ==
  /\ alive[p]
  /\ winOwner = p                          \* in-memory continuation: ungated
  /\ serving' = TRUE
  /\ legState' = "insync"
  /\ window' = "none"
  /\ winOwner' = "none"
  /\ claim' = [claim EXCEPT ![p] = "none"]
  /\ UNCHANGED <<alive, leader, legWarm, crashes>>

(***************************************************************************)
(* The reconciler's scrub — hot-rejoin maintenance performed UNDER THE     *)
(* CATCH-UP CLAIM (the F50 mechanism verbatim): stale+marker looks like a  *)
(* dead window, the scrub wipes the prestaged artifacts (legWarm) and      *)
(* clears the marker.  With MarkerGrace, a YOUNG marker is left            *)
(* completely alone.  The scrubber cannot see winOwner — that is the       *)
(* whole point.                                                            *)
(***************************************************************************)

ScrubMarked(p) ==
  /\ alive[p]
  /\ claim[p] = "catchup"                  \* in-flight: ungated
  /\ window # "none"
  /\ (MarkerGrace => window = "old")
  /\ legWarm' = FALSE
  /\ legState' = "stale"
  /\ window' = "none"
  /\ UNCHANGED <<alive, claim, leader, serving, winOwner, crashes>>

\* The grace's quantitative content as an ordering axiom: a marker ages
\* past the grace only once no live window owns it (windows span ~250ms,
\* the grace 300s).  Weakly fair — time passes.
MarkerAge ==
  /\ window = "young"
  /\ winOwner = "none"
  /\ window' = "old"
  /\ UNCHANGED <<alive, claim, leader, legState, legWarm, serving, winOwner, crashes>>

(***************************************************************************)
(* The environment: process death (in-memory registry and task state       *)
(* vanish — the marker does NOT), and leadership movement.  TakeOver is    *)
(* the Lease TTL doing its job (off a dead holder; unbudgeted, fair).      *)
(* SpuriousChange is a renewal hiccup deposing a LIVE holder (budgeted):   *)
(* the deposed leader's in-flight dispatch keeps running — the overlap     *)
(* the tick-granularity Lease cannot close and the grace must survive.     *)
(***************************************************************************)

ProcDie(p) ==
  /\ alive[p]
  /\ crashes < MaxCrashes
  /\ alive' = [alive EXCEPT ![p] = FALSE]
  /\ claim' = [claim EXCEPT ![p] = "none"]
  /\ winOwner' = IF winOwner = p THEN "none" ELSE winOwner
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<leader, legState, legWarm, serving, window>>

TakeOver(q) ==
  /\ ~alive[leader]
  /\ alive[q]
  /\ leader' = q
  /\ UNCHANGED <<alive, claim, legState, legWarm, serving, window, winOwner, crashes>>

SpuriousChange(q) ==
  /\ alive[leader]
  /\ alive[q]
  /\ q # leader
  /\ crashes < MaxCrashes
  /\ leader' = q
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<alive, claim, legState, legWarm, serving, window, winOwner>>

Next ==
  \/ MarkerAge
  \/ \E p \in Procs :
       \/ AcquireCatchup(p)
       \/ ReleaseClaim(p)
       \/ CatchUp(p)
       \/ WindowOpen(p)
       \/ WindowCommit(p)
       \/ ScrubMarked(p)
       \/ ProcDie(p)
       \/ TakeOver(p)
       \/ SpuriousChange(p)

\* Recovery and progress machinery is weakly fair; deaths and spurious
\* leadership moves are the environment.
Fairness ==
  /\ \A p \in Procs :
       /\ WF_vars(AcquireCatchup(p))
       /\ WF_vars(ReleaseClaim(p))
       /\ WF_vars(CatchUp(p))
       /\ WF_vars(WindowOpen(p))
       /\ WF_vars(WindowCommit(p))
       /\ WF_vars(ScrubMarked(p))
       /\ WF_vars(TakeOver(p))
  /\ WF_vars(MarkerAge)

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* THE THEOREM (the F50 loss, named): a leg is never serving cold — the    *)
(* flip only ever lands the payload its open verified.  The NoGrace        *)
(* mutation must violate this: a spuriously-deposed leader's in-flight     *)
(* catch-up dispatch scrubs the new leader's young window (wiping the      *)
(* prestaged payload), and the blind flip commits a cold leg into the      *)
(* serving raid.                                                           *)
(***************************************************************************)
Inv_NoColdAdmission == serving => legWarm

Inv == TypeOK /\ Inv_NoColdAdmission

(***************************************************************************)
(* Liveness.  Every marker resolves (commit, or age-then-scrub after the   *)
(* owner died) unless every process is dead; and the leg eventually        *)
(* serves once the failure budget settles — including the full crash-      *)
(* recovery story: owner dies mid-window, marker ages, the survivor        *)
(* takes the lease, scrubs, re-warms, re-opens, commits.                   *)
(***************************************************************************)
WindowResolves ==
  [](window # "none" => <>(window = "none" \/ AllDead))

EventuallyServes ==
  <>[](crashes < MaxCrashes \/ serving \/ AllDead)

================================================================================
