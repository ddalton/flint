------------------------------ MODULE LeanSubtree ------------------------------
(***************************************************************************)
(* The lean (checkout/publish) subtree protocol, modelled BEFORE the code  *)
(* exists — the FlintExtents posture.  Spec of record:                     *)
(* docs/plans/flint-lean-plan.md v2 (f0776e0), §2.1 barrier + §2.2         *)
(* inbox/window/takeover.  This module is deliberately SEPARATE from the   *)
(* flint corpus in formal/: lean is a separate system that reuses tier::   *)
(* as a library, and its protocol lives in the bucket, not in the hub.     *)
(*                                                                         *)
(* One subtree.  Two sidecar incarnations (A, then B after takeover), a    *)
(* gateway abstracted to its bucket effects (HITL writes, window check,    *)
(* per-request epoch validation), and the bucket substrate: lease cell,    *)
(* manifest (seq + per-path citation), whole-file objects (per-path        *)
(* generation = ETag), the inbox/window cell.                              *)
(*                                                                         *)
(* Generations model ETags: objects[p] = g means the object at p currently *)
(* has content/ETag g; 0 = absent.  If-Match is modelled as an equality    *)
(* test against the writer's expected generation.  Whole-PUT atomicity is  *)
(* assumed (real S3 gives it for PutObject).                               *)
(*                                                                         *)
(* Deliberate abstractions (tranche 1 — each is a README entry, not a      *)
(* claim of coverage):                                                     *)
(*   - The scan is ATOMIC, so the rename-vs-walk race and the             *)
(*     two-consecutive-scans deletion rule are UNREPRESENTABLE here.       *)
(*   - The 6-quiet-poll takeover observation is abstracted into a single   *)
(*     ClaimB action enabled when A is stalled or dead.  The poll protocol *)
(*     itself is machine-checked in flint's FlintTierEpoch.tla; lean       *)
(*     re-scopes it and tranche 2 may refine it here.                      *)
(*   - Checkout reads the manifest's citations without modelling           *)
(*     hydrate's 412/S3-wins arm.                                          *)
(*   - The sync verb, multi-subtree layout, partial checkout, preStop      *)
(*     timing, and all perf axes are out of scope (tranche 2+ / Phase 0b). *)
(*   - conflicts is a set of (path, generation) RECORDS.  The             *)
(*     implementation obligation this hides: a conflict record must        *)
(*     preserve the BYTES (conflict-suffixed key or versioning), not just  *)
(*     the reference — otherwise "both versions recoverable" is false.     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Paths,          \* model values, e.g. {p1, p2}
  MaxGen,         \* generation mint budget (Init uses 1; mints start at 2)
  MaxSeq,         \* manifest CAS budget
  MaxHitl,        \* HITL write budget
  MaxBarriers,    \* barrier (scan) budget
  MaxCrashes,     \* pod hard-kill budget
  MaxRestarts,    \* container-restart budget
  MaxSyncs,       \* sync-verb budget
  AllowStall,     \* enable the stall/thaw arm (the straggler world)
  \* ---- protocol arms: TRUE = plan v2; FALSE = the refuted design -------
  InboxEnabled,        \* FALSE: gateway direct-manifest-bump (the v1 reading)
  MergeCapable,        \* FALSE: whole-rewrite writer (412 -> re-seed -> overwrite)
  ConflictSurfacing,   \* FALSE: LOCAL-WINS silently (the flush.rs:1391 reuse)
  WindowCheck,         \* FALSE: gateway admits HITL during an open window
  Rotation,            \* successor CAS-rotates manifest seq before serving
  EpochCheck,          \* per-request epoch validation on sidecar writes
  GuardedGC,           \* HEAD etag-guard before the GC delete
  DeletesAfterCAS,     \* FALSE: v1 order upload -> delete -> CAS
  RematerializeOnRestart, \* TRUE = mutation: re-checkout over a live tree
  \* ---- tranche 2: the sync verb x barrier product ----------------------
  SyncEnabled,         \* FALSE in every tranche-1 cfg (spaces preserved)
  SyncScanFirst,       \* FALSE: sync judges dirt from the LAST BARRIER's
                       \* snapshot instead of its own scan (the refuted
                       \* design; the review's steady-state destruction)
  \* ---- tranche 3 product 4: the SCOPED sync verb (boundary-verbs D4) ----
  SyncScope,           \* TRUE: the sentinel's scoped form — remote changes
                       \* apply only to in-scope paths.  FALSE in every
                       \* tranche-1/2 cfg, so those state spaces are
                       \* preserved by construction.
  ScopedInstBase,      \* TRUE  = D4: a scoped sync advances the MERGE BASE
                       \* only for paths it applied or verified in scope.
                       \* FALSE = the mutation: it advances the whole
                       \* instBase to bucket-current, so every out-of-scope
                       \* foreign entry reads as already-integrated at the
                       \* next merge and is lost from the inbox flow
                       \* FOREVER.  `instBase` is the object the model has
                       \* refuted naive designs on twice; D4 rewrites its
                       \* per-path semantics, which is why it is modelled.
  \* ---- tranche 3 product 2: gated citation x version GC x backstop -----
  \* (boundary-verbs D6/D7/D8/D13.)  Generations are unique mints, so a
  \* generation IS a version id and `manifest[p]` already cites one; the
  \* substrate this product adds is `versions[p]` — the generations that
  \* still EXIST as stored versions, which on a versioned bucket is a
  \* different question from what the object currently reads as.
  \*
  \* That distinction is the whole tranche.  `Inv_NoDangling` asks
  \* "does the object exist" and was the right question until D7: gated
  \* staging makes the CITED version noncurrent, so an object can exist,
  \* read as newer uncited bytes, and have nothing behind its citation.
  GatedCitation,       \* FALSE in every pre-existing cfg: the gated
                       \* actions are disabled and `versions`/`stage`/
                       \* `withheldDel` stay frozen at Init, so every
                       \* earlier state space is preserved by construction.
  AtomicCitation,      \* TRUE = D6: ONE CAS installs the entire pending
                       \* set.  FALSE = the mutation: the install is split
                       \* across two CASes, so a reader can see half a
                       \* logical change.
  GCKeepsCurrent,      \* TRUE = the reaper never reclaims the CURRENT
                       \* version.  FALSE = the shipped rule THIS MODEL
                       \* REFUTED: "delete every version of a touched key
                       \* except the one the installed manifest cites"
                       \* deletes a foreign write that landed between the
                       \* lane and the citation — and it was current,
                       \* acked, and about to be read.
  CiteDropsInflightHitl, \* TRUE = a staged path with a LIVE INBOX ENTRY is
                       \* dropped from the boundary rather than cited
                       \* over.  The base-version check cannot see that
                       \* write (it reads the baseline, and the citation
                       \* lane consumes nothing) and the lane opens no
                       \* window, so the inbox is the only witness — and
                       \* the window CAS has already loaded it.
  \* ---- tranche 3 product 1: the boundary VERB x barrier x inbox --------
  \* (boundary-verbs D1/D2/D3/D12.)  The sentinel is a FILE in the tree,
  \* not a bucket object: the agent touches `.flint/publish`, the sidecar
  \* renames it into its own state dir (the consume), honors it with a
  \* real barrier, writes `.flint/publish.ack`, and retires the pending
  \* record.  What this product searches is the interleaving of those
  \* four steps with the barrier, the inbox, restarts and DEPOSAL.
  SentinelEnabled,     \* FALSE in every pre-existing cfg: every sentinel
                       \* action is disabled and the new sc fields stay
                       \* frozen at their (empty) Init values, so earlier
                       \* state spaces are preserved by construction.
  MaxTouches,          \* agent touch budget (a touch id doubles as the
                       \* nonce AND as the sentinel's mtime clock).
  FoldPending,         \* TRUE = D2.1: a consume never overwrites the
                       \* standing pending record, it FOLDS into it, so
                       \* every coalesced nonce is still named by the ack
                       \* that eventually lands.  FALSE = the mutation:
                       \* the rename clobbers the old record and orphans
                       \* its nonces forever.
  AckFromInstall,      \* TRUE = the uniform crash rule: an ok ack is only
                       \* ever written from a barrier that ran STRICTLY
                       \* AFTER the consume.  FALSE = the mutation the
                       \* review retracted from the draft's crash matrix:
                       \* ack from persisted state, which is the same
                       \* observable state for crash-before-CAS as for
                       \* crash-after-step-7 and asserts publication of
                       \* writes that never uploaded.
  RefuseOnFence,       \* TRUE = D2: a deposed incarnation answers every
                       \* owed sentinel with `refused-fenced` and never
                       \* with ok — on the honor path AND on the D12
                       \* heartbeat arm, which is decoupled from publish
                       \* cadence and so usually discovers deposal first.
                       \* FALSE = the mutation: success-ack-after-fence.
  FastPathGuards,      \* TRUE = the shipped skip-on-no-diff fast path
                       \* with all of its guards: nothing local, no
                       \* citation repair owed, and the remote manifest
                       \* where we left it.  FALSE = the mutation that
                       \* drops the last two.  This arm is modelled
                       \* because §10.1 records a DELIBERATE deviation
                       \* from §2.1 here — the shipped honor path lets a
                       \* no-diff sentinel take the fast path, on the
                       \* strength of a prose argument.  This is that
                       \* argument, machine-checked.
  GatedRepair,         \* TRUE = the citation lane carries the same
                       \* citation-repair the fused barrier has: re-cite
                       \* an object this sidecar INTEGRATED (a consumed
                       \* HITL write) whose citation is still behind it.
                       \* FALSE = what shipped, where the repair lived
                       \* only in the fused barrier — which gated mode
                       \* structurally never runs — so an acked HITL
                       \* write stayed cited at its predecessor forever
                       \* (§2.4.2's exemption and D13's "within one
                       \* floor", both prose-only until C2).
  LaneCancelsStaged,   \* TRUE = a withheld delete cancels the version the
                       \* stage still holds for that path (and vice
                       \* versa, in the lane).  FALSE = the shipped lane
                       \* before C3's second half: the stage and the
                       \* tombstone set both reach the citation and
                       \* merge order decides, which cites a file the
                       \* agent deleted.  FALSE in every pre-existing
                       \* cfg — including `LeanGatedInflightHitl`, whose
                       \* counterexample runs through exactly the shape
                       \* this arm closes (see the ledger note there).
  AckHonest,           \* TRUE = a gated citation that DROPPED a path the
                       \* agent declared is answered with a partial ack
                       \* naming it, never with `ok`.  FALSE = the
                       \* mutation, which is what shipped: `status: "ok"`
                       \* unconditionally, with no field in the ack
                       \* schema that could express the exception.
                       \* FALSE in every pre-existing cfg; the drop only
                       \* exists under GatedCitation, so the spaces of
                       \* the sentinel-only runs are preserved either way.
  MineIsNotForeign,    \* TRUE = an entry that matches OUR OWN BASELINE is
                       \* not a foreign change, whatever the merge base
                       \* says.  The merge base is rewritten at step 7,
                       \* so a barrier that crashed between its manifest
                       \* CAS and that rewrite leaves the workspace's own
                       \* installed entry looking like somebody else's —
                       \* and delete/modify then resolves conservatively
                       \* AGAINST the agent's own delete.  FALSE = the
                       \* shipped rule THIS MODEL REFUTED.
  BackstopEnabled      \* TRUE = the noncurrent-retention lifecycle rule
                       \* fires.  It is a BACKSTOP, never the reaper, and
                       \* enabling it is a mutation: on `files/` it cannot
                       \* tell cited from uncited, so it runs a clock
                       \* against live cited data (D8's inversion, and
                       \* §2.4.3's abandoned-mid-stage endgame).

Sidecars == {"A", "B"}

VARIABLES
  \* ---- bucket -----------------------------------------------------------
  cellEpoch,   \* subtree lease cell: current epoch (0 = never claimed)
  cellHolder,  \* "A" | "B" | "none"
  manSeq,      \* manifest document seq (the CAS token)
  manifest,    \* [Paths -> Nat]: cited generation per path (0 = uncited)
  objects,     \* [Paths -> Nat]: current object generation (0 = absent)
  inbox,       \* SUBSET (Paths \X Nat): pending HITL entries
  window,      \* 0 = closed; else the opener's epoch
  \* ---- sidecars ---------------------------------------------------------
  sc,          \* [Sidecars -> record], fields below
  \* ---- the versioned substrate (tranche 3 product 2) --------------------
  versions,    \* [Paths -> SUBSET Nat]: every generation still STORED for
               \* the path.  `objects[p]` is which one it currently reads
               \* as; a PUT over a versioned bucket destroys nothing, so
               \* the two diverge exactly while work is staged-uncited.
  stage,       \* [Sidecars -> [Paths -> Nat]]: the gated pending set —
               \* staged-but-uncited generation per path (0 = none).
  stageBase,   \* [Sidecars -> [Paths -> Nat]]: the generation the BASELINE
               \* cited when we staged.  D7's re-validation guard: if the
               \* baseline has moved by citation time, a HITL consume or a
               \* sync landed after we staged, and installing our staged
               \* generation would let work that PREDATES the foreign
               \* bytes win against them.
  withheldDel, \* [Sidecars -> SUBSET Paths]: deletes withheld from the
               \* manifest until a citation, so a rename never becomes
               \* reader-visible as gone/absent at an undeclared point.
  \* ---- environment / ghosts --------------------------------------------
  hitlAcked,   \* SUBSET (Paths \X Nat): writes acked to the user
  conflicts,   \* SUBSET (Paths \X Nat): surfaced conflict records
  gh           \* ghost/counter record, fields below

gatedVars == <<stage, stageBase, withheldDel>>

vars == <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox, window,
          sc, versions, stage, stageBase, withheldDel, hitlAcked,
          conflicts, gh>>

(* sc[s] fields:
     st       "unstarted" | "claiming" | "running" | "stalled" | "dead"
     pc       "idle" | "consumed" | "scanned" | "delDone" | "cased"
     epoch    the epoch this incarnation believes it holds
     expSeq   the manifest seq it will If-Match
     local    [Paths -> Nat]  the live tree (0 = absent)
     baseline [Paths -> Nat]  the persisted baseline snapshot
     known    SUBSET Nat      generations this incarnation recognizes:
                              checkout + own mints + surfaced consumes.
                              Blind adoption (the LOCAL-WINS mutation) does
                              NOT extend it — that is what makes silent
                              destruction attributable.
     instBase [Paths -> Nat]  the manifest view at this incarnation's last
                              install (or checkout): the MERGE BASE.  It is
                              deliberately distinct from baseline — consume
                              advances baseline (the object-ETag guard) but
                              not instBase, so a consumed adoption is not
                              mistaken for a foreign manifest entry.
     instSnap [Paths -> Nat]  the document THIS barrier installed, snapshot
                              at CASInstall.  Finish MUST advance from this
                              snapshot, never from the live manifest — a
                              HITL bump landing between install and finish
                              would otherwise be absorbed into the merge
                              base and clobbered next barrier (the
                              implementation threads `installed` through
                              for exactly this reason).
     instSeq  Nat             the seq THIS barrier installed.
     scanU    SUBSET Paths    upload set frozen at scan
     scanD    SUBSET Paths    delete-eligible set frozen at scan
     scanGen  [Paths -> Nat]  local generations frozen at scan (re-stat guard:
                              the barrier publishes walk-time content; post-
                              scan agent edits are next barrier's dirt)
     upDone   SUBSET Paths    uploaded (or adopted-own) this barrier
     parked   SUBSET Paths    412-parked this barrier
     gcDone   SUBSET Paths    delete-set entries processed this barrier
     citeDone SUBSET Paths    which staged paths THIS citation has already
                              installed.  Non-empty and not the whole
                              valid pending set = a reader can see half a
                              logical change; the single-CAS design never
                              produces that state, which is exactly why
                              the split-install mutation stays.
     stageCarried BOOLEAN     this incarnation's pending set survived a
                              lane pass (set at Scan when the stage is
                              already non-empty).  Only under gated; FALSE
                              otherwise, so earlier state spaces are
                              preserved by construction.
     sentTok  Nat             the standing (unconsumed) sentinel file:
                              0 = none, else the touch id.  A second
                              touch OVERWRITES it — the agent's own
                              doing, and not an orphan: the protocol
                              owes an ack for CONSUMED nonces.
     pendN    SUBSET Nonces   the pending record's covered nonce set.
                              Non-empty IS the record's existence
                              (`PendLive`), and its maximum IS the
                              record's covered mtime, because every
                              touch in this model carries a nonce.
     pendDirty SUBSET Paths   the paths that were LOCALLY DIRTY at the
                              consume — the agent's own un-published
                              work, which is what the boundary owes it.
                              A path that was clean at consume time
                              carries no promise: its content came from
                              the remote, and the remote is entitled to
                              move it (an inbox adoption does exactly
                              that, and TLC produced one as a
                              counterexample before this field existed).
     pendMint Nat             the generation MINT WATERMARK at consume:
                              every generation >= this was created
                              after the declaration.  D1's guarantee is
                              at-LEAST ("the published state may include
                              later bytes for a racing file, never
                              earlier ones"), so the promise cannot be
                              stated as snapshot equality — see
                              `BoundaryBroken`.
     pendCov  [Paths -> Nat]  the tree AT CONSUME TIME — D1's at-least
                              guarantee is stated at exactly that
                              instant ("every write visible on disk at
                              consume time is in the published set"),
                              so this is what the ack promises.
     honored  BOOLEAN         a barrier COMPLETED strictly after the
                              latest consume.  Lost on restart (it is
                              in-memory barrier state), which is what
                              makes the uniform crash rule reachable.
     pendReRun BOOLEAN        this pending record survived a restart —
                              the ghost `ProbeAckAfterCrash` names.
     owed     SUBSET Nonces   every nonce this incarnation has CONSUMED.
                              Never shrinks while the pod lives; dies
                              with the pod, because a pod replacement
                              takes the agent and the tree with it.
     ackN     SUBSET Nonces   `.flint/<verb>.ack`'s covered nonce set.
                              Status is deliberately absent: the shipped
                              `ack_matches` does not read it.
     lastDirty SUBSET Paths   the dirt set frozen at the LAST barrier's
                              scan.  Tracked only under SyncEnabled (it
                              stays {} otherwise, so every tranche-1
                              state space is preserved by construction).
                              This is what the refuted sync reads.
   gh fields:
     amputated  BOOLEAN  an acked HITL write silently lost (either stamp site)
     resurrected BOOLEAN an unpublished delete undone by re-materialize
     stragglerInstalls, stragglerCas, deposedPuts : Nat
     barriers, done, gc, refusals, cited, takeovers, crashes, restarts : Nat
     adoptOwn : Nat      the own-crashed-PUT 412 adoption fired
     stallUsed BOOLEAN
     nextGen, hitl : Nat
     scopedDeferrals : Nat  paths a SCOPED sync saw changed remotely and
                            deliberately left for the inbox flow (the
                            action-written non-vacuity ghost: the probe
                            names the ACTION, never the situation)
     staged, cites, reaped, withheld, forcedCites, citeSpan : Nat
     carriedCite BOOLEAN    a citation installed a pending set that had
                            survived a lane pass: the durability/visibility
                            split actually ACCUMULATED, rather than every
                            citation happening to follow its own lane
     touches, acks, honors, refusedAcks, coalesced, fastPaths,
     ackAfterRestart : Nat
     fastHonor  BOOLEAN     a pending sentinel was honored by a
                            SKIP-ON-NO-DIFF pass rather than a full
                            barrier — without this the FastPathGuards
                            runs could hold vacuously
     ackEarly   BOOLEAN     an ok ack was written while the agent's own
                            declared work was neither cited, superseded
                            by later bytes, nor surfaced
     ackIncoherent BOOLEAN  an ok ack named a manifest that did not cite
                            everything this workspace had integrated
     fencedOkAck BOOLEAN    an ok ack was written by an incarnation the
                            cell had already deposed
     foreignLost BOOLEAN    a sync advanced the merge base for a path it
                            neither applied nor surfaced a conflict for —
                            i.e. it claimed to have integrated a generation
                            it never saw.  That is exactly the silent,
                            permanent loss D4 exists to prevent: the next
                            merge computes `changed = FALSE` for it and it
                            is never queued again. *)

------------------------------------------------------------------------------
(* Helpers *)

Gens == 0..MaxGen

Nonces      == 1..MaxTouches
PendLive(s) == sc[s].pendN # {}
NoPend      == [p \in Paths |-> 0]
\* `ack_matches`: every pending nonce named by the standing ack.  The
\* implementation ALSO requires the ack's covered mtime not to be older
\* than the pending's; here every touch carries a nonce and touch ids
\* are monotone, so the subset test implies it (ledger entry).
AckMatches(s) == sc[s].pendN \subseteq sc[s].ackN

Deposed(s)  == cellEpoch > sc[s].epoch
Running(s)  == sc[s].st = "running"
Dirty(s)    == {p \in Paths : sc[s].local[p] # sc[s].baseline[p]}
USet(s)     == {p \in Dirty(s) : sc[s].local[p] # 0}
DSet(s)     == {p \in Dirty(s) : sc[s].local[p] = 0}
CitedGens   == {manifest[p] : p \in Paths} \ {0}
UploadsDone(s) == sc[s].scanU \subseteq (sc[s].upDone \cup sc[s].parked)

\* The generation an acked pair refers to is destroyed by a sidecar that
\* never legitimately learned it, with no surfaced record: the amputation
\* stamp for the destruction site.
Destroys(s, p, cur) ==
  /\ <<p, cur>> \in hitlAcked
  /\ cur \notin sc[s].known
  /\ <<p, cur>> \notin conflicts

------------------------------------------------------------------------------
(* Initial state: a project seeded with every path published at gen 1.     *)

Init ==
  /\ cellEpoch = 0 /\ cellHolder = "none" /\ manSeq = 1
  /\ manifest = [p \in Paths |-> 1]
  /\ objects  = [p \in Paths |-> 1]
  /\ inbox = {} /\ window = 0
  \* Every path starts published at gen 1, so gen 1 is its only version.
  /\ versions = [p \in Paths |-> {1}]
  /\ stage = [s \in Sidecars |-> [p \in Paths |-> 0]]
  /\ stageBase = [s \in Sidecars |-> [p \in Paths |-> 0]]
  /\ withheldDel = [s \in Sidecars |-> {}]
  /\ sc = [s \in Sidecars |->
       [st |-> "unstarted", pc |-> "idle", epoch |-> 0, expSeq |-> 0,
        local |-> [p \in Paths |-> 0], baseline |-> [p \in Paths |-> 0],
        instBase |-> [p \in Paths |-> 0],
        instSnap |-> [p \in Paths |-> 0], instSeq |-> 0,
        known |-> {}, scanU |-> {}, scanD |-> {},
        scanGen |-> [p \in Paths |-> 0], upDone |-> {}, parked |-> {},
        gcDone |-> {}, lastDirty |-> {}, stageCarried |-> FALSE,
        citeDone |-> {},
        sentTok |-> 0, pendN |-> {}, pendCov |-> [p \in Paths |-> 0],
        pendMint |-> 0, pendDirty |-> {},
        honored |-> FALSE, pendReRun |-> FALSE, owed |-> {}, ackN |-> {},
        citeDropped |-> {}]]
  /\ hitlAcked = {} /\ conflicts = {}
  /\ gh = [amputated |-> FALSE, resurrected |-> FALSE,
           stragglerInstalls |-> 0, stragglerCas |-> 0, deposedPuts |-> 0,
           barriers |-> 0, done |-> 0, gc |-> 0, refusals |-> 0,
           cited |-> 0, takeovers |-> 0, crashes |-> 0, restarts |-> 0,
           adoptOwn |-> 0, stallUsed |-> FALSE, nextGen |-> 2, hitl |-> 0,
           syncs |-> 0, syncApplied |-> 0, syncConflicts |-> 0,
           syncDestroyed |-> FALSE,
           scopedDeferrals |-> 0, foreignLost |-> FALSE,
           staged |-> 0, cites |-> 0, reaped |-> 0, withheld |-> 0,
           forcedCites |-> 0, citeSpan |-> 0,
           carriedCite |-> FALSE,
           touches |-> 0, acks |-> 0, honors |-> 0, refusedAcks |-> 0,
           coalesced |-> 0, fastPaths |-> 0, ackAfterRestart |-> 0,
           fastHonor |-> FALSE, ackEarly |-> FALSE,
           ackIncoherent |-> FALSE, fencedOkAck |-> FALSE,
           partialAcks |-> 0, declaredDrops |-> 0]

------------------------------------------------------------------------------
(* Lifecycle *)

StartA ==
  /\ sc["A"].st = "unstarted" /\ cellHolder = "none"
  /\ cellHolder' = "A" /\ cellEpoch' = 1
  /\ sc' = [sc EXCEPT
       !["A"].st = "running", !["A"].epoch = 1, !["A"].expSeq = manSeq,
       !["A"].local = [p \in Paths |-> manifest[p]],
       !["A"].baseline = [p \in Paths |-> manifest[p]],
       !["A"].instBase = [p \in Paths |-> manifest[p]],
       !["A"].known = CitedGens]
  /\ UNCHANGED <<manSeq, manifest, objects, inbox, window,
                 hitlAcked, conflicts, gh>>

CrashPod(s) ==
  /\ sc[s].st \in {"running", "stalled"}
  /\ gh.crashes < MaxCrashes
  \* Pod REPLACEMENT: the emptyDir goes, and with it the pending record,
  \* the workspace tree, `.flint/` and the agent that was waiting on it.
  \* Nothing is owed to a process that no longer exists — which is
  \* exactly why `Inv_NoNonceOrphan` is per-incarnation.
  /\ sc' = [sc EXCEPT ![s].st = "dead",
       ![s].sentTok = 0, ![s].pendN = {}, ![s].pendCov = NoPend,
       ![s].pendMint = 0, ![s].pendDirty = {},
       ![s].honored = FALSE, ![s].pendReRun = FALSE,
       ![s].owed = {}, ![s].ackN = {}]
  /\ gh' = [gh EXCEPT !.crashes = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

(* Container restart in the SAME pod: the emptyDir survives, so local,
   baseline, known, expSeq and the incarnation epoch persist; only the
   in-memory barrier state is lost.  The restart matrix's marker-present
   row: never re-materialize — unless the mutation arm says otherwise.   *)
Restart(s) ==
  /\ Running(s)
  /\ gh.restarts < MaxRestarts
  /\ LET res == \E p \in Paths :
          sc[s].local[p] = 0 /\ manifest[p] # 0 /\ sc[s].baseline[p] # 0
         newLocal ==
           IF RematerializeOnRestart
           THEN [p \in Paths |-> IF sc[s].local[p] = 0 /\ manifest[p] # 0
                                 THEN manifest[p] ELSE sc[s].local[p]]
           ELSE sc[s].local
     IN
       /\ sc' = [sc EXCEPT ![s].pc = "idle",
            \* `honored` is in-memory barrier state and dies here; the
            \* pending record is a FILE in the surviving emptyDir.  That
            \* asymmetry is the uniform crash rule's whole subject.
            ![s].honored = FALSE,
            ![s].pendReRun = IF SentinelEnabled /\ PendLive(s)
                             THEN TRUE ELSE @,
            ![s].local = newLocal,
            ![s].known = IF RematerializeOnRestart THEN @ \cup CitedGens ELSE @,
            ![s].scanU = {}, ![s].scanD = {},
            ![s].scanGen = [p \in Paths |-> 0],
            ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}]
       /\ gh' = [gh EXCEPT !.restarts = @ + 1,
            !.resurrected = @ \/ (RematerializeOnRestart /\ res)]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

(* A node freeze / partition: the process persists but takes no steps.
   Costs no crash budget — a stall is not a data-loss event (the
   WriterLimbo lesson).  One stall per run bounds the space.            *)
StallA ==
  /\ AllowStall /\ sc["A"].st = "running" /\ ~gh.stallUsed
  /\ sc' = [sc EXCEPT !["A"].st = "stalled"]
  /\ gh' = [gh EXCEPT !.stallUsed = TRUE]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

ThawA ==
  /\ sc["A"].st = "stalled"
  /\ sc' = [sc EXCEPT !["A"].st = "running"]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts, gh>>

(* A deposed incarnation's next successful cell read (heartbeat renew)
   discovers the higher epoch and self-fences.                          *)
RenewDiscover(s) ==
  /\ Running(s) /\ Deposed(s)
  \* D12 x D2.  The heartbeat renewal runs on its own interval,
  \* decoupled from publish cadence, so on deposal it is usually the
  \* FIRST arm to find out — ahead of the floor tick and ahead of a poll
  \* arm with nothing due.  It therefore owes the refused acks: exiting
  \* unsettled here strands the waiting agent with a marker still
  \* advertising live verbs, which is the hole D2 exists to close.
  \* (Shipped code returned from this arm without settling; the model
  \* was being written when the code was read, and the leg for it is
  \* `the_heartbeat_arm_settles_owed_acks_when_it_finds_the_fence`.)
  /\ LET settle == SentinelEnabled /\ RefuseOnFence /\ PendLive(s) IN
       /\ sc' = [sc EXCEPT ![s].st = "dead",
            ![s].ackN = IF settle THEN @ \cup sc[s].pendN ELSE @,
            ![s].pendN = IF settle THEN {} ELSE @,
            ![s].pendCov = IF settle THEN NoPend ELSE @,
            ![s].pendMint = IF settle THEN 0 ELSE @,
            ![s].pendDirty = IF settle THEN {} ELSE @,
            ![s].honored = FALSE, ![s].pendReRun = FALSE]
       /\ gh' = [gh EXCEPT !.refusedAcks = @ + (IF settle THEN 1 ELSE 0)]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

(* Takeover: B observes the quiet cell (abstracting the 6-poll protocol)
   and claims.  Rotation: the successor CAS-rewrites the manifest
   (seq++, content-identical) BEFORE serving — the straggler fence.     *)
ClaimB ==
  /\ sc["B"].st = "unstarted" /\ cellHolder = "A"
  /\ sc["A"].st \in {"stalled", "dead"}
  /\ (Rotation => manSeq < MaxSeq)
  /\ cellHolder' = "B" /\ cellEpoch' = cellEpoch + 1
  /\ manSeq' = IF Rotation THEN manSeq + 1 ELSE manSeq
  /\ sc' = [sc EXCEPT !["B"].st = "claiming",
                      !["B"].epoch = cellEpoch + 1]
  /\ gh' = [gh EXCEPT !.takeovers = @ + 1]
  /\ UNCHANGED <<manifest, objects, inbox, window, hitlAcked, conflicts>>

CheckoutB ==
  /\ sc["B"].st = "claiming"
  /\ sc' = [sc EXCEPT !["B"].st = "running", !["B"].expSeq = manSeq,
       !["B"].local = [p \in Paths |-> manifest[p]],
       !["B"].baseline = [p \in Paths |-> manifest[p]],
       !["B"].instBase = [p \in Paths |-> manifest[p]],
       !["B"].known = CitedGens]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts, gh>>

------------------------------------------------------------------------------
(* The agent (local edits only — the sidecar publishes them later) *)

AgentWrite(s, p) ==
  /\ Running(s) /\ gh.nextGen <= MaxGen
  /\ sc' = [sc EXCEPT ![s].local[p] = gh.nextGen,
                      ![s].known = @ \cup {gh.nextGen}]
  /\ gh' = [gh EXCEPT !.nextGen = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

AgentDelete(s, p) ==
  /\ Running(s) /\ sc[s].local[p] # 0
  /\ sc' = [sc EXCEPT ![s].local[p] = 0]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts, gh>>

------------------------------------------------------------------------------
(* The gateway, abstracted to its bucket effects *)

(* A HITL (UI) write: object PUT + inbox entry (plan v2), or a direct
   manifest bump (the refuted v1 reading, InboxEnabled = FALSE).  The
   user reads fresh, so the PUT always matches the current object.      *)
HitlWrite(p) ==
  /\ gh.hitl < MaxHitl /\ gh.nextGen <= MaxGen
  /\ ~(WindowCheck /\ window # 0)
  /\ (~InboxEnabled => manSeq < MaxSeq)
  /\ LET g == gh.nextGen IN
       /\ objects' = [objects EXCEPT ![p] = g]
       /\ IF InboxEnabled
          THEN /\ inbox' = inbox \cup {<<p, g>>}
               /\ UNCHANGED <<manifest, manSeq>>
          ELSE /\ manifest' = [manifest EXCEPT ![p] = g]
               /\ manSeq' = manSeq + 1
               /\ UNCHANGED inbox
       /\ hitlAcked' = hitlAcked \cup {<<p, g>>}
       /\ gh' = [gh EXCEPT !.nextGen = @ + 1, !.hitl = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, window, sc, conflicts>>

(* The refusal the window is FOR (availability shape, bounded to keep the
   space small; the starvation bound is a tranche-2 liveness question).  *)
HitlRefused ==
  /\ WindowCheck /\ window # 0 /\ gh.refusals < 1
  /\ gh' = [gh EXCEPT !.refusals = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, sc, hitlAcked, conflicts>>

------------------------------------------------------------------------------
(* The publish barrier (plan §2.1, seven steps; scan+intent merged, the
   consume->scan gap preserved — that gap is where strict-world parks
   come from) *)

(* Step 1: consume the inbox FIRST.  Clean paths adopt; dirty paths keep
   local (locally-dirty wins) and surface a conflict record; stale
   entries (superseded object) drop.  A barrier never runs against an
   unconsumed inbox.                                                     *)
Consume(s) ==
  /\ Running(s) /\ sc[s].pc = "idle"
  \* An honored boundary owes its ack BEFORE anything else runs: the
  \* ack write is the last step of the honoring pass, in the same
  \* single-threaded arm, so the loop cannot start another pass in
  \* between.  Without this the model interleaves a consume there and
  \* reports an incoherence the implementation cannot reach — the ack
  \* would be judged against a baseline that moved after the boundary
  \* it names.
  /\ ~(SentinelEnabled /\ sc[s].honored)
  /\ gh.barriers < MaxBarriers
  /\ LET
       live == {pr \in inbox : objects[pr[1]] = pr[2]}
       adoptable == {pr \in live :
                       sc[s].local[pr[1]] = sc[s].baseline[pr[1]]}
       conflicted == live \ adoptable
       adoptPaths == {pr[1] : pr \in adoptable}
       surfPaths == IF ConflictSurfacing
                    THEN {pr[1] : pr \in conflicted} ELSE {}
       advPaths == adoptPaths \cup surfPaths
     IN
       /\ sc' = [sc EXCEPT ![s].pc = "consumed",
            ![s].local = [p \in Paths |->
              IF p \in adoptPaths THEN objects[p] ELSE @[p]],
            ![s].baseline = [p \in Paths |->
              IF p \in advPaths THEN objects[p] ELSE @[p]],
            ![s].known = @ \cup {objects[p] : p \in advPaths}]
       /\ conflicts' = conflicts \cup
            (IF ConflictSurfacing THEN conflicted ELSE {})
       /\ inbox' = {}
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, window,
                 hitlAcked, gh>>

(* Steps 2+3: scan-diff against the persisted baseline and CAS the
   window open (the intent).  A stale window (a lower epoch's) may be
   overridden; an open window at my own or a higher epoch blocks.       *)
Scan(s) ==
  /\ Running(s) /\ sc[s].pc = "consumed"
  /\ window = 0 \/ window < sc[s].epoch
  /\ sc' = [sc EXCEPT ![s].pc = "scanned",
       ![s].scanU = USet(s), ![s].scanD = DSet(s),
       ![s].lastDirty = IF SyncEnabled THEN USet(s) \cup DSet(s) ELSE {},
       ![s].scanGen = [p \in Paths |-> sc[s].local[p]],
       \* The pending set survived a lane pass: the durability/visibility
       \* split actually ACCUMULATED across ticks, which is the claim
       \* ProbeCitationInstalled has to make non-vacuous.
       ![s].stageCarried = \E q \in Paths : stage[s][q] # 0,
       ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}]
  /\ window' = sc[s].epoch
  /\ gh' = [gh EXCEPT !.barriers = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 hitlAcked, conflicts>>

(* Step 4: guarded per-key upload.  If-Match = the persisted baseline
   generation (If-None-Match:* for creates falls out as baseline = 0).
   The 412 arm: own/known generation => adopt (the crashed-PUT resume);
   foreign => park + surface, or LOCAL-WINS overwrite under the
   mutation.  EpochCheck rejects a deposed writer per-request — the
   sidecar takes the rejection as deposal and fences.                   *)
UploadFenced(s) ==
  /\ ~GatedCitation
  /\ Running(s) /\ sc[s].pc = "scanned"
  /\ EpochCheck /\ Deposed(s)
  /\ sc[s].scanU \ (sc[s].upDone \cup sc[s].parked) # {}
  /\ sc' = [sc EXCEPT ![s].st = "dead"]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts, gh>>

Upload(s, p) ==
  /\ ~GatedCitation          \* gated replaces this with StagePut
  /\ Running(s) /\ sc[s].pc = "scanned"
  /\ p \in sc[s].scanU \ (sc[s].upDone \cup sc[s].parked)
  /\ ~(EpochCheck /\ Deposed(s))
  /\ LET cur == objects[p]
         want == sc[s].scanGen[p]
     IN
       IF cur = sc[s].baseline[p]
       THEN \* If-Match passes: the PUT lands
         /\ objects' = [objects EXCEPT ![p] = want]
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT !.deposedPuts =
                     @ + (IF Deposed(s) THEN 1 ELSE 0)]
         /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, inbox,
                        window, hitlAcked, conflicts>>
       ELSE IF cur \in sc[s].known
       THEN \* 412, but the current ETag is one I minted or consumed:
            \* adopt (my own crashed/torn earlier PUT, or content already
            \* integrated).  AdoptOwn convergence.
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT !.adoptOwn = @ + 1]
         /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects,
                        inbox, window, hitlAcked, conflicts>>
       ELSE IF ConflictSurfacing
       THEN \* foreign ETag: park the path, surface the conflict, never
            \* overwrite an ETag this sidecar did not itself publish.
         /\ sc' = [sc EXCEPT ![s].parked = @ \cup {p}]
         /\ conflicts' = conflicts \cup {<<p, cur>>}
         /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects,
                        inbox, window, hitlAcked, gh>>
       ELSE \* MUTATION: the inherited LOCAL-WINS arbitration — re-read
            \* the ETag and overwrite blind.  known does NOT grow.
         /\ objects' = [objects EXCEPT ![p] = want]
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT
              !.amputated = @ \/ Destroys(s, p, cur),
              !.deposedPuts = @ + (IF Deposed(s) THEN 1 ELSE 0)]
         /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, inbox,
                        window, hitlAcked, conflicts>>

(* Steps 5+6 in the chosen order.  The GC delete (etag-guarded HEAD:
   refuse any ETag the sidecar does not recognize).  Under
   DeletesAfterCAS = FALSE (the v1 order) the deletes run BEFORE the
   CAS — the dangling-manifest mutation.                                *)
GCDelete(s, p) ==
  /\ Running(s)
  /\ \/ (DeletesAfterCAS /\ sc[s].pc = "cased")
     \/ (~DeletesAfterCAS /\ sc[s].pc = "scanned" /\ UploadsDone(s))
  /\ p \in sc[s].scanD \ sc[s].gcDone
  /\ ~(EpochCheck /\ Deposed(s))
  /\ LET cur == objects[p] IN
       \* v2 (deletes-after-CAS): the delete set is "keys the NEW manifest
       \* no longer references" — a key the merge re-cited (delete/modify
       \* resolved foreign-wins) is NOT garbage.  The v1 order cannot make
       \* this check (no new manifest yet) — that asymmetry is part of the
       \* defect the DeletesAfterCAS mutation pins.
       IF \/ cur = 0
          \/ GuardedGC /\ cur \notin sc[s].known
          \/ DeletesAfterCAS /\ manifest[p] # 0
       THEN \* already absent, still referenced, or unrecognized ETag
         /\ sc' = [sc EXCEPT ![s].gcDone = @ \cup {p}]
         /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects,
                        inbox, window, hitlAcked, conflicts, gh>>
       ELSE
         /\ objects' = [objects EXCEPT ![p] = 0]
         /\ sc' = [sc EXCEPT ![s].gcDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT !.gc = @ + 1,
              !.amputated = @ \/ Destroys(s, p, cur)]
         /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, inbox,
                        window, hitlAcked, conflicts>>

