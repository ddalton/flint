--------------------------- MODULE FlintDelegRecall ---------------------------
(***************************************************************************)
(* NFSv4.1 READ-delegation grant/recall/revoke — model BEFORE code (the    *)
(* FlintExtents/FlintTierSession posture), the GATING step 0 of            *)
(* docs/plans/nfs-delegations-design.md (section 7).                       *)
(*                                                                         *)
(* WHY THE MODULE EXISTS: a delegation is a promise that the client may    *)
(* trust its cache WITHOUT ASKING AGAIN.  Every other piece of state the   *)
(* server has ever leaked stale was eventually corrected by the client's   *)
(* next RPC; a delegation holder's next RPC never comes — that is the      *)
(* feature.  So every hole in grant/recall/restart converts directly into  *)
(* the design's named worst case: STALE CACHE SERVED FOREVER.  The         *)
(* adversarial verification of the design found four fatal holes, all of   *)
(* them interleavings (the shape this repo's models have refuted pre-code  *)
(* three times), and this module carries each one as a mutation that the   *)
(* gate REQUIRES to fail.                                                  *)
(*                                                                         *)
(* THE WORLD: one file, one delegation-holding client, one abstract        *)
(* mutator.  The mutator stands for EVERY mutation lane at once — OPEN-    *)
(* write, REMOVE, RENAME, SETATTR, anonymous-stateid WRITE, LINK, the     *)
(* in-process file API, LAYOUTGET(RW/ANY) — because the design's fix for   *)
(* fatal hole 1 is precisely that they all share ONE protocol (the RAII    *)
(* mutation-pending guard taken under the file entry lock at consult,      *)
(* held until the mutation commits; the grant re-check refuses while any   *)
(* guard is live).  A lane that skips the protocol is not a different      *)
(* machine, it is the FenceComplete=FALSE world (the C5-drift lane).       *)
(*                                                                         *)
(* FAITHFULNESS NOTES:                                                     *)
(*   - RenewLease models the load-bearing fact that makes SEQ4 signaling   *)
(*     sound AT ALL: a Linux client renews its lease by SEQUENCE on a      *)
(*     timer even when delegations have eliminated every I/O RPC.  The     *)
(*     revoked-signal ride is that SEQUENCE.  If that assumption ever      *)
(*     breaks (a client that stops renewing), the holder is lease-expired  *)
(*     and cascade-destroyed — a different, already-modeled door           *)
(*     (FlintClientIdentity's sweep).                                      *)
(*   - The SEQ4_STATUS_RECALLABLE_STATE_REVOKED bit is not a variable of   *)
(*     its own: per the design it is computed from RETAINED REVOKED        *)
(*     records, so here signal == revTomb.  The persisted holder-evidence  *)
(*     marker re-materializes as a revoked tombstone at restart — that is  *)
(*     the design's "convert to revoked-tombstones, never erase" rule.     *)
(*   - RevokeDeadline is enabled ANY TIME a recall is outstanding: the     *)
(*     90s ladder without wall-clock.  It may revoke a slow-but-           *)
(*     cooperative client — a performance event, not a safety one, and     *)
(*     the model deliberately does not distinguish slow from dead.  The    *)
(*     deadline-from-first-transmit amendment is a TIMING rule TLC cannot  *)
(*     discharge (same species as FlintClaims' grace axiom residual) and   *)
(*     lives in code review + the conflict-matrix rig, not here.           *)
(*   - The backchannel is KILLABLE AND STAYS DEAD: ChannelDie has no       *)
(*     fairness and nothing forces Rebind.  The design doc demands         *)
(*     exactly this (a lossy-but-eventually-delivering channel would       *)
(*     assume away fatal hole 3).                                          *)
(*   - Restart is the SAME-PVC transparent restore (EXCHANGE_ID case 1):   *)
(*     the client's belief SURVIVES in its own memory, the in-flight       *)
(*     grant reply dies with the TCP connection, all in-memory server      *)
(*     delegation state is wiped, and the persisted client record makes    *)
(*     Linux treat it as session loss — NO reboot recovery, NO             *)
(*     CLAIM_PREVIOUS.  Idle-suspend + wake is the same transition and is  *)
(*     not modeled separately.  The fresh-PVC/STALE arm is out of scope    *)
(*     (rig leg d — the fh itself goes stale there and the client          *)
(*     recovers by the existing path).                                     *)
(*                                                                         *)
(* THE THEOREMS (strict run — the design doc's invariants (a) and (c)):    *)
(*   - Inv_NoAdmittedWriterUnderLiveDeleg: a mutation is never admitted    *)
(*     to execute while a live delegation exists on the file.  This is    *)
(*     the guard protocol's whole claim: every interleaving either sees    *)
(*     the writer (grant refused) or the writer's consult sees the record  *)
(*     (recall + DELAY).                                                   *)
(*   - Inv_NoUnsignalledStaleness: whenever the holder believes a          *)
(*     delegation and the file has moved past its cached content, the      *)
(*     revoked signal is UP (a retained tombstone the holder's own lease   *)
(*     renewal will fetch).  This is the stale-cache-served-forever        *)
(*     guard, and every fatal hole violates it through a different door.   *)
(*   - Inv_BelieverHasEvidence: a believing holder always has a persisted  *)
(*     evidence marker — the premise that makes the restart re-arm sound.  *)
(*   - Inv_RevokeOnlyFromRecall: no revocation the design never asked for  *)
(*     (the detached-ladder-wakeup discipline).                            *)
(*   - RecallResolves / StaleBeliefResolves (liveness run — invariant      *)
(*     (b)): every recall eventually resolves so the DELAYed op            *)
(*     unblocks, and a stale believer eventually stops believing.         *)
(*                                                                         *)
(* THE MUTATIONS (each = the fixed world minus ONE mechanism; the gate     *)
(* requires TLC to FIND the loss):                                         *)
(*   - MutationGuard=FALSE  — fatal hole 1: the lost-wakeup proof covered  *)
(*     write OPENs only; a grant lands between a lane's fence consult and  *)
(*     its execution.  (NoGuard)                                           *)
(*   - FenceComplete=FALSE  — the C5-drift lane: a mutation lane that      *)
(*     never consults the fence at all (the LINK class the verifier        *)
(*     caught missing from the site list).  (NoFence)                      *)
(*   - DisownEvidence=FALSE — fatal hole 2: CB_RECALL crosses the          *)
(*     granting OPEN reply, the client answers BAD_STATEID, and the        *)
(*     insta-drop rule orphans the delegation the client is about to       *)
(*     install.  (DisownDrop)                                              *)
(*   - PersistHolderEvidence=FALSE — fatal hole 4: the same-PVC restart    *)
(*     wipes records and seq-flags, the client's belief survives, and no   *)
(*     signal ever comes.  (NoEvidence)                                    *)
(*   - LadderRecheck=FALSE  — the detached ladder task outlives its        *)
(*     record and revokes a successor grant that was never recalled.       *)
(*     (NoRecheck)                                                         *)
(*   - RebindRearm — fatal hole 3, as an INVERSE PAIR (the                 *)
(*     Inv_RaidRecoveryUnreachable idiom: the violation is the GOOD        *)
(*     news).  RearmWorks: fixed constants, TLC must FIND delivery after   *)
(*     a die+rebind — proving rearm actually re-drives the recall.         *)
(*     RearmStale: RebindRearm=FALSE (HEAD's append-only registry +        *)
(*     .first() send), Inv_NoDeliveryAfterRebind HOLDS — after one TCP     *)
(*     reconnect no recall is ever delivered again, and every conflict     *)
(*     converts to revocation.  Safety survives (the deadline still        *)
(*     raises the signal), which is exactly why this disease ships         *)
(*     silently: it is operational rot, not a stale serve.                 *)
(*                                                                         *)
(* VACUITY PROBES (required to fail against the fixed world):              *)
(*   - Probe_DisownRaceReachable: the recall-crosses-grant-reply state is  *)
(*     actually reached — without it the DisownEvidence strict coverage    *)
(*     would be a green light over an empty road.                          *)
(*   - Probe_StaleSignalledReachable: a believing, stale, SIGNALLED        *)
(*     holder is reachable — the antecedent of the central invariant and   *)
(*     of StaleBeliefResolves is exercised.                                *)
(*                                                                         *)
(* OUT OF SCOPE (checks, not interleavings — owned by unit tests and the   *)
(* rig legs in design doc section 9): OPENMODE rejection, claim-arm        *)
(* conversion (CLAIM_DELEG_CUR_FH), self-conflict carve-out, post-recall   *)
(* cooldown, multi-holder attribute coherence, (dev,ino) vs fh/path        *)
(* keying, the stateid-counter epoch, grace gating, quotas, the circuit    *)
(* breaker, out-of-band PVC edits (no lane exists — operator contract),    *)
(* voluntary DELEGRETURN under client memory pressure (removes state,      *)
(* threatens nothing).                                                     *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
  MaxMut,                \* mutation budget (bounds ver)
  MaxGrants,             \* grant budget (regrant needed by NoRecheck)
  MaxRestarts,           \* same-PVC restart budget
  MaxDies,               \* backchannel death budget
  MutationGuard,         \* TRUE = RAII mutation-pending guard (hole 1 fix)
  FenceComplete,         \* TRUE = every mutation lane consults the fence
  DisownEvidence,        \* TRUE = BAD_STATEID to CB_RECALL never insta-drops
  RebindRearm,           \* TRUE = a rebound backchannel re-drives recalls
  PersistHolderEvidence, \* TRUE = grant persists the holder-evidence marker
  LadderRecheck          \* TRUE = ladder wakeups re-check under entry lock

VARIABLES
  \* ── server, in-memory (dies with the process) ──
  rec,        \* live delegation record: "none"/"granted"/"pending"/"acked"
  revTomb,    \* retained revoked record — IS the SEQ4 signal to the holder
  guard,      \* a mutation lane's RAII guard is held (consulted, executing)
  taskArmed,  \* a recall ladder task exists (detached-task ghost)
  replyQ,     \* the granting OPEN reply is in flight on the fore channel
  ch,         \* backchannel: "live" / "dead" (dead STAYS dead unless rebound)
  everRebound,\* the channel has died and been rebound at least once
  \* ── server, persisted (survives the same-PVC restart) ──
  marker,     \* holder-evidence row: this client holds recallable state
  ver,        \* file content version (the persisted bytes)
  \* ── client memory (survives the restart — that is fatal hole 4) ──
  believes,   \* the client believes it holds a live read delegation
  cVer,       \* content version its cache captured at install
  \* ── budgets ──
  muts, grants, restarts, dies,
  \* ── history witnesses ──
  disowned,             \* the client has disowned a recall (probe)
  deliveredAfterRebind, \* a recall was delivered after die+rebind (inverse)
  revokeUnrecalled      \* a revoke hit a record that was never recalled

vars == <<rec, revTomb, guard, taskArmed, replyQ, ch, everRebound, marker,
          ver, believes, cVer, muts, grants, restarts, dies, disowned,
          deliveredAfterRebind, revokeUnrecalled>>

TypeOK ==
  /\ rec \in {"none", "granted", "pending", "acked"}
  /\ revTomb \in BOOLEAN /\ guard \in BOOLEAN /\ taskArmed \in BOOLEAN
  /\ replyQ \in BOOLEAN /\ ch \in {"live", "dead"}
  /\ everRebound \in BOOLEAN /\ marker \in BOOLEAN
  /\ ver \in 0..MaxMut /\ believes \in BOOLEAN /\ cVer \in 0..MaxMut
  /\ cVer <= ver
  /\ muts \in 0..MaxMut /\ grants \in 0..MaxGrants
  /\ restarts \in 0..MaxRestarts /\ dies \in 0..MaxDies
  /\ disowned \in BOOLEAN /\ deliveredAfterRebind \in BOOLEAN
  /\ revokeUnrecalled \in BOOLEAN

Init ==
  /\ rec = "none" /\ revTomb = FALSE /\ guard = FALSE /\ taskArmed = FALSE
  /\ replyQ = FALSE /\ ch = "live" /\ everRebound = FALSE /\ marker = FALSE
  /\ ver = 0 /\ believes = FALSE /\ cVer = 0
  /\ muts = 0 /\ grants = 0 /\ restarts = 0 /\ dies = 0
  /\ disowned = FALSE /\ deliveredAfterRebind = FALSE
  /\ revokeUnrecalled = FALSE

(***************************************************************************)
(* GRANT.  One atomic step = the under-entry-lock re-check + insert +      *)
(* queue the OPEN reply + persist the evidence marker.  Grant-gate rules   *)
(* modeled: no live record, no retained tombstone (rule 8), requester      *)
(* does not already hold (dup / one outstanding open), callback_ready      *)
(* (rule 7: the channel is live NOW — the ladder owns a channel that       *)
(* later lies), and — THE HOLE-1 FIX — no live mutation guard.  With       *)
(* MutationGuard=FALSE that last conjunct evaporates and the grant lands   *)
(* inside another lane's consult-to-commit window.                         *)
(***************************************************************************)
Grant ==
  /\ grants < MaxGrants
  /\ rec = "none" /\ ~revTomb /\ ~replyQ /\ ~believes
  /\ ch = "live"
  /\ (MutationGuard => ~guard)
  /\ rec' = "granted" /\ replyQ' = TRUE /\ marker' = TRUE
  /\ grants' = grants + 1
  /\ UNCHANGED <<revTomb, guard, taskArmed, ch, everRebound, ver, believes,
                 cVer, muts, restarts, dies, disowned, deliveredAfterRebind,
                 revokeUnrecalled>>

(* The client processes the granting OPEN reply and installs the           *)
(* delegation.  Its cache captures the content as of NOW — from here on    *)
(* it will not ask again.  Note this fires whether or not the server       *)
(* still has the record: an orphaned install is fatal hole 2's payload.   *)
InstallGrant ==
  /\ replyQ
  /\ believes' = TRUE /\ cVer' = ver /\ replyQ' = FALSE
  /\ UNCHANGED <<rec, revTomb, guard, taskArmed, ch, everRebound, marker,
                 ver, muts, grants, restarts, dies, disowned,
                 deliveredAfterRebind, revokeUnrecalled>>

(***************************************************************************)
(* THE MUTATOR.  Consult and execute are SEPARATE steps — the window       *)
(* between them is where fatal hole 1 lives.  A consult that finds a live  *)
(* delegation starts the recall and answers NFS4ERR_DELAY (the mutator     *)
(* holds nothing and retries later — the repo's proven tier pattern); a    *)
(* consult that finds the file clear takes the RAII guard and the lane     *)
(* executes.  The retained tombstone does NOT block mutation (the barrier  *)
(* lifts at revocation; only new GRANTS are refused until FREE_STATEID).   *)
(***************************************************************************)
MutConsultClear ==
  /\ muts < MaxMut /\ ~guard
  /\ rec = "none"
  /\ guard' = TRUE
  /\ UNCHANGED <<rec, revTomb, taskArmed, replyQ, ch, everRebound, marker,
                 ver, believes, cVer, muts, grants, restarts, dies, disowned,
                 deliveredAfterRebind, revokeUnrecalled>>

MutConsultConflict ==
  /\ muts < MaxMut
  /\ rec = "granted"
  /\ rec' = "pending" /\ taskArmed' = TRUE
  /\ UNCHANGED <<revTomb, guard, replyQ, ch, everRebound, marker, ver,
                 believes, cVer, muts, grants, restarts, dies, disowned,
                 deliveredAfterRebind, revokeUnrecalled>>

MutExec ==
  /\ guard
  /\ ver' = ver + 1 /\ muts' = muts + 1 /\ guard' = FALSE
  /\ UNCHANGED <<rec, revTomb, taskArmed, replyQ, ch, everRebound, marker,
                 believes, cVer, grants, restarts, dies, disowned,
                 deliveredAfterRebind, revokeUnrecalled>>

(* The lane the fence inventory missed (LINK, a future bump site added     *)
(* without a consult — the C5 drift).  Exists only in the mutated world;   *)
(* the executable completeness check in code is what keeps it impossible.  *)
MutBypass ==
  /\ ~FenceComplete
  /\ muts < MaxMut
  /\ ver' = ver + 1 /\ muts' = muts + 1
  /\ UNCHANGED <<rec, revTomb, guard, taskArmed, replyQ, ch, everRebound,
                 marker, believes, cVer, grants, restarts, dies, disowned,
                 deliveredAfterRebind, revokeUnrecalled>>

(***************************************************************************)
(* RECALL DELIVERY.  One step = CB_RECALL transmitted on a live channel    *)
(* and processed by the client.  The (RebindRearm \/ ~everRebound) guard   *)
(* is the callback registry's health: with rearm, a rebound channel        *)
(* carries recalls again; without it (HEAD: bind_back_channel only         *)
(* appends, sends take .first()), the first reconnect leaves every later   *)
(* send addressed to a dead writer — delivery never happens again.        *)
(***************************************************************************)
DeliverAck ==
  /\ rec = "pending" /\ ch = "live" /\ (RebindRearm \/ ~everRebound)
  /\ believes
  /\ rec' = "acked"
  /\ deliveredAfterRebind' = (deliveredAfterRebind \/ everRebound)
  /\ UNCHANGED <<revTomb, guard, taskArmed, replyQ, ch, everRebound, marker,
                 ver, believes, cVer, muts, grants, restarts, dies, disowned,
                 revokeUnrecalled>>

(* The crossing: the recall arrives while the granting OPEN reply is       *)
(* still in flight.  The client, not yet holding, answers BAD_STATEID.     *)
(* With evidence, the server keeps the record (the pending state stands;   *)
(* the ladder re-delivers after the install and the writer stays DELAYed   *)
(* — convergence, not loss).  Without it, the insta-drop orphans the      *)
(* delegation the client is about to install: fatal hole 2.               *)
DeliverDisown ==
  /\ rec = "pending" /\ ch = "live" /\ (RebindRearm \/ ~everRebound)
  /\ ~believes /\ replyQ
  /\ disowned' = TRUE
  /\ deliveredAfterRebind' = (deliveredAfterRebind \/ everRebound)
  /\ IF DisownEvidence
       THEN UNCHANGED <<rec, marker>>
       ELSE rec' = "none" /\ marker' = FALSE
  /\ UNCHANGED <<revTomb, guard, taskArmed, replyQ, believes, cVer, ver, ch,
                 everRebound, muts, grants, restarts, dies, revokeUnrecalled>>

(* The cooperative return: DELEGRETURN validates, the record and the       *)
(* evidence marker go together, the barrier lifts.                         *)
DelegReturn ==
  /\ rec = "acked"
  /\ rec' = "none" /\ believes' = FALSE /\ marker' = FALSE
  /\ UNCHANGED <<revTomb, guard, taskArmed, replyQ, ch, everRebound, ver,
                 cVer, muts, grants, restarts, dies, disowned,
                 deliveredAfterRebind, revokeUnrecalled>>

(* The ladder deadline: any outstanding recall may be revoked (time        *)
(* abstracted away — see the faithfulness note).  Revocation moves the     *)
(* record from the live set to the RETAINED tombstone: the barrier lifts   *)
(* for the writer, and the tombstone IS the SEQ4 bit the holder's next     *)
(* lease renewal will fetch.  Silent removal is the disease; this action   *)
(* is the honest version.                                                  *)
RevokeDeadline ==
  /\ rec \in {"pending", "acked"}
  /\ rec' = "none" /\ revTomb' = TRUE
  /\ UNCHANGED <<guard, taskArmed, replyQ, ch, everRebound, marker, ver,
                 believes, cVer, muts, grants, restarts, dies, disowned,
                 deliveredAfterRebind, revokeUnrecalled>>

(* The detached ladder task fires after its record is gone and a NEW       *)
(* grant occupies the slot.  With LadderRecheck the wakeup re-acquires     *)
(* the entry lock and no-ops on record-gone/state-changed (so the action   *)
(* does not exist); without it the successor delegation is revoked         *)
(* without ever having been recalled.                                      *)
TaskFire ==
  /\ ~LadderRecheck
  /\ taskArmed /\ rec = "granted"
  /\ rec' = "none" /\ revTomb' = TRUE /\ revokeUnrecalled' = TRUE
  /\ UNCHANGED <<guard, taskArmed, replyQ, ch, everRebound, marker, ver,
                 believes, cVer, muts, grants, restarts, dies, disowned,
                 deliveredAfterRebind>>

(***************************************************************************)
(* LEASE RENEWAL.  The holder's SEQUENCE arrives (it always eventually     *)
(* does — the faithfulness note); the retained tombstone puts the revoked  *)
(* bit in the reply, the client stops trusting its cache and revalidates.  *)
(* The FREE_STATEID housekeeping retires the tombstone and the evidence    *)
(* marker — but only once no grant reply is still in flight toward the     *)
(* client: a stateid the client has not yet seen cannot be freed, so the   *)
(* bit stays up through the install window (which is what makes a         *)
(* revoke-crosses-install interleaving safe).                              *)
(***************************************************************************)
RenewConsume ==
  /\ revTomb
  /\ believes' = FALSE
  /\ IF replyQ
       THEN UNCHANGED <<revTomb, marker>>
       ELSE revTomb' = FALSE /\ marker' = FALSE
  /\ UNCHANGED <<rec, guard, taskArmed, replyQ, ch, everRebound, ver, cVer,
                 muts, grants, restarts, dies, disowned,
                 deliveredAfterRebind, revokeUnrecalled>>

(* ── environment ─────────────────────────────────────────────────────── *)

ChannelDie ==
  /\ dies < MaxDies /\ ch = "live"
  /\ ch' = "dead" /\ dies' = dies + 1
  /\ UNCHANGED <<rec, revTomb, guard, taskArmed, replyQ, everRebound,
                 marker, ver, believes, cVer, muts, grants, restarts,
                 disowned, deliveredAfterRebind, revokeUnrecalled>>

Rebind ==
  /\ ch = "dead"
  /\ ch' = "live" /\ everRebound' = TRUE
  /\ UNCHANGED <<rec, revTomb, guard, taskArmed, replyQ, marker, ver,
                 believes, cVer, muts, grants, restarts, dies, disowned,
                 deliveredAfterRebind, revokeUnrecalled>>

(***************************************************************************)
(* THE SAME-PVC RESTART (pod roll, upgrade, node drain, idle-suspend +     *)
(* wake, the documented kill switch itself).  In-memory server state       *)
(* dies: live records, guards, ladder tasks, the callback registry, the    *)
(* in-flight OPEN reply (its TCP connection).  Persisted state survives:   *)
(* the file bytes, the client record (which is exactly why the client      *)
(* sees session loss, not a reboot — its BELIEF survives too), and the     *)
(* holder-evidence marker.  With the evidence fix the marker               *)
(* re-materializes as a revoked tombstone, so the survivor's first lease   *)
(* renewal carries the bit.  Without it the server forgets the holder      *)
(* completely while the holder keeps believing: fatal hole 4.              *)
(***************************************************************************)
Restart ==
  /\ restarts < MaxRestarts
  /\ restarts' = restarts + 1
  /\ rec' = "none"
  /\ revTomb' = (PersistHolderEvidence /\ marker)
  /\ guard' = FALSE /\ taskArmed' = FALSE /\ replyQ' = FALSE
  /\ ch' = "live" /\ everRebound' = FALSE
  /\ UNCHANGED <<marker, ver, believes, cVer, muts, grants, dies, disowned,
                 deliveredAfterRebind, revokeUnrecalled>>

Next ==
  \/ Grant \/ InstallGrant
  \/ MutConsultClear \/ MutConsultConflict \/ MutExec \/ MutBypass
  \/ DeliverAck \/ DeliverDisown \/ DelegReturn
  \/ RevokeDeadline \/ TaskFire \/ RenewConsume
  \/ ChannelDie \/ Rebind \/ Restart

Spec == Init /\ [][Next]_vars

(* Liveness world: the deadline always eventually fires on an outstanding  *)
(* recall (the ladder is a timer), the holder always eventually renews     *)
(* its lease, and a queued grant reply is always eventually processed.     *)
(* Deliberately NO fairness on DelegReturn (the client may never           *)
(* cooperate), ChannelDie/Rebind (dead stays dead), or Restart.            *)
SpecLive ==
  /\ Spec
  /\ WF_vars(RevokeDeadline)
  /\ WF_vars(RenewConsume)
  /\ WF_vars(InstallGrant)

(* ── the theorems ────────────────────────────────────────────────────── *)

(* Design invariant (a): no admitted writer under a live delegation.  The  *)
(* guard is only ever taken on a clear consult, and the grant refuses      *)
(* while it is held — so their coexistence is a lost wakeup.               *)
Inv_NoAdmittedWriterUnderLiveDeleg == ~(guard /\ rec # "none")

(* Design invariant (c), the heart of the module: a believing holder whose *)
(* cache is stale ALWAYS has the revoked signal up.  Its own lease         *)
(* renewal is then guaranteed to deliver the news; "believes, stale, and   *)
(* nothing will ever tell it" is stale-cache-served-forever.               *)
Inv_NoUnsignalledStaleness == (believes /\ cVer < ver) => revTomb

(* The restart re-arm's premise: every believer left a durable trace.      *)
Inv_BelieverHasEvidence == believes => marker

(* No revocation the design never asked for.                               *)
Inv_RevokeOnlyFromRecall == ~revokeUnrecalled

Inv ==
  /\ TypeOK
  /\ Inv_NoAdmittedWriterUnderLiveDeleg
  /\ Inv_NoUnsignalledStaleness
  /\ Inv_BelieverHasEvidence
  /\ Inv_RevokeOnlyFromRecall

(* Inverse-idiom invariant (violation = good news), for the rearm pair.    *)
Inv_NoDeliveryAfterRebind == ~deliveredAfterRebind

(* Vacuity probes — the gate REQUIRES TLC to refute these.                 *)
Probe_DisownRaceReachable == ~disowned
Probe_StaleSignalledReachable == ~(believes /\ cVer < ver /\ revTomb)

(* Design invariant (b): the recall barrier always lifts, so the DELAYed   *)
(* conflictor's retry eventually proceeds; and a stale believer            *)
(* eventually stops believing (the signal is not just up but CONSUMED).    *)
RecallResolves == (rec \in {"pending", "acked"}) ~> (rec = "none")
StaleBeliefResolves == (believes /\ cVer < ver) ~> ~believes

===============================================================================
