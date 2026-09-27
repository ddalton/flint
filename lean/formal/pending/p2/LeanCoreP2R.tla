----------------------------- MODULE LeanCoreP2R--------------------------------
(* SANDBOX, 2026-09-24: `LeanCoreP2` plus the SAME restart and concurrent
   sync `LeanCoreP1` has — the baseline P1-lite is compared against, so the
   two runs differ in the protocol only (the queue, the merge base and the
   journal here; the derived owed set there).  A restart keeps the tree,
   the baseline, the merge base, the journal's `jSeq` and the queue (all on
   disk in the code); the sync runs between barriers and prunes the queue
   where it moves the merge base (L-123's fix, `QueueYieldsToSync`).  Everything below this paragraph is `LeanCoreP2`'s.

   `LeanCore` with P2 — THE GATEWAY COMMITS DIRECTLY.
   Not shipped; nothing in the code does this yet.  The design and the
   reasons are in docs/plans/flint-lean-simplification-analysis-2026-09-24.md
   (the recommendation, then the user's requirements G1-G3 at the end).

   WHAT CHANGES FROM `LeanCore`
     The gateway is a PARTY, not a producer of cell entries.  A UI save
     uploads a fresh handle (GPut), then CASes the pointer itself (GCas) and
     acknowledges AFTER the CAS.  A rename and a delete are one CAS each.
     So there is no inbox: no `entries`, `saw`, `removals`, `refused`; no
     supersede/stale judgement, no adoption of an uncited version, no
     citation repair (R2, `KeepsAt`, `MovedElsewhere`, `SupersededByUI`,
     `Entombed`), no pending adoption (L-119), no window rule.  A writer
     takes the UI's work the way it takes a peer's: through the merge and
     its queue.

   WHAT STAYS
     The writer's loop (Consume of its queue, Scan, Upload, Claim, Verify
     R4a, Install with R7, Collect, Sweep R4b, Finish), the lease and its
     FIFO for WRITERS, the merge base with the journal's `jSeq`.

   THE USER'S REQUIREMENTS (G1-G3), and how this module states them
     G1  A UI save never waits on the lease.  Structurally: no gateway
         action's guard reads `holder` or a writer's pc, except through
         `LeaseGuard`, which is TRUE in the design; `GatewayIgnoresLease =
         FALSE` is the mutation that makes the gateway wait for a free lease
         (today's window rule, strengthened to the whole section).
         Behaviourally: `Prop_UISaveCompletes` (every started save is
         acknowledged) under `LSpec`, which is weakly fair to the GATEWAY
         ONLY.  No writer action is fair, so every behaviour in which the
         lease holder stops forever while holding the lease — the STALLED
         HOLDER — is among the behaviours checked.  `ProbeStalledSave` shows
         the state "a save is in flight while a writer holds the lease" is
         reached.
     G2  Under heavy saving the pressure is on the writers.  A writer's
         Install merges onto whatever the gateway installed (the CAS is one
         step here; a lost CAS in the code is a re-merge, a stutter of this
         model).  Safety must hold with it: every invariant below.  The
         model cannot show starvation by retries — its CAS is atomic and
         barriers are bounded — so the retry cost is a MEASUREMENT (the
         writer CAS loss rate under autosave), stated as such in the plan.
     G3  A save's fresh upload is safe before its CAS: the sweep spares a
         handle a save has in flight (`GatewaySweepGrace`, the retire/orphan
         age G the code would use).  FALSE is the mutation.

   THE MERGE TABLE AT THE GATEWAY (the user's rule: mine wins, theirs is
   preserved).  A save carries the version the UI read (`base[h]`).  At the
   CAS the document holds `t`: t = the read version -> install; otherwise
   install and record `t` (R7 for the gateway, `GatewaySurfacesForeign`).

   KNOWN LIMITS OF THIS FIRST CUT
     - A delete and a rename are judged against the CURRENT document (one
       atomic step).  The code would carry the served version as If-Match
       and refuse with 412 on a mismatch; that refinement is not modelled.
     - A writer publishing over a path the gateway DELETED re-creates it
       with no record (today's behaviour, pinned by
       `my_edit_meeting_a_peers_delete_recreates_the_path`).  The user's rule
       wants a record; it is a merge-table row still to add.
     - `tomb` is kept as the document's tombstone for the INVARIANT
       (`Inv_AckedNamed` accounts for a deleted ack through it).  No
       protocol action reads it any more.                                 *)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Nil, Paths, Free, Writers,
  MaxMint,     \* generations 2..MaxMint (1 is the seed)
  MaxUI,       \* UI saves
  MaxRemovals, \* UI deletes and renames
  MaxBarriers, \* writer barriers
  \* Writer rules, as in LeanCore.
  CommitSurfacesForeign,  \* R7
  CommitVerifiesUploads,  \* R4a
  SweepUnderLease,        \* R4b
  ForeignPerPath,
  \* Gateway rules.  TRUE = the design; FALSE = the mutation.
  GatewayIgnoresLease,    \* G1: no gateway step waits for the lease
  GatewaySurfacesForeign, \* a save over a version the UI never read is recorded
  GatewaySweepGrace,      \* G3: the sweep spares a save's in-flight upload
  RenameAtomic,           \* a rename is ONE CAS (FALSE: cite the destination,
                          \*   then drop the source, two CASes)
  MaxRestarts, MaxSyncs,
  QueueYieldsToSync       \* L-123's fix; FALSE = the shape before it

ASSUME Free \subseteq Paths
ASSUME MaxMint \in Nat /\ MaxMint >= 1

Seed == 1
\* A re-upload of bytes already uploaded once (a restarted writer's, or a
\* withheld one's) goes to a FRESH key in the code: a copy handle, one
\* per restart and path at most, above the edits' generations.
MaxCopies == MaxRestarts * Cardinality(Paths)
Gens == Seed..(MaxMint + MaxCopies)
Handles == Paths \X Gens
Gen(h) == h[2]
Later(h, k) == Gen(h) > Gen(k)
Opt(S) == S \cup {Nil}

VARIABLES
  live, minted, doc, seq, tomb, base, acked, conflicts, holder,
  nextGen, ui, reqs, barriers, w, took,
  gw,     \* per path, the save in flight: its fresh handle, not yet cited
  mv,     \* a two-CAS rename between its CASes (only when ~RenameAtomic)
  \* The deletes the gateway ACKNOWLEDGED: <<path, the version deleted>>.
  \* Under P2 a UI delete is one CAS and the UI is told it happened, so it
  \* is an observation like `acked` — and the human's own delete is what
  \* answers for a version they saved and then deleted (the first holds run,
  \* 12 steps: save v2, delete it, an agent creates the path afresh; in
  \* `LeanCore` a writer's tree took v2 and performed the delete, so `took`
  \* answered, and here no tree ever held it).
  udel,
  restarts, syncs, regressed,
  upped, copies, orig  \* as in LeanCoreP1: re-uploads go to fresh keys

bucket == <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel>>
vars == <<live, minted, doc, seq, tomb, base, acked, conflicts, holder,
          nextGen, ui, reqs, barriers, w, took, gw, mv, udel, restarts, syncs, regressed,
          upped, copies, orig>>
aux == <<restarts, syncs, regressed, upped, copies, orig>>

Queued == [path : Paths, handle : Opt(Handles), retired : Opt(Handles)]

Writer ==
  [st : {"off", "on"},
   pc : {"idle", "consumed", "scanned", "claimed", "cased"},
   local : [Paths -> Opt(Handles)],
   baseline : [Paths -> Opt(Handles)],
   instBase : [Paths -> Opt(Handles)],
   integrated : SUBSET Gens,
   queue : SUBSET Queued,
   surfaced : SUBSET Paths,
   uploads : SUBSET Paths, deletes : SUBSET Paths,
   snap : [Paths -> Opt(Handles)],
   upDone : SUBSET Paths, gone : SUBSET Paths, verified : BOOLEAN,
   inst : [Paths -> Opt(Handles)],
   retire : SUBSET Handles,
   collected : BOOLEAN,
   seen : Nat,
   jSeq : Nat]

WriterInit ==
  [st |-> "off", pc |-> "idle",
   local |-> [p \in Paths |-> Nil], baseline |-> [p \in Paths |-> Nil],
   instBase |-> [p \in Paths |-> Nil], integrated |-> {}, queue |-> {},
   surfaced |-> {}, uploads |-> {}, deletes |-> {},
   snap |-> [p \in Paths |-> Nil], upDone |-> {}, gone |-> {},
   verified |-> FALSE, inst |-> [p \in Paths |-> Nil], retire |-> {},
   collected |-> FALSE, seen |-> 0, jSeq |-> 0]

TypeOK ==
  /\ live \subseteq Handles /\ minted \subseteq Handles /\ live \subseteq minted
  /\ doc \in [Paths -> Opt(Handles)]
  /\ seq \in Nat
  /\ tomb \in [Paths -> Opt(Handles)]
  /\ base \in [Handles -> Opt(Handles)]
  /\ acked \subseteq Paths \X Handles
  /\ conflicts \subseteq Paths \X Opt(Handles)
  /\ holder \in Writers \cup {"none"}
  /\ nextGen \in Nat /\ ui \in Nat /\ reqs \in Nat /\ barriers \in Nat
  /\ w \in [Writers -> Writer]
  /\ took \in [Writers -> [Paths -> SUBSET Gens]]
  /\ gw \in [Paths -> Opt(Handles)]
  /\ mv \in Opt(Paths \X Handles)
  /\ udel \subseteq Paths \X Handles
  /\ restarts \in Nat /\ syncs \in Nat /\ regressed \in BOOLEAN

------------------------------------------------------------------------------
On(s) == w[s].st = "on"
\* The bytes a handle holds: a copy holds its original's.
Content(h) == IF h # Nil /\ orig[h] # Nil THEN orig[h] ELSE h
RECURSIVE Derives(_, _)
Derives(k, h) == \/ k = h
                 \/ k # Nil /\ h # Nil /\ Content(k) = Content(h)
                 \/ k # Nil /\ base[k] # Nil /\ Derives(base[k], h)
Cited(h) == \E p \in Paths : doc[p] = h
Preserved(h) == \E c \in conflicts : c[2] = h
InFlight(h) == \E p \in Paths : gw[p] = h
\* G1's switch.  TRUE in the design: the gateway never looks at the lease.
LeaseGuard == GatewayIgnoresLease \/ holder = "none"
\* The gateway serialises a path: one save, rename or delete at a time.
Busy(p) == gw[p] # Nil \/ (mv # Nil /\ mv[1] = p)

------------------------------------------------------------------------------
(* The gateway: a party that CASes the pointer itself.                     *)

\* A UI save starts: the bytes land at a fresh handle, derived from the
\* version the UI read.  Not yet acknowledged.
GPut(p) ==
  /\ ui < MaxUI /\ nextGen <= MaxMint /\ ~Busy(p)
  /\ LET h == <<p, nextGen>> IN
     /\ live' = live \cup {h} /\ minted' = minted \cup {h}
     /\ base' = [base EXCEPT ![h] = doc[p]]
     /\ gw' = [gw EXCEPT ![p] = h]
     /\ nextGen' = nextGen + 1 /\ ui' = ui + 1
     /\ upped' = upped \cup {h}
  /\ UNCHANGED <<doc, seq, tomb, acked, conflicts, holder, reqs, barriers, w, mv, udel, restarts, syncs, regressed, copies, orig>>

\* ...and completes: the CAS onto the CURRENT document, then the ack.  Mine
\* wins; a version the UI never read (a writer published after the read) is
\* preserved by a record.  What the save replaced is retired: nothing cites
\* it again (no party re-cites), so it goes straight to deletion here —
\* the code defers it by the grace G, which only the SWEEP needs (G3).
GCas(p) ==
  /\ gw[p] # Nil /\ LeaseGuard
  /\ LET h == gw[p]
         t == doc[p]
     IN
     /\ doc' = [doc EXCEPT ![p] = h]
     /\ seq' = seq + 1
     /\ tomb' = [tomb EXCEPT ![p] = Nil]
     /\ conflicts' = conflicts \cup
                       (IF GatewaySurfacesForeign /\ t # Nil /\ t # base[h]
                        THEN {<<p, t>>} ELSE {})
     /\ live' = IF t # Nil THEN live \ {t} ELSE live
     /\ acked' = acked \cup {<<p, h>>}
     /\ gw' = [gw EXCEPT ![p] = Nil]
  /\ UNCHANGED <<minted, base, holder, nextGen, ui, reqs, barriers, w, mv, udel, aux>>

\* A rename is ONE CAS: the destination cites the source's handle and the
\* source is gone, in the same generation.  Nothing is minted, nothing
\* retired.  Acknowledged after the CAS.
GRename(p, q) ==
  /\ reqs < MaxRemovals /\ LeaseGuard
  /\ p # q /\ doc[p] # Nil /\ doc[q] = Nil /\ ~Busy(p) /\ ~Busy(q) /\ mv = Nil
  /\ LET h == doc[p] IN
     IF RenameAtomic
     THEN /\ doc' = [doc EXCEPT ![q] = h, ![p] = Nil]
          /\ tomb' = [tomb EXCEPT ![q] = Nil, ![p] = h]
          /\ acked' = acked \cup {<<q, h>>}
          /\ UNCHANGED mv
     ELSE /\ doc' = [doc EXCEPT ![q] = h]
          /\ tomb' = [tomb EXCEPT ![q] = Nil]
          /\ mv' = <<p, h>>
          /\ UNCHANGED acked
  /\ seq' = seq + 1 /\ reqs' = reqs + 1
  /\ UNCHANGED <<live, minted, base, conflicts, holder, gw, udel, nextGen, ui, barriers, w, aux>>

\* The mutation's second CAS: drop the source.
GRenameFinish ==
  /\ mv # Nil /\ LeaseGuard
  /\ LET p == mv[1]
         h == mv[2]
     IN /\ doc' = [doc EXCEPT ![p] = IF doc[p] = h THEN Nil ELSE @]
        /\ tomb' = [tomb EXCEPT ![p] = IF doc[p] = h THEN h ELSE @]
        /\ acked' = acked \cup {<<CHOOSE q \in Paths : q # p /\ doc[q] = h, h>>}
  /\ mv' = Nil /\ seq' = seq + 1
  /\ UNCHANGED <<live, minted, base, conflicts, holder, gw, udel, nextGen, ui, reqs, barriers, w, aux>>

\* A delete is one CAS; the retired handle is deleted with it.
GDelete(p) ==
  /\ reqs < MaxRemovals /\ LeaseGuard
  /\ doc[p] # Nil /\ ~Busy(p)
  /\ doc' = [doc EXCEPT ![p] = Nil]
  /\ tomb' = [tomb EXCEPT ![p] = doc[p]]
  /\ live' = live \ {doc[p]}
  /\ udel' = udel \cup {<<p, doc[p]>>}
  /\ seq' = seq + 1 /\ reqs' = reqs + 1
  /\ UNCHANGED <<minted, base, acked, conflicts, holder, gw, mv, nextGen, ui, barriers, w, aux>>

------------------------------------------------------------------------------
(* The agent.                                                               *)

Edit(s, p) ==
  /\ On(s) /\ nextGen <= MaxMint
  /\ LET h == <<p, nextGen>> IN
     /\ minted' = minted \cup {h}
     /\ base' = [base EXCEPT ![h] = w[s].baseline[p]]
     /\ w' = [w EXCEPT ![s].local[p] = h, ![s].integrated = @ \cup {nextGen}]
     /\ nextGen' = nextGen + 1
  /\ UNCHANGED <<live, doc, seq, tomb, acked, conflicts, holder, ui, reqs, barriers, gw, mv, udel, aux>>

Delete(s, p) ==
  /\ On(s) /\ w[s].local[p] # Nil
  /\ w' = [w EXCEPT ![s].local[p] = Nil]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

Checkout(s) ==
  /\ w[s].st = "off"
  /\ w' = [w EXCEPT ![s].st = "on",
                    ![s].local = doc, ![s].baseline = doc, ![s].instBase = doc, ![s].inst = doc,
                    ![s].integrated = {Gen(doc[p]) : p \in {q \in Paths : doc[q] # Nil}},
                    ![s].seen = seq]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

------------------------------------------------------------------------------
(* Step 1: the writer's queue into the tree — the only way anything reaches
   it now, a UI save included.  Unchanged from `LeanCore` for queued
   changes; the cell's half is gone.                                      *)
Consume(s) ==
  /\ On(s) /\ w[s].pc = "idle" /\ barriers < MaxBarriers
  /\ LET
       W == w[s]
       qUp == {<<r.path, r.handle>> : r \in {x \in W.queue : x.handle # Nil}}
       qDel == {r.path : r \in {x \in W.queue : x.handle = Nil}}
       qRet(p) == (CHOOSE r \in W.queue : r.path = p /\ r.handle = Nil).retired
       already == {e \in qUp : W.baseline[e[1]] = e[2]}
       missing == {e \in qUp \ already : e[2] \notin live}
       liveE == (qUp \ already) \ missing
       adoptable == {e \in liveE : W.local[e[1]] = W.baseline[e[1]]}
       dirtyConf == liveE \ adoptable
       adoptPaths == {e[1] : e \in adoptable}
       surfPaths == {e[1] : e \in dirtyConf}
       advPaths == adoptPaths \cup surfPaths
       GenAt(p) == CHOOSE h \in {e[2] : e \in {x \in liveE : x[1] = p}} : TRUE
       local1 == [p \in Paths |-> IF p \in adoptPaths THEN GenAt(p) ELSE W.local[p]]
       base1 == [p \in Paths |-> IF p \in advPaths THEN GenAt(p) ELSE W.baseline[p]]
       tAbsent == {p \in qDel : local1[p] = Nil}
       supersedes(p) == doc[p] # Nil /\ doc[p] # qRet(p)
       tSuper == {p \in qDel \ tAbsent : supersedes(p)}
       tKept == {p \in (qDel \ tAbsent) \ tSuper : base1[p] = Nil \/ local1[p] # base1[p]}
       tRemoved == qDel \ (tAbsent \cup tSuper \cup tKept)
     IN
       /\ w' = [w EXCEPT ![s].pc = "consumed",
                 ![s].local = [p \in Paths |-> IF p \in tRemoved THEN Nil ELSE local1[p]],
                 ![s].baseline = [p \in Paths |-> IF p \in tAbsent \cup tRemoved THEN Nil ELSE base1[p]],
                 ![s].integrated = @ \cup {Gen(GenAt(p)) : p \in advPaths},
                 ![s].queue = {},
                 ![s].instBase = [p \in Paths |->
                                    IF p \in tSuper /\ qRet(p) # Nil THEN qRet(p) ELSE @[p]],
                 ![s].surfaced = (@ \ (adoptPaths \cup tAbsent \cup tRemoved)) \cup surfPaths]
       /\ conflicts' = conflicts \cup dirtyConf \cup missing \cup {<<p, Nil>> : p \in tKept}
       /\ regressed' = (regressed \/ \E p \in adoptPaths :
                          W.baseline[p] # Nil /\ GenAt(p) # W.baseline[p] /\ Derives(W.baseline[p], GenAt(p))
                          /\ <<p, W.baseline[p]>> \notin conflicts)
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, restarts, syncs, upped, copies, orig>>

Scan(s) ==
  /\ On(s) /\ w[s].pc = "consumed"
  /\ LET W == w[s]
         dirty == {p \in Paths : W.local[p] # W.baseline[p]}
         ups == {p \in dirty : W.local[p] # Nil}
         absent == {p \in dirty : W.local[p] = Nil}
     IN \E dels \in SUBSET absent :
          w' = [w EXCEPT ![s].pc = "scanned",
                         ![s].uploads = ups, ![s].deletes = dels,
                         ![s].snap = W.local,
                         ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
                         ![s].retire = {}, ![s].collected = FALSE]
  /\ barriers' = barriers + 1
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, aux>>

Skip(s) ==
  /\ On(s) /\ w[s].pc = "consumed" /\ barriers < MaxBarriers
  /\ \A p \in Paths : w[s].local[p] = w[s].baseline[p] /\ w[s].baseline[p] = w[s].instBase[p]
  /\ seq = w[s].seen
  /\ w' = [w EXCEPT ![s].pc = "idle"]
  /\ barriers' = barriers + 1
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, aux>>

Upload(s, p) ==
  /\ On(s) /\ w[s].pc = "scanned"
  /\ p \in w[s].uploads \ w[s].upDone
  /\ LET h == w[s].snap[p] IN
     IF h \notin upped
     THEN /\ live' = live \cup {h} /\ upped' = upped \cup {h}
          /\ w' = [w EXCEPT ![s].upDone = @ \cup {p}]
          /\ UNCHANGED <<minted, base, copies, orig>>
     \* Bytes PUT before: the code's PUT goes to a fresh key.  The tree's file
     \* is those bytes; the copy handle names them from here on.
     ELSE /\ copies < MaxCopies
          /\ LET c == <<p, MaxMint + copies + 1>> IN
             /\ live' = live \cup {c} /\ upped' = upped \cup {c} /\ minted' = minted \cup {c}
             /\ base' = [base EXCEPT ![c] = base[h]]
             /\ orig' = [orig EXCEPT ![c] = Content(h)]
             /\ w' = [w EXCEPT ![s].upDone = @ \cup {p}, ![s].snap[p] = c,
                               ![s].local[p] = IF @ = h THEN c ELSE @]
          /\ copies' = copies + 1
  /\ UNCHANGED <<doc, seq, tomb, acked, conflicts, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, restarts, syncs, regressed>>

------------------------------------------------------------------------------
MergeBase(s) ==
  IF w[s].jSeq # 0 /\ w[s].jSeq = seq THEN doc ELSE w[s].instBase
Foreign(s, p) == doc[p] # MergeBase(s)[p]
Retired(s, p) == IF MergeBase(s)[p] # Nil THEN MergeBase(s)[p] ELSE w[s].baseline[p]
MineInMerge(s) == w[s].uploads \cap w[s].upDone
MergeForeign(s) ==
  {[path |-> p, handle |-> doc[p], retired |-> Nil] :
     p \in {q \in Paths : q \notin MineInMerge(s) /\ Foreign(s, q) /\ doc[q] # Nil}}
MergeGone(s) ==
  {[path |-> p, handle |-> Nil, retired |-> Retired(s, p)] :
     p \in {q \in Paths : /\ q \notin MineInMerge(s) \cup w[s].deletes
                          /\ doc[q] = Nil /\ MergeBase(s)[q] # Nil}}
QueueUpsert(q, new) == {r \in q : r.path \notin {x.path : x \in new}} \cup new

PullOnly(s) ==
  /\ On(s) /\ w[s].pc = "scanned"
  /\ w[s].uploads = {} /\ w[s].deletes = {}
  /\ w' = [w EXCEPT ![s].pc = "idle",
                    ![s].queue = QueueUpsert(@, MergeForeign(s) \cup MergeGone(s)),
                    ![s].instBase = doc, ![s].inst = doc, ![s].seen = seq,
                    ![s].snap = [p \in Paths |-> Nil],
                    ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

------------------------------------------------------------------------------
(* The writers' commit section: one writer at a time.  The gateway is not in
   it and does not wait for it (G1).                                       *)

Claim(s) ==
  /\ On(s) /\ w[s].pc = "scanned"
  /\ w[s].uploads \subseteq w[s].upDone
  /\ w[s].uploads \cup w[s].deletes # {}
  /\ holder = "none"
  /\ holder' = s
  /\ w' = [w EXCEPT ![s].pc = "claimed"]
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                 nextGen, ui, reqs, barriers, aux>>

Verify(s) ==
  /\ On(s) /\ w[s].pc = "claimed" /\ holder = s /\ ~w[s].verified
  /\ w' = [w EXCEPT ![s].verified = TRUE,
                    ![s].gone = IF CommitVerifiesUploads
                                THEN {p \in w[s].uploads \cap w[s].upDone : w[s].snap[p] \notin live}
                                ELSE {}]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

\* The merge and the CAS: mine = the uploads that survived the re-read and
\* the deletes; theirs = what the document has that the merge base did not
\* — a peer's publish or the gateway's, alike.  No repairs: every handle
\* this install cites is its own fresh upload or already in the document.
Install(s) ==
  /\ On(s) /\ w[s].pc = "claimed" /\ holder = s /\ w[s].verified
  /\ LET W == w[s]
         mine == W.uploads \cap W.upDone
         contested == IF CommitSurfacesForeign
                      THEN {p \in mine \ W.gone :
                              /\ Foreign(s, p) /\ doc[p] # Nil
                              /\ IF ForeignPerPath
                                 THEN doc[p] # W.baseline[p]
                                 ELSE Gen(doc[p]) \notin W.integrated}
                      ELSE {}
         inst == [p \in Paths |->
                    IF p \in W.gone THEN doc[p]
                    ELSE IF p \in mine THEN W.snap[p]
                    ELSE IF Foreign(s, p) THEN doc[p]
                    ELSE IF p \in W.deletes THEN Nil
                    ELSE doc[p]]
         nothing == inst = doc
         retired == {doc[p] : p \in {q \in Paths : doc[q] # Nil /\ inst[q] # doc[q]}}
     IN
       /\ doc' = inst
       /\ seq' = IF nothing THEN seq ELSE seq + 1
       /\ tomb' = [p \in Paths |-> IF inst[p] # Nil THEN Nil
                                   ELSE IF doc[p] # Nil THEN doc[p]
                                   ELSE tomb[p]]
       /\ conflicts' = conflicts \cup {<<p, W.snap[p]>> : p \in W.gone}
                                 \cup {<<p, doc[p]>> : p \in contested}
       /\ w' = [w EXCEPT ![s].pc = "cased",
                         ![s].upDone = @ \ W.gone,
                         ![s].inst = inst, ![s].retire = retired,
                         ![s].seen = IF nothing THEN seq ELSE seq + 1,
                         ![s].jSeq = IF nothing THEN seq ELSE seq + 1,
                         ![s].collected = retired = {},
                         ![s].queue = QueueUpsert(@, MergeForeign(s) \cup MergeGone(s))]
  /\ UNCHANGED <<live, minted, base, acked, holder, gw, mv, udel, nextGen, ui, reqs, barriers, aux>>

\* Step 6: the retired set, in one batch, sparing NOTHING.  `LeanCore`
\* spared what the install still cites elsewhere (R3) and what an unconsumed
\* entry names; with no re-cite and no cell neither can happen — the claim
\* this module exists to test (Inv_CitationsLive).
Collect(s) ==
  /\ On(s) /\ w[s].pc = "cased" /\ ~w[s].collected
  /\ live' = live \ w[s].retire
  /\ w' = [w EXCEPT ![s].retire = {}, ![s].collected = TRUE]
  /\ UNCHANGED <<minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, aux>>

\* The orphan sweep (R4b), sparing a save's in-flight upload (G3).
Sweep(s, h) ==
  /\ On(s)
  /\ SweepUnderLease => (holder = s /\ w[s].pc \in {"claimed", "cased"})
  /\ h \in live /\ ~Cited(h) /\ ~Preserved(h)
  /\ GatewaySweepGrace => ~InFlight(h)
  /\ ~\E p \in w[s].upDone : w[s].snap[p] = h
  /\ live' = live \ {h}
  /\ UNCHANGED <<minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, w, aux>>

Finish(s) ==
  /\ On(s) /\ w[s].pc = "cased" /\ w[s].collected
  /\ LET W == w[s] IN
     w' = [w EXCEPT ![s].pc = "idle",
             ![s].baseline = [p \in Paths |->
                                IF p \in W.uploads \cap W.upDone THEN W.snap[p]
                                ELSE IF p \in W.deletes /\ W.inst[p] = Nil THEN Nil
                                ELSE @[p]],
             ![s].instBase = W.inst,
             ![s].surfaced = @ \ ((W.uploads \cap W.upDone)
                                  \cup {p \in W.deletes : W.inst[p] = Nil}),
             ![s].uploads = {}, ![s].deletes = {}, ![s].snap = [p \in Paths |-> Nil],
             ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
             ![s].collected = FALSE]
  /\ holder' = "none"
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                 nextGen, ui, reqs, barriers, aux>>


------------------------------------------------------------------------------
(* The restart and the sync, the same as `LeanCoreP1`'s.                   *)

Owed(s, p) == doc[p] # w[s].baseline[p] /\ w[s].local[p] = w[s].baseline[p]
\* A version displaced KNOWINGLY carries a conflict record (R7): stepping
\* back over it is a recorded revert, not L-123's silent one — the same
\* exemption as LeanSubtree's Inv_ConsumeNeverRegresses.  (Found as a
\* regress at first: a restarted writer re-publishes its version over a UI
\* save derived from it, recording the save, and a peer follows.)
Back(s, p) == /\ doc[p] # Nil /\ w[s].baseline[p] # Nil /\ Derives(w[s].baseline[p], doc[p])
              /\ Content(doc[p]) # Content(w[s].baseline[p])
              /\ <<p, w[s].baseline[p]>> \notin conflicts

Restart(s) ==
  /\ On(s) /\ restarts < MaxRestarts
  /\ w' = [w EXCEPT ![s].pc = "idle",
                    ![s].uploads = {}, ![s].deletes = {},
                    ![s].snap = [p \in Paths |-> Nil],
                    ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
                    ![s].inst = [p \in Paths |-> Nil], ![s].retire = {},
                    ![s].collected = FALSE]
  /\ holder' = IF holder = s THEN "none" ELSE holder
  /\ restarts' = restarts + 1
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                 nextGen, ui, reqs, barriers, regressed, syncs, upped, copies, orig>>

\* A sync, between barriers (one process owns a tree): it takes what is
\* owed, moves the merge base there, and — `QueueYieldsToSync`, L-123's fix,
\* `sync.rs` step 6 — prunes the queue where it moved the merge base to a
\* version other than the queued one.
Sync(s) ==
  /\ On(s) /\ w[s].pc = "idle" /\ syncs < MaxSyncs
  /\ \E p \in Paths : Owed(s, p)
  /\ LET owed == {p \in Paths : Owed(s, p)} IN
     w' = [w EXCEPT ![s].local = [p \in Paths |-> IF p \in owed THEN doc[p] ELSE @[p]],
                    ![s].baseline = [p \in Paths |-> IF p \in owed THEN doc[p] ELSE @[p]],
                    ![s].instBase = [p \in Paths |-> IF p \in owed THEN doc[p] ELSE @[p]],
                    ![s].queue = IF QueueYieldsToSync
                                 THEN {r \in @ : r.path \notin owed \/ r.handle = doc[r.path]}
                                 ELSE @,
                    ![s].integrated = @ \cup {Gen(doc[p]) : p \in {q \in owed : doc[q] # Nil}}]
  /\ syncs' = syncs + 1
  /\ regressed' = (regressed \/ \E p \in Paths : Owed(s, p) /\ Back(s, p))
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, restarts, upped, copies, orig>>

------------------------------------------------------------------------------
Init ==
  /\ live = {<<p, Seed>> : p \in Paths \ Free}
  /\ minted = live
  /\ doc = [p \in Paths |-> IF p \in Free THEN Nil ELSE <<p, Seed>>]
  /\ seq = 1
  /\ tomb = [p \in Paths |-> Nil]
  /\ base = [h \in Handles |-> Nil]
  /\ acked = {} /\ conflicts = {}
  /\ holder = "none"
  /\ nextGen = Seed + 1 /\ ui = 0 /\ reqs = 0 /\ barriers = 0
  /\ w = [s \in Writers |-> WriterInit]
  /\ took = [s \in Writers |-> [p \in Paths |-> {}]]
  /\ gw = [p \in Paths |-> Nil]
  /\ mv = Nil
  /\ udel = {}
  /\ restarts = 0 /\ syncs = 0 /\ regressed = FALSE
  /\ upped = live /\ copies = 0 /\ orig = [h \in Handles |-> Nil]

GatewayStep ==
  \/ \E p \in Paths : GPut(p) \/ GCas(p) \/ GDelete(p)
  \/ \E p, q \in Paths : GRename(p, q)
  \/ GRenameFinish

WriterStep ==
  \/ \E s \in Writers :
       \/ Checkout(s) \/ Consume(s) \/ Scan(s) \/ Skip(s) \/ PullOnly(s) \/ Claim(s)
       \/ Verify(s) \/ Install(s) \/ Collect(s) \/ Finish(s) \/ Restart(s) \/ Sync(s)
  \/ \E s \in Writers, p \in Paths : Edit(s, p) \/ Delete(s, p) \/ Upload(s, p)
  \/ \E s \in Writers, h \in Handles : Sweep(s, h)

TookUpdate ==
  took' = [s \in Writers |-> [p \in Paths |->
             took[s][p]
               \cup (IF w'[s].baseline[p] # Nil /\ w'[s].baseline[p] # w[s].baseline[p]
                     THEN {Gen(w'[s].baseline[p])} ELSE {})
               \cup (IF w'[s].local[p] # Nil /\ w'[s].local[p] # w[s].local[p]
                     THEN {Gen(w'[s].local[p])} ELSE {})]]

Next == (GatewayStep \/ WriterStep) /\ TookUpdate
Spec == Init /\ [][Next]_vars

\* G1, behaviourally.  Fair to the gateway's CAS ONLY: a writer may stop at
\* any point, the lease holder included, and never move again.
LSpec == Spec /\ \A p \in Paths : WF_vars(GCas(p) /\ TookUpdate)

------------------------------------------------------------------------------
(* The claims.  The first four and the action property are LeanCore's,
   word for word, over this module's state.                                *)

Inv_CitationsLive == \A p \in Paths : doc[p] # Nil => doc[p] \in live
Inv_OneName == \A p, q \in Paths : p # q /\ doc[p] # Nil => doc[p] # doc[q]


Accounted(p, h) ==
  \/ h \in live
  \/ Preserved(h)
  \/ \E q \in Paths :
       /\ q = p \/ (p = h[1] /\ <<q, h>> \in acked)
       /\ \/ doc[q] # Nil /\ Derives(doc[q], h)
          \/ tomb[q] # Nil /\ Derives(tomb[q], h)
          \/ \E s \in Writers : Gen(h) \in took[s][q] /\ w[s].local[q] # h
          \* ...or the human deleted it, or a version derived from it, there,
          \* and was told so (P2's acknowledged delete; `LeanCore` answers
          \* the same case through the writer that performed the removal).
          \/ \E d \in udel : d[1] = q /\ Derives(d[2], h)
  \/ \E b \in acked : b[1] = p /\ Later(b[2], h)
Inv_AckedNamed == \A a \in acked : Accounted(a[1], a[2])

RECURSIVE Supersedes(_, _, _)
Supersedes(k, h, p) ==
  \/ k = h
  \/ k # Nil /\ h # Nil /\ Content(k) = Content(h)
  \/ k # Nil /\ base[k] # Nil /\ Supersedes(base[k], h, p)
  \/ <<p, k>> \in acked /\ <<p, h>> \in acked /\ Later(k, h)
SilentRevert(p) ==
  /\ doc[p] # Nil /\ doc'[p] # Nil /\ doc'[p] # doc[p]
  /\ ~Supersedes(doc'[p], doc[p], p)
  /\ ~\E c \in conflicts' : c[2] = doc[p]
  /\ ~\E q \in Paths \ {p} : doc'[q] = doc[p]
  \* The publishing writer knew the displaced version: its baseline holds
  \* it, or its tree held it at p (`took`).  The second clause is for a
  \* writer that restarted between its CAS and step 7: its baseline is
  \* stale, and its next publish replaces its OWN earlier one (2026-09-25,
  \* the paired run's first holds world; applied to both models).
  /\ ~\E s \in Writers : w[s].pc = "claimed"
                         /\ (w[s].baseline[p] = doc[p] \/ Gen(doc[p]) \in took[s][p])
Prop_NoSilentRevert == [][\A p \in Paths : ~SilentRevert(p)]_vars

Inv_OneHolder == \A s \in Writers : w[s].pc \in {"claimed", "cased"} => holder = s

\* G1: every save the gateway starts is acknowledged.  Checked under LSpec.
Prop_UISaveCompletes == \A p \in Paths : (gw[p] # Nil) ~> (gw[p] = Nil)

------------------------------------------------------------------------------
(* Probes (non-vacuity): a world that violates one has taken the step.     *)
\* A save in flight while a writer holds the lease: the stalled-holder shape.
ProbeStalledSave == ~(holder # "none" /\ \E p \in Paths : gw[p] # Nil)
\* ...and a save COMPLETED while a writer held the lease (an action probe,
\* checked as a PROPERTY: violated = the step was taken).
ProbeSavedUnderLease ==
  [][~(holder # "none" /\ \E p \in Paths : gw[p] # Nil /\ gw'[p] = Nil)]_vars
ProbeRenamed == ~\E p, q \in Paths : p # q /\ doc[q] # Nil /\ doc[q][1] = p
ProbeSurfaced == conflicts = {}

Inv_NoRegress == ~regressed
ProbeRepublish ==
  [][~\E s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased"
        /\ \E p \in (w[s].uploads \cap w[s].upDone) \ w'[s].gone :
              doc[p] # Nil /\ doc[p] # w[s].snap[p] /\ Derives(doc[p], w[s].snap[p])]_vars
ProbeRestartAfterCas == [][~\E s \in Writers : w[s].pc = "cased" /\ restarts' = restarts + 1]_vars
=============================================================================
