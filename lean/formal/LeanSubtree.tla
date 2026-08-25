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
  SyncScanFirst        \* FALSE: sync judges dirt from the LAST BARRIER's
                       \* snapshot instead of its own scan (the refuted
                       \* design; the review's steady-state destruction)

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
  \* ---- environment / ghosts --------------------------------------------
  hitlAcked,   \* SUBSET (Paths \X Nat): writes acked to the user
  conflicts,   \* SUBSET (Paths \X Nat): surfaced conflict records
  gh           \* ghost/counter record, fields below

vars == <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox, window,
          sc, hitlAcked, conflicts, gh>>

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
     nextGen, hitl : Nat *)

------------------------------------------------------------------------------
(* Helpers *)

Gens == 0..MaxGen

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
  /\ sc = [s \in Sidecars |->
       [st |-> "unstarted", pc |-> "idle", epoch |-> 0, expSeq |-> 0,
        local |-> [p \in Paths |-> 0], baseline |-> [p \in Paths |-> 0],
        instBase |-> [p \in Paths |-> 0],
        instSnap |-> [p \in Paths |-> 0], instSeq |-> 0,
        known |-> {}, scanU |-> {}, scanD |-> {},
        scanGen |-> [p \in Paths |-> 0], upDone |-> {}, parked |-> {},
        gcDone |-> {}, lastDirty |-> {}]]
  /\ hitlAcked = {} /\ conflicts = {}
  /\ gh = [amputated |-> FALSE, resurrected |-> FALSE,
           stragglerInstalls |-> 0, stragglerCas |-> 0, deposedPuts |-> 0,
           barriers |-> 0, done |-> 0, gc |-> 0, refusals |-> 0,
           cited |-> 0, takeovers |-> 0, crashes |-> 0, restarts |-> 0,
           adoptOwn |-> 0, stallUsed |-> FALSE, nextGen |-> 2, hitl |-> 0,
           syncs |-> 0, syncApplied |-> 0, syncConflicts |-> 0,
           syncDestroyed |-> FALSE]

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
  /\ sc' = [sc EXCEPT ![s].st = "dead"]
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
  /\ sc' = [sc EXCEPT ![s].st = "dead"]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts, gh>>

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
  /\ Running(s) /\ sc[s].pc = "scanned"
  /\ EpochCheck /\ Deposed(s)
  /\ sc[s].scanU \ (sc[s].upDone \cup sc[s].parked) # {}
  /\ sc' = [sc EXCEPT ![s].st = "dead"]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked, conflicts, gh>>

Upload(s, p) ==
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
       foreign(p) == MergeCapable /\ manifest[p] # sc[s].instBase[p]
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
  /\ LET
       \* Ground truth, independent of the arm under test.
       trueDirty == {p \in Paths : sc[s].local[p] # sc[s].baseline[p]}
       \* What THIS arm believes is dirty.
       dirt == IF SyncScanFirst THEN trueDirty ELSE sc[s].lastDirty
       \* Remote truth = the manifest, overlaid by live inbox entries (a
       \* HITL write no barrier has re-cited yet is still remote truth).
       remote(p) == IF \E pr \in inbox : pr[1] = p /\ objects[p] = pr[2]
                    THEN objects[p] ELSE manifest[p]
       changed == {p \in Paths : remote(p) # sc[s].instBase[p]}
       applicable == changed \ dirt
       conflicted == changed \cap dirt
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
            ![s].instBase = [p \in Paths |-> manifest[p]],
            ![s].known = @ \cup {remote(p) : p \in applicable},
            ![s].lastDirty = {}]
       /\ conflicts' = conflicts \cup {<<p, remote(p)>> : p \in conflicted}
       /\ gh' = [gh EXCEPT !.syncs = @ + 1,
            !.syncApplied = @ + Cardinality(applicable),
            !.syncConflicts = @ + Cardinality(conflicted),
            !.syncDestroyed = @ \/ destroys]
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manifest, objects, inbox,
                 window, hitlAcked>>

------------------------------------------------------------------------------
Next ==
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

Spec == Init /\ [][Next]_vars

------------------------------------------------------------------------------
(* Invariants *)

TypeOK ==
  /\ cellEpoch \in 0..3 /\ cellHolder \in Sidecars \cup {"none"}
  /\ manSeq \in 1..MaxSeq+1
  /\ manifest \in [Paths -> Gens] /\ objects \in [Paths -> Gens]
  /\ inbox \subseteq (Paths \X Gens) /\ window \in 0..3
  /\ hitlAcked \subseteq (Paths \X Gens) /\ conflicts \subseteq (Paths \X Gens)

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

==============================================================================
