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
  FoldNoCoverageCheck,  \* mutation: the commit lands a roll-up that does not hold its inputs
  FoldSupersedesArrivals, \* mutation: a fold's commit unnames every input, held or not (runcd)
  FoldReachableCoverage, \* mutation: an input may go if its UNCOVERED objects are unreachable (the pack-pinning 'direction 1')
  ReclaimAtRestore,     \* DIRECTION 4: a reclaiming rebuild between the restore and the first served hook
  ReclaimWhileServing,  \* mutation: the reclaim's relaxed rule, taken on a SERVING syncer
  ReclaimBySet,         \* DIRECTION 4, THE FREE FORM: the coverer is the KEPT SET, not a new pack
  RestoreLosesQueue,    \* a restore begins a FRESH incarnation: pending requests are gone
  ReclaimUnlinks,       \* the reclaim UNLINKS what it drops instead of retaining it
  NameAcceptedSet,      \* DIRECTION 5: a batch names the packs it ACCEPTED, never the directory
  ForgetPushPack,       \* mutation: direction 5 without the push->pack mapping
  ProveFromDisk,        \* mutation: a proof is taken over the object DIRECTORY, not the named packs (F2)
  ListingKeepsRetained, \* mutation: a batch lists the packs retention holds on disk
  GraceOutlivesUpload   \* the grace axiom; FALSE is lean's RacyGrace mutation

Stages == {"none", "judged", "renewed", "hashed", "initiated", "uploaded",
           "cas", "refs"}
States == {"idle", "watching", "claimed", "rotating", "restoring",
           "reclaiming", "serving", "pushing"}
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
  retained,    \* [Syncers -> SUBSET PackIds]  on disk, unnamed, kept for readers
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
  stragglerLand, unrestorable, toldFailedButDurable, renewOverWedge,
  provedOffBucket, provedOverRetained

vars == <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease, lastTok,
          quiet, belief, localMain, localPacks, migrating, batch, sensorMoved,
          realMoved, hbDue, pushState, pushTo, holds, fold, foldBudget,
          crashes, renewBudget, claimBudget, ackNotDurable, skipOverMovement,
          stragglerLand, unrestorable, toldFailedButDurable, renewOverWedge,
          retained, provedOffBucket, provedOverRetained>>

NoBatch   == [push |-> 0, stage |-> "none", listed |-> {}]
ZeroLease == [ep |-> 0, tok |-> 0]
NoBelief  == [etag |-> 0, main |-> 0, packs |-> {}]
NoFold    == [id |-> 0, inputs |-> {}, stage |-> "none", base |-> FALSE, at |-> {}, atRest |-> FALSE]

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
  /\ retained \in [Syncers -> SUBSET PackIds]
  /\ provedOffBucket \in BOOLEAN /\ provedOverRetained \in BOOLEAN
  /\ migrating \in [Syncers -> SUBSET Pushes]
  /\ batch \in [Syncers -> [push: PushIds, stage: Stages, listed: SUBSET PackIds]]
  /\ holds \in [PackIds -> SUBSET Pushes]
  /\ fold \in [Syncers -> [id: FoldIds \cup {0}, inputs: SUBSET PackIds, stage: FoldStages,
                          base: BOOLEAN, at: SUBSET Pushes, atRest: BOOLEAN]]
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
  /\ retained = [s \in Syncers |-> {}]
  /\ provedOffBucket = FALSE /\ provedOverRetained = FALSE
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

Witnesses == <<ackNotDurable, skipOverMovement, stragglerLand, unrestorable,
               toldFailedButDurable, renewOverWedge, provedOffBucket,
               provedOverRetained>>
\* What an action that touches neither a witness nor the disk's retention
\* leaves alone.  `retained` rides here rather than in `Local` because
\* only two actions in the whole module change it, and every other one
\* already named `Witnesses`.
Untouched == <<Witnesses, retained>>
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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