GCDeleteFenced(s) ==
  /\ Running(s) /\ EpochCheck /\ Deposed(s)
  /\ \/ (DeletesAfterCAS /\ sc[s].pc = "cased")
     \/ (~DeletesAfterCAS /\ sc[s].pc = "scanned" /\ UploadsDone(s))
  /\ sc[s].scanD \ sc[s].gcDone # {}
  /\ sc' = [sc EXCEPT ![s].st = "dead"]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts, gh>>

PreDeletesDone(s) ==
  /\ Running(s) /\ ~DeletesAfterCAS
  /\ sc[s].pc = "scanned" /\ UploadsDone(s)
  /\ sc[s].scanD \subseteq sc[s].gcDone
  /\ sc' = [sc EXCEPT ![s].pc = "delDone"]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts, gh>>

(* Step 5: the manifest CAS.  Parked paths withhold their entry.  A
   merge-capable writer preserves foreign entries and queues them; the
   whole-rewrite writer (MergeCapable = FALSE) writes the local view and
   re-seeds on 412 — the amputation engine.  A deposed writer is caught
   by EpochCheck (per-request), by Rotation (seq mismatch => cell
   re-read => fence), or — with both arms off — LANDS: the straggler
   install.                                                             *)
CASReady(s) ==
  IF DeletesAfterCAS
  THEN sc[s].pc = "scanned" /\ UploadsDone(s)
  ELSE sc[s].pc = "delDone"

