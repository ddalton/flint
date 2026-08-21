--------------------------- MODULE FlintShareDisk ---------------------------
(***************************************************************************)
(* The FlintShare disk lifecycle — the operator's ladder over one share's  *)
(* PersistentVolumeClaim (spdk-csi-driver/src/lite_operator/idle.rs,       *)
(* reconcile.rs `drive_reprovision` / `verify_and_hibernate` /             *)
(* `maybe_auto_expand` / `claim_plan`, and persistence.rs's effective      *)
(* size).  Modeled AFTER the code and after the kind drill: the drill      *)
(* samples one interleaving per leg, this module enumerates them.          *)
(*                                                                         *)
(* WHY THIS MODULE EXISTS.  Two features now destroy or resize a volume    *)
(* — hibernate (delete, wake by DR import) and reprovision-on-shrink       *)
(* (delete, recreate smaller) — and a third (auto-expand) changes the      *)
(* size under both.  They are driven by a LEVEL-TRIGGERED reconciler that  *)
(* may restart between any two steps, against a hub that can die, a front  *)
(* door that can stamp a wake at any instant, and a user who can edit spec *)
(* at any instant.  Six ladder states times four independent event         *)
(* sources is past hand enumeration; the shrink-versus-expand interaction  *)
(* below was found by hand only because someone asked whether this could   *)
(* be modeled.                                                             *)
(*                                                                         *)
(* The facts encoded, each from the implementation:                        *)
(*                                                                         *)
(*   - The ladder position is a DURABLE ANNOTATION on the CR, so every     *)
(*     intermediate state survives an operator restart and is resumed      *)
(*     rather than restarted (idle.rs's module doc).  `OpRestart` here     *)
(*     wipes only in-memory belief; `ladder` persists.                     *)
(*   - `is_down()` decides replicas: Suspended, Hibernated and             *)
(*     ReprovisionDraining scale to zero; ReprovisionVerifying does NOT.   *)
(*     That two-step is forced — the hub must be UP to answer whether the  *)
(*     bucket is current, and DOWN before anything takes its claim away.   *)
(*   - The operator holds no bucket credentials, so recoverability is not  *)
(*     something it can check: it polls the hub's /status for `rpoClean`.  *)
(*     The poll's answer is therefore a MESSAGE ABOUT THE PAST, and this   *)
(*     module keeps it one (see FAITHFULNESS).                             *)
(*   - The claim is deleted only once the pod is genuinely gone            *)
(*     (`reclaim_hibernated_disk`, `drive_reprovision`): deleting one a    *)
(*     pod still mounts parks it Terminating, where an interrupted         *)
(*     operator cannot tell a finished drain from an aborted one.          *)
(*   - Hibernation ABORTS on a wake request; reprovision does NOT.  The    *)
(*     asymmetry is deliberate and is a liveness property here, not a      *)
(*     preference: a rebuild that any keepalive can cancel never finishes  *)
(*     on the shares anyone uses.                                          *)
(*   - Auto-expand writes a TARGET annotation, never spec, and the target  *)
(*     records the `size` it was derived from.  A `size` edit invalidates  *)
(*     it, which is what lets a user shrink at all.                        *)
(*                                                                         *)
(* FAITHFULNESS NOTES (the reasoning, so it cannot drift silently):        *)
(*                                                                         *)
(*   - `rpoClean` IS NOT A FREE BOOLEAN.  Modeling it as one would prove   *)
(*     nothing about the real predicate and is exactly the mistake this    *)
(*     project has made three times ("THE ABSTRACTION WAS THE BUG").  It   *)
(*     is derived here from `dirty` — unflushed local bytes — which a      *)
(*     RUNNING pod can set at any moment and only a completed DRAIN        *)
(*     clears.  That is what makes the poll's answer stale by             *)
(*     construction and gives Inv_NoUnverifiedDelete something to fail     *)
(*     against.                                                            *)
(*   - THE POLL IS NOT WHAT MAKES THE DELETE SAFE — the DRAIN is, and     *)
(*     this module is where that stopped being an opinion.  The header     *)
(*     first claimed both `VerifyFirst=FALSE` and `DrainFirst=FALSE` would *)
(*     lose Inv_NoUnverifiedDelete.  TLC disagreed: with the drain intact, *)
(*     dropping the rpoClean gate entirely loses NOTHING, because          *)
(*     scale-to-zero flushes on the way out and only a pod-GONE            *)
(*     observation admits the delete.  So NoVerify is not in the gate      *)
(*     ("a mutation that cannot lose proves nothing" — README, the         *)
(*     dropped run 5q), and the poll stands revealed as an ADMISSION gate: *)
(*     it decides whether a share is worth draining at all and gives the   *)
(*     operator something to report, not whether the bytes are safe.       *)
(*     The operational consequence is worth stating plainly: anyone        *)
(*     "optimising" the pod-gone wait because the poll already said clean  *)
(*     would delete a live project's disk.                                 *)
(*   - Poll-and-decide is ONE action.  `drive_reprovision` reads /status   *)
(*     and acts on it inside a single reconcile pass, so no client write   *)
(*     interleaves between the answer and its use; decomposing them        *)
(*     manufactured a lasso the code does not have.  The staleness that IS *)
(*     real — between that decision and the delete, many reconciles later  *)
(*     — is preserved, and is exactly what DrainFirst exploits.            *)
(*   - A RUNNING hub flushes on its flush floor, not only at drain.        *)
(*     Modeling the drain as the only flush also manufactured a lasso.     *)
(*     Both of these were model bugs found by TLC and fixed in the model;  *)
(*     they are recorded because a reader's first instinct on seeing       *)
(*     `Flush` is to ask why a shipped-code module has one.                *)
(*   - Sizes are small naturals, not quantities.  `effective_size`'s       *)
(*     parsing is a pure function with its own unit tests; what is         *)
(*     interesting here is the BASIS rule, which is ordering, not          *)
(*     arithmetic.                                                         *)
(*   - One share.  Conflict arbitration across shares is                   *)
(*     FlintTierEpoch's axis and the operator's `conflict` module; nothing *)
(*     here turns on a second share existing.                              *)
(*                                                                         *)
(* THE THEOREMS (strict run):                                              *)
(*   - Inv_NoUnverifiedDelete: the claim is never deleted while bytes are  *)
(*     unflushed.  The DrainFirst mutation (delete as soon as replicas hit *)
(*     zero, without waiting the pod out) must rediscover the loss.        *)
(*   - Inv_ShrinkHonoured: if a rebuild ran because the user asked for a   *)
(*     smaller disk, the disk it settles on is not bigger than the user    *)
(*     asked for.  The GuardOscillation mutation must rediscover the loss  *)
(*     — auto-expand growing the rebuild straight back, so the outage      *)
(*     bought nothing and the request was silently discarded.              *)
(*   - Inv_NeverDeleteAdopted: an adopted claim is never deleted by any    *)
(*     path.  The AdoptGuard mutation must rediscover the loss.            *)
(*                                                                         *)
(* THE VACUITY PROBE (required-fail):                                      *)
(*   - NoRebuildEverHappened must be VIOLATED.  The first configuration    *)
(*     of this module had ProjectWants above every size a user could ask   *)
(*     for, so the oscillation guard refused every shrink, no rebuild was  *)
(*     ever reachable, and Inv_ShrinkHonoured passed by never firing.      *)
(*     The strict run looked identical.  Keep this probe: a config edit    *)
(*     can re-create that silence at any time.                             *)
(*                                                                         *)
(* THE LIVENESS THEOREM (liveness run):                                    *)
(*   - RebuildCompletes: a started rebuild eventually reaches Active on a  *)
(*     claim at the requested size.  The AbortOnWake mutation must find    *)
(*     the lasso — a front door keepaliving on its own cadence cancels the *)
(*     rebuild forever, which is why reprovision does not inherit          *)
(*     hibernation's abort.                                                *)
(*                                                                         *)
(* Out of scope: the epoch cell and takeover (FlintTierEpoch), eviction    *)
(* and hydration content (FlintTierMarker), multi-share arbitration,       *)
(* and the arithmetic of quantity parsing.                                 *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Sizes,            \* the size lattice, e.g. {1, 2, 4} — small naturals
  ProjectWants,     \* the size auto-expand computes for this project
  MaxRebuilds,      \* budget: rebuilds admitted before the model stops
  MaxEdits,         \* budget: user spec edits
  AutoExpand,       \* TRUE = spec.persistence.autoExpand.enabled
  Adopted,          \* TRUE = spec.existingClaim — the operator must never delete
  DrainFirst,       \* TRUE = shipped: delete only once the pod is GONE
  VerifyFirst,      \* TRUE = shipped: enter Draining only after a clean poll
  GuardOscillation, \* TRUE = shipped: refuse a shrink auto-expand would undo
  AdoptGuard,       \* TRUE = shipped: adopted claims are never deleted
  AbortOnWake       \* TRUE = hibernation's behaviour (WRONG for a rebuild)

VARIABLES
  ladder,      \* the durable annotation: the ladder's position
  pvcExists,   \* is there a claim at all
  pvcSize,     \* what size it is provisioned at
  podState,    \* "running" | "terminating" | "gone"
  dirty,       \* unflushed local bytes — what makes rpoClean false
  specSize,    \* USER-owned: spec.persistence.size
  tBasis,      \* auto-expand target's basis (0 = no target recorded)
  tSize,       \* auto-expand target's size (0 = no target recorded)
  wakeReq,     \* the front door's flint.io/requested-at is present
  rebuilds,    \* budget counter
  edits,       \* budget counter
  askedFor,    \* the size the user last explicitly asked for (0 = never)
  unverifiedDelete, \* witness: a delete happened with bytes unflushed
  adoptedDelete     \* witness: an adopted claim was deleted

vars == <<ladder, pvcExists, pvcSize, podState, dirty, specSize, tBasis, tSize,
          wakeReq, rebuilds, edits, askedFor, unverifiedDelete, adoptedDelete>>

Ladders == {"Active", "Suspended", "Hibernated", "HibernateVerifying",
            "ReprovisionVerifying", "ReprovisionDraining"}

\* is_down(): which positions render replicas 0.  ReprovisionVerifying is
\* deliberately absent — the hub must be up to be asked.
IsDown(l) == l \in {"Suspended", "Hibernated", "ReprovisionDraining"}

IsRebuilding(l) == l \in {"ReprovisionVerifying", "ReprovisionDraining"}

\* persistence::effective_size — spec unless a target was recorded FOR
\* THIS EXACT spec size and is bigger.  A stale basis discards it, which
\* is what lets a user edit win.
Effective == IF tBasis = specSize /\ tSize > specSize THEN tSize ELSE specSize

\* reconcile::auto_expand_would_undo_it — decidable from spec alone.
\* MaxSize is modeled as ProjectWants' ceiling: the guard fires when
\* auto-expand could still ask for more than the user wants.
WouldUndo == AutoExpand /\ ProjectWants > specSize

Init ==
  /\ ladder = "Active"
  /\ pvcExists = TRUE
  /\ pvcSize = CHOOSE s \in Sizes : \A t \in Sizes : s <= t
  /\ podState = "running"
  /\ dirty = FALSE
  /\ specSize = CHOOSE s \in Sizes : \A t \in Sizes : s <= t
  /\ tBasis = 0
  /\ tSize = 0
  /\ wakeReq = FALSE
  /\ rebuilds = 0
  /\ edits = 0
  /\ askedFor = 0
  /\ unverifiedDelete = FALSE
  /\ adoptedDelete = FALSE

(***************************************************************************)
(* The world: a running pod dirties bytes; a draining one flushes them.    *)
(***************************************************************************)

Write ==
  /\ podState = "running"
  /\ dirty' = TRUE
  /\ UNCHANGED <<ladder, pvcExists, pvcSize, podState, specSize, tBasis, tSize,
                 wakeReq, rebuilds, edits, askedFor,
                 unverifiedDelete, adoptedDelete>>

\* The flush orchestrator: a RUNNING hub publishes on its flush floor,
\* so `rpoClean` goes true between writes rather than only at drain.
\* Modeling the drain as the only flush manufactured a livelock the
\* product does not have — TLC found it as a RebuildCompletes lasso, and
\* the fix was to the model, not the code.
Flush ==
  /\ podState = "running"
  /\ dirty
  /\ dirty' = FALSE
  /\ UNCHANGED <<ladder, pvcExists, pvcSize, podState, specSize, tBasis, tSize,
                 wakeReq, rebuilds, edits, askedFor,
                 unverifiedDelete, adoptedDelete>>

\* Scale-to-zero starts a termination; the hub flushes on its way out.
BeginTerminate ==
  /\ IsDown(ladder)
  /\ podState = "running"
  /\ podState' = "terminating"
  /\ UNCHANGED <<ladder, pvcExists, pvcSize, dirty, specSize, tBasis, tSize,
                 wakeReq, rebuilds, edits, askedFor,
                 unverifiedDelete, adoptedDelete>>

\* The drain: the grace period is sized for a real flush (A3).
FinishDrain ==
  /\ podState = "terminating"
  /\ podState' = "gone"
  /\ dirty' = FALSE
  /\ UNCHANGED <<ladder, pvcExists, pvcSize, specSize, tBasis, tSize, wakeReq,
                 rebuilds, edits, askedFor, unverifiedDelete,
                 adoptedDelete>>

\* The render brings the pod back whenever the ladder is not down and a
\* claim exists.
StartPod ==
  /\ ~IsDown(ladder)
  /\ podState = "gone"
  /\ pvcExists
  /\ podState' = "running"
  /\ UNCHANGED <<ladder, pvcExists, pvcSize, dirty, specSize, tBasis, tSize,
                 wakeReq, rebuilds, edits, askedFor,
                 unverifiedDelete, adoptedDelete>>

\* The front door, on its own cadence.
Keepalive ==
  /\ wakeReq' = TRUE
  /\ UNCHANGED <<ladder, pvcExists, pvcSize, podState, dirty, specSize, tBasis,
                 tSize, rebuilds, edits, askedFor,
                 unverifiedDelete, adoptedDelete>>

\* An operator restart is a STUTTERING STEP here, and that is the point
\* of the durable annotation rather than an omission: the ladder's
\* position lives on the CR, the operator caches no decision across
\* passes, and it re-polls every time. There is no in-memory belief for
\* a restart to lose, so a restart changes nothing this model can see.
\* (What a restart DOES cost — nothing progresses while no operator is
\* running — is real, drilled at 245s, and outside a spec that assumes
\* the reconciler eventually runs.)

(***************************************************************************)
(* The operator.                                                           *)
(***************************************************************************)

\* The user edits spec.persistence.size. Always wins; discards the target.
UserResize(s) ==
  /\ edits < MaxEdits
  /\ s # specSize
  /\ specSize' = s
  /\ askedFor' = s
  /\ edits' = edits + 1
  /\ UNCHANGED <<ladder, pvcExists, pvcSize, podState, dirty, tBasis, tSize,
                 wakeReq, rebuilds, unverifiedDelete,
                 adoptedDelete>>

\* claim_plan's shrink arm + shrink_reprovision_ok: start a rebuild.
StartRebuild ==
  /\ ladder = "Active"
  /\ pvcExists
  /\ pvcSize > Effective          \* a shrink is being asked for
  /\ rebuilds < MaxRebuilds
  /\ IF AdoptGuard THEN ~Adopted ELSE TRUE
  /\ IF GuardOscillation THEN ~WouldUndo ELSE TRUE
  /\ ladder' = "ReprovisionVerifying"
  /\ rebuilds' = rebuilds + 1
  /\ UNCHANGED <<pvcExists, pvcSize, podState, dirty, specSize, tBasis, tSize,
                 wakeReq, edits, askedFor,
                 unverifiedDelete, adoptedDelete>>

\* drive_reprovision, verify half. VerifyFirst=FALSE drops the gate.
\* ONE action, deliberately: `drive_reprovision` polls and decides inside
\* a single reconcile pass, so no client write interleaves BETWEEN the
\* answer and its use.  Decomposing them manufactured a lasso the code
\* does not have.  The staleness that IS real — between this decision
\* and the delete, many reconciles later — is preserved below.
VerifyRebuild ==
  /\ ladder = "ReprovisionVerifying"
  /\ podState = "running"
  /\ IF VerifyFirst THEN ~dirty ELSE TRUE
  /\ ladder' = "ReprovisionDraining"
  /\ UNCHANGED <<pvcExists, pvcSize, podState, dirty, specSize, tBasis, tSize,
                 wakeReq, rebuilds, edits, askedFor,
                 unverifiedDelete, adoptedDelete>>

\* Hibernation's abort, which reprovision deliberately does NOT inherit.
AbortRebuild ==
  /\ AbortOnWake
  /\ ladder = "ReprovisionVerifying"
  /\ wakeReq
  /\ ladder' = "Active"
  /\ wakeReq' = FALSE
  /\ UNCHANGED <<pvcExists, pvcSize, podState, dirty, specSize, tBasis, tSize,
                 rebuilds, edits, askedFor, unverifiedDelete,
                 adoptedDelete>>

\* drive_reprovision, drain half: delete the claim once the pod is gone.
\* DrainFirst=FALSE deletes as soon as replicas are zero — the mutation.
DeleteClaim ==
  /\ ladder = "ReprovisionDraining"
  /\ pvcExists
  /\ IF DrainFirst THEN podState = "gone" ELSE podState # "running"
  /\ pvcExists' = FALSE
  /\ unverifiedDelete' = (unverifiedDelete \/ dirty)
  /\ adoptedDelete' = (adoptedDelete \/ Adopted)
  /\ UNCHANGED <<ladder, pvcSize, podState, dirty, specSize, tBasis, tSize,
                 wakeReq, rebuilds, edits, askedFor>>

\* The claim is gone: back to Active, where the render makes a new one.
FinishRebuild ==
  /\ ladder = "ReprovisionDraining"
  /\ ~pvcExists
  /\ ladder' = "Active"
  /\ UNCHANGED <<pvcExists, pvcSize, podState, dirty, specSize, tBasis, tSize,
                 wakeReq, rebuilds, edits, askedFor,
                 unverifiedDelete, adoptedDelete>>

\* render::pvc — a missing claim is created at the EFFECTIVE size.
CreateClaim ==
  /\ ~pvcExists
  /\ ~IsRebuilding(ladder)
  /\ ladder # "Hibernated"
  /\ pvcExists' = TRUE
  /\ pvcSize' = Effective
  /\ UNCHANGED <<ladder, podState, dirty, specSize, tBasis, tSize, wakeReq,
                 rebuilds, edits, askedFor, unverifiedDelete,
                 adoptedDelete>>

\* A growth applies in place — Kubernetes expands, it never rebuilds.
GrowClaim ==
  /\ pvcExists
  /\ ~IsRebuilding(ladder)
  /\ Effective > pvcSize
  /\ pvcSize' = Effective
  /\ UNCHANGED <<ladder, pvcExists, podState, dirty, specSize, tBasis, tSize,
                 wakeReq, rebuilds, edits, askedFor,
                 unverifiedDelete, adoptedDelete>>

\* maybe_auto_expand: record a target for the CURRENT spec size.
AutoExpandStep ==
  /\ AutoExpand
  /\ ~IsRebuilding(ladder)
  /\ ProjectWants > Effective
  /\ tBasis' = specSize
  /\ tSize' = ProjectWants
  /\ UNCHANGED <<ladder, pvcExists, pvcSize, podState, dirty, specSize,
                 wakeReq, rebuilds, edits, askedFor,
                 unverifiedDelete, adoptedDelete>>

Next ==
  \/ Write \/ Flush \/ BeginTerminate \/ FinishDrain \/ StartPod \/ Keepalive
  \/ (\E s \in Sizes : UserResize(s))
  \/ StartRebuild \/ VerifyRebuild \/ AbortRebuild \/ DeleteClaim
  \/ FinishRebuild \/ CreateClaim \/ GrowClaim \/ AutoExpandStep

\* Fairness is PER ACTION, not on the disjunction. `WF_vars(Next)` only
\* promises that SOME step is eventually taken, which a model with an
\* always-enabled Write/Keepalive pair satisfies while starving every
\* step that makes progress — a lasso that says nothing about the code.
\* Named here are the steps the operator and kubelet actually guarantee:
\* the reconciler runs, the flusher runs, and a terminating pod exits.
Fair ==
  /\ WF_vars(Flush)
  /\ WF_vars(BeginTerminate)
  /\ WF_vars(FinishDrain)
  \* STRONG fairness, and the distinction is the product's: a share
  \* under continuous write load alternates dirty/clean, so the verify
  \* is only INFINITELY OFTEN enabled, never continuously. Weak fairness
  \* would call that a starvation lasso; the operator in fact re-polls
  \* on its own timer (REQUEUE_PROGRESS) and samples many windows, which
  \* is what SF encodes. What TLA cannot discharge is the quantitative
  \* claim that a flush window exists inside a poll interval — the same
  \* class as FlintTierEpoch's quiet-wait timing axiom. A share that is
  \* NEVER clean defers forever, on purpose, and says so (the drill's
  \* ReprovisionDeferred / HibernateDeferred arms).
  /\ SF_vars(VerifyRebuild)
  /\ WF_vars(DeleteClaim)
  /\ WF_vars(FinishRebuild)
  /\ WF_vars(CreateClaim)
  /\ WF_vars(StartPod)

Spec == Init /\ [][Next]_vars /\ Fair

(***************************************************************************)
(* The theorems.                                                           *)
(***************************************************************************)

\* The claim is never destroyed while the bucket cannot rebuild it.
Inv_NoUnverifiedDelete == ~unverifiedDelete

\* An adopted claim belongs to the user; no path may delete it.
Inv_NeverDeleteAdopted == ~adoptedDelete

\* If a rebuild ran because the user asked for something smaller, the
\* disk it settles on is not bigger than what they asked for. Violated
\* when auto-expand grows the rebuild straight back: the outage bought
\* nothing and the request was silently discarded.
Inv_ShrinkHonoured ==
  (rebuilds > 0 /\ askedFor > 0 /\ ladder = "Active" /\ pvcExists)
    => pvcSize <= askedFor

\* Vacuity probe: TLC must VIOLATE this, or no behaviour in the run
\* ever rebuilt and every rebuild theorem above passed by never firing.
NoRebuildEverHappened == rebuilds = 0

Inv == /\ ladder \in Ladders
       /\ Inv_NoUnverifiedDelete
       /\ Inv_NeverDeleteAdopted
       /\ Inv_ShrinkHonoured

\* A started rebuild finishes. With AbortOnWake, a front door keepaliving
\* on its own cadence cancels it forever — the lasso this forbids.
\* Reaching Active is NOT enough — hibernation's abort reaches Active
\* too, on the ORIGINAL oversized claim, having achieved nothing. The
\* property has to name the outcome.
RebuildCompletes ==
  (ladder = "ReprovisionVerifying")
    ~> (ladder = "Active" /\ pvcExists /\ pvcSize <= askedFor)

=============================================================================