\* DIRECTION 4's exit: the window closes and the syncer starts answering
\* hooks. Enabled only with no fold in flight, so the reclaim either
\* committed or was abandoned before a single push can arrive. It is
\* always enabled at once, which is what makes the reclaim OPTIONAL —
\* a syncer that does not want one simply leaves.
ReclaimDone(s) ==
  /\ st[s] = "reclaiming"
  /\ fold[s].stage = "none"
  /\ st' = [st EXCEPT ![s] = "serving"]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, lease, lastTok, quiet,
                 belief, localMain, localPacks, migrating, batch, sensorMoved,
                 realMoved, hbDue, pushState, pushTo, crashes, renewBudget,
                 claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
              \* WHAT THIS CONDITION DOES NOT ASK. It checks the TIP —
              \* `snap.main` in some usable pack — and nothing about the
              \* tip's ANCESTORS, because this module has no parent
              \* relation and no server-built commits (no merge, no
              \* commit_tree, no refs/for: the words appear nowhere in
              \* it). The shipped syncer runs `git fsck
              \* --connectivity-only` over the whole reachable graph,
              \* which is strictly stronger.
              \*
              \* On 2026-09-08 F14 found a bucket this predicate calls
              \* RESTORABLE and the real syncer refuses: a published tip
              \* whose parent reached no pack. Do not cite
              \* Inv_NoUnrestorable as covering that class — it cannot
              \* fire for it. `ForgeMergeChain.tla` adds the dimension
              \* and carries both mutations.
              THEN \* exit 78: refused, and the restart refuses again.
                   /\ unrestorable' = TRUE
                   /\ Fall(s)
                   /\ UNCHANGED <<belief, localMain, localPacks, migrating>>
              ELSE /\ belief' = [belief EXCEPT ![s] =
                                   [etag |-> snap.etag, main |-> snap.main, packs |-> snap.packs]]
                   /\ localMain' = [localMain EXCEPT ![s] = snap.main]
                   \* `restore.rs` unlinks a pack the snapshot does not
                   \* name UNLESS retention keeps it, and the retained
                   \* list comes back from the state file a restart
                   \* inherits — so retention survives a restore.
                   /\ localPacks' = [localPacks EXCEPT ![s] = usable \cup retained[s]]
                   /\ migrating' = [migrating EXCEPT ![s] = {}]
                   /\ realMoved' = [realMoved EXCEPT ![s] = TRUE]
                   /\ sensorMoved' = [sensorMoved EXCEPT ![s] = TRUE]
                   \* DIRECTION 4. The refs are installed and the hook is
                   \* not answered yet: `PushSend` requires "serving" or
                   \* "pushing", so while this syncer sits here NO push
                   \* can reach it and no pack can be waiting for a ref.
                   /\ st' = [st EXCEPT ![s] = IF ReclaimAtRestore THEN "reclaiming" ELSE "serving"]
                   \* A RESTORE BEGINS A FRESH INCARNATION. `restore::restore`
                   \* is called ONCE (server.rs:238) before the first
                   \* Phase::Serving, and a lease loss returns Fenced and
                   \* exits — so the process that restores has an EMPTY
                   \* request queue and cannot land a push whose
                   \* proc-receive request it never received. Without this
                   \* the model lands such a push anyway, because Queued(s)
                   \* is derived from the pack being on DISK.
                   \*
                   \* Reset to "new", not to a dead state: the client is
                   \* free to RETRY, and the pack is still on disk under the
                   \* same name — pack names are many-to-one, so an
                   \* identical-content retry reuses it (measured
                   \* 2026-09-08). That is the hazard this must not assume
                   \* away while removing the other one.
                   /\ pushState' = IF RestoreLosesQueue
                                     THEN [q \in Pushes |->
                                             IF pushTo[q] = s /\ q \notin snap.history
                                               THEN "new" ELSE pushState[q]]
                                     ELSE pushState
                   /\ UNCHANGED <<lease, batch, fold, unrestorable, quiet>>
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, lastTok, hbDue,
                 pushTo, crashes, renewBudget, claimBudget, ackNotDurable,
                 skipOverMovement, stragglerLand, toldFailedButDurable,
                 renewOverWedge, retained, provedOffBucket,
                 provedOverRetained>>
  /\ UNCHANGED FoldPlanVars

(***************************************************************************)
(* The heartbeat (lease.rs spawn_renewer): unconditional while serving,    *)
(* progress-gated while restoring or pushing.  A skip while the holder     *)
(* actually moved is the sensor lying — the witness.  A 412 is the fence.  *)
(***************************************************************************)

MustProgress(s) == st[s] \in {"restoring", "reclaiming", "pushing"}

RenewCas(s) ==
  IF cell.held /\ cell.holder = s /\ cell.tok = lease[s].tok
    THEN /\ cell' = [cell EXCEPT !.tok = nextTok]
         /\ lease' = [lease EXCEPT ![s].tok = nextTok]
         /\ nextTok' = nextTok + 1
         /\ UNCHANGED <<st, batch, fold, pushState, quiet>>
    ELSE /\ Fall(s)                             \* deposed at renew: the fence
         /\ UNCHANGED <<cell, nextTok>>

RenewTick(s) ==
  /\ st[s] \in {"restoring", "reclaiming", "serving", "pushing"}
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
                 toldFailedButDurable, retained, provedOffBucket,
                 provedOverRetained>>
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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