CASFenced(s) ==
  /\ Running(s) /\ CASReady(s)
  /\ EpochCheck /\ Deposed(s)
  /\ sc' = [sc EXCEPT ![s].st = "dead"]
  /\ gh' = [gh EXCEPT !.stragglerCas = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

CASMiss(s) ==
  /\ Running(s) /\ CASReady(s)
  /\ ~(EpochCheck /\ Deposed(s))
  /\ manSeq # sc[s].expSeq
  /\ IF Deposed(s)
     THEN \* the 412 handler re-reads the cell and discovers deposal
       /\ sc' = [sc EXCEPT ![s].st = "dead"]
       /\ gh' = [gh EXCEPT !.stragglerCas = @ + 1]
       /\ UNCHANGED <<window, conflicts>>
     ELSE IF MergeCapable
     THEN \* refresh the token and retry as a three-way merge
       /\ sc' = [sc EXCEPT ![s].expSeq = manSeq]
       /\ UNCHANGED <<window, conflicts, gh>>
     ELSE \* the whole-rewrite writer: re-seed and FAIL the barrier;
          \* the next barrier overwrites from the local walk
       /\ sc' = [sc EXCEPT ![s].expSeq = manSeq, ![s].pc = "idle",
            ![s].scanU = {}, ![s].scanD = {},
            ![s].scanGen = [p \in Paths |-> 0],
            ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}]
       /\ window' = 0
       /\ UNCHANGED <<conflicts, gh>>
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 hitlAcked>>

