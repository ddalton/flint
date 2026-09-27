------------------------------ MODULE LeanCore --------------------------------
(* Lean's handles protocol — the SHIPPED shape, and nothing else.

   `LeanSubtree.tla` is a history: every design the protocol ever had is
   still inside it as an arm, seventy-odd constants pick the shape, and most
   of its invariants are ghost stamps that certify the action that wrote the
   stamp rather than the harm it names.  This module is the protocol as it
   ships, once, with no arms: the objects and the rules on one page, and
   invariants written over STATE.  `LeanRefine.tla` checks with TLC that
   every behaviour of `LeanSubtree` under its shipped constants
   (`IMPL` + `ImmutableObjects`) is a behaviour of this module: whatever this
   module's invariants say holds of the history model's shipped worlds too.

   THE OBJECTS
     A HANDLE names one immutable object: <<path, gen>> — the path it was
     minted under and a generation number, unique across the run (the seed
     generation is one number at every seeded path but a handle at each).
     Nothing ever overwrites a handle; a handle is deleted only by the
     collector or the sweep, below.

     live      the handles that exist in the bucket
     doc, seq  the committed document (path -> handle) and the pointer's
               generation; the pointer CAS is the only commit
     tomb      the document's tombstones: the handle the last delete at a
               path retired (cleared when the path is cited again)
     minted    every handle name ever issued (the flush ids)
     base      provenance: the handle each minted handle was written FROM
               (an agent's edit from the version its tree had integrated, a
               UI write from the citation the UI read, a refused rename's
               copy from the handle it copies).  The code knows it at the
               upload (`upload_one`'s `base`) and could stamp it; here it
               is what "these bytes were derived from those" means.
     entries   the cell's entries <<path, handle>>: UI writes and rename
               destinations, appended by the gateway, consumed by writers
     saw       for each entry, the citation the gateway saw when it appended
               it (`InboxEntry.cited`) — what a writer judges the entry by
     removals  declared removals waiting in the cell (deletes and renames'
               sources), each naming the handle it was judged against and,
               for a rename, its destination
     refused   the removals writers REFUSED: durable records the human reads
     acked     the pairs the gateway acknowledged — the observation the
               third invariant is about
     conflicts the conflict records, each naming a preserved copy
     holder    the cell's holder: the one writer in its commit section
     w         the writers: a tree (local), the version integrated at each
               path (baseline) and the document it last merged onto
               (instBase), its queue of foreign changes, and the barrier in
               flight
     took      per writer and path, every version that writer has taken in
               THERE: the trees' memory, written by the step itself

   THE RULES, one action each, in the order a barrier takes them:
     UIWrite, UIRename, UIDelete       the gateway's verbs
     Edit, Delete                      the agent
     Checkout                          a writer starts from the document
     Consume                           step 1: entries into the tree
     Scan                              step 2: what the tree changed
     Skip                              nothing to do: no scan, no CAS
     Upload                            step 4: a fresh handle per upload
     PullOnly                          nothing to publish: merge base only
     Claim                             the commit section opens
     Verify                            the commit re-reads its uploads (R4a)
     Install                           step 5: the merge and the CAS —
                                       a repair yields to a later UI write
                                       the document cites; mine over a
                                       version the tree never integrated is
                                       surfaced, upload or repair (R7)
                                       a decline for the move alone, at a
                                       rename's waiting destination, is
                                       PENDING, not gone (L-119)
     Collect                           step 6: the retired set, in one batch
     Sweep                             the orphan sweep, under the lease (R4)
     Finish                            step 7: the tree's baseline follows;
                                       the consumed entries leave the cell —
                                       a pending adoption's is not among them

   THE INVARIANTS, over state:
     Inv_CitationsLive  every citation names a live handle
     Inv_OneName        one handle is never cited under two names
     Inv_AckedNamed     an acknowledged handle that is gone is accounted
                        for by something in the state that names it: a
                        conflict record, a citation or tombstone derived
                        from it, a later acknowledged write at its path, or
                        a tree that integrated it and no longer holds it
   Each rule constant below is TRUE in the design; FALSE is the mutation,
   and each invariant has at least one world (`gen-cfgs.sh`) that violates
   it with the rule off.  Two of the rules were found by this module's own
   first run, in five minutes on a laptop, before the history model reached
   them: a rename MOVES the source's pending entry with the citation
   (RenameMovesEntry), and a removal one writer refused still moves the
   named version out of another tree that holds it clean at the source
   (AnsweredRecordsApply) — without either, one handle was cited under two
   names.

   One honest limit.  `took` — per path, every version a tree has taken in
   there — is the trees' memory, and the shipped baseline keeps only the
   CURRENT one, so the last disjunct of Inv_AckedNamed reads more than the
   workspace on disk holds.  The history model stamps the same fact at the
   decision (`hitlRetired`); this module keeps it as state a step writes,
   so the claim stays a predicate on a state.  It has to be PER PATH: with
   one flat set per writer, a tree that never held the path answered for
   it, and the L-119 mutation went green on an invariant that had nothing
   to say (2026-09-20).

   `took` is the INVARIANT's memory and nothing else.  The MERGE asks a
   smaller question — "does my baseline hold that handle at that path
   RIGHT NOW?" — because that is the question the code asks
   (`barrier.rs`'s R7 loop).  Both models ask it the same way since
   2026-09-21, so `LeanRefine` maps `ForeignPerPath <- TRUE` and the
   refinement is checked against the per-path rule rather than an
   approximation of it.  The queue world holds at 606,916 states, where
   the flat rule gave 636,652.                                      *)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Nil,         \* "no handle": a model value, so TLC can compare it with one
  Paths,       \* the workspace's paths
  Free,        \* the paths the seed does not cite
  Writers,     \* the syncers
  MaxMint,     \* the mint budget: generations 2..MaxMint (1 is the seed)
  MaxSeq,      \* the pointer's budget
  MaxUI,       \* UI writes
  MaxRemovals, \* UI deletes and renames
  MaxBarriers, \* barriers
  \* The rules.  TRUE = the design; FALSE = the mutation that removes it.
  CommitSurfacesForeign,  \* R7: a commit surfaces what it publishes over
  RepairRespectsMoves,    \* R2: a repair never re-cites a handle the document
                          \*     cites at another path this install keeps
  RepairYieldsToLaterUI,  \* a repair yields to a later acked UI write the
                          \*     document already cites at the path
  RenameMovesEntry,       \* a rename takes the source's pending entry with it
  AnsweredRecordsApply,   \* a refused removal still applies to a tree that
                          \*     holds the named version clean at the source
  CollectorSparesCited,   \* R3: retirement spares a handle the installed
                          \*     document still cites (a rename's destination)
  SweepSparesNamed,       \* R4c: the sweep spares what an entry names
  CommitVerifiesUploads,  \* R4a: the commit re-reads its uploads before the CAS
  SweepUnderLease,        \* R4b: the sweep runs inside a commit section
  PendingAdoptionStays,   \* an adoption declined for the MOVE ALONE, at the
                          \*     destination of a rename still waiting at its
                          \*     source, keeps its entry in the cell (L-119)
  ForeignPerPath          \* TRUE = the code: a citation is theirs unless the
                          \*     tree's baseline holds THAT handle AT THIS PATH
                          \*     (`barrier.rs`'s R7 loop: `baseline.entries
                          \*     .get(path).key == was.key`).  FALSE = one flat
                          \*     set per writer, the shape both models had
                          \*     until 2026-09-21; `LeanCoreForeignFlat` keeps
                          \*     it as the mutation and it violates at depth 20
  , OutrankedRemovalLeavesBaseline
                          \* L-125: a DECLARED removal whose delete the merge
                          \*     outranked leaves the baseline at Finish (the
                          \*     tree has left the version it named), so the
                          \*     newer version adopts at a clean path.  FALSE =
                          \*     the code before 2026-09-24: the named version
                          \*     stayed in the baseline, the newer one met an
                          \*     absence that read as the agent's delete, and
                          \*     this writer's next barrier deleted it
  , ParkedKeepsMergeBase  \* L-126: a path this barrier PARKED (its upload
                          \*     withheld) keeps the merge base it had, at
                          \*     step 7 and in the journal's installed
                          \*     document; FALSE = the shape before the fix,
                          \*     where the next re-upload read theirs as
                          \*     unchanged and replaced it with no record
  , CommitRecordsDeleteOverride
                          \* the user's rule (2026-09-24): an install whose
                          \*     upload lands over a DELETE theirs made since
                          \*     the merge base records the deleted version;
                          \*     FALSE = the shape before, no record

ASSUME Free \subseteq Paths
ASSUME MaxMint \in Nat /\ MaxMint >= 1

Seed == 1
Gens == Seed..MaxMint
Handles == Paths \X Gens
Gen(h) == h[2]
Later(h, k) == Gen(h) > Gen(k)
Opt(S) == S \cup {Nil}

VARIABLES
  live, minted, doc, seq, tomb, base, entries, saw, removals, refused, acked,
  conflicts, holder, nextGen, ui, reqs, barriers, w,
  \* Per path, the generations each tree has taken IN there: every version
  \* its baseline ever named at that path.  The trees' memory, and the one
  \* piece of it the shipped baseline does not keep — it holds only the
  \* current version.  Written by no action: one conjunct of `Next` reads
  \* the step itself (a baseline that moved to a new version at p), which
  \* is what makes it a HISTORY of the same steps rather than a stamp an
  \* action chooses to write.  `LeanRefine` carries it as a history
  \* variable over exactly the same rule.
  took

bucket == <<live, minted, doc, seq, tomb, base, entries, saw, removals, refused,
            acked, conflicts, holder>>
vars == <<live, minted, doc, seq, tomb, base, entries, saw, removals, refused,
          acked, conflicts, holder, nextGen, ui, reqs, barriers, w, took>>

\* A declared removal: the version it was judged against, where a rename
\* moved it, and — for a rename of a pending entry — the version that entry
\* was written over (a tree holding THAT holds a version the request covers).
Removal == [path : Paths, handle : Handles, to : Opt(Paths), over : Opt(Handles)]
Queued  == [path : Paths, handle : Opt(Handles), retired : Opt(Handles)]

Writer ==
  [st : {"off", "on"},
   pc : {"idle", "consumed", "scanned", "claimed", "cased"},
   local : [Paths -> Opt(Handles)],     \* the tree
   baseline : [Paths -> Opt(Handles)],  \* what the tree integrated, per path
   instBase : [Paths -> Opt(Handles)],  \* the document it last merged onto
   integrated : SUBSET Gens,            \* every generation it ever took in,
                                        \* flat: what `ForeignPerPath = FALSE`
                                        \* reads, and what the history model
                                        \* still judges theirs-or-mine by
   queue : SUBSET Queued,               \* foreign changes owed to the tree
   consumed : SUBSET (Paths \X Handles),\* the entries this barrier consumed
   declared : SUBSET Paths,             \* removals this barrier performed
   surfaced : SUBSET Paths,             \* the baseline names a version the tree
                                        \* publishes OVER, not one it holds (the
                                        \* code's consume-dirty sentinel)
   uploads : SUBSET Paths, deletes : SUBSET Paths,
   snap : [Paths -> Opt(Handles)],      \* the tree as the scan saw it
   upDone : SUBSET Paths, gone : SUBSET Paths, verified : BOOLEAN,
   inst : [Paths -> Opt(Handles)],      \* the document this barrier installed
   retire : SUBSET Handles,             \* what that install stopped citing
   collected : BOOLEAN,
   seen : Nat,                          \* the pointer generation it last synced to
   jSeq : Nat,                          \* ...and the one it last INSTALLED (the journal)
   jParked : SUBSET Paths]              \* ...and what that install PARKED (L-126)

WriterInit ==
  [st |-> "off", pc |-> "idle",
   local |-> [p \in Paths |-> Nil], baseline |-> [p \in Paths |-> Nil],
   instBase |-> [p \in Paths |-> Nil], integrated |-> {}, queue |-> {},
   consumed |-> {}, declared |-> {}, surfaced |-> {}, uploads |-> {}, deletes |-> {},
   snap |-> [p \in Paths |-> Nil], upDone |-> {}, gone |-> {},
   verified |-> FALSE, inst |-> [p \in Paths |-> Nil], retire |-> {},
   collected |-> FALSE, seen |-> 0, jSeq |-> 0, jParked |-> {}]

TypeOK ==
  /\ live \subseteq Handles /\ minted \subseteq Handles /\ live \subseteq minted
  /\ doc \in [Paths -> Opt(Handles)]
  /\ seq \in Nat
  /\ tomb \in [Paths -> Opt(Handles)]
  /\ base \in [Handles -> Opt(Handles)]
  /\ entries \subseteq Paths \X Handles
  /\ saw \subseteq Paths \X Handles \X Opt(Handles)
  /\ removals \subseteq Removal /\ refused \subseteq Removal
  /\ acked \subseteq Paths \X Handles
  /\ conflicts \subseteq Paths \X Opt(Handles)
  /\ holder \in Writers \cup {"none"}
  /\ nextGen \in Nat /\ ui \in Nat /\ reqs \in Nat /\ barriers \in Nat
  /\ w \in [Writers -> Writer]
  /\ took \in [Writers -> [Paths -> SUBSET Gens]]

------------------------------------------------------------------------------
(* What the gateway and the writers read.                                   *)

On(s) == w[s].st = "on"
Tracked(p) == doc[p] # Nil \/ \E e \in entries : e[1] = p
EntriesAt(p) == {e[2] : e \in {x \in entries : x[1] = p}}
Newest(S) == CHOOSE h \in S : \A k \in S : Gen(k) <= Gen(h)
\* The version the read door serves: the newest entry, else the citation.
TrackedHandle(p) == IF EntriesAt(p) = {} THEN doc[p] ELSE Newest(EntriesAt(p))
LiveEntriesAt(p) == {h \in EntriesAt(p) : h \in live}
TrackedOrCited(p) == IF LiveEntriesAt(p) = {} THEN doc[p] ELSE Newest(LiveEntriesAt(p))
\* The window: a UI write waits while a commit section is between its claim
\* and its CAS.
WindowOpen == \E s \in Writers : w[s].pc = "claimed"
Pending == {r.path : r \in removals}
Cited(h) == \E p \in Paths : doc[p] = h
Named(h) == \E e \in entries : e[2] = h
Preserved(h) == \E c \in conflicts : c[2] = h

------------------------------------------------------------------------------
(* The gateway's verbs.                                                     *)

\* A UI write lands at a fresh handle and replaces the path's entry; the
\* entry carries the citation the gateway saw.  Acknowledged at once.
UIWrite(p) ==
  /\ ui < MaxUI /\ nextGen <= MaxMint /\ ~WindowOpen
  /\ LET h == <<p, nextGen>>
         old == {e \in entries : e[1] = p}
     IN /\ live' = live \cup {h} /\ minted' = minted \cup {h}
        /\ base' = [base EXCEPT ![h] = doc[p]]
        /\ entries' = (entries \ old) \cup {<<p, h>>}
        /\ saw' = saw \cup {<<p, h, doc[p]>>}
        /\ acked' = acked \cup {<<p, h>>}
        /\ nextGen' = nextGen + 1 /\ ui' = ui + 1
  /\ UNCHANGED <<doc, seq, tomb, removals, refused, conflicts, holder, reqs,
                 barriers, w>>

\* A rename is a CITATION MOVE: the destination's entry names the source's
\* handle, and the source's removal is declared, judged against that handle.
\* No bytes move and nothing is minted.  A pending entry at the source IS
\* the citation being moved, so it goes with it; the removal then names the
\* version that entry was written over, and a fresh request supersedes an
\* answered record for the path.
UIRename(p, q) ==
  /\ reqs < MaxRemovals /\ ~WindowOpen
  /\ p # q /\ p \notin Pending /\ q \notin Pending
  /\ Tracked(p) /\ ~Tracked(q)
  /\ LET h == TrackedHandle(p)
         fromEntry == EntriesAt(p) # {}
         moved == IF RenameMovesEntry THEN {e \in entries : e[1] = p /\ e[2] = h} ELSE {}
     IN
     /\ entries' = (entries \ moved) \cup {<<q, h>>}
     /\ saw' = saw \cup {<<q, h, Nil>>}
     /\ removals' = removals \cup {[path |-> p, handle |-> h, to |-> q,
                                    over |-> IF fromEntry THEN doc[p] ELSE Nil]}
     /\ refused' = {r \in refused : r.path # p}
     /\ acked' = acked \cup {<<q, h>>}
     /\ reqs' = reqs + 1
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, conflicts, holder,
                 nextGen, ui, barriers, w>>

\* A delete is a declared removal, judged against the version the door
\* served (H4): a newer version was never asked about.
UIDelete(p) ==
  /\ reqs < MaxRemovals
  /\ p \notin Pending /\ Tracked(p)
  /\ removals' = removals \cup {[path |-> p, handle |-> TrackedHandle(p), to |-> Nil,
                                 over |-> Nil]}
  /\ refused' = {r \in refused : r.path # p}
  /\ reqs' = reqs + 1
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, entries, saw, acked,
                 conflicts, holder, nextGen, ui, barriers, w>>

------------------------------------------------------------------------------
(* The agent: local edits only.  An edit mints the handle its upload will
   land at, derived from the version the tree had integrated at the path.  *)

Edit(s, p) ==
  /\ On(s) /\ nextGen <= MaxMint
  /\ LET h == <<p, nextGen>> IN
     /\ minted' = minted \cup {h}
     /\ base' = [base EXCEPT ![h] = w[s].baseline[p]]
     /\ w' = [w EXCEPT ![s].local[p] = h, ![s].integrated = @ \cup {nextGen}]
     /\ nextGen' = nextGen + 1
  /\ UNCHANGED <<live, doc, seq, tomb, entries, saw, removals, refused, acked,
                 conflicts, holder, ui, reqs, barriers>>

Delete(s, p) ==
  /\ On(s) /\ w[s].local[p] # Nil
  /\ w' = [w EXCEPT ![s].local[p] = Nil]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers>>

\* A writer starts by checking the document out: tree, baseline and merge
\* base are the document, and `took` records what it took in at each path
\* (the step's own rule, in `Next`).
Checkout(s) ==
  /\ w[s].st = "off"
  /\ w' = [w EXCEPT ![s].st = "on",
                    ![s].local = doc, ![s].baseline = doc, ![s].instBase = doc, ![s].inst = doc,
                    ![s].integrated = {Gen(doc[p]) : p \in {q \in Paths : doc[q] # Nil}},
                    ![s].seen = seq]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers>>

------------------------------------------------------------------------------
(* Step 1: the cell into the tree.  The writer's own queue of foreign
   changes (what earlier merges found the document had that the tree did
   not) joins the cell's entries.  An entry whose handle the baseline
   already holds is settled; one whose handle is gone leaves a record; a
   UI entry is judged against the citation the gateway saw when it appended
   it — a citation that moved to a LATER UI write supersedes it (dropped:
   retired by the UI's own hand), one that moved to a writer's publish
   makes it STALE (a conflict record preserves the copy), one that moved to
   the entry itself or to an earlier UI write leaves it live.  A live entry
   adopts at a clean path and surfaces at a dirty one (the baseline
   advances either way: the tree publishes over it knowingly).  Then the
   queued deletions, against the tree the entries left; then the declared
   removals: a rename's source leaves only once its destination is
   integrated and clean, a dirty source refuses, a destination the agent
   took refuses, a source whose version moved on refuses unless the named
   version is still arriving.                                              *)
Consume(s) ==
  /\ On(s) /\ w[s].pc = "idle" /\ barriers < MaxBarriers
  /\ LET
       W == w[s]
       qUp == {<<r.path, r.handle>> : r \in {x \in W.queue : x.handle # Nil}}
       qDel == {r.path : r \in {x \in W.queue : x.handle = Nil}}
       qRet(p) == (CHOOSE r \in W.queue : r.path = p /\ r.handle = Nil).retired
       cand == entries \cup qUp
       already == {e \in cand : W.baseline[e[1]] = e[2]}
       missing == {e \in cand \ already : e[2] \notin live}
       live0 == (cand \ already) \ missing
       SawOf(e) == CHOOSE c \in {x[3] : x \in {y \in saw : y[1] = e[1] /\ y[2] = e[2]}} : TRUE
       judged == {e \in (live0 \cap entries) \ qUp :
                    /\ \E x \in saw : x[1] = e[1] /\ x[2] = e[2]
                    /\ doc[e[1]] \notin {Nil, SawOf(e), e[2]}
                    /\ ~(<<e[1], doc[e[1]]>> \in acked /\ Later(e[2], doc[e[1]]))}
       superseded == {e \in judged : <<e[1], doc[e[1]]>> \in acked}
       stale == judged \ superseded
       liveE == live0 \ judged
       adoptable == {e \in liveE : W.local[e[1]] = W.baseline[e[1]]}
       dirtyConf == liveE \ adoptable
       adoptPaths == {e[1] : e \in adoptable}
       surfPaths == {e[1] : e \in dirtyConf}
       advPaths == adoptPaths \cup surfPaths
       At(p, S) == Newest({e[2] : e \in {x \in S : x[1] = p}})
       GenAt(p) == IF p \in adoptPaths THEN At(p, adoptable) ELSE At(p, dirtyConf)
       local1 == [p \in Paths |-> IF p \in adoptPaths THEN GenAt(p) ELSE W.local[p]]
       base1 == [p \in Paths |-> IF p \in advPaths THEN GenAt(p) ELSE W.baseline[p]]
       \* the queued deletions
       tAbsent == {p \in qDel : local1[p] = Nil}
       trackedAt(p) == \E e \in liveE : e[1] = p
       supersedes(p) == (doc[p] # Nil /\ doc[p] # qRet(p)) \/ trackedAt(p)
       tSuper == {p \in qDel \ tAbsent : supersedes(p)}
       tKept == {p \in (qDel \ tAbsent) \ tSuper : base1[p] = Nil \/ local1[p] # base1[p]}
       tRemoved == qDel \ (tAbsent \cup tSuper \cup tKept)
       local2 == [p \in Paths |-> IF p \in tRemoved THEN Nil ELSE local1[p]]
       base2 == [p \in Paths |-> IF p \in tAbsent \cup tRemoved THEN Nil ELSE base1[p]]
       \* the declared removals
       DstOf(r) == IF r.to = Nil THEN {} ELSE {r.to}
       dstReady(r) == \A q \in DstOf(r) : base2[q] = TrackedOrCited(q) /\ local2[q] = base2[q]
       staleAt(q) == \E e \in stale : e[1] = q
       dstTaken(r) == \E q \in DstOf(r) :
                        \/ q \in surfPaths \/ staleAt(q)
                        \/ (base2[q] = TrackedOrCited(q) /\ local2[q] # base2[q])
       moved(r) == /\ base2[r.path] # Nil /\ base2[r.path] # r.handle
                   /\ base2[r.path] # r.over
       arriving(r) == <<r.path, r.handle>> \in entries
       refusedNow == {r \in removals :
                        \/ (local2[r.path] # Nil /\ local2[r.path] # base2[r.path])
                        \/ dstTaken(r)
                        \/ (moved(r) /\ ~arriving(r))}
       \* An answered record is one tree's answer, not the document's: it
       \* still moves the named version out of a tree that holds it clean.
       applicable == IF AnsweredRecordsApply THEN removals \cup refused ELSE removals
       applied == {r \in applicable \ refusedNow :
                     /\ (local2[r.path] = Nil \/ local2[r.path] = base2[r.path])
                     /\ dstReady(r) /\ ~moved(r)}
       appliedPaths == {r.path : r \in applied}
     IN
       /\ w' = [w EXCEPT ![s].pc = "consumed",
                 ![s].local = [p \in Paths |-> IF p \in appliedPaths THEN Nil ELSE local2[p]],
                 ![s].baseline = base2,
                 ![s].integrated = @ \cup {Gen(GenAt(p)) : p \in advPaths},
                 ![s].queue = {},
                 ![s].instBase = [p \in Paths |->
                                    IF p \in tSuper /\ qRet(p) # Nil THEN qRet(p) ELSE @[p]],
                 ![s].declared = @ \cup appliedPaths,
                 ![s].surfaced = (@ \ (adoptPaths \cup tAbsent \cup tRemoved)) \cup surfPaths,
                 ![s].consumed = entries]
       /\ conflicts' = conflicts \cup dirtyConf \cup stale \cup missing
                         \cup {<<r.path, local2[r.path]>> : r \in refusedNow}
                         \cup {<<p, Nil>> : p \in tKept}
       /\ removals' = removals \ refusedNow
       /\ refused' = (refused \cup refusedNow) \ applied
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, entries, saw, acked, holder,
                 nextGen, ui, reqs, barriers>>

------------------------------------------------------------------------------
(* Step 2: the scan.  What differs from the baseline is an upload or a
   deletion; a deletion may wait for the next walk (the two-scan guard —
   here, any subset of the absences), a declared removal never does.       *)
Scan(s) ==
  /\ On(s) /\ w[s].pc = "consumed"
  /\ LET W == w[s]
         dirty == {p \in Paths : W.local[p] # W.baseline[p]}
         ups == {p \in dirty : W.local[p] # Nil}
         absent == {p \in dirty : W.local[p] = Nil}
         \* A declared removal whose path the agent RE-CREATED between the
         \* removal pass and the walk is not a delete: the declaration named
         \* the file that is gone, and what is there now is the agent's new
         \* work, which publishes as an upload (`barrier.rs`'s scan skips it
         \* for the same reason).  Without this the same path was a delete
         \* AND an upload, and when the re-read then withheld the upload the
         \* path kept its citation while the destination of its rename was
         \* repaired beside it: one handle under two names, in the model
         \* only (the two-path world, Inv_OneName at depth 21, 2026-09-20).
         declared == {p \in W.declared : W.local[p] = Nil}
     IN \E dels \in SUBSET absent :
          w' = [w EXCEPT ![s].pc = "scanned",
                         ![s].uploads = ups, ![s].deletes = dels \cup declared,
                         ![s].snap = W.local,
                         ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
                         ![s].retire = {}, ![s].collected = FALSE]
  /\ barriers' = barriers + 1
  /\ UNCHANGED <<bucket, nextGen, ui, reqs>>

\* Skip-on-no-diff: nothing to publish, nothing consumed, no repair owed
\* and the document where the tree left it — the barrier returns from the
\* consume with no scan, no window and no CAS.
Skip(s) ==
  /\ On(s) /\ w[s].pc = "consumed" /\ barriers < MaxBarriers
  /\ w[s].consumed = {}
  /\ \A p \in Paths : w[s].local[p] = w[s].baseline[p] /\ w[s].baseline[p] = w[s].instBase[p]
  /\ seq = w[s].seen
  /\ w' = [w EXCEPT ![s].pc = "idle"]
  /\ barriers' = barriers + 1
  /\ UNCHANGED <<bucket, nextGen, ui, reqs>>

\* Step 4: every upload lands at the fresh handle its edit minted.
Upload(s, p) ==
  /\ On(s) /\ w[s].pc = "scanned"
  /\ p \in w[s].uploads \ w[s].upDone
  /\ live' = live \cup {w[s].snap[p]}
  /\ w' = [w EXCEPT ![s].upDone = @ \cup {p}]
  /\ UNCHANGED <<minted, doc, seq, tomb, base, entries, saw, removals, refused, acked,
                 conflicts, holder, nextGen, ui, reqs, barriers>>

------------------------------------------------------------------------------
(* The merge's vocabulary, read at the CAS against the CURRENT document.   *)

\* A citation the document moved past this writer's merge base, to a
\* version it never integrated: theirs.
\* PER PATH, as the merge is: theirs moved off the merge base AT p to a
\* version this tree never took in THERE.  Asked of a flat "every
\* generation this tree ever integrated", a peer that had the seed at the
\* rename's SOURCE published its own file over the renamed DESTINATION
\* without surfacing it — the version it published over was familiar, just
\* not at that path — and the handle was retired with no record (the
\* two-path core world, `Inv_AckedNamed` at depth 19, 2026-09-20).  The
\* code compares the merge base with theirs at the path, and reads the
\* baseline at the path to decide whether it integrated it.
\* THE MERGE BASE this commit merges onto, which is not always the
\* persisted one.  Step 7 (`Finish`) rewrites `instBase` AFTER the CAS, so
\* a barrier that has not reached it — deposed here, and in the history
\* model a restart in that window too — would read its OWN install as a
\* peer's change: its agent's delete outranked by "theirs" that is really
\* its own, and the ack still saying ok.  The code reads this from the one
\* record that survives the window, the intent journal's `installed_etag`,
\* written the instant the CAS returns: "if the bucket is still at the
\* document THIS workspace installed, that document IS the merge base"
\* (`barrier.rs::merge_onto`).  The test is the POINTER's, not the path's.
\* `jSeq` and not `seen`: a checkout and a PULL-ONLY also move `seen`,
\* and those name the document an ACK points at.  The journal records
\* CASes only, and the document it names is the one the POINTER holds —
\* `theirs`, exactly as `merge_onto` reads it.
\* L-126: a path that install PARKED keeps the base it had — the installed
\* document cites theirs there, which the tree never integrated.
MergeBase(s) ==
  IF w[s].jSeq # 0 /\ w[s].jSeq = seq
  THEN [p \in Paths |-> IF ParkedKeepsMergeBase /\ p \in w[s].jParked THEN w[s].instBase[p] ELSE doc[p]]
  ELSE w[s].instBase
Foreign(s, p) == doc[p] # MergeBase(s)[p]
\* The version a delete since the merge base removed: the document's
\* tombstone, else the merge base's citation.
DeletedAt(s, p) == IF tomb[p] # Nil THEN tomb[p] ELSE MergeBase(s)[p]
\* R7's own filter, and it is a different question from `Foreign`.  The
\* code's merge reports every version mine publishes over (`overridden`);
\* the CALLER surfaces the ones this tree never integrated, per path, by
\* the CURRENT baseline (`barrier.rs`: `baseline.entries.get(path).key ==
\* was.key`).  A version the tree holds AT THAT PATH needs no record.
\* `ForeignPerPath = FALSE` is one flat set per writer instead — every
\* generation it ever knew, anywhere — and `LeanCoreForeignFlat` is what
\* that costs: a peer that held the seed at a rename's SOURCE publishes
\* over the renamed destination and surfaces nothing (depth 20).
\* The document's tombstone names the version this tree holds: it was
\* published and then removed, knowingly (H1e).
Entombed(s, p) == doc[p] = Nil /\ tomb[p] = w[s].baseline[p]
\* A citation REPAIR: the tree integrated a version (a consumed entry) the
\* merge base does not cite.  Re-cited by name, with no HEAD — unless the
\* document's tombstone names it, or the handle moved elsewhere.
RepairCandidate(s, p) ==
  /\ p \notin w[s].uploads \cup w[s].deletes \cup w[s].surfaced
  /\ w[s].baseline[p] # w[s].instBase[p]
  /\ w[s].baseline[p] \in live
\* The document cites a LATER acknowledged UI write at p than the version
\* this tree adopted: the UI's own hand.  The repair yields, and the queue
\* brings the newer version to the tree.
SupersededByUI(s, p) ==
  /\ RepairYieldsToLaterUI
  /\ doc[p] # Nil /\ <<p, doc[p]>> \in acked
  /\ Later(doc[p], w[s].baseline[p])
\* A repair this install would make at q, judged without the move rule —
\* the one clause that would make this definition circular.
RepairHere(s, q) ==
  RepairCandidate(s, q) /\ ~Entombed(s, q) /\ ~SupersededByUI(s, q)
\* This install keeps q at the handle h.  It is `inst[q] = h` in the same
\* order the CAS builds `inst`, which is the order the code reads off its
\* own upsert map: an upload the re-read WITHHELD (`gone`) leaves nothing
\* of this barrier at q, so q keeps its citation; then this install's own
\* upload, then its repair, then its delete, which theirs outranks.
\* Written as two guesses instead — the scan's upload set, and any path
\* whose baseline had moved — it said a rename's source had moved off the
\* handle when the install left it exactly where it was, and the same
\* install cited the destination's repair beside it: one handle under two
\* names, in the model only (the two-path core world, `Inv_OneName` at
\* depth 21, 2026-09-20; the code's `void_stale_repairs` reads `upserts`,
\* which a withheld upload has already left).
KeepsAt(s, q, h) ==
  CASE q \in w[s].gone                        -> TRUE
    [] q \in w[s].uploads \cap w[s].upDone    -> w[s].snap[q] = h
    [] RepairHere(s, q)                       -> w[s].baseline[q] = h
    [] q \in w[s].deletes                     -> Foreign(s, q)
    [] OTHER                                  -> TRUE
\* R2: the handle this tree would re-cite at p is cited by the document at
\* another path — a citation move took it there, p was its source — and
\* this install keeps it there.  Not p's to re-cite.
MovedElsewhere(s, p) ==
  /\ RepairRespectsMoves
  /\ \E q \in Paths \ {p} : doc[q] = w[s].baseline[p] /\ KeepsAt(s, q, w[s].baseline[p])
RepairOwed(s, p) == RepairHere(s, p) /\ ~MovedElsewhere(s, p)
\* L-119: a decline for the MOVE ALONE, at the DESTINATION of a rename
\* whose record still waits at its source, is not a verdict that the
\* adoption is gone — it is early.  The source leaves as soon as a tree
\* holding it clean applies the record, and this path is where the handle
\* is going.  So the adoption is PENDING: the entry stays in the cell, the
\* collector and the sweep spare what it names, and the next barrier of
\* any writer cites it once the source has left.  Without the rule nothing
\* but the source's citation named the handle; the peer that applied the
\* record removed it, and the collector took it (Inv_AckedNamed).
\* A handle moved AWAY from the adopted path — no record coming back for
\* it — is gone as before: that is what R2 is for.
PendingAdoption(s, p) ==
  /\ PendingAdoptionStays /\ RepairRespectsMoves
  /\ RepairHere(s, p)
  /\ \E q \in Paths \ {p} :
       /\ doc[q] = w[s].baseline[p] /\ KeepsAt(s, q, w[s].baseline[p])
       /\ \E r \in removals \cup refused : r.path = q /\ r.to = p
\* What this merge finds the document has that the tree does not: queued
\* for the tree, never applied to it here (the next Consume does).
Retired(s, p) == IF MergeBase(s)[p] # Nil THEN MergeBase(s)[p] ELSE w[s].baseline[p]
MineInMerge(s) == w[s].uploads \cap w[s].upDone
MergeForeign(s) ==
  {[path |-> p, handle |-> doc[p], retired |-> Nil] :
     p \in {q \in Paths : /\ q \notin MineInMerge(s)
                          /\ ~RepairOwed(s, q)
                          /\ Foreign(s, q) /\ doc[q] # Nil}}
MergeGone(s) ==
  {[path |-> p, handle |-> Nil, retired |-> Retired(s, p)] :
     p \in {q \in Paths : /\ q \notin MineInMerge(s) \cup w[s].deletes
                          /\ ~RepairOwed(s, q)
                          /\ doc[q] = Nil
                          /\ (MergeBase(s)[q] # Nil \/ (Entombed(s, q) /\ w[s].baseline[q] # Nil))}}
QueueUpsert(q, new) == {r \in q : r.path \notin {x.path : x \in new}} \cup new

\* A barrier with nothing to publish: no claim, no CAS; the merge base
\* follows the document and its changes are queued for the tree.
PullOnly(s) ==
  /\ On(s) /\ w[s].pc = "scanned"
  /\ w[s].uploads = {} /\ w[s].deletes = {}
  /\ w[s].consumed = {} /\ w[s].declared = {}
  /\ ~\E p \in Paths : RepairOwed(s, p)
  /\ w' = [w EXCEPT ![s].pc = "idle",
                    ![s].queue = QueueUpsert(@, MergeForeign(s) \cup MergeGone(s)),
                    ![s].instBase = doc, ![s].inst = doc, ![s].seen = seq,
                    ![s].snap = [p \in Paths |-> Nil],
                    ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers>>

------------------------------------------------------------------------------
(* The commit section: one writer at a time, from the claim to the end of
   the barrier.  The UI's window is the section's first half.              *)

Claim(s) ==
  /\ On(s) /\ w[s].pc = "scanned"
  /\ w[s].uploads \subseteq w[s].upDone
  /\ ~(/\ w[s].deletes = {} /\ w[s].consumed = {} /\ w[s].declared = {}
       /\ w[s].uploads = {} /\ ~\E p \in Paths : RepairOwed(s, p))
  /\ holder = "none"
  /\ holder' = s
  /\ w' = [w EXCEPT ![s].pc = "claimed"]
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, entries, saw, removals, refused,
                 acked, conflicts, nextGen, ui, reqs, barriers>>

\* R4a: the commit re-reads every upload it is about to cite and withholds
\* what a sweep took — never cited, recorded, dirty again for the next
\* barrier.
Verify(s) ==
  /\ On(s) /\ w[s].pc = "claimed" /\ holder = s /\ ~w[s].verified
  /\ w' = [w EXCEPT ![s].verified = TRUE,
                    ![s].gone = IF CommitVerifiesUploads
                                THEN {p \in w[s].uploads \cap w[s].upDone : w[s].snap[p] \notin live}
                                ELSE {}]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers>>

\* Step 5: the three-way merge onto the CURRENT document, and the CAS.
\* Mine: the uploads that survived the re-read, the citation repairs, the
\* deletes.  Theirs: what the document has that the merge base did not.
\* Theirs wins a delete/modify race (the tree's delete is outranked); mine
\* wins a modify/modify one — and R7 surfaces what mine publishes over that
\* this tree never integrated, with a record naming the cited handle.  The
\* installed document's tombstones name what its deletes retired.  The
\* handles the document cited that the installed one does not are RETIRED,
\* for step 6.
Install(s) ==
  /\ On(s) /\ w[s].pc = "claimed" /\ holder = s /\ w[s].verified
  /\ LET W == w[s]
         mine == W.uploads \cap W.upDone
         published == mine \cup {q \in Paths : RepairOwed(s, q)}
         contested == IF CommitSurfacesForeign
                      THEN {p \in published \ W.gone :
                              /\ Foreign(s, p) /\ doc[p] # Nil
                              /\ IF ForeignPerPath
                                 THEN doc[p] # W.baseline[p]
                                 ELSE Gen(doc[p]) \notin W.integrated}
                      ELSE {}
         \* Mine re-creates a path theirs deleted since the merge base: the
         \* delete is overridden, and named (`manifest::merge`'s `recreated`).
         delOverridden == IF CommitRecordsDeleteOverride
                          THEN {p \in published \ W.gone : Foreign(s, p) /\ doc[p] = Nil /\ MergeBase(s)[p] # Nil}
                          ELSE {}
         inst == [p \in Paths |->
                    IF p \in W.gone THEN doc[p]
                    ELSE IF p \in mine THEN W.snap[p]
                    ELSE IF RepairOwed(s, p) THEN W.baseline[p]
                    ELSE IF Foreign(s, p) THEN doc[p]
                    ELSE IF p \in W.deletes THEN Nil
                    ELSE doc[p]]
         nothing == inst = doc
         retired == {doc[p] : p \in {q \in Paths : doc[q] # Nil /\ inst[q] # doc[q]}}
         \* A pending adoption leaves this barrier's consumed set HERE, at
         \* the CAS that declined to cite it: before Collect, which spares
         \* what an entry it did not consume names, and before Finish,
         \* which drops from the cell what it did.  The code filters its
         \* `consumed` on the way out of the same CAS loop.
         pending == {e \in W.consumed : PendingAdoption(s, e[1])}
     IN
       /\ seq < MaxSeq \/ nothing
       /\ doc' = inst
       /\ seq' = IF nothing THEN seq ELSE seq + 1
       /\ tomb' = [p \in Paths |-> IF inst[p] # Nil THEN Nil
                                   ELSE IF doc[p] # Nil THEN doc[p]
                                   ELSE tomb[p]]
       /\ conflicts' = conflicts \cup {<<p, W.snap[p]>> : p \in W.gone}
                                 \cup {<<p, doc[p]>> : p \in contested}
                                 \cup {<<p, DeletedAt(s, p)>> : p \in delOverridden}
       /\ w' = [w EXCEPT ![s].pc = "cased",
                         ![s].upDone = @ \ W.gone,
                         ![s].consumed = @ \ pending,
                         ![s].inst = inst, ![s].retire = retired,
                         ![s].seen = IF nothing THEN seq ELSE seq + 1,
                         ![s].jSeq = IF nothing THEN seq ELSE seq + 1,
                         ![s].jParked = IF ParkedKeepsMergeBase THEN W.gone ELSE {},
                         ![s].collected = retired = {},
                         ![s].queue = QueueUpsert(@, MergeForeign(s) \cup MergeGone(s))]
  /\ UNCHANGED <<live, minted, base, entries, saw, removals, refused, acked, holder,
                 nextGen, ui, reqs, barriers>>

\* Step 6: the retired set is deleted in one batch — no HEAD, no condition
\* — sparing what the installed document still cites at another path (a
\* rename's destination) and what an entry no barrier has consumed names.
Collect(s) ==
  /\ On(s) /\ w[s].pc = "cased" /\ ~w[s].collected
  /\ LET W == w[s]
         kept(h) == /\ CollectorSparesCited
                    /\ \/ \E q \in Paths : W.inst[q] = h
                       \/ \E e \in entries \ W.consumed : e[2] = h
         taken == {h \in W.retire : ~kept(h)}
     IN /\ live' = live \ taken
        /\ w' = [w EXCEPT ![s].retire = {}, ![s].collected = TRUE]
  /\ UNCHANGED <<minted, doc, seq, tomb, base, entries, saw, removals, refused, acked,
                 conflicts, holder, nextGen, ui, reqs, barriers>>

\* The orphan sweep (R4): a handle nothing cites, no entry names and no
\* record preserves — a lost writer's upload, a superseded predecessor —
\* is garbage.  Inside a commit section, so that section's re-read and CAS
\* are race-free against it; never the sweeper's own in-flight uploads.
Sweep(s, h) ==
  /\ On(s)
  /\ SweepUnderLease => (holder = s /\ w[s].pc \in {"claimed", "cased"})
  /\ h \in live /\ ~Cited(h) /\ ~Preserved(h)
  /\ SweepSparesNamed => ~Named(h)
  /\ ~\E p \in w[s].upDone : w[s].snap[p] = h
  /\ live' = live \ {h}
  /\ UNCHANGED <<minted, doc, seq, tomb, base, entries, saw, removals, refused, acked,
                 conflicts, holder, nextGen, ui, reqs, barriers, w>>

\* Step 7: the baseline follows what this barrier published (its uploads,
\* its landed deletes), the merge base becomes the installed document, the
\* consumed entries and the performed removals leave the cell, and the
\* section closes.  A pending adoption's entry left `consumed` at the CAS
\* (L-119), so it stays in the cell for the barrier that can cite it.
Finish(s) ==
  /\ On(s) /\ w[s].pc = "cased" /\ w[s].collected
  /\ LET W == w[s] IN
     /\ w' = [w EXCEPT ![s].pc = "idle",
               ![s].baseline = [p \in Paths |->
                                  IF p \in W.uploads \cap W.upDone THEN W.snap[p]
                                  ELSE IF p \in W.deletes /\ W.inst[p] = Nil THEN Nil
                                  \* L-125: a declared removal the merge
                                  \* outranked -- the tree already left it.
                                  ELSE IF /\ OutrankedRemovalLeavesBaseline
                                          /\ p \in W.deletes \cap W.declared
                                       THEN Nil
                                  ELSE @[p]],
               ![s].instBase = [p \in Paths |->
                                  IF ParkedKeepsMergeBase /\ p \in W.gone THEN @[p] ELSE W.inst[p]],
               ![s].surfaced = @ \ ((W.uploads \cap W.upDone)
                                    \cup {p \in W.deletes : W.inst[p] = Nil}),
               ![s].uploads = {}, ![s].deletes = {}, ![s].snap = [p \in Paths |-> Nil],
               ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
               ![s].declared = {}, ![s].consumed = {}, ![s].collected = FALSE]
     /\ entries' = entries \ W.consumed
     /\ removals' = {r \in removals : r.path \notin W.declared}
  /\ holder' = "none"
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, saw, refused, acked, conflicts,
                 nextGen, ui, reqs, barriers>>

------------------------------------------------------------------------------
Init ==
  /\ live = {<<p, Seed>> : p \in Paths \ Free}
  /\ minted = live
  /\ doc = [p \in Paths |-> IF p \in Free THEN Nil ELSE <<p, Seed>>]
  /\ seq = 1
  /\ tomb = [p \in Paths |-> Nil]
  /\ base = [h \in Handles |-> Nil]
  /\ entries = {} /\ saw = {} /\ removals = {} /\ refused = {}
  /\ acked = {} /\ conflicts = {}
  /\ holder = "none"
  /\ nextGen = Seed + 1 /\ ui = 0 /\ reqs = 0 /\ barriers = 0
  /\ w = [s \in Writers |-> WriterInit]
  /\ took = [s \in Writers |-> [p \in Paths |-> {}]]

Step ==
  \/ \E p \in Paths : UIWrite(p) \/ UIDelete(p)
  \/ \E p, q \in Paths : UIRename(p, q)
  \/ \E s \in Writers :
       \/ Checkout(s) \/ Consume(s) \/ Scan(s) \/ Skip(s) \/ PullOnly(s) \/ Claim(s)
       \/ Verify(s) \/ Install(s) \/ Collect(s) \/ Finish(s)
  \/ \E s \in Writers, p \in Paths :
       Edit(s, p) \/ Delete(s, p) \/ Upload(s, p)
  \/ \E s \in Writers, h \in Handles : Sweep(s, h)

\* A tree has had a version at p where its baseline moved to a new version
\* there — a checkout, a consumed entry, a queued change, its own upload
\* becoming the baseline at Finish — or where its TREE did, which is the
\* agent's own edit.  No action writes this; the step does.
TookUpdate ==
  took' = [s \in Writers |-> [p \in Paths |->
             took[s][p]
               \cup (IF w'[s].baseline[p] # Nil /\ w'[s].baseline[p] # w[s].baseline[p]
                     THEN {Gen(w'[s].baseline[p])} ELSE {})
               \* ...and what the tree itself put there: an agent's edit is
               \* a version this writer has had at p, and the history model
               \* counts its own mints the same way (`known`).
               \cup (IF w'[s].local[p] # Nil /\ w'[s].local[p] # w[s].local[p]
                     THEN {Gen(w'[s].local[p])} ELSE {})]]

Next == Step /\ TookUpdate

Spec == Init /\ [][Next]_vars

------------------------------------------------------------------------------
(* The invariants, over state.                                              *)

\* Every citation names a live handle.
Inv_CitationsLive == \A p \in Paths : doc[p] # Nil => doc[p] \in live

\* One handle, one name.
Inv_OneName == \A p, q \in Paths : p # q /\ doc[p] # Nil => doc[p] # doc[q]

\* k was written from h, through any number of edits and copies.
RECURSIVE Derives(_, _)
Derives(k, h) == k = h \/ (k # Nil /\ base[k] # Nil /\ Derives(base[k], h))

\* An acknowledged handle that is gone is accounted for by something in the
\* state that names it.
\* A citation or tombstone at ANOTHER path accounts for it only where the
\* human moved it there (the rename acked the destination): the source's
\* tombstone of a refused rename does not answer for the destination.  A
\* tree accounts for it by having deleted it or by an EDIT on top of it —
\* not by holding it unchanged, which it can never publish once the handle
\* is gone (no diff to upload, a repair the tombstone voids).
Accounted(p, h) ==
  \/ h \in live
  \* A record that names the handle answers for it wherever it was
  \* acknowledged — `Preserved`, which is how the sweep reads records too.
  \* Keyed by PATH instead, a record written where a peer published over the
  \* renamed destination answered for the ack at the destination and not for
  \* the one at the source, though it preserves the one handle both name.
  \/ Preserved(h)
  \/ \E q \in Paths :
       \* At the path the acknowledgement names — or, if that path is where
       \* the handle was MINTED, at a path the human moved it to and was
       \* acknowledged at.  The direction is the move's: an ack at the place
       \* the bytes CAME FROM is answered by what happened where they went;
       \* an ack at the DESTINATION is not answered by the source, whose
       \* tombstone is only the first half of the move (L-119's harm reads
       \* exactly that way round).
       /\ q = p \/ (p = h[1] /\ <<q, h>> \in acked)
       /\ \/ doc[q] # Nil /\ Derives(doc[q], h)
          \/ tomb[q] # Nil /\ Derives(tomb[q], h)
          \* ...or a tree took the handle in THERE and no longer holds it:
          \* it deleted it, edited on top of it, or replaced it, which is
          \* integration followed by ordinary editing.  Still holding it
          \* unchanged is not an answer — that copy can never be published
          \* once the handle is gone.
          \/ \E s \in Writers : Gen(h) \in took[s][q] /\ w[s].local[q] # h
  \/ \E b \in acked : b[1] = p /\ Later(b[2], h)
Inv_AckedNamed == \A a \in acked : Accounted(a[1], a[2])

\* A published version is never silently reverted (2026-09-23, after L-123).
\* When a step replaces the citation `h` at a path with a different `k`, the
\* replacement must be an edit on top of `h` (`k` derives from it), a
\* KNOWING override (a conflict record preserves `h` after the step), or a
\* move (`h` is cited at another path after the step: a rename), the
\* human's own later write, or the committing tree's own edit on top of
\* the version it held there (both below).  Anything
\* else undid a published version with no record — a peer's write lost in
\* every tree.  An ACTION property: "moving forward" is a statement about
\* the step, and `base` (provenance), not mint order, is what orders
\* versions: a version minted first can be published last (r43's false
\* positive, which read `Gen` as publish order).  Deletes are out of scope
\* here: a delete retires into `tomb`, and `Inv_AckedNamed` reads that.
\* Provenance, extended by the human's own sequence at a path: `k` is
\* written on top of `h` (through `base`), or both are acknowledged at `p`
\* and `k` is the later one — the gateway serialises a path's
\* acknowledgements and a rename targets an untracked path, so mint order
\* is the human's order there, as `Inv_AckedNamed`'s last disjunct reads it.
\* Followed as a CHAIN: an agent's edit on top of a UI write that the human
\* made over their own unpublished rename supersedes the renamed version
\* (the third holds run, 18 steps).  The first two drafts allowed each link
\* alone, and each holds run found the next composition.
RECURSIVE Supersedes(_, _, _)
Supersedes(k, h, p) ==
  \/ k = h
  \/ k # Nil /\ base[k] # Nil /\ Supersedes(base[k], h, p)
  \/ <<p, k>> \in acked /\ <<p, h>> \in acked /\ Later(k, h)
SilentRevert(p) ==
  /\ doc[p] # Nil /\ doc'[p] # Nil /\ doc'[p] # doc[p]
  /\ ~Supersedes(doc'[p], doc[p], p)
  /\ ~\E c \in conflicts' : c[2] = doc[p]
  /\ ~\E q \in Paths \ {p} : doc'[q] = doc[p]
  \* The ordinary edit: the committing tree's baseline held the replaced
  \* version at this path, so what it publishes is its own edit on top of
  \* what it had (R7's "a version this tree holds at that path needs no
  \* record").  An edit's `base` is the INTEGRATED version, so two edits made
  \* before the first was published derive from the same base and not from
  \* each other (the second holds run, one writer, 17 steps).  L-123's revert
  \* is not excused by this: the tree held the ADOPTED older version, not the
  \* one its commit replaced.
  /\ ~\E s \in Writers : w[s].pc = "claimed" /\ w[s].baseline[p] = doc[p]
Prop_NoSilentRevert == [][\A p \in Paths : ~SilentRevert(p)]_vars

\* The commit section is one writer's.
Inv_OneHolder == \A s \in Writers : w[s].pc \in {"claimed", "cased"} => holder = s

------------------------------------------------------------------------------
(* Probes: each names an action through what only that action changes.
   A world that violates a probe has taken the step; a probe that holds
   means the world never got there (non-vacuity).                          *)
\* L-125's state: a writer at Finish with a declared removal whose delete
\* its install left cited (outranked).  VIOLATED = the core reaches it.
ProbeRemovalOutranked ==
  ~\E s \in Writers : /\ w[s].pc = "cased"
                     /\ \E p \in w[s].deletes \cap w[s].declared : w[s].inst[p] # Nil
ProbeRenamed == ~\E p, q \in Paths : p # q /\ doc[q] # Nil /\ doc[q][1] = p
ProbeGone    == minted \subseteq live
ProbeRefused == refused = {}
\* L-119: an entry in the cell whose handle the document cites at the
\* SOURCE of a rename still waiting there.  The SHAPE the rule is about,
\* reachable — it fires on the rename itself, at depth 3.  That the RULE
\* fires is the history model's `ProbePendingKept`, a counter written only
\* by the CAS that declines an adoption for the move alone.
ProbePending ==
  ~\E e \in entries :
     \E q \in Paths \ {e[1]} :
       /\ doc[q] = e[2]
       /\ \E r \in removals \cup refused : r.path = q /\ r.to = e[1]
\* The delete-override record's arm, ENABLED: a writer about to install
\* an upload over a path theirs deleted since the merge base.  No
\* invariant judges the record (a delete retires into `tomb`, which
\* `Inv_AckedNamed` already accounts), so `CommitRecordsDeleteOverride =
\* FALSE` fires nothing here; the code's record is pinned by the trace
\* check's `control-delete-override-unrecorded`.  This says the arm is live.
ProbeDeleteOverridden ==
  ~\E s \in Writers, p \in Paths :
     /\ w[s].pc = "claimed" /\ holder = s /\ w[s].verified
     /\ p \in (w[s].uploads \cap w[s].upDone) \ w[s].gone
     /\ Foreign(s, p) /\ doc[p] = Nil /\ MergeBase(s)[p] # Nil
ProbeSurfaced == ~\E c \in conflicts : c[2] # Nil /\ ~Cited(c[2]) /\ \E p \in Paths : doc[p] # Nil /\ base[doc[p]] # c[2] /\ c[1] = p
=============================================================================