\* The client gives up (the door's bound, a cut) before the report.  The
\* syncer never learns it and the batch runs to its end.
ClientHangup(p) ==
  /\ pushState[p] = "sent"
  /\ pushState' = [pushState EXCEPT ![p] = "failed"]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localMain, localPacks, migrating,
                 batch, sensorMoved, realMoved, hbDue, pushTo, crashes,
                 renewBudget, claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Untouched

\* THE TWO PACK SETS, and the difference is the whole of audit F2.
\*
\* `Syncer::listed_packs` is what a batch may list, upload and name;
\* `Git::local_packs` is the object DIRECTORY, which also holds what a
\* fold superseded and retention keeps for readers.  `localPacks` here
\* is the FORMER — the subtraction is by construction, so no run can
\* forget it.  Modelling the directory instead and deriving the listing
\* was tried first and TLC refuted it in 3.6M states, exactly as the
\* code's own comment warns: "a retained pack re-listed would be
\* re-named and re-uploaded — the collision every 'keep the old packs a
\* while' fix has".  A batch named a retained pack, the ledger sweep had
\* already taken it out of the bucket, and Inv_NamedIsUploaded fell.
\*
\* `localPacks` is the DIRECTORY and the listing is derived, rather than
\* the other way round.  The two factorings are isomorphic and cost TLC
\* the same (measured, against a guess that the second would be
\* cheaper — it is not).  This one is chosen because it makes the
\* subtraction a RULE the model states rather than a fact of the
\* encoding, so a mutation can take it away: as the listing,
\* `retained` would be read by nothing in the strict run — a variable
\* paying for its states and holding up no property.
\*
\* The rule is load-bearing and TLC said so before it was written.  The
\* first draft of this module's retention had no subtraction, and TLC
\* refuted it in 3.6M states, exactly as the code's own comment warns:
\* "a retained pack re-listed would be re-named and re-uploaded — the
\* collision every 'keep the old packs a while' fix has".  A batch named
\* a retained pack, the ledger sweep had already taken it out of the
\* bucket, and Inv_NamedIsUploaded fell.  Mutation ListingKeepsRetained.
Listing(s) == (IF ListingKeepsRetained THEN localPacks[s]
                                       ELSE localPacks[s] \ retained[s])
                \cup (IF IdxGate THEN {} ELSE migrating[s])

(***************************************************************************)
(* DIRECTION 5: NAME WHAT WAS DECIDED, NOT WHAT WAS OBSERVED.              *)
(*                                                                         *)
(* `Listing` above is the shipped rule and it is a DIRECTORY read, so the  *)
(* snapshot names packs forge never decided to name — including a push it  *)
(* refused.  That is where the 81% residue comes from, and it is why every *)
(* later "may I drop this?" is a reachability question with no answer      *)
(* while serving (ForgeSyncFoldReachableCoverage).                          *)
(*                                                                         *)
(* The alternative is what walgit gets for free by owning its own receive  *)
(* path: name the packs of the pushes this batch ACCEPTED, plus whatever   *)
(* was already named.  forge can have it without reimplementing anything,  *)
(* because `pre-receive` sees GIT_QUARANTINE_PATH before git migrates it   *)
(* and the pack keeps its name across the migration — so the push->pack    *)
(* mapping is recordable at the door.                                      *)
(*                                                                         *)
(* The question for TLC is whether it is SAFE: the directory read is what  *)
(* makes a QUEUED push durable (runcd), because that push's pack is named  *)
(* a batch before its ref moves.  Under this rule it is not named early —  *)
(* it is named by its OWN batch, when its own commands are accepted.       *)
(* Inv_LandedPackComplete is the question, and the mutation below is the   *)
(* teeth: keep the rule and LOSE the mapping.                              *)
(***************************************************************************)
AcceptedListing(s) ==
  LET kept == belief[s].packs \ retained[s] IN
  IF ForgetPushPack THEN kept ELSE kept \cup {batch[s].push}

\* The checksum pass over every pack above the whole-PUT ceiling: real
\* work; it ticks progress only in the fixed tree.
BatchHash(s) ==
  /\ st[s] = "pushing" /\ batch[s].stage = "renewed"
  /\ batch' = [batch EXCEPT ![s].stage = "hashed",
                 ![s].listed = IF NameAcceptedSet THEN AcceptedListing(s) ELSE Listing(s)]
  /\ realMoved' = [realMoved EXCEPT ![s] = TRUE]
  /\ sensorMoved' = [sensorMoved EXCEPT ![s] = @ \/ TickOnHash]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localMain, localPacks, migrating,
                 hbDue, pushState, pushTo, crashes, renewBudget, claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Untouched

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
                 unrestorable, toldFailedButDurable, renewOverWedge,
                 retained, provedOffBucket,
                 provedOverRetained>>
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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

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
                 stragglerLand, unrestorable, renewOverWedge,
                 retained, provedOffBucket,
                 provedOverRetained>>
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
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Untouched

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
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Untouched

(***************************************************************************)
(* Compaction tiers (fold.rs, design §3.4): the task beside the loop, the  *)
(* commit on it, and the sweep that deletes.                               *)
(***************************************************************************)

\* The plan, between batches: S frozen from the BELIEF (never the
\* directory), the roll-up given its contents.  One named pack is
\* allowed (the base rebuild's case), which is what lets two pushes
\* reach every ordering the mutations need.
FoldPlan(s) ==
  /\ st[s] \in {"serving", "reclaiming"} /\ batch[s].stage = "none"
  /\ fold[s].stage = "none" /\ foldBudget > 0
  /\ \E f \in FoldIds, S \in SUBSET belief[s].packs, base \in BOOLEAN :
       /\ holds[f] = {} /\ Cardinality(S) >= 1
       \* `atRest` is stamped at PLAN time and read at COMMIT time, which
       \* is the honest shape: an implementation computes what the refs
       \* reach while it builds the pack, and commits later. Whether that
       \* gap can be exploited is the question this models.
       /\ fold' = [fold EXCEPT ![s] = [id |-> f, inputs |-> S, stage |-> "planned",
                                      base |-> base, at |-> snap.history,
                                      atRest |-> (ReclaimWhileServing \/ st[s] = "reclaiming")]]
       \* TWO kinds of fold, with two content rules — and this used to be
       \* one rule, the tier fold's, written in as an axiom. That is how
       \* runcd's defect (2026-09-07) got past a checker that carried the
       \* exact invariant it violated:
       \*
       \*   * a TIER fold (`pack-objects --stdin-packs`) holds the union
       \*     of its inputs' contents;
       \*   * a BASE rebuild (`pack-objects --all`) holds what the REFS
       \*     REACH when it reads them — `snap.history` here — and not
       \*     whatever else its inputs hold. A batch names the DIRECTORY
       \*     (`batch.listed`), and git migrates a push's pack out of
       \*     quarantine before the hook that will queue it runs, so a
       \*     pack can be named one batch BEFORE its push lands. A base
       \*     planned in that gap has the pack as an input and cannot
       \*     see the push. Its commit must not unname that pack.
       \*
       \* Either kind may also DROP one object: `pack-objects` is a
       \* subprocess this design trusts nowhere else, and the code now
       \* checks its output against the indexes (FoldCovers).
       /\ LET U == UNION {holds[q] : q \in S}
              R == IF base THEN U \cap snap.history ELSE U IN
            \/ holds' = [holds EXCEPT ![f] = R]
            \/ \E lost \in R : holds' = [holds EXCEPT ![f] = R \ {lost}]
  /\ foldBudget' = foldBudget - 1
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localMain, localPacks, migrating,
                 batch, sensorMoved, realMoved, hbDue, pushState, pushTo,
                 crashes, renewBudget, claimBudget>>
  /\ UNCHANGED Untouched