CASInstall(s) ==
  /\ ~GatedCitation          \* gated replaces this with CitePassStep
  /\ Running(s) /\ CASReady(s)
  /\ ~(EpochCheck /\ Deposed(s))
  /\ manSeq = sc[s].expSeq
  /\ manSeq < MaxSeq
  /\ LET
       \* Merge semantics: base = instBase (the last-installed view), mine
       \* = the scan-time walk, theirs = the current bucket manifest.  The
       \* merge STARTS FROM THEIRS (untouched paths keep theirs' entry —
       \* writing the walk view instead betrays a foreign entry one
       \* barrier after preserving it, once Finish absorbs it into the
       \* base).  A foreign change (theirs # base) is PRESERVED —
       \* including over a local delete (delete/modify resolves
       \* conservative).  The REPAIR arm re-cites an object this sidecar
       \* integrated (consume advanced baseline past the citation),
       \* guarded on the object still holding that generation — the
       \* implementation's citation-repair with its HEAD guard.
       \* An entry this incarnation RECOGNIZES is not a foreign change,
       \* whatever the merge base says.  Step 7 rewrites the merge base
       \* AND the baseline, so a restart between the manifest CAS and
       \* step 7 leaves both behind — and our own freshly installed
       \* entry then reads as somebody else's change at the next merge.
       \* delete/modify resolves conservatively, so the agent's delete
       \* is dropped from the boundary it is about to be acked for, and
       \* the path is queued into the inbox as a phantom conflict
       \* nobody else ever touched.  TLC found this in shipped code.
       \*
       \* `known` stands for two witnesses the implementation already
       \* has and neither of which step 7 can lose: the entry's `epoch`
       \* is the publishing writer's LEASE EPOCH, which is ours across
       \* a container restart (a HITL write carries 0, a successor a
       \* higher one); and the entry's etag can equal what our own
       \* baseline holds for the path.  Two routes, and the model
       \* produced one counterexample for each.
       foreign(p) == /\ MergeCapable
                     /\ manifest[p] # sc[s].instBase[p]
                     /\ (MineIsNotForeign => manifest[p] \notin sc[s].known)
       repair(p) ==
         /\ p \notin (sc[s].scanU \cup sc[s].scanD \cup sc[s].parked)
         /\ sc[s].baseline[p] # sc[s].instBase[p]
         /\ objects[p] = sc[s].baseline[p]
       inst == [p \in Paths |->
         IF p \in sc[s].parked THEN manifest[p]
         ELSE IF p \in sc[s].scanU \cap sc[s].upDone THEN sc[s].scanGen[p]
         ELSE IF repair(p) THEN sc[s].baseline[p]
         ELSE IF foreign(p) THEN manifest[p]
         ELSE IF p \in sc[s].scanD THEN 0
         ELSE IF MergeCapable THEN manifest[p]  \* start-from-theirs
         ELSE sc[s].scanGen[p]]                 \* whole-rewrite walk view
       foreignQ ==
         IF MergeCapable /\ InboxEnabled
         THEN {<<p, manifest[p]>> : p \in
                {q \in Paths :
                   /\ q \notin sc[s].parked \cup (sc[s].scanU \cap sc[s].upDone)
                   /\ foreign(q)
                   /\ manifest[q] # 0}}
         ELSE {}
       inbox2 == inbox \cup foreignQ
       \* An install amputates when it drops the last tracked reference to
       \* an acked generation the installer NEVER LEARNED (pr[2] \notin
       \* known).  A sidecar that consumed the write and then published a
       \* delete of it speaks for the workspace — that is integration
       \* followed by ordinary editing, not amputation.
       amp == \E pr \in hitlAcked :
                /\ (manifest[pr[1]] = pr[2]) \/ (pr \in inbox)
                /\ inst[pr[1]] # pr[2]
                /\ pr \notin inbox2
                /\ pr \notin conflicts
                /\ pr[2] \notin sc[s].known
       cite == \E pr \in hitlAcked : inst[pr[1]] = pr[2]
     IN
       /\ manifest' = inst
       /\ manSeq' = manSeq + 1
       /\ inbox' = inbox2
       /\ window' = 0
       /\ sc' = [sc EXCEPT ![s].pc = "cased",
                           ![s].instSnap = inst, ![s].instSeq = manSeq + 1]
       /\ gh' = [gh EXCEPT
            !.amputated = @ \/ amp,
            !.cited = IF cite THEN 1 ELSE @,
            !.stragglerCas = @ + (IF Deposed(s) THEN 1 ELSE 0),
            !.stragglerInstalls = @ + (IF Deposed(s) THEN 1 ELSE 0)]
  /\ UNCHANGED <<cellEpoch, cellHolder, objects, hitlAcked, conflicts>>

(* Step 7: rewrite the baseline, clear the barrier state.  The baseline
   advances ONLY for keys whose bytes this sidecar integrated (its own
   uploads and its own landed deletes) — never for merge-preserved
   foreign entries, whose bytes arrive at the next Consume.  known
   likewise never absorbs cited-but-unintegrated generations.           *)
Finish(s) ==
  /\ Running(s) /\ sc[s].pc = "cased"
  /\ DeletesAfterCAS => sc[s].scanD \subseteq sc[s].gcDone
  /\ sc' = [sc EXCEPT ![s].pc = "idle",
       \* A barrier that BEGAN after the consume has now completed: this
       \* is the only thing that entitles an ok ack (D2's uniform rule).
       ![s].honored = IF SentinelEnabled /\ PendLive(s) THEN TRUE ELSE @,
       ![s].baseline = [p \in Paths |->
         IF p \in sc[s].scanU \cap sc[s].upDone THEN sc[s].scanGen[p]
         ELSE IF p \in sc[s].scanD /\ sc[s].instSnap[p] = 0 THEN 0
         ELSE @[p]],
       ![s].instBase = sc[s].instSnap,
       ![s].expSeq = sc[s].instSeq,
       ![s].scanU = {}, ![s].scanD = {},
       ![s].scanGen = [p \in Paths |-> 0],
       ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}]
  /\ gh' = [gh EXCEPT !.done = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>


