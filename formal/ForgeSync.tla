------------------------------ MODULE ForgeSync ------------------------------
(***************************************************************************)
(* flint forge's push path: hook -> batch -> pack upload -> ONE snapshot   *)
(* CAS -> ref update -> acknowledgement, with a crash at every step; the   *)
(* lease's progress-gated renewer beside it; a challenger that counts     *)
(* quiet polls; and a successor's claim, rotation, sweep and restore.      *)
(* Modeled AFTER the code (forge/syncer/src/{batch,lease,server,snapshot,  *)
(* restore,sweep,packio,gitcmd}.rs) and AFTER the runbw/runbx drills       *)
(* (forge/e2e/scale/README.md): the drills sampled interleavings, this     *)
(* module enumerates them, and its mutations are the defects the drills    *)
(* and the simplification exploration found, kept as regression tests.     *)
(*                                                                         *)
(* The facts encoded, each from the implementation:                        *)
(*                                                                         *)
(*   - The lease cell is FlintTierEpoch's: acquire If-None-Match:* or      *)
(*     If-Match(observed token) with epoch+1; renew If-Match(own token)    *)
(*     rotates the token; a challenger judges a holder dead only by an     *)
(*     UNCHANGED token across QUIET_POLLS consecutive polls; a 412 on any  *)
(*     CAS of ours is the fence, and the fence stops reads too.            *)
(*   - The renewer is its own task (lease.rs spawn_renewer): it renews     *)
(*     unconditionally while serving and ONLY IF THE PROGRESS COUNTER      *)
(*     MOVED since its last renewal while importing or pushing.  The       *)
(*     counter is a SENSOR: batch steps tick it; a step that does work     *)
(*     and does not tick makes a moving holder look wedged (run 3: the     *)
(*     checksum pass over a 40 GiB pack, ~70 s, ticked nothing).          *)
(*   - A batch (batch.rs run_batch): judge under the agreed view, renew    *)
(*     once, upload every local pack the snapshot does not name (content-  *)
(*     named keys, unconditional, multipart above the whole-PUT ceiling), *)
(*     ONE snapshot CAS on the etag last seen, update-ref, THEN the        *)
(*     report to the hook.  Any error before the report is ng for every   *)
(*     push and the process exits; a 412 fences.                          *)
(*   - git migrates a push's quarantine .keep, .pack, .rev, .idx in that   *)
(*     order (tmp-objdir.c pack_copy_priority), so a neighbour's pack is  *)
(*     on disk before its index for a moment; the listing that feeds the  *)
(*     upload and the snapshot's pack list either requires the index      *)
(*     (IdxGate, the X1 fix) or does not.                                 *)
(*   - Every claim but a released cell's rotates the snapshot (same       *)
(*     content, new etag) BEFORE the successor restores, so a straggler's  *)
(*     If-Match goes stale before the successor serves — on self-         *)
(*     recognition too, since the incarnation that died may have been a   *)
(*     successor that had not rotated yet (the second counterexample) —  *)
(*     and                                                                *)
(*     CREATES the empty snapshot when none exists, for the same reason   *)
(*     (this module's first strict counterexample: the skipped rotation   *)
(*     let a straggler's If-None-Match:* create land after the successor  *)
(*     served, fencing the successor with its predecessor's push).        *)
(*   - The restore fetches the packs the snapshot names, installs the      *)
(*     snapshot's refs EXACTLY (deleting any other), and refuses (exit 78) *)
(*     when a named pack is absent or a ref's objects are in no pack git   *)
(*     can see — a pack without its index is invisible to git.            *)
(*   - The client may hang up at any time before the report; the syncer   *)
(*     never learns it, so the batch lands anyway ("told failed but        *)
(*     durable", run 3 finding 3): a PROBE here, not a theorem.           *)
(*                                                                         *)
(* FAITHFULNESS NOTES:                                                     *)
(*   - Read-then-CAS pairs collapse to one action where the CAS            *)
(*     revalidates the read (acquire, renew, the snapshot CAS).  The quiet *)
(*     count is multi-round and stays decomposed.                          *)
(*   - THE SCHEDULING AXIOM (PollsNoFasterThanHeartbeat): a challenger's   *)
(*     poll of a LIVE holder is preceded by that holder's heartbeat since  *)
(*     the challenger's previous poll — the two run at the same period.   *)
(*     A dead holder (idle) has no heartbeat to wait for.  This is the     *)
(*     quantitative content ("six quiet polls = one minute") TLA cannot    *)
(*     discharge; what the module proves under it is that the SENSOR is   *)
(*     honest — the renewer never skips while the holder moved — and the  *)
(*     CAS does the rest (a renewing holder is undeposable, structurally). *)
(*   - Oids are push ids: push p creates commit p in pack p.  History is   *)
(*     linear and every accepted push is a fast-forward; a ref's objects   *)
(*     are "in pack p" iff the ref is p.                                   *)
(*   - Multipart: parts are invisible until Complete; Complete is NOT      *)
(*     conditional; the claim-time sweep aborts every in-flight upload and *)
(*     a swept upload's Complete fails, which ends the straggler's process. *)
(*     The sweep is hygiene, not integrity: a straggler's pack that DOES   *)
(*     complete is content-named and unnamed by any snapshot, so no       *)
(*     mutation run exists for it (a mutation that cannot lose proves      *)
(*     nothing).                                                           *)
(*                                                                         *)
(* THE THEOREMS (strict run):                                              *)
(*   - Inv_AckedIsDurable: a push told ok has its ref landed in the        *)
(*     snapshot and its pack complete (with index) in the bucket.          *)
(*     Mutation EarlyAck (AckAfterCas=FALSE, option B1 of the              *)
(*     simplification note) must lose it.                                  *)
(*   - Inv_LandedPackComplete: every landed push's pack is in the bucket   *)
(*     WITH its index.  Mutation NoIdxGate (X1: a pack listed before its   *)
(*     index lands is named and its index never uploaded) must lose it;    *)
(*     mutation CasBeforePacks (the §4 ordering reversed) must lose it.    *)
(*   - Inv_NoSkipOverMovement: the renewer never skips a heartbeat while   *)
(*     the holder took a step since the last one — the sensor is honest.  *)
(*     Mutation NoTickOnHash (run 3 finding 1) must lose it.               *)
(*   - Inv_NoStragglerLandAfterRestore: no CAS lands from a deposed        *)
(*     syncer after its successor restored (the successor would be fenced *)
(*     by its own predecessor).  Mutation NoRotate must lose it.           *)
(*   - Inv_NoUnrestorable: a restore never refuses — the bucket is always  *)
(*     restorable.                                                         *)
(* THE PROBE (required-fail against the shipped protocol):                 *)
(*   - Inv_NoToldFailedButDurable: a push whose client gave up lands       *)
(*     anyway.  Reachable by design; the retry converges.                  *)
(*                                                                         *)
(* COMPACTION TIERS (X18, fold.rs; docs/plans/forge-compaction-tiers-      *)
(* design.md §5.3).  A fold breaks the push = pack identification: a pack  *)
(* now HOLDS a set of pushes (`holds`), a push pack its own push, a fold   *)
(* pack the union of its inputs'.  The fold's task runs BESIDE the loop    *)
(* (plan, initiate, complete — through `uploads`, so the claim-time sweep  *)
(* ends a straggler's fold with NoSuchUpload, which clears the fold and    *)
(* does not fall the process); its COMMIT is on the loop between batches: *)
(* ONE CAS on the loop's current belief naming (belief.packs \ S) ∪ {f},  *)
(* never the directory.  A fold's completion ticks its OWN counter, not   *)
(* the hold's.  With a fold the sweep that DELETES becomes load-bearing:  *)
(* `SweepDelete` takes an object no snapshot names, never while this      *)
(* holder's own fold is uploaded and uncommitted, never mid-batch, and    *)
(* never one still inside the GRACE.  THE GRACE IS AN AXIOM, the second   *)
(* in this module (GraceOutlivesUpload): `orphan_grace_secs` must outlive *)
(* the LONGEST upload, not the longest plausible one — lean's             *)
(* `LeanChunkGCRacyGrace` rule — so an object another incarnation's       *)
(* in-flight batch or fold uploaded and has not yet named is not yet      *)
(* deletable.  This module's first fold run found exactly that with the   *)
(* age abstracted away, which the design's §5.3 had proposed: a deposed   *)
(* holder, still serving on a belief the successor's rotation happened to *)
(* match, swept the successor's just-uploaded pack between its Complete   *)
(* and its CAS.  The etag check alone does NOT cover it, and `RacyGrace`  *)
(* is now a mutation.  A rebuild over ONE  *)
(* named pack is allowed (the base rebuild's case), which is what lets two *)
(* pushes reach every ordering below.                                      *)
(*   - Inv_NamedIsUploaded: every pack the snapshot names is in the bucket *)
(*     with its index — true of the shipped protocol (a CAS names only     *)
(*     what it uploaded or a prior CAS named) and what the fold's formula  *)
(*     preserves.  Mutations FoldCasBeforeUpload (the commit before the    *)
(*     upload), FoldCasFromDisk (the commit names localPacks \ S ∪ {f}:   *)
(*     a pack landed since the last batch is named without an upload) and *)
(*     SweepDuringFold (the holder's sweep takes its own uploaded-         *)
(*     uncommitted fold pack) must lose it.                                *)
(*   - Inv_AckedIsDurable, restated over `holds`.  Mutation                *)
(*     FoldInputsAfterStart (the inputs unnamed are read at the commit,    *)
(*     the fold's contents were fixed at the plan: a push named in between *)
(*     is unnamed and held by nothing) must lose it.                       *)
(*   - Inv_NoUnrestorable / Inv_NamedIsUploaded against FoldCommitMidBatch *)
(*     (the commit beside a batch, which the loop makes impossible: the    *)
(*     batch's earlier listing re-names S and omits f, and a sweep in the  *)
(*     window between the two CASes has taken S).                          *)
(*   - Inv_NoRenewOverWedge: the renewer never renews a must-progress     *)
(*     phase on a sensor tick with no real movement behind it — the twin   *)
(*     of Inv_NoSkipOverMovement.  Mutation FoldTicksBatchSensor (the fold *)
(*     ticks the hold's counter) must lose it.                             *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
  Syncers,      \* incarnations with distinct holder ids, e.g. {s1, s2}
  Pushes,       \* push ids = commit ids = pack ids, e.g. {p1, p2}
  NoSyncer,     \* the holder of a cell nobody holds; the target of a push not yet sent
  Misses,       \* QUIET_POLLS
  MaxCrashes,
  MaxRenews,    \* heartbeat budget (bounds tokens)
  MaxClaims,
  IdxGate,          \* TRUE: local_packs lists a pack only with its .idx (X1)
  TickOnHash,       \* TRUE: the checksum pass ticks progress (4d66c48a)
  RotateOnTakeover, \* TRUE: takeover rotates the snapshot before restoring
  AckAfterCas,      \* TRUE: the hook is answered after CAS + update-ref
  PacksBeforeCas,   \* TRUE: packs are uploaded before the CAS names them
  SweepAtClaim,     \* TRUE: a claim aborts every in-flight upload
  PollsNoFasterThanHeartbeat, \* the scheduling axiom (see the header)
  \* ── compaction tiers (X18) ──
  FoldIds,          \* fold pack ids, e.g. {f1}; distinct from Pushes
  MaxFolds,         \* folds planned per run (bounds FoldIds' use)
  FoldCasBeforeUpload,  \* mutation: the commit is enabled before the upload
  FoldCasFromDisk,      \* mutation: the commit names localPacks \ S ∪ {f}
  FoldInputsAfterStart, \* mutation: the packs unnamed are read at the commit
  FoldCommitMidBatch,   \* mutation: the commit is enabled beside a batch (and the sweep with it)
  SweepDuringFold,      \* mutation: the sweep runs with a fold uploaded and uncommitted
  FoldTicksBatchSensor, \* mutation: the fold's completion ticks the hold's counter
  FoldNoRenew,          \* mutation: the fold's commit does not renew the lease first
  GraceOutlivesUpload   \* the grace axiom; FALSE is lean's RacyGrace mutation

Stages == {"none", "judged", "renewed", "hashed", "initiated", "uploaded",
           "cas", "refs"}
States == {"idle", "watching", "claimed", "rotating", "restoring",
           "serving", "pushing"}
PushStates == {"new", "sent", "acked", "failed"}
FoldStages == {"none", "planned", "initiated", "uploaded", "renewed"}

VARIABLES
  \* ── the bucket ──
  cell,        \* [held, holder, ep, tok, released] — the lease cell
  nextTok,     \* one generator for tokens and etags
  snap,        \* [etag, main, packs, history]; etag 0 = absent, main 0 = none
  packObj,     \* pack ids whose .pack is in the bucket
  idxObj,      \* pack ids whose .idx is in the bucket
  uploads,     \* in-flight multipart uploads: {[s, p, ep]}
  \* ── per-syncer (the pod's emptyDir survives a container restart) ──
  st,          \* [Syncers -> States]
  lease,       \* [Syncers -> [ep, tok]]
  lastTok,     \* [Syncers -> Nat]      claim-loop last observed token
  quiet,       \* [Syncers -> 0..Misses]
  belief,      \* [Syncers -> [etag, main, packs]] the cached snapshot
  localMain,   \* [Syncers -> Nat]      the local ref (0 = none)
  localPacks,  \* [Syncers -> SUBSET Pushes]  packs on disk WITH index
  migrating,   \* [Syncers -> SUBSET Pushes]  packs on disk, index pending
  batch,       \* [Syncers -> [push, stage, listed]]
  sensorMoved, \* [Syncers -> BOOLEAN] progress ticked since the last heartbeat
  realMoved,   \* [Syncers -> BOOLEAN] a step taken since the last heartbeat
  hbDue,       \* [Syncers -> BOOLEAN] a challenger polled since the last heartbeat
  \* ── the client ──
  pushState,   \* [Pushes -> PushStates]
  pushTo,      \* [Pushes -> Syncers]
  \* ── compaction tiers ──
  holds,       \* [PackIds -> SUBSET Pushes]  what each pack in the bucket holds
  fold,        \* [Syncers -> [id, inputs, stage]]  the fold beside the loop, at most one
  foldBudget,
  \* ── budgets and witnesses ──
  crashes, renewBudget, claimBudget,
  ackNotDurable, skipOverMovement,
  stragglerLand, unrestorable, toldFailedButDurable, renewOverWedge

vars == <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease, lastTok,
          quiet, belief, localMain, localPacks, migrating, batch, sensorMoved,
          realMoved, hbDue, pushState, pushTo, holds, fold, foldBudget,
          crashes, renewBudget, claimBudget, ackNotDurable, skipOverMovement,
          stragglerLand, unrestorable, toldFailedButDurable, renewOverWedge>>

NoBatch   == [push |-> 0, stage |-> "none", listed |-> {}]
ZeroLease == [ep |-> 0, tok |-> 0]
NoBelief  == [etag |-> 0, main |-> 0, packs |-> {}]
NoFold    == [id |-> 0, inputs |-> {}, stage |-> "none"]

PushIds == Pushes \cup {0}
PackIds == Pushes \cup FoldIds

TypeOK ==
  /\ cell \in [held: BOOLEAN, holder: Syncers \cup {NoSyncer}, ep: Nat, tok: Nat, released: BOOLEAN]
  /\ nextTok \in Nat
  /\ snap \in [etag: Nat, main: PushIds, packs: SUBSET PackIds, history: SUBSET Pushes]
  /\ packObj \subseteq PackIds /\ idxObj \subseteq PackIds
  /\ uploads \subseteq [s: Syncers, p: PackIds, ep: Nat]
  /\ st \in [Syncers -> States]
  /\ lease \in [Syncers -> [ep: Nat, tok: Nat]]
  /\ lastTok \in [Syncers -> Nat]
  /\ quiet \in [Syncers -> 0..Misses]
  /\ belief \in [Syncers -> [etag: Nat, main: PushIds, packs: SUBSET PackIds]]
  /\ localMain \in [Syncers -> PushIds]
  /\ localPacks \in [Syncers -> SUBSET PackIds]
  /\ migrating \in [Syncers -> SUBSET Pushes]
  /\ batch \in [Syncers -> [push: PushIds, stage: Stages, listed: SUBSET PackIds]]
  /\ holds \in [PackIds -> SUBSET Pushes]
  /\ fold \in [Syncers -> [id: FoldIds \cup {0}, inputs: SUBSET PackIds, stage: FoldStages]]
  /\ foldBudget \in 0..MaxFolds
  /\ renewOverWedge \in BOOLEAN
  /\ sensorMoved \in [Syncers -> BOOLEAN] /\ realMoved \in [Syncers -> BOOLEAN]
  /\ hbDue \in [Syncers -> BOOLEAN]
  /\ pushState \in [Pushes -> PushStates] /\ pushTo \in [Pushes -> Syncers \cup {NoSyncer}]
  /\ crashes \in 0..MaxCrashes /\ renewBudget \in 0..MaxRenews
  /\ claimBudget \in 0..MaxClaims

Init ==
  /\ cell = [held |-> FALSE, holder |-> NoSyncer, ep |-> 0, tok |-> 0, released |-> FALSE]
  /\ nextTok = 1
  /\ snap = [etag |-> 0, main |-> 0, packs |-> {}, history |-> {}]
  /\ packObj = {} /\ idxObj = {} /\ uploads = {}
  /\ st = [s \in Syncers |-> "idle"]
  /\ lease = [s \in Syncers |-> ZeroLease]
  /\ lastTok = [s \in Syncers |-> 0]
  /\ quiet = [s \in Syncers |-> 0]
  /\ belief = [s \in Syncers |-> NoBelief]
  /\ localMain = [s \in Syncers |-> 0]
  /\ localPacks = [s \in Syncers |-> {}]
  /\ migrating = [s \in Syncers |-> {}]
  /\ batch = [s \in Syncers |-> NoBatch]
  /\ sensorMoved = [s \in Syncers |-> FALSE]
  /\ realMoved = [s \in Syncers |-> FALSE]
  /\ hbDue = [s \in Syncers |-> FALSE]
  /\ pushState = [p \in Pushes |-> "new"]
  /\ pushTo = [p \in Pushes |-> NoSyncer]
  /\ holds = [q \in PackIds |-> IF q \in Pushes THEN {q} ELSE {}]
  /\ fold = [s \in Syncers |-> NoFold]
  /\ foldBudget = MaxFolds
  /\ crashes = 0 /\ renewBudget = MaxRenews /\ claimBudget = MaxClaims
  /\ ackNotDurable = FALSE /\ skipOverMovement = FALSE
  /\ stragglerLand = FALSE
  /\ unrestorable = FALSE /\ toldFailedButDurable = FALSE
  /\ renewOverWedge = FALSE

Witnesses == <<ackNotDurable, skipOverMovement,
               stragglerLand, unrestorable, toldFailedButDurable, renewOverWedge>>
\* What no action but the plan changes.
FoldPlanVars == <<holds, foldBudget>>
Bucket    == <<cell, nextTok, snap, packObj, idxObj, uploads>>
Client    == <<pushState, pushTo>>
Budgets   == <<crashes, renewBudget, claimBudget>>
Sensors   == <<sensorMoved, realMoved, hbDue>>
Local     == <<belief, localMain, localPacks, migrating, batch>>
Watch     == <<lastTok, quiet>>

\* The pushes a syncer's in-flight batch would take down with it.
BatchPush(s) == batch[s].push

\* A syncer stops believing: the fence, a crash, an exit. EVERY push
\* waiting on it is told ng unless it was already answered — the batch's
\* and the queued alike: each hook holds a socket into the process, and
\* the process's exit closes them all (server.rs: the SIGTERM arm returns
\* between batches with the queue dropped; a crash drops it too). The
\* liveness run's first execution had only the batch's push failing here,
\* and found a queued push left "sent" forever by a clean release.
Fall(s) ==
  /\ st' = [st EXCEPT ![s] = "idle"]
  /\ lease' = [lease EXCEPT ![s] = ZeroLease]
  /\ batch' = [batch EXCEPT ![s] = NoBatch]
  /\ fold' = [fold EXCEPT ![s] = NoFold]
  /\ sensorMoved' = [sensorMoved EXCEPT ![s] = FALSE]
  /\ realMoved' = [realMoved EXCEPT ![s] = FALSE]
  /\ quiet' = [quiet EXCEPT ![s] = 0]
  /\ pushState' = [p \in Pushes |->
                     IF pushTo[p] = s /\ pushState[p] = "sent"
                       THEN "failed" ELSE pushState[p]]

(***************************************************************************)
(* The claim (lease.rs claim_step).                                        *)
(***************************************************************************)

AcquireCreate(s) ==
  /\ st[s] = "idle" /\ claimBudget > 0
  /\ ~cell.held
  /\ cell' = [held |-> TRUE, holder |-> s, ep |-> 1, tok |-> nextTok, released |-> FALSE]
  /\ lease' = [lease EXCEPT ![s] = [ep |-> 1, tok |-> nextTok]]
  /\ nextTok' = nextTok + 1
  /\ claimBudget' = claimBudget - 1
  /\ st' = [st EXCEPT ![s] = "claimed"]
  /\ UNCHANGED <<snap, packObj, idxObj, uploads, lastTok, quiet, belief,
                 localMain, localPacks, migrating, batch, sensorMoved,
                 realMoved, hbDue, pushState, pushTo, crashes, renewBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

\* Our own previous incarnation died holding: supersede at once — and
\* ROTATE, because that incarnation may itself have been a successor
\* that died between its takeover CAS and its rotation, with the
\* straggler from the epoch before still holding a valid If-Match.  The
\* first shape skipped the rotation here ("nothing of ours can
\* straggle"); this module's second strict counterexample was exactly
\* that restart, and the straggler's CAS landing after the restarted
\* successor served.
SupersedeOwn(s) ==
  /\ st[s] \in {"idle", "watching"} /\ claimBudget > 0
  /\ cell.held /\ cell.holder = s /\ ~cell.released
  /\ cell' = [held |-> TRUE, holder |-> s, ep |-> cell.ep + 1, tok |-> nextTok,
              released |-> FALSE]
  /\ lease' = [lease EXCEPT ![s] = [ep |-> cell.ep + 1, tok |-> nextTok]]
  /\ nextTok' = nextTok + 1
  /\ claimBudget' = claimBudget - 1
  /\ st' = [st EXCEPT ![s] = IF RotateOnTakeover THEN "rotating" ELSE "claimed"]
  /\ UNCHANGED <<snap, packObj, idxObj, uploads, lastTok, quiet, belief,
                 localMain, localPacks, migrating, batch, sensorMoved,
                 realMoved, hbDue, pushState, pushTo, crashes, renewBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

\* A released cell is a clean handoff: its holder fenced itself before
\* it wrote the mark, so nothing can straggle and no rotation is owed.
ClaimReleased(s) ==
  /\ st[s] \in {"idle", "watching"} /\ claimBudget > 0
  /\ cell.held /\ cell.released
  /\ cell' = [held |-> TRUE, holder |-> s, ep |-> cell.ep + 1, tok |-> nextTok,
              released |-> FALSE]
  /\ lease' = [lease EXCEPT ![s] = [ep |-> cell.ep + 1, tok |-> nextTok]]
  /\ nextTok' = nextTok + 1
  /\ claimBudget' = claimBudget - 1
  /\ st' = [st EXCEPT ![s] = "claimed"]
  /\ UNCHANGED <<snap, packObj, idxObj, uploads, lastTok, quiet, belief,
                 localMain, localPacks, migrating, batch, sensorMoved,
                 realMoved, hbDue, pushState, pushTo, crashes, renewBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

ObserveForeign(s) ==
  /\ st[s] = "idle"
  /\ cell.held /\ cell.holder # s /\ ~cell.released
  /\ st' = [st EXCEPT ![s] = "watching"]
  /\ lastTok' = [lastTok EXCEPT ![s] = cell.tok]
  /\ quiet' = [quiet EXCEPT ![s] = 0]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, lease, belief,
                 localMain, localPacks, migrating, batch, sensorMoved,
                 realMoved, hbDue, pushState, pushTo, crashes, renewBudget,
                 claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

\* The scheduling axiom: a live holder's heartbeat ran since this
\* challenger's previous poll.  A dead holder has none to wait for.
HolderBeatSinceMyPoll(s) ==
  \/ ~PollsNoFasterThanHeartbeat
  \/ cell.holder \notin Syncers
  \/ st[cell.holder] = "idle"
  \/ ~hbDue[cell.holder]

PollQuiet(s) ==
  /\ st[s] = "watching"
  /\ cell.held /\ cell.holder # s
  /\ HolderBeatSinceMyPoll(s)
  /\ hbDue' = IF cell.holder \in Syncers THEN [hbDue EXCEPT ![cell.holder] = TRUE] ELSE hbDue
  /\ IF cell.tok = lastTok[s]
       THEN /\ quiet[s] < Misses
            /\ quiet' = [quiet EXCEPT ![s] = quiet[s] + 1]
            /\ UNCHANGED lastTok
       ELSE /\ lastTok' = [lastTok EXCEPT ![s] = cell.tok]
            /\ quiet' = [quiet EXCEPT ![s] = 0]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 belief, localMain, localPacks, migrating, batch, sensorMoved,
                 realMoved, pushState, pushTo, crashes, renewBudget,
                 claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

\* The takeover CAS on the last observed token.  A holder that renewed
\* after this observer's last poll is refused by the CAS itself — token
\* rotation is the store's, and FlintTierEpoch proves that theorem; it
\* is not restated here as a witness that could never fire.
Takeover(s) ==
  /\ st[s] = "watching" /\ claimBudget > 0
  /\ quiet[s] >= Misses
  /\ cell.held /\ cell.holder # s /\ cell.tok = lastTok[s]
  /\ cell' = [held |-> TRUE, holder |-> s, ep |-> cell.ep + 1, tok |-> nextTok,
              released |-> FALSE]
  /\ lease' = [lease EXCEPT ![s] = [ep |-> cell.ep + 1, tok |-> nextTok]]
  /\ nextTok' = nextTok + 1
  /\ claimBudget' = claimBudget - 1
  /\ st' = [st EXCEPT ![s] = IF RotateOnTakeover THEN "rotating" ELSE "claimed"]
  /\ UNCHANGED <<snap, packObj, idxObj, uploads, lastTok, quiet, belief,
                 localMain, localPacks, migrating, batch, sensorMoved,
                 realMoved, hbDue, pushState, pushTo, crashes, renewBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

\* The rotation (snapshot.rs rotate_for_takeover): same content, new
\* etag, so a straggler's If-Match is stale before we serve a byte.  A
\* bucket nobody published gets its EMPTY snapshot created here for the
\* same reason: the first shape returned early ("the first CAS's
\* If-None-Match:* is the fence") and this module's first strict run
\* found the straggler landing that create AFTER the successor served,
\* fencing the successor with its own predecessor's push.
RotateSnapshot(s) ==
  /\ st[s] = "rotating"
  /\ snap' = [snap EXCEPT !.etag = nextTok]
  /\ nextTok' = nextTok + 1
  /\ st' = [st EXCEPT ![s] = "claimed"]
  /\ UNCHANGED <<cell, packObj, idxObj, uploads, lease, lastTok, quiet, belief,
                 localMain, localPacks, migrating, batch, sensorMoved,
                 realMoved, hbDue, pushState, pushTo, crashes, renewBudget,
                 claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

\* The claim-time sweep (sweep.rs abort_orphaned_uploads): nothing of
\* ours is in flight, so everything pending is a predecessor's.
SweepDone(s) ==
  /\ st[s] = "claimed"
  /\ uploads' = IF SweepAtClaim THEN {} ELSE uploads
  /\ st' = [st EXCEPT ![s] = "restoring"]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, lease, lastTok, quiet,
                 belief, localMain, localPacks, migrating, batch, sensorMoved,
                 realMoved, hbDue, pushState, pushTo, crashes, renewBudget,
                 claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

(***************************************************************************)
(* The restore (restore.rs): the snapshot's packs, the snapshot's refs     *)
(* exactly, and a refusal when the bucket cannot be restored.  One step;   *)
(* the renewer beats through it (hold.tick at the preamble and per chunk). *)
(***************************************************************************)

Restore(s) ==
  /\ st[s] = "restoring"
  /\ IF snap.etag = 0
       THEN /\ belief' = [belief EXCEPT ![s] = NoBelief]
            /\ localMain' = [localMain EXCEPT ![s] = 0]
            /\ st' = [st EXCEPT ![s] = "serving"]
            /\ UNCHANGED <<lease, batch, fold, unrestorable, pushState, localPacks,
                           migrating, sensorMoved, realMoved, quiet>>
       ELSE LET fetched == snap.packs \cap packObj
                usable  == fetched \cap idxObj IN
            IF \/ snap.packs # fetched                       \* a named pack is absent
               \/ (snap.main # 0 /\ ~\E q \in usable : snap.main \in holds[q]) \* the ref's objects are in no pack git sees
              THEN \* exit 78: refused, and the restart refuses again.
                   /\ unrestorable' = TRUE
                   /\ Fall(s)
                   /\ UNCHANGED <<belief, localMain, localPacks, migrating>>
              ELSE /\ belief' = [belief EXCEPT ![s] =
                                   [etag |-> snap.etag, main |-> snap.main, packs |-> snap.packs]]
                   /\ localMain' = [localMain EXCEPT ![s] = snap.main]
                   /\ localPacks' = [localPacks EXCEPT ![s] = usable]
                   /\ migrating' = [migrating EXCEPT ![s] = {}]
                   /\ realMoved' = [realMoved EXCEPT ![s] = TRUE]
                   /\ sensorMoved' = [sensorMoved EXCEPT ![s] = TRUE]
                   /\ st' = [st EXCEPT ![s] = "serving"]
                   /\ UNCHANGED <<lease, batch, fold, unrestorable, pushState, quiet>>
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, lastTok, hbDue,
                 pushTo, crashes, renewBudget, claimBudget, ackNotDurable,
                 skipOverMovement, stragglerLand, toldFailedButDurable,
                 renewOverWedge>>
  /\ UNCHANGED FoldPlanVars

(***************************************************************************)
(* The heartbeat (lease.rs spawn_renewer): unconditional while serving,    *)
(* progress-gated while restoring or pushing.  A skip while the holder     *)
(* actually moved is the sensor lying — the witness.  A 412 is the fence.  *)
(***************************************************************************)

MustProgress(s) == st[s] \in {"restoring", "pushing"}

RenewCas(s) ==
  IF cell.held /\ cell.holder = s /\ cell.tok = lease[s].tok
    THEN /\ cell' = [cell EXCEPT !.tok = nextTok]
         /\ lease' = [lease EXCEPT ![s].tok = nextTok]
         /\ nextTok' = nextTok + 1
         /\ UNCHANGED <<st, batch, fold, pushState, quiet>>
    ELSE /\ Fall(s)                             \* deposed at renew: the fence
         /\ UNCHANGED <<cell, nextTok>>

RenewTick(s) ==
  /\ st[s] \in {"restoring", "serving", "pushing"}
  /\ renewBudget > 0
  /\ hbDue' = [hbDue EXCEPT ![s] = FALSE]
  /\ IF MustProgress(s) /\ ~sensorMoved[s]
       THEN \* the token stays quiet so a wedged server can be taken over
            /\ skipOverMovement' = (skipOverMovement \/ realMoved[s])
            /\ UNCHANGED <<cell, nextTok, lease, st, batch, fold, pushState, quiet,
                           sensorMoved, realMoved, renewBudget, renewOverWedge>>
       ELSE \* the twin witness: a renewal of a must-progress phase on a
            \* sensor tick with no real movement behind it (a fold ticking
            \* the hold's counter would keep a wedged batch's holder renewing)
            /\ renewOverWedge' = (renewOverWedge \/ (MustProgress(s) /\ sensorMoved[s] /\ ~realMoved[s]))
            /\ renewBudget' = renewBudget - 1
            /\ RenewCas(s)
            /\ sensorMoved' = [sensorMoved EXCEPT ![s] = FALSE]
            /\ realMoved' = [realMoved EXCEPT ![s] = FALSE]
            /\ UNCHANGED skipOverMovement
  /\ UNCHANGED <<snap, packObj, idxObj, uploads, lastTok, belief, localMain,
                 localPacks, migrating, pushTo, crashes, claimBudget,
                 ackNotDurable, stragglerLand, unrestorable,
                 toldFailedButDurable>>
  /\ UNCHANGED FoldPlanVars

(***************************************************************************)
(* The push, as git and the hook see it.                                   *)
(***************************************************************************)

\* The transfer and index-pack: the pack lands on disk, its index a
\* rename away.  A pushing server accepts new pushes (git runs them
\* concurrently; the hook queues behind the batch).
PushSend(p, s) ==
  /\ pushState[p] = "new"
  /\ st[s] \in {"serving", "pushing"}
  /\ pushState' = [pushState EXCEPT ![p] = "sent"]
  /\ pushTo' = [pushTo EXCEPT ![p] = s]
  /\ migrating' = [migrating EXCEPT ![s] = @ \cup {p}]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localMain, localPacks, batch,
                 sensorMoved, realMoved, hbDue, crashes, renewBudget,
                 claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

\* git renames the .idx last; only then is the pack complete on disk and
\* the hook runs.
IdxLand(s, p) ==
  /\ p \in migrating[s]
  /\ migrating' = [migrating EXCEPT ![s] = @ \ {p}]
  /\ localPacks' = [localPacks EXCEPT ![s] = @ \cup {p}]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localMain, batch, sensorMoved,
                 realMoved, hbDue, pushState, pushTo, crashes, renewBudget,
                 claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

\* The client gives up (the door's bound, a cut) before the report.  The
\* syncer never learns it and the batch runs to its end.
ClientHangup(p) ==
  /\ pushState[p] = "sent"
  /\ pushState' = [pushState EXCEPT ![p] = "failed"]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localMain, localPacks, migrating,
                 batch, sensorMoved, realMoved, hbDue, pushTo, crashes,
                 renewBudget, claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

(***************************************************************************)
(* The batch (batch.rs run_batch), one push at a time, each step a store   *)
(* request or a git transaction, and a crash possible between any two.    *)
(***************************************************************************)

Queued(s) == {p \in Pushes : pushState[p] # "new" /\ pushTo[p] = s
                             /\ p \in localPacks[s]
                             /\ p \notin snap.history
                             /\ batch[s].push # p}

\* Step 2: the judgement under the agreed view.  A ref the bucket and the
\* local repository disagree about refuses the push; otherwise a fast-
\* forward is accepted.  With AckAfterCas=FALSE (mutation B1) the hook
\* is answered HERE, with the objects local and nothing durable.
BatchStart(s) ==
  /\ st[s] = "serving" /\ batch[s].stage = "none"
  /\ \E p \in Queued(s) :
       IF localMain[s] # belief[s].main
         THEN /\ pushState' = [pushState EXCEPT ![p] =
                                 IF @ = "sent" THEN "failed" ELSE @]
              /\ UNCHANGED <<st, batch, sensorMoved, realMoved>>
         ELSE /\ st' = [st EXCEPT ![s] = "pushing"]
              /\ batch' = [batch EXCEPT ![s] = [push |-> p, stage |-> "judged", listed |-> {}]]
              /\ sensorMoved' = [sensorMoved EXCEPT ![s] = TRUE]
              /\ realMoved' = [realMoved EXCEPT ![s] = TRUE]
              /\ pushState' = [pushState EXCEPT ![p] =
                                 IF ~AckAfterCas /\ @ = "sent" THEN "acked" ELSE @]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, lease, lastTok,
                 quiet, belief, localMain, localPacks, migrating, hbDue,
                 pushTo, crashes, renewBudget, claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

\* Step 3: the batch's own renewal, through the renewer's path.
BatchRenew(s) ==
  /\ st[s] = "pushing" /\ batch[s].stage = "judged"
  /\ renewBudget > 0
  /\ renewBudget' = renewBudget - 1
  /\ IF cell.held /\ cell.holder = s /\ cell.tok = lease[s].tok
       THEN /\ cell' = [cell EXCEPT !.tok = nextTok]
            /\ lease' = [lease EXCEPT ![s].tok = nextTok]
            /\ nextTok' = nextTok + 1
            /\ batch' = [batch EXCEPT ![s].stage = "renewed"]
            /\ sensorMoved' = [sensorMoved EXCEPT ![s] = FALSE]
            /\ realMoved' = [realMoved EXCEPT ![s] = FALSE]
            /\ UNCHANGED <<st, fold, pushState, quiet>>
       ELSE /\ Fall(s)
            /\ UNCHANGED <<cell, nextTok>>
  /\ UNCHANGED <<snap, packObj, idxObj, uploads, lastTok, belief, localMain,
                 localPacks, migrating, hbDue, pushTo, crashes, claimBudget>>
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Witnesses

\* The listing that feeds the upload and the snapshot's pack list.  With
\* the gate, a pack is listed only once its index landed; without it
\* (the X1 world) a neighbour's mid-migration pack is listed too.
Listing(s) == localPacks[s] \cup (IF IdxGate THEN {} ELSE migrating[s])

\* The checksum pass over every pack above the whole-PUT ceiling: real
\* work; it ticks progress only in the fixed tree.
BatchHash(s) ==
  /\ st[s] = "pushing" /\ batch[s].stage = "renewed"
  /\ batch' = [batch EXCEPT ![s].stage = "hashed", ![s].listed = Listing(s)]
  /\ realMoved' = [realMoved EXCEPT ![s] = TRUE]
  /\ sensorMoved' = [sensorMoved EXCEPT ![s] = @ \/ TickOnHash]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localMain, localPacks, migrating,
                 hbDue, pushState, pushTo, crashes, renewBudget, claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

ToUpload(s) == batch[s].listed \ belief[s].packs

\* Step 4a: the multipart uploads are created (parts invisible until
\* Complete).
BatchInit(s) ==
  /\ st[s] = "pushing" /\ batch[s].stage = "hashed"
  /\ PacksBeforeCas
  /\ uploads' = uploads \cup {[s |-> s, p |-> p, ep |-> lease[s].ep] : p \in ToUpload(s)}
  /\ batch' = [batch EXCEPT ![s].stage = "initiated"]
  /\ realMoved' = [realMoved EXCEPT ![s] = TRUE]
  /\ sensorMoved' = [sensorMoved EXCEPT ![s] = TRUE]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, st, lease, lastTok,
                 quiet, belief, localMain, localPacks, migrating, hbDue,
                 pushState, pushTo, crashes, renewBudget, claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

\* Step 4b: Complete.  NOT conditional: a swept upload fails NoSuchUpload
\* and the process exits (any batch error ends the server); otherwise the
\* pack lands, its index beside it only if the index was on disk.  The
\* exit is the protocol's own doing, not a crash, so it is not budgeted:
\* the liveness run's second execution had it drawing on MaxCrashes, and
\* with the budget spent the deposed holder's batch could neither finish
\* nor fall, and its push stayed "sent" forever.
BatchComplete(s) ==
  /\ st[s] = "pushing" /\ batch[s].stage = "initiated"
  /\ LET mine == {[s |-> s, p |-> p, ep |-> lease[s].ep] : p \in ToUpload(s)} IN
     IF mine \subseteq uploads
       THEN /\ uploads' = uploads \ mine
            /\ packObj' = packObj \cup ToUpload(s)
            /\ idxObj' = idxObj \cup (ToUpload(s) \cap localPacks[s])
            /\ batch' = [batch EXCEPT ![s].stage = "uploaded"]
            /\ realMoved' = [realMoved EXCEPT ![s] = TRUE]
            /\ sensorMoved' = [sensorMoved EXCEPT ![s] = TRUE]
            /\ UNCHANGED <<st, lease, fold, pushState, quiet, crashes>>
       ELSE /\ Fall(s)
            /\ UNCHANGED <<uploads, packObj, idxObj, crashes>>
  /\ UNCHANGED <<cell, nextTok, snap, lastTok, belief, localMain, localPacks,
                 migrating, hbDue, pushTo, renewBudget, claimBudget>>
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Witnesses

\* Step 5: ONE snapshot CAS on the etag last seen (If-None-Match:* when
\* none).  A 412 is the fence.  The witness: this CAS lands from a syncer
\* that is not the cell's holder while its successor already restored.
CasReady(s) ==
  \/ batch[s].stage = "uploaded"
  \/ (batch[s].stage = "hashed" /\ ~PacksBeforeCas)

SuccessorRestored(s) ==
  \E t \in Syncers \ {s} : st[t] \in {"serving", "pushing"} /\ lease[t].ep > lease[s].ep

BatchCas(s) ==
  /\ st[s] = "pushing" /\ CasReady(s)
  /\ LET p == batch[s].push IN
     IF snap.etag = belief[s].etag
       THEN /\ snap' = [etag |-> nextTok, main |-> p,
                        packs |-> batch[s].listed, history |-> snap.history \cup {p}]
            /\ nextTok' = nextTok + 1
            /\ belief' = [belief EXCEPT ![s] =
                            [etag |-> nextTok, main |-> p, packs |-> batch[s].listed]]
            /\ batch' = [batch EXCEPT ![s].stage = "cas"]
            /\ realMoved' = [realMoved EXCEPT ![s] = TRUE]
            /\ sensorMoved' = [sensorMoved EXCEPT ![s] = TRUE]
            /\ stragglerLand' = (stragglerLand \/ SuccessorRestored(s))
            /\ UNCHANGED <<st, lease, fold, pushState, quiet>>
       ELSE /\ Fall(s)
            /\ UNCHANGED <<snap, nextTok, belief, stragglerLand>>
  /\ UNCHANGED <<cell, packObj, idxObj, uploads, lastTok, localMain, localPacks,
                 migrating, hbDue, pushTo, crashes, renewBudget, claimBudget,
                 ackNotDurable, skipOverMovement,
                 unrestorable, toldFailedButDurable, renewOverWedge>>
  /\ UNCHANGED FoldPlanVars

\* The reversed ordering (mutation): packs go up AFTER the CAS named them.
BatchLateUpload(s) ==
  /\ st[s] = "pushing" /\ batch[s].stage = "cas" /\ ~PacksBeforeCas
  /\ packObj' = packObj \cup ToUpload(s)
  /\ idxObj' = idxObj \cup (ToUpload(s) \cap localPacks[s])
  /\ batch' = [batch EXCEPT ![s].stage = "uploaded"]
  /\ UNCHANGED <<cell, nextTok, snap, uploads, st, lease, lastTok, quiet,
                 belief, localMain, localPacks, migrating, sensorMoved,
                 realMoved, hbDue, pushState, pushTo, crashes, renewBudget,
                 claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

RefsReady(s) ==
  \/ (batch[s].stage = "cas" /\ PacksBeforeCas)
  \/ (batch[s].stage = "uploaded" /\ ~PacksBeforeCas)

\* Step 6: the ref transaction.
BatchRefs(s) ==
  /\ st[s] = "pushing" /\ RefsReady(s)
  /\ localMain' = [localMain EXCEPT ![s] = batch[s].push]
  /\ batch' = [batch EXCEPT ![s].stage = "refs"]
  /\ realMoved' = [realMoved EXCEPT ![s] = TRUE]
  /\ sensorMoved' = [sensorMoved EXCEPT ![s] = TRUE]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localPacks, migrating, hbDue,
                 pushState, pushTo, crashes, renewBudget, claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

\* The report.  A client still waiting is told ok; one that hung up
\* learns nothing, and the bucket holds its push anyway — the probe.
BatchAck(s) ==
  /\ st[s] = "pushing" /\ batch[s].stage = "refs"
  /\ LET p == batch[s].push IN
       /\ pushState' = [pushState EXCEPT ![p] = IF @ = "sent" THEN "acked" ELSE @]
       /\ toldFailedButDurable' = (toldFailedButDurable \/ pushState[p] = "failed")
  /\ batch' = [batch EXCEPT ![s] = NoBatch]
  /\ st' = [st EXCEPT ![s] = "serving"]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, lease, lastTok,
                 quiet, belief, localMain, localPacks, migrating, sensorMoved,
                 realMoved, hbDue, pushTo, crashes, renewBudget, claimBudget,
                 ackNotDurable, skipOverMovement,
                 stragglerLand, unrestorable, renewOverWedge>>
  /\ UNCHANGED <<fold, FoldPlanVars>>

(***************************************************************************)
(* The clean handoff and the environment.                                  *)
(***************************************************************************)

\* SIGTERM between batches (the select! arm runs only there): fence, then
\* mark the cell released.  Mid-batch the kubelet's SIGKILL is a Crash.
CleanRelease(s) ==
  /\ st[s] = "serving"
  /\ cell.held /\ cell.holder = s /\ cell.tok = lease[s].tok
  /\ cell' = [cell EXCEPT !.released = TRUE, !.tok = nextTok]
  /\ nextTok' = nextTok + 1
  /\ Fall(s)
  /\ UNCHANGED <<snap, packObj, idxObj, uploads, lastTok, belief, localMain,
                 localPacks, migrating, hbDue, pushTo, crashes, renewBudget,
                 claimBudget>>
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Witnesses

\* Process death: memory vanishes; the emptyDir (packs, refs, the
\* incarnation file) survives a container restart.  A pod replacement is
\* the other syncer starting fresh.
Crash(s) ==
  /\ st[s] # "idle"
  /\ crashes < MaxCrashes
  /\ crashes' = crashes + 1
  /\ Fall(s)
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, lastTok, belief,
                 localMain, localPacks, migrating, hbDue, pushTo, renewBudget,
                 claimBudget>>
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Witnesses

(***************************************************************************)
(* Compaction tiers (fold.rs, design §3.4): the task beside the loop, the  *)
(* commit on it, and the sweep that deletes.                               *)
(***************************************************************************)

\* The plan, between batches: S frozen from the BELIEF (never the
\* directory), the roll-up given its contents.  One named pack is
\* allowed (the base rebuild's case), which is what lets two pushes
\* reach every ordering the mutations need.
FoldPlan(s) ==
  /\ st[s] = "serving" /\ batch[s].stage = "none"
  /\ fold[s].stage = "none" /\ foldBudget > 0
  /\ \E f \in FoldIds, S \in SUBSET belief[s].packs :
       /\ holds[f] = {} /\ Cardinality(S) >= 1
       /\ fold' = [fold EXCEPT ![s] = [id |-> f, inputs |-> S, stage |-> "planned"]]
       /\ holds' = [holds EXCEPT ![f] = UNION {holds[q] : q \in S}]
  /\ foldBudget' = foldBudget - 1
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localMain, localPacks, migrating,
                 batch, sensorMoved, realMoved, hbDue, pushState, pushTo,
                 crashes, renewBudget, claimBudget>>
  /\ UNCHANGED Witnesses

\* The task's upload, through the multipart path: initiated, then
\* Complete.  Beside anything the loop does.
FoldInit(s) ==
  /\ st[s] \in {"serving", "pushing"} /\ fold[s].stage = "planned"
  /\ uploads' = uploads \cup {[s |-> s, p |-> fold[s].id, ep |-> lease[s].ep]}
  /\ fold' = [fold EXCEPT ![s].stage = "initiated"]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, st, lease, lastTok,
                 quiet, belief, localMain, localPacks, migrating, batch,
                 sensorMoved, realMoved, hbDue, pushState, pushTo, crashes,
                 renewBudget, claimBudget>>
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Witnesses

\* Complete: the roll-up lands with its index, or — the claim-time sweep
\* took the upload — NoSuchUpload, which clears the fold and falls
\* nothing (fold_landed logs it; the next plan runs at the next tick).
\* Rule 3: the fold ticks its OWN counter, never the hold's.
FoldComplete(s) ==
  /\ st[s] \in {"serving", "pushing"} /\ fold[s].stage = "initiated"
  /\ LET u == [s |-> s, p |-> fold[s].id, ep |-> lease[s].ep] IN
     IF u \in uploads
       THEN /\ uploads' = uploads \ {u}
            /\ packObj' = packObj \cup {fold[s].id}
            /\ idxObj' = idxObj \cup {fold[s].id}
            /\ fold' = [fold EXCEPT ![s].stage = "uploaded"]
            /\ sensorMoved' = [sensorMoved EXCEPT ![s] = @ \/ FoldTicksBatchSensor]
       ELSE /\ fold' = [fold EXCEPT ![s] = NoFold]
            /\ UNCHANGED <<uploads, packObj, idxObj, sensorMoved>>
  /\ UNCHANGED <<cell, nextTok, snap, st, lease, lastTok, quiet, belief,
                 localMain, localPacks, migrating, batch, realMoved, hbDue,
                 pushState, pushTo, crashes, renewBudget, claimBudget>>
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Witnesses

\* The commit, on the loop between batches: ONE CAS on the loop's
\* CURRENT belief, naming (belief.packs \ S) ∪ {f}; a mismatch is the
\* fence.  The inputs leave the listing (retained for readers, then
\* unlinked) and git sees the roll-up.  The commit's one tick is the
\* loop's.
FoldCommitReady(s) ==
  /\ \/ fold[s].stage = "uploaded"
     \/ (FoldCasBeforeUpload /\ fold[s].stage \in {"planned", "initiated"})
  /\ \/ (st[s] = "serving" /\ batch[s].stage = "none")
     \/ (FoldCommitMidBatch /\ st[s] = "pushing")

\* The commit's own renewal, the batch's step 3 (`lease::renew`): a
\* conditional write on the CELL, which a deposed holder fails.  Without
\* it the commit is the ONE CAS on the loop that never revalidates the
\* lease, and the run that added this module's fold found the trace: a
\* holder deposed WHILE ITS RESTORE RAN, whose restore then read the
\* successor's rotated snapshot, so its If-Match matched and its fold
\* landed after the successor served.  Mutation FoldNoRenew.
FoldReadyToCommit(s) ==
  /\ \/ fold[s].stage = "uploaded"
     \/ (FoldCasBeforeUpload /\ fold[s].stage \in {"planned", "initiated"})
  /\ \/ (st[s] = "serving" /\ batch[s].stage = "none")
     \/ (FoldCommitMidBatch /\ st[s] = "pushing")

FoldRenew(s) ==
  /\ ~FoldNoRenew
  /\ FoldReadyToCommit(s)
  /\ renewBudget > 0
  /\ renewBudget' = renewBudget - 1
  /\ IF cell.held /\ cell.holder = s /\ cell.tok = lease[s].tok
       THEN /\ cell' = [cell EXCEPT !.tok = nextTok]
            /\ lease' = [lease EXCEPT ![s].tok = nextTok]
            /\ nextTok' = nextTok + 1
            /\ fold' = [fold EXCEPT ![s].stage = "renewed"]
            /\ UNCHANGED <<st, batch, pushState, quiet>>
       ELSE /\ Fall(s)                    \* deposed at renew: the fence
            /\ UNCHANGED <<cell, nextTok>>
  /\ UNCHANGED <<snap, packObj, idxObj, uploads, lastTok, belief, localMain,
                 localPacks, migrating, sensorMoved, realMoved, hbDue, pushTo,
                 crashes, claimBudget>>
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Witnesses

\* The commit itself: ONE CAS on the loop's CURRENT belief, naming
\* (belief.packs \ S) ∪ {f}; a mismatch is the fence.  The inputs leave
\* the listing (retained for readers, then unlinked) and git sees the
\* roll-up.  The commit's one tick is the loop's.
FoldCommit(s) ==
  /\ IF FoldNoRenew THEN FoldReadyToCommit(s) ELSE fold[s].stage = "renewed"
  /\ LET f == fold[s].id
         S == IF FoldInputsAfterStart THEN belief[s].packs ELSE fold[s].inputs
         named == IF FoldCasFromDisk THEN (localPacks[s] \ S) \cup {f}
                                    ELSE (belief[s].packs \ S) \cup {f} IN
     IF snap.etag = belief[s].etag
       THEN /\ snap' = [snap EXCEPT !.etag = nextTok, !.packs = named]
            /\ nextTok' = nextTok + 1
            /\ belief' = [belief EXCEPT ![s].etag = nextTok, ![s].packs = named]
            /\ localPacks' = [localPacks EXCEPT ![s] = (@ \ S) \cup {f}]
            /\ fold' = [fold EXCEPT ![s] = NoFold]
            /\ realMoved' = [realMoved EXCEPT ![s] = TRUE]
            /\ sensorMoved' = [sensorMoved EXCEPT ![s] = TRUE]
            /\ stragglerLand' = (stragglerLand \/ SuccessorRestored(s))
            /\ UNCHANGED <<st, lease, batch, pushState, quiet>>
       ELSE /\ Fall(s)
            /\ UNCHANGED <<snap, nextTok, belief, localPacks, stragglerLand>>
  /\ UNCHANGED <<cell, packObj, idxObj, uploads, lastTok, localMain, migrating,
                 hbDue, pushTo, crashes, renewBudget, claimBudget,
                 ackNotDurable, skipOverMovement, unrestorable,
                 toldFailedButDurable, renewOverWedge>>
  /\ UNCHANGED FoldPlanVars

\* THE GRACE AXIOM.  `orphan_grace_secs` (an hour) must outlive the
\* LONGEST upload, not the longest plausible one — lean's
\* `LeanChunkGCRacyGrace` rule, and the reason the shipped sweep reads
\* the object's age from the STORE's clock at the delete.  An object an
\* in-flight batch or fold uploaded and has not yet named is inside its
\* grace, whoever uploaded it; nothing else is.
InsideGrace(q) ==
  /\ GraceOutlivesUpload
  /\ \E t \in Syncers :
       \/ (batch[t].stage # "none" /\ q \in batch[t].listed /\ q \notin belief[t].packs)
       \/ (fold[t].stage # "none" /\ q = fold[t].id)

\* The sweep that deletes (sweep.rs: the ledger sweep and the LIST
\* sweep): an object no snapshot names, past the grace.  Its
\* list-then-read-then-etag-check collapses to one action whose guard is
\* the etag check; the grace is what keeps a deposed sweeper from taking
\* a live uploader's object.  Never mid-batch (the tick runs between
\* batches) and never with this holder's fold uploaded and uncommitted
\* (both sweeps refuse mid-fold).
SweepDelete(s) ==
  /\ st[s] = "serving" /\ batch[s].stage = "none"
  /\ snap.etag = belief[s].etag
  /\ \/ fold[s].stage \notin {"initiated", "uploaded"}
     \/ SweepDuringFold
  /\ \E q \in packObj \ snap.packs :
       /\ ~InsideGrace(q)
       /\ packObj' = packObj \ {q}
       /\ idxObj' = idxObj \ {q}
  /\ UNCHANGED <<cell, nextTok, snap, uploads, st, lease, lastTok, quiet,
                 belief, localMain, localPacks, migrating, batch, sensorMoved,
                 realMoved, hbDue, pushState, pushTo, crashes, renewBudget,
                 claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

Next ==
  \/ \E s \in Syncers :
       \/ AcquireCreate(s) \/ SupersedeOwn(s) \/ ClaimReleased(s) \/ ObserveForeign(s)
       \/ PollQuiet(s) \/ Takeover(s) \/ RotateSnapshot(s) \/ SweepDone(s)
       \/ Restore(s) \/ RenewTick(s)
       \/ BatchStart(s) \/ BatchRenew(s) \/ BatchHash(s) \/ BatchInit(s)
       \/ BatchComplete(s) \/ BatchCas(s) \/ BatchLateUpload(s)
       \/ BatchRefs(s) \/ BatchAck(s)
       \/ FoldPlan(s) \/ FoldInit(s) \/ FoldComplete(s)
       \/ FoldRenew(s) \/ FoldCommit(s)
       \/ SweepDelete(s)
       \/ CleanRelease(s) \/ Crash(s)
       \/ \E p \in Pushes : PushSend(p, s) \/ IdxLand(s, p)
  \/ \E p \in Pushes : ClientHangup(p)

\* Protocol machinery is weakly fair; crashes, hangups and pushes are the
\* environment.  RenewTick is fair (the renewer is its own task) — which
\* is exactly why a sensor that lies matters: the task runs and declines.
Fairness ==
  \A s \in Syncers :
    /\ WF_vars(AcquireCreate(s)) /\ WF_vars(SupersedeOwn(s)) /\ WF_vars(ClaimReleased(s))
    /\ WF_vars(ObserveForeign(s)) /\ WF_vars(PollQuiet(s)) /\ WF_vars(Takeover(s))
    /\ WF_vars(RotateSnapshot(s)) /\ WF_vars(SweepDone(s)) /\ WF_vars(Restore(s))
    /\ WF_vars(RenewTick(s))
    /\ WF_vars(BatchStart(s)) /\ WF_vars(BatchRenew(s)) /\ WF_vars(BatchHash(s))
    /\ WF_vars(BatchInit(s)) /\ WF_vars(BatchComplete(s)) /\ WF_vars(BatchCas(s))
    /\ WF_vars(BatchLateUpload(s)) /\ WF_vars(BatchRefs(s)) /\ WF_vars(BatchAck(s))
    /\ WF_vars(FoldInit(s)) /\ WF_vars(FoldComplete(s))
    /\ WF_vars(FoldRenew(s)) /\ WF_vars(FoldCommit(s))
    /\ \A p \in Pushes : WF_vars(IdxLand(s, p))

Spec == Init /\ [][Next]_vars /\ Fairness

\* Syncers are interchangeable and so are pushes: nothing picks one by
\* name (NoSyncer stands in where a value is needed and none is meant),
\* so permuting them is sound.  Not for the liveness run — TLC's
\* symmetry reduction and temporal checking do not combine.
Sym == Permutations(Syncers) \cup Permutations(Pushes)

(***************************************************************************)
(* The view.  Tokens and etags are minted from one counter and compared    *)
(* ONLY for equality (a claim's If-Match, a renew's, the snapshot CAS, a   *)
(* challenger's unchanged-token test); no guard orders them.  So two       *)
(* states that differ only in the NUMBERING of the tokens they reference   *)
(* have isomorphic futures — fresh values come from the counter alone —   *)
(* and TLC may fingerprint a state by each token's RANK among the tokens   *)
(* the state references.  Without this the strict run passed 23 million   *)
(* distinct states with its queue still growing: every heartbeat, claim,  *)
(* rotation and CAS multiplied a small structural space by the number of   *)
(* ways to number it.  Zero (absent) keeps its identity: the restore and   *)
(* the CAS condition test for it.  The counter itself is not part of the  *)
(* view.  Epochs are small (bounded by MaxClaims) and ordered by           *)
(* SuccessorRestored, so they stay as they are.                            *)
(***************************************************************************)

Toks == {cell.tok, snap.etag}
        \cup {lease[s].tok : s \in Syncers}
        \cup {lastTok[s] : s \in Syncers}
        \cup {belief[s].etag : s \in Syncers}

Rank(x) == IF x = 0 THEN 0 ELSE 1 + Cardinality({y \in Toks \ {0} : y < x})

View == <<[cell EXCEPT !.tok = Rank(cell.tok)],
          [snap EXCEPT !.etag = Rank(snap.etag)],
          packObj, idxObj, uploads, st,
          [s \in Syncers |-> [lease[s] EXCEPT !.tok = Rank(lease[s].tok)]],
          [s \in Syncers |-> Rank(lastTok[s])],
          quiet,
          [s \in Syncers |-> [belief[s] EXCEPT !.etag = Rank(belief[s].etag)]],
          localMain, localPacks, migrating, batch, sensorMoved, realMoved,
          hbDue, pushState, pushTo, holds, fold, foldBudget,
          crashes, renewBudget, claimBudget,
          ackNotDurable, skipOverMovement, stragglerLand, unrestorable,
          toldFailedButDurable, renewOverWedge>>

(***************************************************************************)
(* Theorems, and the probe.                                                *)
(***************************************************************************)

\* A pack the snapshot names, in the bucket with its index, holding p.
HeldByNamedComplete(p) ==
  \E q \in snap.packs : p \in holds[q] /\ q \in packObj /\ q \in idxObj

Durable(p) ==
  /\ p \in snap.history
  /\ HeldByNamedComplete(p)

\* Told ok => in the bucket, with the pack complete.  Stated over the
\* state rather than a witness so that a later transition (a rotation, a
\* restore) cannot un-durable an acknowledged push either.
Inv_AckedIsDurable ==
  \A p \in Pushes : pushState[p] = "acked" => Durable(p)

\* Every landed push's pack is in the bucket with its index — a restore
\* can see its objects.
Inv_LandedPackComplete ==
  \A p \in snap.history : HeldByNamedComplete(p)

\* Every pack the snapshot names is in the bucket with its index: what a
\* CAS that names only what it uploaded or a prior CAS named preserves,
\* and what the fold's formula must preserve too.
Inv_NamedIsUploaded ==
  \A q \in snap.packs : q \in packObj /\ q \in idxObj

Inv_NoSkipOverMovement          == ~skipOverMovement
Inv_NoRenewOverWedge            == ~renewOverWedge
Inv_NoStragglerLandAfterRestore == ~stragglerLand
Inv_NoUnrestorable              == ~unrestorable

\* THE PROBE — reachable in the shipped protocol, kept out of the strict
\* run: the client gave up and the push landed anyway.
Inv_NoToldFailedButDurable == ~toldFailedButDurable

Inv == /\ TypeOK
       /\ Inv_AckedIsDurable
       /\ Inv_LandedPackComplete
       /\ Inv_NamedIsUploaded
       /\ Inv_NoSkipOverMovement
       /\ Inv_NoRenewOverWedge
       /\ Inv_NoStragglerLandAfterRestore
       /\ Inv_NoUnrestorable

\* A watcher over a crashed holder's frozen token eventually takes over
\* (or the configuration otherwise breaks).
DeadHolderWatchResolves ==
  \A s \in Syncers : \A h \in Syncers \ {s} :
    []((st[s] = "watching" /\ cell.held /\ cell.holder = h /\ st[h] = "idle")
       => <>(st[s] # "watching" \/ st[h] # "idle" \/ claimBudget = 0))

\* A push that reached a serving syncer is eventually answered, one way
\* or the other: acked, or told ng — by the batch, by the hangup, or by
\* the process falling out from under its hook. The only escape is the
\* renewal budget, which the batch's own renewal needs.
SentPushResolves ==
  \A p \in Pushes :
    [](pushState[p] = "sent" => <>(pushState[p] # "sent" \/ renewBudget = 0))

================================================================================