\* The task's upload, through the multipart path: initiated, then
\* Complete.  Beside anything the loop does.
\* "reclaiming" is in these two guards because DIRECTION 4 IS DEAD CODE
\* WITHOUT IT, and its absence made ForgeSyncReclaimAtRestore's green
\* VACUOUS for a whole day. `FoldPlan` was extended to allow a fold to be
\* planned in the reclaim window; `FoldInit` and `FoldComplete` were not,
\* so such a fold could never reach "uploaded", therefore never
\* "renewed", therefore never COMMIT — and `ReclaimDone` requires
\* `fold[s].stage = "none"`, so the syncer cannot leave the window
\* carrying one either. The atRest arm of `FoldCommit` was unreachable.
\*
\* It was caught by the state counts: ForgeSyncReclaimBySet returned
\* EXACTLY ForgeSyncReclaimAtRestore's 215,837,588 generated and
\* 47,449,859 distinct. A mutation that fires cannot leave the generated
\* count untouched. Comparing counts against a neighbouring run is now
\* the acceptance test for any run in this family, not a courtesy.
FoldInit(s) ==
  /\ st[s] \in {"serving", "pushing", "reclaiming"} /\ fold[s].stage = "planned"
  /\ uploads' = uploads \cup {[s |-> s, p |-> fold[s].id, ep |-> lease[s].ep]}
  /\ fold' = [fold EXCEPT ![s].stage = "initiated"]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, st, lease, lastTok,
                 quiet, belief, localMain, localPacks, migrating, batch,
                 sensorMoved, realMoved, hbDue, pushState, pushTo, crashes,
                 renewBudget, claimBudget>>
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Untouched

\* Complete: the roll-up lands with its index, or — the claim-time sweep
\* took the upload — NoSuchUpload, which clears the fold and falls
\* nothing (fold_landed logs it; the next plan runs at the next tick).
\* Rule 3: the fold ticks its OWN counter, never the hold's.
FoldComplete(s) ==
  /\ st[s] \in {"serving", "pushing", "reclaiming"} /\ fold[s].stage = "initiated"
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
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Untouched