------------------------------------------------------------------------------
(* TRANCHE 2: the sync verb (v1, HITL) x the barrier.                       *)
(*                                                                          *)
(* Harness-invoked, never background, serialized against this sidecar's own *)
(* barrier (hence pc = "idle").  Sync BEGINS WITH A FULL SCAN: "locally     *)
(* dirty" means dirty per THAT scan against the baseline, never per the     *)
(* last barrier's snapshot — otherwise sync honors a remote delete (or      *)
(* fetches a remote edit) over the agent's un-scanned latest work, which is *)
(* steady-state destruction of live work by the verb itself.  Policy:       *)
(* locally-dirty wins; remote changes apply only to locally-clean paths;    *)
(* every skipped apply is a surfaced conflict, never silent.                *)

Sync(s) ==
  /\ SyncEnabled
  /\ Running(s) /\ sc[s].pc = "idle"
  /\ gh.syncs < MaxSyncs
  \* The agent's scope (D4).  A whole-tree sync is the shipped verb and
  \* is modelled as scope = Paths; the sentinel's scoped form lets TLC
  \* choose any proper non-empty subset, which is stronger than fixing
  \* one — the loss this product exists to catch depends on WHICH path
  \* is left out.
  /\ \E scope \in (IF SyncScope
                   THEN {sub \in SUBSET Paths : sub # {} /\ sub # Paths}
                   ELSE {Paths}) :
     LET
       \* Ground truth, independent of the arm under test.
       trueDirty == {p \in Paths : sc[s].local[p] # sc[s].baseline[p]}
       \* What THIS arm believes is dirty.
       dirt == IF SyncScanFirst THEN trueDirty ELSE sc[s].lastDirty
       \* Remote truth = the manifest, overlaid by live inbox entries (a
       \* HITL write no barrier has re-cited yet is still remote truth).
       remote(p) == IF \E pr \in inbox : pr[1] = p /\ objects[p] = pr[2]
                    THEN objects[p] ELSE manifest[p]
       changed == {p \in Paths : remote(p) # sc[s].instBase[p]}
       \* Out-of-scope remote changes are NOT integrated and NOT
       \* advanced: they stay foreign and reach this workspace through
       \* the normal merge -> inbox -> consume path at the next barrier.
       deferred == changed \ scope
       applicable == (changed \ dirt) \cap scope
       conflicted == (changed \cap dirt) \cap scope
       \* Which paths this sync is ENTITLED to advance the merge base
       \* for: the ones it applied, plus the ones it verified unchanged.
       \* A conflicted path is deliberately NOT advanced — its local
       \* bytes won, and the remote generation is still owed to us.
       advanced == applicable \cup (Paths \ changed)
       newInstBase ==
         IF SyncScope /\ ScopedInstBase
         THEN [p \in Paths |->
                IF p \in advanced THEN manifest[p] ELSE sc[s].instBase[p]]
         ELSE [p \in Paths |-> manifest[p]]
       \* THE LOSS STAMP.  The merge base moved for a path this sync
       \* neither applied nor surfaced a conflict for, and whose bytes
       \* we do not hold: we have just claimed to have integrated a
       \* generation we never saw.  `foreign(p)` is FALSE at every
       \* subsequent merge, so it is never queued again — silent and
       \* permanent.
       lost == \E p \in Paths :
                 /\ p \notin applicable
                 /\ p \notin conflicted
                 /\ newInstBase[p] # sc[s].instBase[p]
                 /\ sc[s].baseline[p] # remote(p)
       \* A DESTROYING apply: the path was TRULY dirty, sync moved its
       \* local content anyway (a remote fetch or a remote-delete), and
       \* no conflict was surfaced for it.  Under SyncScanFirst this is
       \* unreachable by construction; under the refuted arm it is the
       \* review's finding.
       destroys == \E p \in applicable :
                     /\ p \in trueDirty
                     /\ remote(p) # sc[s].local[p]
     IN
       /\ sc' = [sc EXCEPT
            ![s].local = [p \in Paths |->
              IF p \in applicable THEN remote(p) ELSE @[p]],
            ![s].baseline = [p \in Paths |->
              IF p \in applicable THEN remote(p) ELSE @[p]],
            ![s].instBase = newInstBase,
            ![s].known = @ \cup {remote(p) : p \in applicable},
            ![s].lastDirty = {}]
       /\ conflicts' = conflicts \cup {<<p, remote(p)>> : p \in conflicted}
       /\ gh' = [gh EXCEPT !.syncs = @ + 1,
            !.syncApplied = @ + Cardinality(applicable),
            !.syncConflicts = @ + Cardinality(conflicted),
            !.syncDestroyed = @ \/ destroys,
            !.scopedDeferrals = @ + (IF SyncScope THEN Cardinality(deferred) ELSE 0),
            !.foreignLost = @ \/ lost]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked>>

------------------------------------------------------------------------------
(* TRANCHE 3, PRODUCT 2: gated manifest advance — durability split from     *)
(* visibility (boundary-verbs D6/D7/D8/D13).                                *)
(*                                                                          *)
(* Two lanes instead of one fused barrier:                                  *)
(*   - the UPLOAD LANE puts in place, minting a new VERSION and citing       *)
(*     nothing.  The cited generation survives as a noncurrent version —     *)
(*     that is the premise the whole design rests on, and it is what makes   *)
(*     `objects[p]` (what the key reads as) a different question from        *)
(*     `versions[p]` (what is still fetchable).                              *)
(*   - the CITATION LANE installs the entire pending set in ONE CAS, then    *)
(*     applies the withheld deletes and runs the EXACT version reaper.       *)
(*                                                                          *)
(* The lane opens no HITL window; window open/clear belong to the citation.  *)
(* Modelled abstractions, named rather than assumed: the citation and its    *)
(* reaper are one step (the real code holds the HITL window across both, so  *)
(* no foreign write can interleave), and the four citation SOURCES collapse  *)
(* to nondeterminism — a citation is enabled whenever the stage is           *)
(* non-empty, which is strictly more permissive than any of them.            *)

Staged(s) == {p \in Paths : stage[s][p] # 0}

\* D7's re-validation: a staged entry is still installable only if the
\* baseline it staged against has not moved under it.
(* Which staged entries this citation may install.
   D7 ALSO specifies a base-version re-validation — drop a staged entry
   whose BASELINE moved under it.  That guard is deliberately NOT modelled
   as an arm, because the model showed it is UNREACHABLE given the lane's
   own discipline: the lane never advances the baseline, so a staged path
   is by construction locally-dirty, and every route that could move a
   baseline (consume, sync) refuses dirty paths and surfaces a conflict
   instead.  It stays in the implementation as defence in depth; what the
   model says is that it is not what protects anything today, and the
   hazard it was written for arrives by a route it cannot see. *)
Valid(s) ==
  IF CiteDropsInflightHitl
  THEN {p \in Staged(s) : ~\E pr \in inbox : pr[1] = p}
  ELSE Staged(s)

(* The upload lane.  Same guard chain as Upload — If-Match on the
   recognized baseline, AdoptOwn on a known ETag, park on a foreign one —
   but the PUT lands as a new VERSION and no manifest CAS runs.          *)
StagePut(s, p) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "scanned"
  /\ p \in sc[s].scanU \ (sc[s].upDone \cup sc[s].parked)
  /\ ~(EpochCheck /\ Deposed(s))
  /\ LET cur == objects[p]
         want == sc[s].scanGen[p]
     IN
       IF cur = sc[s].baseline[p]
       THEN
         /\ objects' = [objects EXCEPT ![p] = want]
         /\ versions' = [versions EXCEPT ![p] = @ \cup {want}]
         /\ stage' = [stage EXCEPT ![s] = [@ EXCEPT ![p] = want]]
         /\ stageBase' = [stageBase EXCEPT ![s] = [@ EXCEPT ![p] = cur]]
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT !.staged = @ + 1,
                     !.deposedPuts = @ + (IF Deposed(s) THEN 1 ELSE 0)]
         /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, inbox,
                        window, withheldDel, hitlAcked, conflicts>>
       ELSE IF cur \in sc[s].known /\ cur = want
       THEN \* our own crashed/torn earlier PUT of THESE bytes: adopt the
            \* current version as the staged one.  The equality matters
            \* and the model was coarser than the code without it —
            \* `upload_one`'s 412 arm adopts only when the object's CRC
            \* is the CRC of the body it is uploading, and otherwise
            \* SUPERSEDES it knowingly (the arm below).  Adopting any
            \* recognized generation stages bytes the agent has already
            \* replaced, which reads as a boundary that lost the
            \* agent's declared write — a counterexample the
            \* implementation cannot produce.
         /\ stage' = [stage EXCEPT ![s] = [@ EXCEPT ![p] = cur]]
         /\ stageBase' = [stageBase EXCEPT ![s] = [@ EXCEPT ![p] = cur]]
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT !.adoptOwn = @ + 1]
         /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects,
                        versions, inbox, window, withheldDel, hitlAcked,
                        conflicts>>
       ELSE IF cur \in sc[s].known
       THEN \* our own earlier PUT, OLDER content: supersede it knowingly
            \* (If-Match on what we recognize), which is what the shipped
            \* arm does rather than citing the stale generation.
         /\ objects' = [objects EXCEPT ![p] = want]
         /\ versions' = [versions EXCEPT ![p] = @ \cup {want}]
         /\ stage' = [stage EXCEPT ![s] = [@ EXCEPT ![p] = want]]
         /\ stageBase' = [stageBase EXCEPT ![s] = [@ EXCEPT ![p] = cur]]
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT !.adoptOwn = @ + 1,
                     !.deposedPuts = @ + (IF Deposed(s) THEN 1 ELSE 0)]
         /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, inbox,
                        window, withheldDel, hitlAcked, conflicts>>
       ELSE \* foreign ETag: park and surface; never overwrite.
         /\ sc' = [sc EXCEPT ![s].parked = @ \cup {p}]
         /\ conflicts' = conflicts \cup {<<p, cur>>}
         /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects,
                        versions, inbox, window, stage, stageBase,
                        withheldDel, hitlAcked, gh>>

StagePutFenced(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "scanned"
  /\ EpochCheck /\ Deposed(s)
  /\ sc[s].scanU \ (sc[s].upDone \cup sc[s].parked) # {}
  /\ sc' = [sc EXCEPT ![s].st = "dead"]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, versions,
                 inbox, window, stage, stageBase, withheldDel, hitlAcked,
                 conflicts, gh>>

(* The lane ends.  Deletes are WITHHELD — a rename r->s must not become
   reader-visible as r-gone/s-absent at a point nobody declared.

   ...and a withheld delete CANCELS any version this stage was still
   holding for that path.  The stage and the tombstone set carry no
   ordering between them, so a citation handed both installs one by
   accident of merge order: the shipped merge applied deletes last
   (right for create-then-delete, which amputates a re-created file),
   and making upserts win instead cites a file the agent deleted.  Only
   the lane knows which it saw last, so the lane is where they cancel.
   TLC found this the first time the sentinel and the citation lane ran
   in one world — against a fix two hours old.                          *)
LaneDone(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "scanned"
  /\ sc[s].scanU \subseteq (sc[s].upDone \cup sc[s].parked)
  /\ withheldDel' = [withheldDel EXCEPT ![s] = @ \cup sc[s].scanD]
  /\ stage' = [stage EXCEPT ![s] =
       [p \in Paths |-> IF LaneCancelsStaged /\ p \in sc[s].scanD THEN 0 ELSE @[p]]]
  /\ stageBase' = [stageBase EXCEPT ![s] =
       [p \in Paths |-> IF LaneCancelsStaged /\ p \in sc[s].scanD THEN 0 ELSE @[p]]]
  /\ sc' = [sc EXCEPT ![s].pc = "laneDone"]
  /\ gh' = [gh EXCEPT !.withheld = @ + Cardinality(sc[s].scanD \ withheldDel[s])]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, versions,
                 inbox, window, hitlAcked, conflicts>>

(* No coherent point is due: the tick ends with the bytes DURABLE and the
   manifest un-advanced, and the pending set survives to the next lane.
   This is the mode working, and it is the state every invariant in this
   product has to hold in.                                              *)
LaneOnly(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "laneDone"
  /\ sc[s].citeDone = {}
  /\ window' = 0
  /\ sc' = [sc EXCEPT ![s].pc = "idle",
       ![s].scanU = {}, ![s].scanD = {},
       ![s].scanGen = [p \in Paths |-> 0],
       ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, versions,
                 inbox, stage, stageBase, withheldDel, hitlAcked, conflicts,
                 gh>>

CiteFenced(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "laneDone"
  /\ EpochCheck /\ Deposed(s)
  /\ Valid(s) \ sc[s].citeDone # {}
  /\ sc' = [sc EXCEPT ![s].st = "dead"]
  /\ gh' = [gh EXCEPT !.stragglerCas = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, versions,
                 inbox, window, stage, stageBase, withheldDel, hitlAcked,
                 conflicts>>

(* THE citation.  Under AtomicCitation it installs the whole valid
   pending set in one CAS — the versions already exist, so there is no
   copy phase and no half-boundary.  Under the mutation TLC may install
   any non-empty subset, which is what a two-CAS design looks like from
   a reader's side.                                                     *)
CitePassStep(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "laneDone"
  /\ ~(EpochCheck /\ Deposed(s))
  /\ manSeq = sc[s].expSeq
  /\ manSeq < MaxSeq
  /\ Valid(s) \ sc[s].citeDone # {}
  /\ \E sub \in SUBSET (Valid(s) \ sc[s].citeDone) :
       /\ sub # {}
       /\ AtomicCitation => sub = Valid(s) \ sc[s].citeDone
       /\ LET \* §2.4.2's ungated repair, in the lane that never had it.
              \* Same shape and same guard as the fused barrier's arm
              \* (`repair(p)`, in Finish): the object must still hold
              \* the generation this workspace integrated, or the
              \* citation would name bytes it never saw.
              repairG(p) ==
                /\ GatedRepair
                /\ p \notin sub
                /\ sc[s].baseline[p] # sc[s].instBase[p]
                /\ objects[p] = sc[s].baseline[p]
              inst == [p \in Paths |->
                IF p \in sub THEN stage[s][p]
                ELSE IF repairG(p) THEN sc[s].baseline[p]
                ELSE manifest[p]]
              \* This citation names a generation OTHER than the acked
              \* user bytes the key currently holds, and says nothing
              \* about it.  Not "the agent integrated the user's bytes
              \* and then edited" — `known` exempts that case throughout
              \* this model — but work that PREDATES the user's write
              \* winning against it.  Two arms reach it, and each has its
              \* own guard: the write was consumed after we staged (D7's
              \* base re-validation), or it is still in flight in the
              \* inbox (the window the lane deliberately does not open).
              staleWin == \E p \in sub :
                /\ <<p, objects[p]>> \in hitlAcked
                /\ objects[p] # stage[s][p]
                /\ <<p, objects[p]>> \notin conflicts
              forced == \E p \in sub : sc[s].local[p] # stage[s][p]
          IN
            /\ manifest' = inst
            /\ manSeq' = manSeq + 1
            /\ sc' = [sc EXCEPT ![s].citeDone = @ \cup sub,
                                ![s].expSeq = manSeq + 1,
                                ![s].instSnap = inst,
                                ![s].instSeq = manSeq + 1]
            /\ gh' = [gh EXCEPT
                 !.cites = @ + 1,
                 !.amputated = @ \/ staleWin,
                 !.forcedCites = @ + (IF forced THEN 1 ELSE 0),
                 !.citeSpan = IF Cardinality(sub) > @ THEN Cardinality(sub) ELSE @,
                 !.carriedCite = @ \/ (sc[s].stageCarried /\ Cardinality(sub) > 1),
                 !.stragglerCas = @ + (IF Deposed(s) THEN 1 ELSE 0),
                 !.stragglerInstalls = @ + (IF Deposed(s) THEN 1 ELSE 0)]
  /\ UNCHANGED <<cellEpoch, cellHolder, objects, versions, inbox, window,
                 stage, stageBase, withheldDel, hitlAcked, conflicts>>

(* The citation completes: withheld deletes land WITH it, the EXACT
   version reaper runs, the baseline advances, the window clears.

   The reaper is flint's own and it is the ONLY reaper that can tell
   cited from uncited.  Lifecycle cannot do this job on `files/`: gated
   staging makes the cited version noncurrent the moment a newer
   generation is staged, so a NoncurrentVersionExpiration rule runs a
   clock against live cited data and never reaches the newest uncited
   bytes, which are current.  That inversion is BackstopExpire.         *)
CiteFinish(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "laneDone"
  /\ sc[s].citeDone # {} /\ sc[s].citeDone = Valid(s)
  \* Two different questions, and the shipped code answers them in two
  \* different places: may this boundary UNCITE the path (the manifest
  \* CAS — yes, the agent deleted it), and may it DELETE THE OBJECT (the
  \* GC, which HEADs first and refuses an etag it does not recognize).
  \* Guarding the uncite with the GC's guard, as this model did, means a
  \* foreign write to a path the agent deleted keeps the path CITED —
  \* and an ok ack for a boundary that declared it gone.  TLC reported
  \* that as a defect; the defect was in the model.
  /\ LET uncite == {p \in withheldDel[s] :
                      /\ manifest[p] # 0
                      /\ p \notin sc[s].citeDone}
         dels == {p \in uncite : ~GuardedGC \/ objects[p] \in sc[s].known}
         man2 == [p \in Paths |-> IF p \in uncite THEN 0 ELSE manifest[p]]
         obj2 == [p \in Paths |-> IF p \in dels THEN 0 ELSE objects[p]]
         \* The reaper runs over the paths this citation INSTALLED and
         \* keeps what the INSTALLED DOCUMENT cites — never the writer's
         \* own idea of what it cited.
         scope == sc[s].citeDone
         \* ...plus the CURRENT version, unconditionally.  If current is
         \* not what we just cited then a foreign write landed between
         \* the lane and this citation: live bytes somebody is about to
         \* read, not a generation this workspace superseded.  THE MODEL
         \* FOUND THIS — the rule without this clause deleted an acked
         \* HITL write in shipped code.
         current(p) == IF GCKeepsCurrent /\ obj2[p] # 0 THEN {obj2[p]} ELSE {}
         ver2 == [p \in Paths |->
                    IF p \in scope /\ man2[p] # 0
                    \* FAIL CLOSED: if the installed manifest names no
                    \* version for this path we do not know what is
                    \* cited, and "delete everything unrecognized" would
                    \* reap live data.  Reclaim nothing.
                    THEN (versions[p] \cap {man2[p]}) \cup current(p)
                    ELSE versions[p]]
         dropped == Staged(s) \ Valid(s)
     IN
       /\ manifest' = man2
       /\ objects' = obj2
       /\ versions' = ver2
       /\ stage' = [stage EXCEPT ![s] = [p \in Paths |-> 0]]
       /\ stageBase' = [stageBase EXCEPT ![s] = [p \in Paths |-> 0]]
       /\ withheldDel' = [withheldDel EXCEPT ![s] = @ \ uncite]
       /\ window' = 0
       \* A dropped staged generation is never silently forgotten.
       /\ conflicts' = conflicts \cup {<<p, sc[s].baseline[p]>> : p \in dropped}
       /\ sc' = [sc EXCEPT ![s].pc = "idle",
            ![s].honored = IF SentinelEnabled /\ PendLive(s) THEN TRUE ELSE @,
            \* Which paths this boundary does NOT carry.  Kept apart
            \* from `conflicts` on purpose: a conflict record is the
            \* ack's `report.parked` in the FUSED path, which is why
            \* BoundaryBroken exempts it — a correspondence the gated
            \* honor cannot maintain, because the drop happens inside
            \* the citation and the honor writes one ack for the lot.
            ![s].citeDropped = dropped,
            ![s].citeDone = {}, ![s].stageCarried = FALSE,
            ![s].instBase = man2,
            ![s].baseline = [p \in Paths |->
              IF p \in sc[s].citeDone THEN stage[s][p]
              ELSE IF p \in uncite THEN 0
              ELSE @[p]],
            ![s].known = @ \cup {stage[s][p] : p \in sc[s].citeDone},
            ![s].scanU = {}, ![s].scanD = {},
            ![s].scanGen = [p \in Paths |-> 0],
            ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}]
       /\ gh' = [gh EXCEPT !.done = @ + 1,
            !.gc = @ + Cardinality(dels),
            !.reaped = @ + Cardinality(UNION {versions[p] \ ver2[p] : p \in Paths}),
            !.cited = IF \E pr \in hitlAcked : man2[pr[1]] = pr[2]
                      THEN 1 ELSE @,
            !.declaredDrops = @ + (IF SentinelEnabled /\ PendLive(s)
                                   THEN Cardinality(dropped \cap sc[s].pendDirty)
                                   ELSE 0)]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, inbox, hitlAcked>>

(* The noncurrent-retention BACKSTOP.  It is not the reaper and it must
   never be creditable for the reaper's work: on `files/` it cannot tell
   cited from uncited, so when a workspace is abandoned mid-stage it
   reaps the CITED (now noncurrent) version while the uncited current
   one survives.  The manifest then dangles — checkout refuses rather
   than serving a hole, and `recover-staged` re-cites the survivor
   FORWARD.  Enabling this action is a mutation, and the counterexample
   it must find IS the abandoned-mid-stage endgame.                     *)
BackstopExpire(p) ==
  /\ GatedCitation /\ BackstopEnabled
  /\ \E g \in versions[p] :
       /\ g # objects[p]          \* noncurrent only, exactly as S3
       /\ versions' = [versions EXCEPT ![p] = @ \ {g}]
  /\ gh' = [gh EXCEPT !.reaped = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, sc, stage, stageBase, withheldDel, hitlAcked,
                 conflicts>>

GatedNext ==
  \/ \E s \in Sidecars, p \in Paths : StagePut(s, p)
  \/ \E s \in Sidecars :
       StagePutFenced(s) \/ LaneDone(s) \/ LaneOnly(s) \/ CiteFenced(s)
       \/ CitePassStep(s) \/ CiteFinish(s)
  \/ \E p \in Paths : BackstopExpire(p)

------------------------------------------------------------------------------
(* TRANCHE 3, PRODUCT 1: the boundary VERB x the barrier x the inbox.      *)
(*                                                                         *)
(* The agent declares a coherent point by touching `.flint/publish`; the   *)
(* sidecar consumes it (rename into its own state dir), honors it with a   *)
(* real barrier, writes `.flint/publish.ack`, and retires the pending      *)
(* record.  Four steps, each of which a crash, a restart or a deposal can  *)
(* land between, and all of them racing the inbox and the manifest CAS.    *)
(*                                                                         *)
(* Modelled abstractions, named rather than assumed:                       *)
(*   - The two verbs collapse to ONE.  `sync` differs in what its honor    *)
(*     does (tranche 2/product 4 model that) and not in the consume /      *)
(*     honor / ack / retire protocol, which is what this product searches. *)
(*   - A touch id doubles as the nonce and as the sentinel's mtime clock:  *)
(*     ids are monotone, so "the ack's covered mtime is not older than the *)
(*     pending's" is implied by the nonce subset test in `AckMatches`.     *)
(*   - The bare touch (a sentinel with no nonce) is not modelled; every    *)
(*     touch carries one, which is what lets `pendN # {}` stand in for     *)
(*     "the pending file exists".  The torn-body rule is a battery leg.    *)
(*   - The min-interval and the hourly work budget are OUT of the safety   *)
(*     gate (the house rule for rate limiting): a deferred honor is        *)
(*     modelled as a later honor, which is strictly more permissive.       *)
(*   - The refusal (write the refused ack, retire, flip the marker, exit)  *)
(*     is one step, as is the ack rename.  The crash point that matters —  *)
(*     between the ack and the retire — is modelled, because it is the one *)
(*     the retracted crash matrix got wrong.                               *)

(* The agent's declaration.  A second touch overwrites an unconsumed
   sentinel: one file, one body.  That is the agent's own doing and not
   an orphan — the protocol owes an ack for nonces it CONSUMED.         *)
Touch(s) ==
  /\ SentinelEnabled /\ Running(s)
  /\ gh.touches < MaxTouches
  /\ sc' = [sc EXCEPT ![s].sentTok = gh.touches + 1]
  /\ gh' = [gh EXCEPT !.touches = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

(* The consume: rename the sentinel out of the agent's reach and FOLD it
   into the standing pending record (D2.1).  Folding rather than
   overwriting is the whole rule — a rename onto a live pending record
   clobbers its nonces, and the agents behind them wait forever.

   `honored` is cleared unconditionally: D1's guarantee is that the
   barrier which acknowledges a sentinel BEGINS ITS SCAN strictly after
   consuming it, so a barrier that completed before this consume
   entitles nothing.                                                    *)
TakeSentinel(s) ==
  /\ SentinelEnabled /\ Running(s)
  /\ sc[s].sentTok # 0
  /\ sc[s].pc = "idle"
  /\ LET t == sc[s].sentTok
         fold == FoldPending /\ PendLive(s)
     IN
       /\ sc' = [sc EXCEPT
            ![s].sentTok = 0,
            ![s].pendN = IF fold THEN @ \cup {t} ELSE {t},
            ![s].pendCov = sc[s].local,
            ![s].pendMint = gh.nextGen,
            ![s].pendDirty = Dirty(s),
            ![s].honored = FALSE,
            ![s].owed = @ \cup {t}]
       /\ gh' = [gh EXCEPT !.coalesced = @ + (IF fold THEN 1 ELSE 0)]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

(* Skip-on-no-diff (`barrier.rs`): nothing local to publish, no citation
   repair owed, and the remote manifest document where we left it — so
   every local byte is already cited and the barrier returns without a
   window, without a CAS, and without touching the manifest.
   §2.1 prescribes that a pending sentinel DEFEAT this fast path;
   §10.1 records why the shipped code deliberately does not (§7 prices a
   no-diff honor at one HEAD, and defeating it would cost a manifest CAS
   at up to 720/hour/workspace — the exact amplification the budget
   exists to prevent).  That deviation rests on an argument in a
   document.  `FastPathGuards` is that argument, machine-checked: with
   the guards on the strict runs must hold, and with the last two
   dropped the ack must be caught claiming a boundary that is not
   installed.                                                           *)
FastPathClean(s) ==
  /\ USet(s) = {} /\ DSet(s) = {}
  /\ (~FastPathGuards \/ \A p \in Paths : sc[s].baseline[p] = sc[s].instBase[p])
  /\ (~FastPathGuards \/ manSeq = sc[s].expSeq)

FastPath(s) ==
  /\ SentinelEnabled /\ ~GatedCitation
  /\ Running(s) /\ sc[s].pc = "consumed"
  \* A no-diff pass IS a barrier tick and charges the barrier budget.
  \* Not bookkeeping: without it Consume -> FastPath -> Consume is a
  \* free cycle that consumes nothing, and the state graph's DIAMETER
  \* grows without bound (the pilot ran to depth 148 and 17M states
  \* before this line existed).
  /\ gh.barriers < MaxBarriers
  /\ FastPathClean(s)
  /\ sc' = [sc EXCEPT ![s].pc = "idle",
       ![s].honored = IF PendLive(s) THEN TRUE ELSE @,
       ![s].lastDirty = IF SyncEnabled THEN {} ELSE @]
  /\ gh' = [gh EXCEPT !.barriers = @ + 1, !.fastPaths = @ + 1,
                      !.fastHonor = @ \/ PendLive(s)]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

(* THE PROMISE, evaluated at the instant the ack is written and stamped
   into a ghost (the house's action-written rule).  D1: "everything
   ordered-before T is a coherent point; publish it", with the at-least
   guarantee stated at CONSUME time — so every path whose consume-time
   content is still the tree's content must be cited by the manifest the
   ack names.  Two exemptions, both of them the protocol working:

     - the AGENT itself moved the path after the consume, so its
       declared bytes are superseded rather than owed.  A DELETE is
       the case that needs this clause on its own: it supersedes
       without minting anything, so the watermark below cannot see it,
       and TLC's third counterexample was exactly an agent deleting
       its own declared file after declaring it.
     - the citation names a generation MINTED AFTER the declaration.
       D1's guarantee is at-LEAST: "the published state may include
       later bytes for a racing file, never earlier ones".  Stating the
       promise as snapshot equality is WRONG, and TLC said so on the
       first strict run of this product — its counterexample was an
       agent that deleted a path, declared, re-created it, let the
       barrier publish the re-creation, then deleted it again, so the
       consume-time snapshot matched the tree again at ack time while
       the manifest legitimately cited later bytes.  The mint watermark
       is what distinguishes "later" from "stale", and it is why
       `pendMint` exists.
     - the path carries a surfaced conflict record — it was answered
       loudly, which is `report.parked` in the ack the implementation
       writes.  "Never a silent winner" is the standing rule; a silent
       LOSER is what this invariant is looking for.

   The two exemptions overlap and neither subsumes the other, which is
   why both are here and each has a counterexample behind it: an agent
   can move a path away and back (write, delete, write) so that the
   tree matches the declaration again at ack time while the manifest
   legitimately cites later bytes — the watermark is what covers that —
   and it can supersede by deleting, which mints nothing at all — the
   tree comparison is what covers that.  What neither covers is an
   agent restoring byte-identical content, which this model cannot
   express (mints are unique) and a real filesystem can: ledger entry,
   and harmless, because the boundary then names bytes equal to the
   declared ones.

   Note what the conflict clause does NOT excuse: a citation of a
   generation minted BEFORE the declaration and different from the
   declared one is stale whichever direction it points — an unpublished
   write (manifest older than the agent's bytes) and an unpublished
   DELETE (the agent declared the path gone, the manifest still cites
   its old generation) are the same defect and the same test.          *)
BoundaryBroken(s) ==
  \E p \in sc[s].pendDirty :
    /\ manifest[p] # sc[s].pendCov[p]
    /\ manifest[p] < sc[s].pendMint
    /\ sc[s].local[p] = sc[s].pendCov[p]
    \* The conflict exemption reads the ack's `report.parked`.  A path
    \* the CITATION dropped is in no such field — the gated honor
    \* writes one ack for the whole boundary — so its record excuses
    \* nothing here.  That is the exemption holding the finding's own
    \* combined world vacuously green until it was narrowed.  Note the
    \* shape: the drop DEFEATS the exemption, it does not exclude the
    \* path from the search.  Writing it as a plain conjunct excuses
    \* exactly the case the clause exists to catch.
    /\ (p \in sc[s].citeDropped \/ ~\E pr \in conflicts : pr[1] = p)

(* The OTHER half of what an ok ack asserts, and it is not the same
   claim.  `BoundaryBroken` asks whether the agent's own declared work
   survived; this asks whether the point the ack names is a coherent
   one AT ALL: every generation this workspace has integrated — its
   persisted baseline, which includes the inbox writes it adopted and
   the foreign entries it merged — is cited by the manifest the ack
   points at.  When it is not, a reader resolving that manifest gets
   bytes this workspace has already superseded, and a re-checkout would
   MATERIALIZE them over the newer ones.

   This is what `repairs_pending` defends in the shipped fast path, and
   why the two halves need separate invariants: TLC's second
   counterexample was an inbox adoption whose citation repair was still
   owed — no work of the agent's was at risk, and the boundary was
   still not the point the ack claimed.  Parked paths are exempt:
   their conflict record is the ack's `report.parked`.               *)
BoundaryIncoherent(s) ==
  \E p \in Paths :
    /\ sc[s].baseline[p] # manifest[p]
    /\ ~\E pr \in conflicts : pr[1] = p

AckOk(s) ==
  /\ SentinelEnabled /\ Running(s) /\ sc[s].pc = "idle"
  /\ PendLive(s) /\ ~AckMatches(s)
  /\ ~(RefuseOnFence /\ Deposed(s))
  /\ (AckFromInstall => sc[s].honored)
  \* D1, at the one place a gated boundary can break it: a citation
  \* that dropped a declared path installed a point that does not
  \* carry it, and `ok` would assert the opposite.
  /\ (AckHonest => sc[s].pendDirty \cap sc[s].citeDropped = {})
  /\ sc' = [sc EXCEPT ![s].ackN = @ \cup sc[s].pendN, ![s].honored = FALSE,
       ![s].citeDropped = {}]
  /\ gh' = [gh EXCEPT
       !.acks = @ + 1,
       !.honors = @ + (IF sc[s].honored THEN 1 ELSE 0),
       !.ackAfterRestart = @ + (IF sc[s].pendReRun THEN 1 ELSE 0),
       !.ackEarly = @ \/ BoundaryBroken(s),
       !.ackIncoherent = @ \/ BoundaryIncoherent(s),
       !.fencedOkAck = @ \/ Deposed(s)]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

(* The honest answer when the boundary does not carry the declared
   point: the agent is still ANSWERED — a partial ack names the nonces
   and the dropped paths — but nothing claims the point landed.  An
   agent that treats it as failure and re-touches is behaving
   correctly, and the next boundary carries the path the ordinary way
   (the racing write is queued in the inbox; the lane consumes it, the
   local file is still dirty, and the conflict rule publishes it).

   `Inv_NoNonceOrphan` is what makes this an ANSWER rather than a
   silence, and it is checked over this action like any other.        *)
AckPartial(s) ==
  /\ SentinelEnabled /\ AckHonest /\ Running(s) /\ sc[s].pc = "idle"
  /\ PendLive(s) /\ ~AckMatches(s)
  /\ ~(RefuseOnFence /\ Deposed(s))
  /\ sc[s].pendDirty \cap sc[s].citeDropped # {}
  /\ sc' = [sc EXCEPT ![s].ackN = @ \cup sc[s].pendN, ![s].honored = FALSE,
       ![s].citeDropped = {}]
  /\ gh' = [gh EXCEPT !.acks = @ + 1, !.partialAcks = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

(* Retire AFTER the ack rename.  Splitting these two is not ceremony:
   the crash between them is the one the draft's crash matrix answered
   wrongly, and it is the only reachable way to observe a pending record
   that a standing ack already answers.                                 *)
RetirePending(s) ==
  /\ SentinelEnabled /\ Running(s) /\ sc[s].pc = "idle"
  /\ PendLive(s) /\ AckMatches(s)
  /\ sc' = [sc EXCEPT ![s].pendN = {}, ![s].pendCov = NoPend,
       ![s].pendMint = 0, ![s].pendDirty = {},
       ![s].honored = FALSE, ![s].pendReRun = FALSE]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts, gh>>

(* D2's refused ack: deposal must never strand a waiting agent.  Write
   the refusal naming every covered nonce, retire, flip the marker, and
   exit fenced — one step here.                                         *)
AckRefused(s) ==
  /\ SentinelEnabled /\ Running(s)
  /\ PendLive(s) /\ RefuseOnFence /\ Deposed(s)
  /\ sc' = [sc EXCEPT ![s].ackN = @ \cup sc[s].pendN,
       ![s].pendN = {}, ![s].pendCov = NoPend, ![s].pendMint = 0,
       ![s].pendDirty = {},
       ![s].honored = FALSE, ![s].pendReRun = FALSE,
       ![s].st = "dead"]
  /\ gh' = [gh EXCEPT !.refusedAcks = @ + 1]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts>>

SentinelNext ==
  \E s \in Sidecars :
    Touch(s) \/ TakeSentinel(s) \/ FastPath(s) \/ AckOk(s)
    \/ AckPartial(s) \/ RetirePending(s) \/ AckRefused(s)

------------------------------------------------------------------------------
------------------------------------------------------------------------------
(* Under gated, ANY action that moves an object mints a version: the
   bucket is versioned, so a PUT destroys nothing and a delete leaves
   every prior version fetchable (S3 writes a delete marker).  Composing
   this once at the Next level rather than threading it through twenty
   actions is also what keeps every pre-gated state space intact — with
   GatedCitation = FALSE, `versions` is frozen at Init and adds no
   distinct states at all.                                             *)
VersionsFollow ==
  IF GatedCitation
  THEN versions' = [p \in Paths |->
         IF objects'[p] = 0 THEN versions[p]
         ELSE versions[p] \cup {objects'[p]}]
  ELSE versions' = versions

BaseNext ==
  \/ StartA
  \/ \E s \in Sidecars : CrashPod(s) \/ Restart(s) \/ RenewDiscover(s)
  \/ StallA \/ ThawA \/ ClaimB \/ CheckoutB
  \/ \E s \in Sidecars, p \in Paths :
       AgentWrite(s, p) \/ AgentDelete(s, p) \/ Upload(s, p)
       \/ GCDelete(s, p)
  \/ \E p \in Paths : HitlWrite(p)
  \/ HitlRefused
  \/ \E s \in Sidecars :
       Consume(s) \/ Scan(s) \/ UploadFenced(s) \/ GCDeleteFenced(s)
       \/ PreDeletesDone(s) \/ CASFenced(s) \/ CASMiss(s) \/ CASInstall(s)
       \/ Finish(s) \/ Sync(s)
  \/ SentinelNext

Next ==
  \/ (BaseNext /\ VersionsFollow /\ UNCHANGED gatedVars)
  \/ GatedNext

Spec == Init /\ [][Next]_vars

------------------------------------------------------------------------------
(* Invariants *)

TypeOK ==
  /\ cellEpoch \in 0..3 /\ cellHolder \in Sidecars \cup {"none"}
  /\ manSeq \in 1..MaxSeq+1
  /\ manifest \in [Paths -> Gens] /\ objects \in [Paths -> Gens]
  /\ inbox \subseteq (Paths \X Gens) /\ window \in 0..3
  /\ hitlAcked \subseteq (Paths \X Gens) /\ conflicts \subseteq (Paths \X Gens)
  /\ versions \in [Paths -> SUBSET Gens]
  /\ stage \in [Sidecars -> [Paths -> Gens]]
  /\ stageBase \in [Sidecars -> [Paths -> Gens]]

\* An acked HITL write is never silently lost: its bytes are never
\* destroyed by a writer that did not legitimately learn them, and no
\* manifest install drops its last tracked reference without a conflict
\* record.  (Both stamp sites feed gh.amputated.)
Inv_HITLDurable == ~gh.amputated

\* Every cited manifest entry has a live object behind it: the barrier
\* order (uploads -> CAS -> deletes) keeps checkouts satisfiable.
Inv_NoDangling == \A p \in Paths : manifest[p] # 0 => objects[p] # 0

\* A deposed writer's manifest CAS never lands.
Inv_NoStragglerInstall == gh.stragglerInstalls = 0

\* A deposed writer's data PUT never lands.
Inv_NoDeposedPut == gh.deposedPuts = 0

\* A container restart never resurrects an unpublished delete.
Inv_NoResurrection == ~gh.resurrected

\* The sync verb never destroys genuinely-dirty local work without
\* surfacing it (tranche 2).
Inv_SyncNeverDestroysDirty == ~gh.syncDestroyed

\* D4 (boundary-verbs plan §2.2).  A sync never advances the MERGE BASE
\* for a path it did not integrate and did not surface.  Violating this
\* is not a stale read — it is permanent: `foreign(p)` at every later
\* merge compares theirs against the base we just falsified, computes
\* "unchanged", and the entry is never queued into the inbox again.
Inv_NoForeignLost == ~gh.foreignLost

\* ---- tranche 3, product 2: version lifetime (D7/D8) ---------------------

\* THE invariant of this product.  Every cited generation is still
\* STORED — which on a versioned bucket is a strictly stronger claim
\* than Inv_NoDangling's "the object exists".  Gated staging makes the
\* cited version NONCURRENT, so an object can exist, read as newer
\* uncited bytes, and have nothing at all behind its citation.
\*
\* Not hypothetical: the shipped implementation violated this for one
\* session, because the store reported its ObjectMeta before the version
\* id was minted, every citation named the empty version, and the exact
\* reaper — matching nothing — deleted every live version of every cited
\* key.  The unit tests caught it only because assertions happened to sit
\* in the right places.
Inv_CitedVersionLives ==
  \A p \in Paths : manifest[p] # 0 => manifest[p] \in versions[p]

\* The reaper never removes the version a path currently READS as.  That
\* generation is either the citation it just installed or live
\* staged-uncited work; either way it is not garbage.
Inv_NoUncitedGC ==
  \A p \in Paths : objects[p] # 0 => objects[p] \in versions[p]

\* A boundary is all-or-nothing.  No reachable state may show a citation
\* that has installed SOME of its pending set and not the rest — that is
\* a reader seeing half a logical change, which is the one thing gated
\* mode exists to prevent.  The single-CAS design makes it true by
\* construction; the split-install mutation is what keeps that from
\* being an untested claim.
Inv_BoundaryAtomic ==
  \A s \in Sidecars :
    sc[s].citeDone = {} \/ sc[s].citeDone = Valid(s)

\* ---- tranche 3, product 1: the boundary verb (D1/D2/D12) ----------------

\* THE invariant of this product.  An ok ack asserts that the coherent
\* point the agent declared is INSTALLED — not durable, not queued:
\* cited by the manifest, at the seq the ack names.  Stamped at the ack
\* rather than checked over states, because the promise is about the
\* instant the agent is told "done" and nothing later can un-tell it.
Inv_AckImpliesCited == ~gh.ackEarly

\* The second half, and the one the plan's draft called
\* `Inv_AckNotEarly`: the boundary an ok ack names is a coherent point,
\* citing every generation this workspace has integrated.  A citation
\* repair still owed at ack time means a reader — or this workspace's
\* own next checkout — resolves to bytes already superseded here.
Inv_AckBoundaryCoherent == ~gh.ackIncoherent

\* Every CONSUMED nonce is still named by something: the pending record
\* that will answer it, or the ack that already did (ok or refused).
\* Consuming is the commitment point — the rename takes the sentinel out
\* of the agent's reach, so nothing else can ever answer it.  Per
\* incarnation, because a pod replacement takes the agent and the tree
\* with the pending file.
Inv_NoNonceOrphan ==
  \A s \in Sidecars : sc[s].owed \subseteq (sc[s].pendN \cup sc[s].ackN)

\* A deposed incarnation never tells an agent its boundary landed.  The
\* plan calls this `Inv_RefusedNeverInstalled`; it is stated here as the
\* ack side, which is the side the agent reads and the only side a
\* fenced incarnation still controls.
Inv_NoFencedOkAck == ~gh.fencedOkAck

------------------------------------------------------------------------------
(* Non-vacuity probes — each names an ACTION via a ghost that only that
   action writes, and TLC is REQUIRED to violate it (the A2 probe rule:
   probe the action, never the situation).                              *)

ProbeBarrierDone      == gh.done = 0
ProbeHITLCited        == gh.cited = 0
ProbeTakeover         == gh.takeovers = 0
ProbeStragglerAttempt == gh.stragglerCas = 0
ProbePark             == conflicts = {}
ProbeGC               == gh.gc = 0
ProbeRefusal          == gh.refusals = 0
ProbeAdoptOwn         == gh.adoptOwn = 0
ProbeRestart          == gh.restarts = 0
ProbeSyncApplied      == gh.syncApplied = 0
ProbeSyncConflict     == gh.syncConflicts = 0
\* Action-written (Sync's own ghost): a SCOPED sync actually deferred a
\* remote change, rather than the scoped arm never having fired.
ProbeScopedDeferral   == gh.scopedDeferrals = 0

\* ---- tranche 3, product 2 ------------------------------------------------
\* One CAS installed >= 2 paths from a pending set that had SURVIVED a
\* lane pass.  Both halves matter: without the size the split is
\* untested, and without the carry every citation might simply be
\* following its own lane, which is hybrid wearing gated's name.
ProbeCitationInstalled == ~gh.carriedCite
\* A delete was actually withheld from the manifest until a citation.
ProbeWithheldDelete    == gh.withheld = 0
\* A citation actually fired mid-change (a staged path had already been
\* edited again locally) — the lag/backlog caps' shape, and the reason
\* the source is stamped bucket-visibly.
ProbeForcedCite        == gh.forcedCites = 0
\* REQUIRED-REACHABLE, deliberately.  §3 residual 11: a reader that does
\* not resolve through the manifest sees mid-logical-change bytes where
\* it previously saw the last boundary.  This probe proves the exposure
\* is PRESENT rather than assumed away — and a future design that
\* quietly closes it must fail this probe and force the residual to be
\* rewritten.  It names a state rather than an action on purpose: the
\* exposure IS a state, and there is no action that "does" it.
\* ---- tranche 3, product 1 ------------------------------------------------
\* An ack was actually written off a REAL barrier install (not merely
\* written): without this the strict runs could hold with the honor path
\* never having fired.
ProbeSentinelHonored == gh.honors = 0
\* The refusal fired: deposal answered a waiting agent.
ProbeRefusedAck      == gh.refusedAcks = 0
\* An ack was written for a pending record that SURVIVED A RESTART —
\* the uniform crash rule's own path, exercised.
ProbeAckAfterCrash   == gh.ackAfterRestart = 0
\* Two touches actually coalesced into one pending record.  Product 2's
\* lesson, applied: the orphan mutation checks a state space that never
\* contained two live nonces unless this fires.
ProbeCoalescedAck    == gh.coalesced = 0
\* A pending sentinel was honored by the SKIP-ON-NO-DIFF pass rather
\* than a full barrier.  This is what makes the FastPathGuards runs
\* non-vacuous: without it, "the fast path is sound" could hold because
\* the fast path never ran.
ProbeFastPathHonor   == ~gh.fastHonor

\* ---- tranche 3, product 1 x 2: the sentinel over the CITATION lane ------
\* A gated citation actually DROPPED a path the agent had declared.  The
\* honesty rule is vacuous without it: "an ok ack never claims a dropped
\* path" holds trivially in a world where nothing is ever dropped.
ProbeDeclaredDrop == gh.declaredDrops = 0
\* ...and the partial ack — the honest answer — actually fired, so the
\* agent is ANSWERED rather than left waiting on a boundary that will
\* never be claimed.
ProbePartialAck   == gh.partialAcks = 0

ProbeRawReaderSeesUncited ==
  \A p \in Paths : objects[p] = manifest[p] \/ manifest[p] = 0

==============================================================================
