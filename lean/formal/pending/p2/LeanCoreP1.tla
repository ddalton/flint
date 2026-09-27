----------------------------- MODULE LeanCoreP1 --------------------------------
(* SANDBOX, 2026-09-24: P1-lite on top of P2 (`LeanCoreP2.tla`).  Step 3 of
   docs/plans/flint-lean-simplification-analysis-2026-09-24.md.  Not shipped.

   P1-LITE: THE BASELINE IS THE MERGE BASE; WHAT THE TREE IS OWED IS DERIVED
     P2 removed adoption: no tree ever holds a version the document never
     cited, so the reason to keep a merge base apart from the baseline
     (`state.rs:59-66`) is gone.  This module deletes
       - `instBase` (the merge base), `jSeq` (the journal's installed seq,
         `installed_etag` in the code) and `MergeBase`: a writer merges
         against its baseline;
       - the writer-local QUEUE and the Consume step that drained it:
     and derives instead
       Owed(s, p) == doc[p] # baseline[p] /\ the tree is clean at p
     which the barrier's Consume takes in one step (a barrier waits for
     its fetches before it scans), and so does a SYNC, between barriers
     (one process owns a tree; `sync` and `run_barrier` take `&mut
     Syncer`).  Nothing is stored between deriving what is owed and taking
     it, so nothing can be overtaken: L-123's regress has no route here,
     where the queue needs its prune (`LeanCoreP2R`'s control).  Two
     earlier cuts let fetches land later — at any point, then from a
     concurrent sync — which the code cannot do; the second found a
     counterexample in BOTH models that no code path reaches.

   THREE ADDITIONS THE ANALYSIS NAMED
     - CONTENT CONVERGENCE (`ContentConverges`): the scan adopts a dirty
       path whose bytes ARE the document's (here: the tree holds the handle
       the document cites) — the baseline moves, nothing is published.  With
       no journal this is what recovers a writer that restarted after its
       CAS and before step 7 (`Restart`).
     - M3, THE DELETE-VS-EDIT ROW (`DeleteWinsPreserved`): the user's rule
       "mine wins, theirs is preserved".  A delete meeting a version theirs
       installed since the baseline APPLIES, and theirs is recorded.  FALSE
       is today's outranking, which under P1-lite never resolves: the tree
       lacks p, the document cites theirs, the path is dirty so nothing is
       owed, and every barrier repeats (`Prop_DeleteSettles`).
     - RESTART: a writer loses everything but its tree and its baseline
       (the state db), and a lease it held expires.

   CONTROLS (each must break its claim)
     - `UIAdoptsUncited = TRUE`: P1 WITHOUT P2.  A tree takes a UI save
       before the document cites it (today's cell adoption); the derived
       owed set then points BACKWARDS and the tree takes the older cited
       version over it (`Inv_NoRegress`, the README's item 3).
     - `DeleteWinsPreserved = FALSE` (`Prop_DeleteSettles`).
     - `ForeignPerPath`, `CommitSurfacesForeign`, `CommitVerifiesUploads`
       as in `LeanCoreP2`.

   NOT MODELLED: the owed set is not filtered by a sync scope (the core has
   no scope), so `Owed \subseteq Scope` is not checked here; the H10 ack
   carrier and the D5 ticker read the queue in the code today (M2), and
   what replaces them is a code change, not a protocol one.              *)
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
  \* P1-lite.  TRUE = the design unless noted.
  ContentConverges,       \* the scan adopts dirty bytes that ARE the document's
  DeleteWinsPreserved,    \* M3: a delete over theirs applies; theirs is recorded
  UIAdoptsUncited,        \* FALSE = P2.  TRUE: a tree may adopt an in-flight save
  MaxRestarts,
  MaxSyncs

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
  restarts,
  syncs,
  upped,  \* every handle ever PUT (a second PUT of its bytes is a copy)
  copies, \* copies minted
  orig,   \* a copy's original: the handle whose bytes it holds
  \* Ghost: a tree took a version its own version derives from (L-123's
  \* class: a stale change landing over the newer one it was overtaken by).
  regressed

bucket == <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel>>
vars == <<live, minted, doc, seq, tomb, base, acked, conflicts, holder,
          nextGen, ui, reqs, barriers, w, took, gw, mv, udel, restarts, regressed, syncs,
          upped, copies, orig>>
aux == <<restarts, regressed, syncs, upped, copies, orig>>

Writer ==
  [st : {"off", "on"},
   pc : {"idle", "consumed", "scanned", "claimed", "cased"},
   local : [Paths -> Opt(Handles)],
   baseline : [Paths -> Opt(Handles)],   \* ALSO the merge base
   integrated : SUBSET Gens,
   uploads : SUBSET Paths, deletes : SUBSET Paths,
   snap : [Paths -> Opt(Handles)],
   upDone : SUBSET Paths, gone : SUBSET Paths, verified : BOOLEAN,
   inst : [Paths -> Opt(Handles)],
   retire : SUBSET Handles,
   collected : BOOLEAN,
   seen : Nat]

WriterInit ==
  [st |-> "off", pc |-> "idle",
   local |-> [p \in Paths |-> Nil], baseline |-> [p \in Paths |-> Nil],
   integrated |-> {},
   uploads |-> {}, deletes |-> {},
   snap |-> [p \in Paths |-> Nil], upDone |-> {}, gone |-> {},
   verified |-> FALSE, inst |-> [p \in Paths |-> Nil], retire |-> {},
   collected |-> FALSE, seen |-> 0]

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
  /\ restarts \in Nat /\ regressed \in BOOLEAN /\ syncs \in Nat

------------------------------------------------------------------------------
On(s) == w[s].st = "on"
\* h is k or an ancestor of it: the version k was derived from, transitively.
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
                    ![s].local = doc, ![s].baseline = doc, ![s].inst = doc,
                    ![s].integrated = {Gen(doc[p]) : p \in {q \in Paths : doc[q] # Nil}},
                    ![s].seen = seq]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

------------------------------------------------------------------------------
(* Step 1, P1-lite: the tree is OWED what the document cites where the
   baseline differs and the tree is clean.  Derived, never stored.        *)
Owed(s, p) == doc[p] # w[s].baseline[p] /\ w[s].local[p] = w[s].baseline[p]
\* Taking doc[p] at p would step the tree BACK to a version its own derives
\* from (L-123's class).
\* A version displaced KNOWINGLY carries a conflict record (R7): stepping
\* back over it is a recorded revert, not L-123's silent one — the same
\* exemption as LeanSubtree's Inv_ConsumeNeverRegresses.  (Found as a
\* regress at first: a restarted writer re-publishes its version over a UI
\* save derived from it, recording the save, and a peer follows.)
Back(s, p) == /\ doc[p] # Nil /\ w[s].baseline[p] # Nil /\ Derives(w[s].baseline[p], doc[p])
              /\ Content(doc[p]) # Content(w[s].baseline[p])
              /\ <<p, w[s].baseline[p]>> \notin conflicts

\* Step 1, the barrier's own fetches: every owed path, in ONE step.  A
\* barrier fetches each path once and waits for its fetches before it scans
\* (the fetcher's shape the design fixes, 2026-09-24), so its own fetches
\* cannot overtake one another.  A version the document cites is live
\* (Inv_CitationsLive), so the GET succeeds.
Consume(s) ==
  /\ On(s) /\ w[s].pc = "idle" /\ barriers < MaxBarriers
  /\ LET owed == {p \in Paths : Owed(s, p)} IN
     /\ w' = [w EXCEPT ![s].pc = "consumed",
                    ![s].local = [p \in Paths |-> IF p \in owed THEN doc[p] ELSE @[p]],
                    ![s].baseline = [p \in Paths |-> IF p \in owed THEN doc[p] ELSE @[p]],
                    ![s].integrated = @ \cup {Gen(doc[p]) : p \in {q \in owed : doc[q] # Nil}}]
     /\ regressed' = (regressed \/ \E p \in owed : Back(s, p))
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, restarts, syncs, upped, copies, orig>>

\* A SYNC (`flint sync`, the reader's pull) takes what is owed, in ONE step
\* between barriers.  The code cannot run it inside one: `sync` and
\* `run_barrier` both take `&mut Syncer`, and the state directory's flock
\* (`state.rs:356-361`) admits one process per tree.  (A second cut let a
\* sync's fetches land at any point of the loop; it found a counterexample
\* in both this module and the queue baseline that the code cannot reach.)
\* With nothing stored between the derivation and the fetch, no fetch can be
\* overtaken — which is what makes L-123 impossible here rather than fixed.
Sync(s) ==
  /\ On(s) /\ w[s].pc = "idle" /\ syncs < MaxSyncs
  /\ \E p \in Paths : Owed(s, p)
  /\ LET owed == {p \in Paths : Owed(s, p)} IN
     w' = [w EXCEPT ![s].local = [p \in Paths |-> IF p \in owed THEN doc[p] ELSE @[p]],
                    ![s].baseline = [p \in Paths |-> IF p \in owed THEN doc[p] ELSE @[p]],
                    ![s].integrated = @ \cup {Gen(doc[p]) : p \in {q \in owed : doc[q] # Nil}}]
  /\ syncs' = syncs + 1
  /\ regressed' = (regressed \/ \E p \in Paths : Owed(s, p) /\ Back(s, p))
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, restarts, upped, copies, orig>>

\* The control for "P1-lite requires P2": a tree adopts a save the document
\* does not cite yet — what today's cell lets a consume do.
GAdopt(s, p) ==
  /\ UIAdoptsUncited
  /\ On(s) /\ gw[p] # Nil
  /\ w[s].local[p] = w[s].baseline[p] /\ w[s].baseline[p] # gw[p]
  /\ w' = [w EXCEPT ![s].local[p] = gw[p], ![s].baseline[p] = gw[p],
                    ![s].integrated = @ \cup {Gen(gw[p])}]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

\* A writer restarts: it keeps its tree and its baseline (the state db) and
\* loses the rest; a lease it held expires.  No journal is read back.
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

Scan(s) ==
  /\ On(s) /\ w[s].pc = "consumed"
  /\ LET W == w[s]
         conv == IF ContentConverges
                 THEN {p \in Paths : W.local[p] # Nil /\ doc[p] # Nil
                                      /\ Content(W.local[p]) = Content(doc[p]) /\ W.baseline[p] # doc[p]}
                 ELSE {}
         bl == [p \in Paths |-> IF p \in conv THEN doc[p] ELSE W.baseline[p]]
         lc == [p \in Paths |-> IF p \in conv THEN doc[p] ELSE W.local[p]]
         dirty == {p \in Paths : lc[p] # bl[p]}
         ups == {p \in dirty : lc[p] # Nil}
         absent == {p \in dirty : lc[p] = Nil}
     IN \E dels \in SUBSET absent :
          w' = [w EXCEPT ![s].pc = "scanned", ![s].baseline = bl,
                         ![s].local = [p \in Paths |-> IF p \in conv THEN doc[p] ELSE W.local[p]],
                         ![s].uploads = ups, ![s].deletes = dels,
                         ![s].snap = lc,
                         ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
                         ![s].retire = {}, ![s].collected = FALSE]
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
\* The merge base IS the baseline.
Foreign(s, p) == doc[p] # w[s].baseline[p]

PullOnly(s) ==
  /\ On(s) /\ w[s].pc = "scanned"
  /\ w[s].uploads = {} /\ w[s].deletes = {}
  /\ w' = [w EXCEPT ![s].pc = "idle", ![s].seen = seq,
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
         \* M3: a delete meeting theirs.  The design applies it and records
         \* theirs; today's rule leaves theirs cited (outranked).
         delOver == {p \in W.deletes : Foreign(s, p) /\ doc[p] # Nil}
         inst == [p \in Paths |->
                    IF p \in W.gone THEN doc[p]
                    ELSE IF p \in mine THEN W.snap[p]
                    ELSE IF p \in W.deletes /\ DeleteWinsPreserved THEN Nil
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
                                 \cup (IF DeleteWinsPreserved THEN {<<p, doc[p]>> : p \in delOver} ELSE {})
       /\ w' = [w EXCEPT ![s].pc = "cased",
                         ![s].upDone = @ \ W.gone,
                         ![s].inst = inst, ![s].retire = retired,
                         ![s].seen = IF nothing THEN seq ELSE seq + 1,
                         ![s].collected = retired = {}]
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
             ![s].uploads = {}, ![s].deletes = {}, ![s].snap = [p \in Paths |-> Nil],
             ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
             ![s].collected = FALSE]
  /\ holder' = "none"
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                 nextGen, ui, reqs, barriers, aux>>

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
  /\ restarts = 0 /\ regressed = FALSE /\ syncs = 0
  /\ upped = live /\ copies = 0 /\ orig = [h \in Handles |-> Nil]

GatewayStep ==
  \/ \E p \in Paths : GPut(p) \/ GCas(p) \/ GDelete(p)
  \/ \E p, q \in Paths : GRename(p, q)
  \/ GRenameFinish

WriterStep ==
  \/ \E s \in Writers :
       \/ Checkout(s) \/ Consume(s) \/ Scan(s) \/ PullOnly(s) \/ Claim(s) \/ Sync(s)
       \/ Verify(s) \/ Install(s) \/ Collect(s) \/ Finish(s) \/ Restart(s)
  \/ \E s \in Writers, p \in Paths :
       Edit(s, p) \/ Delete(s, p) \/ Upload(s, p) \/ GAdopt(s, p)
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

\* P1-lite's claims.
Inv_NoRegress == ~regressed
\* M3: a barrier that published a delete leaves the tree and the document
\* agreeing at that path (applied, or the tree took what stands).
Prop_DeleteSettles ==
  [][\A s \in Writers :
       Finish(s) => \A p \in w[s].deletes : w[s].inst[p] = Nil \/ w'[s].local[p] = w[s].inst[p]]_vars

\* P1-lite probes (action properties: violated = the step was taken).
\* The scan adopted bytes that were already the document's.
ProbeConverged ==
  [][~\E s \in Writers : w[s].pc = "consumed" /\ w'[s].pc = "scanned"
                         /\ w'[s].baseline # w[s].baseline]_vars
\* A writer restarted between its CAS and step 7 (the case the journal served).
ProbeRestartAfterCas == [][~\E s \in Writers : w[s].pc = "cased" /\ restarts' = restarts + 1]_vars
\* A delete met theirs at the CAS (the M3 row taken).
ProbeDeleteOverTheirs ==
  [][~\E s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased"
                         /\ \E p \in w[s].deletes : Foreign(s, p) /\ doc[p] # Nil]_vars
\* A writer's install publishes, at p, its own version over a successor the
\* document derived FROM it: a peer took the version and moved on, and this
\* writer publishes it again (with R7's record).  The route is a restart
\* between the CAS and step 7 followed by a peer's publish; recorded, so
\* not a silent revert, but a revert.  A probe, to learn if it is reachable.
ProbeRepublish ==
  [][~\E s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased"
        /\ \E p \in (w[s].uploads \cap w[s].upDone) \ w'[s].gone :
              doc[p] # Nil /\ doc[p] # w[s].snap[p] /\ Derives(doc[p], w[s].snap[p])]_vars
=============================================================================