\* The commit, on the loop between batches: ONE CAS on the loop's
\* CURRENT belief, naming (belief.packs \ S) ∪ {f}; a mismatch is the
\* fence.  The inputs leave the listing (retained for readers, then
\* unlinked) and git sees the roll-up.  The commit's one tick is the
\* loop's.
\* The commit's own renewal, the batch's step 3 (`lease::renew`): a
\* conditional write on the CELL, which a deposed holder fails.  Without
\* it the commit is the ONE CAS on the loop that never revalidates the
\* lease, and the run that added this module's fold found the trace: a
\* holder deposed WHILE ITS RESTORE RAN, whose restore then read the
\* successor's rotated snapshot, so its If-Match matched and its fold
\* landed after the successor served.  Mutation FoldNoRenew.
\* The batch-quiescence half of the commit's guard, factored out so the
\* COMMIT can re-assert it.  `fold::commit` renews the lease and CASes
\* inside ONE call holding `&mut Syncer`, reached from the fold-result
\* arm of the serving loop's `tokio::select!` (server.rs:363); the push
\* arm holds the same borrow across every await it makes, so no batch
\* can begin between the fold's renewal and its CAS.
\*
\* Checking this at the RENEW and not again at the COMMIT was a real
\* modelling gap, and the strict run found it the day retention was
\* added: BatchInit -> FoldCommit -> BatchCas, where the batch's CAS
\* writes a `listed` set taken BEFORE the fold retained its inputs and
\* so re-names a retained pack.  `Listing` subtracts `retained`, but it
\* is evaluated when the batch lists, and at that moment there was
\* nothing to subtract.  Unreachable in the code for the borrow reason
\* above; reachable here only because two model steps admitted an
\* interleaving one function call does not.
FoldQuiet(s) ==
  \/ (st[s] = "serving" /\ batch[s].stage = "none")
  \/ (st[s] = "reclaiming" /\ batch[s].stage = "none")
  \/ (FoldCommitMidBatch /\ st[s] = "pushing")

FoldReadyToCommit(s) ==
  /\ \/ fold[s].stage = "uploaded"
     \/ (FoldCasBeforeUpload /\ fold[s].stage \in {"planned", "initiated"})
  /\ FoldQuiet(s)

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
            \* The fold's renewal does NOT tick the batch's movement sensor;
            \* that is exactly what mutation FoldTicksBatchSensor turns on.
            \* This must be said HERE and not in the trailing UNCHANGED: the
            \* ELSE branch's Fall clears both sensors, and a trailing
            \* UNCHANGED would silently kill the fence whenever a sensor is
            \* set -- which, after a restore, is its normal state.
            /\ UNCHANGED <<st, batch, pushState, quiet, sensorMoved, realMoved>>
       ELSE /\ Fall(s)                    \* deposed at renew: the fence
            /\ UNCHANGED <<cell, nextTok>>
  /\ UNCHANGED <<snap, packObj, idxObj, uploads, lastTok, belief, localMain,
                 localPacks, migrating, hbDue, pushTo,
                 crashes, claimBudget>>
  /\ UNCHANGED FoldPlanVars /\ UNCHANGED Untouched

\* The commit itself: ONE CAS on the loop's CURRENT belief, naming
\* (belief.packs \ S) ∪ {f}; a mismatch is the fence.  The inputs leave
\* the listing (retained for readers, then unlinked) and git sees the
\* roll-up.  The commit's one tick is the loop's.
\* What the roll-up must hold before it may replace its inputs: every
\* object those inputs hold. `fold::run_task` establishes this by
\* comparing the pack INDEXES before it uploads (a tier fold must cover
\* its inputs exactly; a base rebuild must cover everything REACHABLE,
\* since collecting the unreachable is what a base is for).
FoldCovers(s) ==
  LET f == fold[s].id
      U == UNION {holds[q] : q \in fold[s].inputs}
      R == IF fold[s].base THEN U \cap fold[s].at ELSE U IN
    R \subseteq holds[f]

\* DIRECTION 4, THE FREE FORM: which inputs a reclaim may drop, and the
\* set it is chosen from. Forced to {{}} — a single choice — whenever the
\* constant is off, so the other 21 ForgeSync runs keep exactly the state
\* spaces they had.
FoldInputSet(s) == IF FoldInputsAfterStart THEN belief[s].packs ELSE fold[s].inputs

ReclaimDropSet(s) ==
  IF ReclaimBySet /\ fold[s].atRest THEN SUBSET FoldInputSet(s) ELSE {{}}

\* A roll-up that does not cover its inputs is thrown away, which is
\* what `run_task` does when the index comparison fails: it returns an
\* error before the upload, the loop clears the fold and the scratch
\* goes. Nothing is named, nothing is unnamed, and the plan can run
\* again. Without this the model would simply wedge on a lossy fold
\* rather than recover from one.
FoldAbandon(s) ==
  /\ ~FoldNoCoverageCheck
  /\ fold[s].stage # "none"
  /\ ~FoldCovers(s)
  /\ fold' = [fold EXCEPT ![s] = NoFold]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localMain, localPacks, migrating,
                 batch, sensorMoved, realMoved, hbDue, pushState, pushTo,
                 crashes, renewBudget, claimBudget, holds, foldBudget>>
  /\ UNCHANGED Untouched

FoldCommit(s) ==
  /\ IF FoldNoRenew THEN FoldReadyToCommit(s) ELSE fold[s].stage = "renewed"
  \* Re-asserted, not inherited from the renewal: see FoldQuiet.
  /\ FoldQuiet(s)
  /\ (FoldNoCoverageCheck \/ FoldCovers(s))
  \* DIRECTION 4, THE FREE FORM (measured 2026-09-08). The rule below
  \* drops an input whose reachable objects the NEW pack covers, and
  \* `FoldPlan` demands that pack be fresh (`holds[f] = {}`) — so the
  \* reclaim TLC proved safe always pays a whole-repository
  \* `pack-objects --all --write-bitmap-index` plus its upload, on the
  \* WAKE path. The rule never asked the coverer to be NEW. Keeping a
  \* SET that still holds everything reachable collects 100% of the
  \* redundant bytes at every base-rebuild cadence including the shipped
  \* one, building nothing (forge/e2e/results/direction4-cadence-*.log).
  \*
  \* Modelled as a nondeterministic choice rather than as the greedy an
  \* implementation would run: TLC then explores EVERY set that greedy
  \* could reach and many it could not, so a green covers the algorithm
  \* without pinning the algorithm. `Drop` is forced to {} when the
  \* constant is off, which is what keeps the other 21 runs' state
  \* spaces exactly as they were.
  \*
  \* The kept set here INCLUDES the fold's own output, so the modelled
  \* rule is strictly more permissive than an implementation that builds
  \* nothing — a green covers the free form a fortiori.
  /\ \E Drop \in ReclaimDropSet(s) :
     LET f == fold[s].id
           S == FoldInputSet(s)
           \* WHICH INPUTS THE COMMIT MAY UNNAME. Not "all of them", which
           \* is what runcd did and what the mutation restores: an input
           \* may go only if the roll-up HOLDS everything it holds.
           \*
           \* Two weaker rules were tried and TLC refuted both. "Unname
           \* every input" is runcd itself (the mutation). "Unname an
           \* input whose uncovered objects have already landed" fails
           \* too: a base rebuild reads the refs before a queued push's
           \* ref moves, so its roll-up cannot hold that push, and by the
           \* time the fold commits the push HAS landed — the test passes
           \* and the object is stranded anyway. Every landed push must
           \* stay held (Inv_LandedPackComplete), so coverage is the only
           \* rule that survives. An input holding something the roll-up
           \* missed simply stays named for a later fold.
           \* DIRECTION 1 of the pack-pinning finding (2026-09-08), and the
           \* THIRD weakening of this rule TLC has now refuted. Dead
           \* objects pin live packs: on runcl 58% of the snapshot's named
           \* bytes were two packs kept only because each held three
           \* UNREACHABLE objects — the residue of a correctly refused
           \* push. The tempting fix is to supersede on REACHABLE
           \* coverage: let an input go when everything it holds that the
           \* refs can reach is in the roll-up, and ignore what they
           \* cannot reach.
           \*
           \* It is runcd again, with the arrow reversed. A base rebuild
           \* holds what the refs REACHED when it read them, and a batch
           \* names the DIRECTORY, so a queued push's pack is named one
           \* batch before its ref moves. In that gap its objects are
           \* exactly "unreachable objects in a named pack" — the same
           \* observation as a refusal's residue, and NO test at this
           \* moment separates them. The reachable rule unnames the pack;
           \* the push then lands; its objects are in nothing named.
           \*
           \* So the two rules already refuted here have a third sibling:
           \* "unname every input" (FoldSupersedesArrivals) loses a push
           \* that has landed, "unname an input whose uncovered objects
           \* have landed" loses one that lands during the fold, and this
           \* one loses the push that has not landed YET. Coverage — the
           \* whole of it, reachable or not — remains the only rule that
           \* survives, and the pinning must be paid for somewhere the
           \* queue cannot reach.
           \* DIRECTION 4. The SAME rule the mutation above refutes —
           \* "drop an input whose uncovered objects no ref reaches" —
           \* taken only where the observation is not ambiguous. Between
           \* the restore and the first served hook `PushSend` cannot fire
           \* at this syncer, so no pack on disk is waiting for a ref that
           \* is about to move. Reachability is read at PLAN time
           \* (`fold[s].at`), so the plan-to-commit gap is exposed rather
           \* than assumed away, and a straggler's landing is free to
           \* exploit it. Mutation ReclaimWhileServing takes the same rule
           \* out of the window and must lose.
           D == IF ReclaimBySet /\ fold[s].atRest THEN Drop
                  ELSE IF FoldSupersedesArrivals THEN S
                  ELSE IF FoldReachableCoverage
                         THEN {q \in S : (holds[q] \cap snap.history) \subseteq holds[f]}
                         ELSE IF fold[s].atRest
                                THEN {q \in S : (holds[q] \cap fold[s].at) \subseteq holds[f]}
                                ELSE {q \in S : holds[q] \subseteq holds[f]}
           named == IF FoldCasFromDisk THEN (localPacks[s] \ D) \cup {f}
                                      ELSE (belief[s].packs \ D) \cup {f} IN
       \* THE SET RULE'S ONLY CONDITION: what stays NAMED must still
       \* hold everything the refs reached when the plan was taken.
       \* Read at PLAN time (`fold[s].at`), so the plan-to-commit gap is
       \* exposed rather than assumed away and a straggler's landing is
       \* free to exploit it — the same shape the atRest rule uses.
       /\ ((ReclaimBySet /\ fold[s].atRest)
             => fold[s].at \subseteq UNION {holds[q] : q \in named})
       /\ IF snap.etag = belief[s].etag
         THEN /\ snap' = [snap EXCEPT !.etag = nextTok, !.packs = named]
              /\ nextTok' = nextTok + 1
              /\ belief' = [belief EXCEPT ![s].etag = nextTok, ![s].packs = named]
              \* The inputs leave the LISTING and stay on the DISK: a
              \* reader mid-clone keeps the pack it is streaming, and
              \* `unlink_retained` takes them a retention window later.
              \* The second conjunct is what this module used to lack
              \* entirely — the inputs simply vanished here — so the state
              \* audit F2 lives in did not exist to be reached.
              \* RETENTION EXISTS FOR READERS MID-CLONE. In the reclaim
              \* window there are none — the syncer is not serving — so a
              \* reclaim may UNLINK what it drops rather than retain it.
              \* Not cosmetic: `Listing == localPacks \ retained` excludes
              \* a retained pack PERMANENTLY, and direction 4's refutation
              \* runs entirely through that exclusion — the client retries,
              \* the retry reuses the pack NAME because names are
              \* many-to-one, and the listing cannot name it. Unlinking
              \* takes the pack off the DISK instead, so a retry's pack
              \* lands in localPacks and IS nameable.
              /\ IF ReclaimUnlinks /\ fold[s].atRest
                   THEN /\ localPacks' = [localPacks EXCEPT ![s] = (@ \ D) \cup {f}]
                        /\ retained' = retained
                   ELSE /\ localPacks' = [localPacks EXCEPT ![s] = @ \cup {f}]
                        /\ retained' = [retained EXCEPT ![s] = @ \cup D]
              /\ fold' = [fold EXCEPT ![s] = NoFold]
              /\ realMoved' = [realMoved EXCEPT ![s] = TRUE]
              /\ sensorMoved' = [sensorMoved EXCEPT ![s] = TRUE]
              /\ stragglerLand' = (stragglerLand \/ SuccessorRestored(s))
              /\ UNCHANGED <<st, lease, batch, pushState, quiet>>
         ELSE /\ Fall(s)
              /\ UNCHANGED <<snap, nextTok, belief, localPacks, retained, stragglerLand>>
  /\ UNCHANGED <<cell, packObj, idxObj, uploads, lastTok, localMain, migrating,
                 hbDue, pushTo, crashes, renewBudget, claimBudget,
                 ackNotDurable, skipOverMovement, unrestorable,
                 toldFailedButDurable, renewOverWedge, provedOffBucket,
                 provedOverRetained>>
  /\ UNCHANGED FoldPlanVars

(***************************************************************************)
(* THE PROOF, AND THE RETENTION IT MUST NOT REST ON (follow.rs, fold.rs).  *)
(*                                                                         *)
(* `fold::commit` leaves a roll-up's superseded inputs ON DISK for         *)
(* `fold_retain_secs` so a reader mid-clone keeps the pack it is           *)
(* streaming, while the ledger sweep takes them out of the BUCKET on its   *)
(* own schedule.  Between the two, the disk holds objects the bucket is    *)
(* on its way to losing.                                                   *)
(*                                                                         *)
(* This module had neither the retention nor the proof.  `FoldCommit`      *)
(* dropped the inputs from the disk in the same step that unnamed them,    *)
(* so the state audit F2 lives in did not exist here and no run could      *)
(* have reached it — the same shape as the fold defect itself, where       *)
(* `FoldPlan` DEFINED a roll-up's contents to be its inputs' union.        *)
(***************************************************************************)

\* What a proof is taken over.  `follow::prove` walks the packs the
\* SNAPSHOT NAMES, in a scratch object directory holding hardlinks to
\* those and nothing else (`gitcmd.rs`, ScopedOdb).  The mutation walks
\* the object directory, which is what shipped until 2026-09-07: it
\* makes the verdict a statement about this disk while every caller
\* reads it as one about the repository.
ProofScope(s) == IF ProveFromDisk THEN localPacks[s] ELSE belief[s].packs

\* `follow::checkpoint`, on the serving loop's slow tick and after a
\* restore: record what has been proved.  The witness fires when the
\* scope walked reaches past what the snapshot names — which is the
\* whole of the discipline, and all this action does, so in the strict
\* run it is a self-loop that costs no state.
Checkpoint(s) ==
  /\ st[s] = "serving" /\ batch[s].stage = "none"
  /\ provedOffBucket' = (provedOffBucket \/ (ProofScope(s) \ belief[s].packs # {}))
  \* The second witness is not redundant, and TLC is why it exists.  The
  \* first one's SHORTEST counterexample has nothing to do with
  \* retention: a push's pack is on disk one step before its batch names
  \* it, so a directory-scoped proof reaches past the snapshot with
  \* `retained` still empty.  That is the same defect and a fair
  \* refutation, but it is not audit F2's shape, and a run that stops
  \* there would let this module claim retention coverage it never
  \* exercised.  This one can only fire on a pack a fold superseded.
  /\ provedOverRetained' = (provedOverRetained \/ (ProofScope(s) \cap retained[s] # {}))
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localMain, localPacks, retained,
                 migrating, batch, sensorMoved, realMoved, hbDue, pushState,
                 pushTo, crashes, renewBudget, claimBudget, ackNotDurable,
                 skipOverMovement, stragglerLand, unrestorable,
                 toldFailedButDurable, renewOverWedge>>
  /\ UNCHANGED <<fold, FoldPlanVars>>

\* Retention ending (`fold::unlink_retained`): the packs a roll-up
\* superseded leave the disk.  Only ever ones the commit unnamed — the
\* retained list is populated nowhere else, which is why this is not
\* `localPacks[s] \ belief[s].packs` (that set also holds a push's pack
\* that has landed and whose batch has not named it yet).
\*
\* The whole set at once, not one pack at a time, because that is what
\* the code does: a commit stamps every input with the SAME
\* `unlink_after_unix`, and one call sweeps the list.  The restriction
\* is deliberate and it costs no coverage — `Listing` subtracts
\* `retained`, so a half-unlinked state has the same listing as the
\* state before it, and the proof witness fires on any retained pack
\* remaining.  (The code can leave one behind when a `.keep` is on the
\* stem; nothing here turns on which.)
UnlinkRetained(s) ==
  /\ retained[s] # {}
  /\ localPacks' = [localPacks EXCEPT ![s] = @ \ retained[s]]
  /\ retained' = [retained EXCEPT ![s] = {}]
  /\ UNCHANGED <<cell, nextTok, snap, packObj, idxObj, uploads, st, lease,
                 lastTok, quiet, belief, localMain, migrating, batch,
                 sensorMoved, realMoved, hbDue, pushState, pushTo, crashes,
                 renewBudget, claimBudget>>
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Witnesses

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
  /\ UNCHANGED <<fold, FoldPlanVars>> /\ UNCHANGED Untouched

Next ==
  \/ \E s \in Syncers :
       \/ AcquireCreate(s) \/ SupersedeOwn(s) \/ ClaimReleased(s) \/ ObserveForeign(s)
       \/ PollQuiet(s) \/ Takeover(s) \/ RotateSnapshot(s) \/ SweepDone(s)
       \/ Restore(s) \/ ReclaimDone(s) \/ RenewTick(s)
       \/ BatchStart(s) \/ BatchRenew(s) \/ BatchHash(s) \/ BatchInit(s)
       \/ BatchComplete(s) \/ BatchCas(s) \/ BatchLateUpload(s)
       \/ BatchRefs(s) \/ BatchAck(s)
       \/ FoldPlan(s) \/ FoldInit(s) \/ FoldComplete(s)
       \/ FoldRenew(s) \/ FoldCommit(s) \/ FoldAbandon(s)
       \/ SweepDelete(s) \/ Checkpoint(s) \/ UnlinkRetained(s)
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
    /\ WF_vars(ReclaimDone(s))
    /\ WF_vars(RenewTick(s))
    /\ WF_vars(BatchStart(s)) /\ WF_vars(BatchRenew(s)) /\ WF_vars(BatchHash(s))
    /\ WF_vars(BatchInit(s)) /\ WF_vars(BatchComplete(s)) /\ WF_vars(BatchCas(s))
    /\ WF_vars(BatchLateUpload(s)) /\ WF_vars(BatchRefs(s)) /\ WF_vars(BatchAck(s))
    /\ WF_vars(FoldInit(s)) /\ WF_vars(FoldComplete(s))
    /\ WF_vars(FoldRenew(s)) /\ WF_vars(FoldCommit(s)) /\ WF_vars(FoldAbandon(s))
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
          localMain, localPacks, retained, migrating, batch, sensorMoved, realMoved,
          hbDue, pushState, pushTo, holds, fold, foldBudget,
          crashes, renewBudget, claimBudget,
          ackNotDurable, skipOverMovement, stragglerLand, unrestorable,
          toldFailedButDurable, renewOverWedge, provedOffBucket,
          provedOverRetained>>

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

\* A PROOF IS A STATEMENT ABOUT THE BUCKET.  `follow::prove` may only
\* walk the packs the snapshot names.  The retained inputs on disk are
\* on their way out of the bucket, so a proof resting on them is true of
\* this disk and false of the repository — it passes for the whole
\* retention window and becomes false the moment the ledger sweep runs.
\* Audit F2; the code was fixed in 98293bfa, and mutation ProveFromDisk
\* is what shipped before it.
Inv_ProofIsOfTheBucket          == ~provedOffBucket

\* The same discipline, narrowed to audit F2's own shape: a proof that
\* rested on packs RETENTION is holding — the ones the ledger sweep is
\* on its way to deleting from the bucket.  Implied by the invariant
\* above and checked separately, because TLC reaches that one by a
\* shorter route (see Checkpoint).
Inv_ProofNeverRestsOnRetention  == ~provedOverRetained

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
       /\ Inv_ProofIsOfTheBucket
       /\ Inv_ProofNeverRestsOnRetention

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
