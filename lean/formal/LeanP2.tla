------------------------------- MODULE LeanP2 ---------------------------------
(* Lean's handles protocol as the code runs it after P2, step 5 slices 1-3
   (2026-09-25): THE GATEWAY COMMITS.  `LeanCore.tla` is the shape before
   it — the cell's entries and declared removals, consumed by writers — and
   no longer describes the code; the trace check (`trace/TraceCore.tla`)
   targets this module now.

   Built from two parents, and each part says which:
     - the writer's loop is `LeanCore`'s with the cell taken out: Consume
       takes the writer's QUEUE only; no entries, removals, repairs (R2),
       pending adoptions (L-119), declared removals (L-125) or window.
       What stays is what the code kept: R4a (the commit re-reads its
       uploads), R7 (per path), the parked merge base (L-126), the record
       of a delete mine publishes over, the collector sparing what the
       installed document cites, the orphan sweep under the lease.
     - the gateway, the restart, the sync and the re-upload copies are
       `pending/p2/LeanCoreP2R.tla`'s, with the two places the code differs
       from that sandbox:
         * a save is JUDGED at its CAS against the version the UI read
           (`judge_save`: an overwrite must name what it read, re-judged at
           every CAS attempt).  A document that moved refuses the save
           (412) — no record, no ack; the fresh handle is an orphan.  The
           sandbox installed and recorded instead (`GatewaySurfacesForeign`).
         * the gateway deletes NOTHING.  What a save, delete or rename stops
           citing is an orphan, and the writers' orphan sweep takes it
           (under the lease; slice 5 adds the retire age).  The sandbox
           deleted it at the CAS.

   THE GATEWAY (a party; it never waits for the writers' lease — G1)
     GPut(p)            a save's bytes land at a fresh handle
     GCas(p)            ...and one CAS cites it, judged against the read;
                        acknowledged after the CAS
     GRename(p, q)      one CAS moves the citation (no bytes move)
     GDelete(p)         one CAS stops citing the path; the tombstone names it
   THE WRITER (one barrier: Consume, Scan, Upload, Claim, Verify, Install,
   Collect, Finish; PullOnly and Skip when there is nothing to publish;
   Sweep inside the commit section; Restart and Sync between barriers)

   THE CLAIMS, over state and steps, as `LeanCore` and `LeanCoreP2R` word
   them: Inv_CitationsLive, Inv_OneName, Inv_AckedNamed (an acknowledged
   delete answers too), Prop_NoSilentRevert, Inv_OneHolder, Inv_NoRegress
   (L-123), and G1's liveness Prop_UISaveCompletes under LSpec.

   Known restriction: one gateway verb per path at a time (`Busy`).  The
   code does not serialise a path; two concurrent saves both judge at the
   CAS, and the second is refused because the first moved the document.  *)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Nil, Paths, Free, Writers,
  MaxMint,      \* generations 2..MaxMint (1 is the seed)
  MaxUI,        \* UI saves
  MaxRemovals,  \* UI deletes and renames
  MaxBarriers,  \* writer barriers
  MaxRestarts, MaxSyncs,
  MaxCopies,    \* re-uploads of bytes already PUT once (a withheld upload's,
                \*   a restarted writer's): each goes to a fresh key
  \* Writer rules.  TRUE = the code; FALSE = the mutation.
  CommitSurfacesForeign,        \* R7
  CommitVerifiesUploads,        \* R4a
  SweepUnderLease,              \* R4b
  ForeignPerPath,               \* R7 asks per path, by the current baseline
  CollectorSparesCited,         \* step 6 spares what the install cites
  ParkedKeepsMergeBase,         \* L-126
  CommitRecordsDeleteOverride,  \* mine re-creating a path theirs deleted is recorded
  QueueYieldsToSync,            \* L-123
  \* Gateway rules.
  GatewayIgnoresLease,          \* G1
  GatewayJudgesRead,            \* a save lands only over the version it read
  GatewaySweepGrace,            \* G3: the sweep spares a save in flight
  RenameAtomic                  \* one CAS (FALSE: two)

ASSUME Free \subseteq Paths
ASSUME MaxMint \in Nat /\ MaxMint >= 1

Seed == 1
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
  udel,   \* the deletes the gateway ACKNOWLEDGED: <<path, version deleted>>
  restarts, syncs, regressed,
  upped, copies, orig   \* every handle PUT; the copies made; a copy's original

bucket == <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel>>
aux == <<restarts, syncs, regressed, upped, copies, orig>>
vars == <<live, minted, doc, seq, tomb, base, acked, conflicts, holder,
          nextGen, ui, reqs, barriers, w, took, gw, mv, udel, aux>>

Queued == [path : Paths, handle : Opt(Handles), retired : Opt(Handles)]

Writer ==
  [st : {"off", "on"},
   pc : {"idle", "consumed", "scanned", "claimed", "cased"},
   local : [Paths -> Opt(Handles)],     \* the tree
   baseline : [Paths -> Opt(Handles)],  \* what the tree integrated, per path
   instBase : [Paths -> Opt(Handles)],  \* the document it last merged onto
   integrated : SUBSET Gens,            \* flat, for ForeignPerPath = FALSE
   queue : SUBSET Queued,               \* foreign changes owed to the tree
   uploads : SUBSET Paths, deletes : SUBSET Paths,
   snap : [Paths -> Opt(Handles)],
   upDone : SUBSET Paths, gone : SUBSET Paths, verified : BOOLEAN,
   inst : [Paths -> Opt(Handles)],
   retire : SUBSET Handles,
   collected : BOOLEAN,
   seen : Nat,
   jSeq : Nat,                          \* the journal: the seq it last INSTALLED
   jParked : SUBSET Paths]              \* ...and what that install PARKED

WriterInit ==
  [st |-> "off", pc |-> "idle",
   local |-> [p \in Paths |-> Nil], baseline |-> [p \in Paths |-> Nil],
   instBase |-> [p \in Paths |-> Nil], integrated |-> {}, queue |-> {},
   uploads |-> {}, deletes |-> {},
   snap |-> [p \in Paths |-> Nil], upDone |-> {}, gone |-> {},
   verified |-> FALSE, inst |-> [p \in Paths |-> Nil], retire |-> {},
   collected |-> FALSE, seen |-> 0, jSeq |-> 0, jParked |-> {}]

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
LeaseGuard == GatewayIgnoresLease \/ holder = "none"
Busy(p) == gw[p] # Nil \/ (mv # Nil /\ mv[1] = p)

------------------------------------------------------------------------------
(* The gateway (`lean/gateway/src/workspace.rs`: `put_file` → `commit_save`,
   `remove_files` and `rename_files` → `commit_edit`).                      *)

\* A save's bytes land at a fresh handle, derived from the version the UI
\* read.  Not yet acknowledged.
GPut(p) ==
  /\ ui < MaxUI /\ nextGen <= MaxMint /\ ~Busy(p)
  /\ LET h == <<p, nextGen>> IN
     /\ live' = live \cup {h} /\ minted' = minted \cup {h}
     /\ base' = [base EXCEPT ![h] = doc[p]]
     /\ gw' = [gw EXCEPT ![p] = h]
     /\ nextGen' = nextGen + 1 /\ ui' = ui + 1
     /\ upped' = upped \cup {h}
  /\ UNCHANGED <<doc, seq, tomb, acked, conflicts, holder, reqs, barriers, w, mv, udel,
                 restarts, syncs, regressed, copies, orig>>

\* ...and its CAS, judged against the CURRENT document: it lands only over
\* the version the UI read.  Otherwise 412, not acknowledged, and the fresh
\* handle is an orphan for the sweep.  What a landed save replaced is not
\* deleted here either: nothing cites it, and the sweep takes it.
GCas(p) ==
  /\ gw[p] # Nil /\ LeaseGuard
  /\ LET h == gw[p]
         ok == doc[p] = base[h] \/ ~GatewayJudgesRead
     IN /\ doc' = IF ok THEN [doc EXCEPT ![p] = h] ELSE doc
        /\ seq' = IF ok THEN seq + 1 ELSE seq
        /\ tomb' = IF ok THEN [tomb EXCEPT ![p] = Nil] ELSE tomb
        /\ acked' = IF ok THEN acked \cup {<<p, h>>} ELSE acked
  /\ gw' = [gw EXCEPT ![p] = Nil]
  /\ UNCHANGED <<live, minted, base, conflicts, holder, nextGen, ui, reqs, barriers, w, mv, udel, aux>>

\* A rename is ONE CAS: the destination cites the source's handle, the
\* source's tombstone names it.  The destination must be uncited
\* (`DestinationExists`).
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

\* A delete is one CAS; the tombstone names what it retired.
GDelete(p) ==
  /\ reqs < MaxRemovals /\ LeaseGuard
  /\ doc[p] # Nil /\ ~Busy(p)
  /\ doc' = [doc EXCEPT ![p] = Nil]
  /\ tomb' = [tomb EXCEPT ![p] = doc[p]]
  /\ udel' = udel \cup {<<p, doc[p]>>}
  /\ seq' = seq + 1 /\ reqs' = reqs + 1
  /\ UNCHANGED <<live, minted, base, acked, conflicts, holder, gw, mv, nextGen, ui, barriers, w, aux>>

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
   it, a UI save included (`consume_counted`, `from: queue`).  An upsert
   the baseline holds is settled; one whose handle is gone leaves a record;
   a live one adopts at a clean path and is preserved at a dirty one (the
   baseline advances either way).  Then the queued deletions: absent,
   superseded by a newer citation, kept (dirty: a record), or removed.   *)
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
       advPaths == adoptPaths \cup {e[1] : e \in dirtyConf}
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
                                    IF p \in tSuper /\ qRet(p) # Nil THEN qRet(p) ELSE @[p]]]
       /\ conflicts' = conflicts \cup dirtyConf \cup missing \cup {<<p, Nil>> : p \in tKept}
       /\ regressed' = (regressed \/ \E p \in adoptPaths :
                          W.baseline[p] # Nil /\ GenAt(p) # W.baseline[p] /\ Derives(W.baseline[p], GenAt(p))
                          /\ <<p, W.baseline[p]>> \notin conflicts)
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, restarts, syncs, upped, copies, orig>>

\* Step 2: what differs from the baseline is an upload or a deletion; a
\* deletion may wait for the next walk (the two-scan guard: any subset).
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

\* Skip-on-no-diff: no scan, no CAS.
Skip(s) ==
  /\ On(s) /\ w[s].pc = "consumed" /\ barriers < MaxBarriers
  /\ \A p \in Paths : w[s].local[p] = w[s].baseline[p] /\ w[s].baseline[p] = w[s].instBase[p]
  /\ seq = w[s].seen
  /\ w' = [w EXCEPT ![s].pc = "idle"]
  /\ barriers' = barriers + 1
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, aux>>

\* Step 4: each upload lands at a fresh key.  Bytes PUT before (a withheld
\* upload's, a restarted writer's) land at a COPY handle, which names them
\* from here on.
Upload(s, p) ==
  /\ On(s) /\ w[s].pc = "scanned"
  /\ p \in w[s].uploads \ w[s].upDone
  /\ LET h == w[s].snap[p] IN
     IF h \notin upped
     THEN /\ live' = live \cup {h} /\ upped' = upped \cup {h}
          /\ w' = [w EXCEPT ![s].upDone = @ \cup {p}]
          /\ UNCHANGED <<minted, base, copies, orig>>
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
(* The merge's vocabulary, read at the CAS against the CURRENT document
   (`barrier.rs::merge_onto`, `manifest::merge`).                           *)

\* The journal's document is the merge base while the pointer still holds
\* it — except at a path that install PARKED, which keeps the base it had
\* (L-126).
MergeBase(s) ==
  IF w[s].jSeq # 0 /\ w[s].jSeq = seq
  THEN [p \in Paths |-> IF ParkedKeepsMergeBase /\ p \in w[s].jParked THEN w[s].instBase[p] ELSE doc[p]]
  ELSE w[s].instBase
Foreign(s, p) == doc[p] # MergeBase(s)[p]
\* The version a delete since the merge base removed.
DeletedAt(s, p) == IF tomb[p] # Nil THEN tomb[p] ELSE MergeBase(s)[p]
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

\* Nothing to publish: no claim, no CAS; the merge base follows the document.
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

\* R4a: the commit re-reads every upload it is about to cite and withholds
\* what a sweep took.
Verify(s) ==
  /\ On(s) /\ w[s].pc = "claimed" /\ holder = s /\ ~w[s].verified
  /\ w' = [w EXCEPT ![s].verified = TRUE,
                    ![s].gone = IF CommitVerifiesUploads
                                THEN {p \in w[s].uploads \cap w[s].upDone : w[s].snap[p] \notin live}
                                ELSE {}]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

\* Step 5: the merge onto the CURRENT document, and the CAS.  Mine: the
\* uploads that survived the re-read, and the deletes.  Theirs — a peer's
\* publish or the gateway's alike — wins a delete/modify race; mine wins a
\* modify/modify one, and R7 records what mine publishes over that this
\* tree never integrated at that path.  Mine re-creating a path theirs
\* deleted since the merge base records the deleted version.
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
         delOverridden == IF CommitRecordsDeleteOverride
                          THEN {p \in mine \ W.gone : Foreign(s, p) /\ doc[p] = Nil /\ MergeBase(s)[p] # Nil}
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
                                 \cup {<<p, DeletedAt(s, p)>> : p \in delOverridden}
       /\ w' = [w EXCEPT ![s].pc = "cased",
                         ![s].upDone = @ \ W.gone,
                         ![s].inst = inst, ![s].retire = retired,
                         ![s].seen = IF nothing THEN seq ELSE seq + 1,
                         ![s].jSeq = IF nothing THEN seq ELSE seq + 1,
                         ![s].jParked = IF ParkedKeepsMergeBase THEN W.gone ELSE {},
                         ![s].collected = retired = {},
                         ![s].queue = QueueUpsert(@, MergeForeign(s) \cup MergeGone(s))]
  /\ UNCHANGED <<live, minted, base, acked, holder, gw, mv, udel, nextGen, ui, reqs, barriers, aux>>

\* Step 6: the retired set, in one batch, sparing what the installed
\* document still cites.
Collect(s) ==
  /\ On(s) /\ w[s].pc = "cased" /\ ~w[s].collected
  /\ LET W == w[s]
         taken == {h \in W.retire : ~(CollectorSparesCited /\ \E q \in Paths : W.inst[q] = h)}
     IN /\ live' = live \ taken
        /\ w' = [w EXCEPT ![s].retire = {}, ![s].collected = TRUE]
  /\ UNCHANGED <<minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, aux>>

\* The orphan sweep (R4b): nothing cites it, no record preserves it — a
\* lost upload, what a save or a delete replaced.  Inside the commit
\* section; never the sweeper's own uploads, never a save in flight (G3).
Sweep(s, h) ==
  /\ On(s)
  /\ SweepUnderLease => (holder = s /\ w[s].pc \in {"claimed", "cased"})
  /\ h \in live /\ ~Cited(h) /\ ~Preserved(h)
  /\ GatewaySweepGrace => ~InFlight(h)
  /\ ~\E p \in w[s].upDone : w[s].snap[p] = h
  /\ live' = live \ {h}
  /\ UNCHANGED <<minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, w, aux>>

\* Step 7: the baseline follows what this barrier published; the merge base
\* becomes the installed document, except at a parked path (L-126).
Finish(s) ==
  /\ On(s) /\ w[s].pc = "cased" /\ w[s].collected
  /\ LET W == w[s] IN
     w' = [w EXCEPT ![s].pc = "idle",
             ![s].baseline = [p \in Paths |->
                                IF p \in W.uploads \cap W.upDone THEN W.snap[p]
                                ELSE IF p \in W.deletes /\ W.inst[p] = Nil THEN Nil
                                ELSE @[p]],
             ![s].instBase = [p \in Paths |->
                                IF ParkedKeepsMergeBase /\ p \in W.gone THEN @[p] ELSE W.inst[p]],
             ![s].uploads = {}, ![s].deletes = {}, ![s].snap = [p \in Paths |-> Nil],
             ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
             ![s].collected = FALSE]
  /\ holder' = "none"
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                 nextGen, ui, reqs, barriers, aux>>

------------------------------------------------------------------------------
(* The restart and the sync (`LeanCoreP2R`'s).                              *)

Owed(s, p) == doc[p] # w[s].baseline[p] /\ w[s].local[p] = w[s].baseline[p]
\* A version displaced KNOWINGLY carries a record: stepping back over it is
\* a recorded revert, not L-123's silent one.
Back(s, p) == /\ doc[p] # Nil /\ w[s].baseline[p] # Nil /\ Derives(w[s].baseline[p], doc[p])
              /\ Content(doc[p]) # Content(w[s].baseline[p])
              /\ <<p, w[s].baseline[p]>> \notin conflicts

\* A restart keeps the tree, the baseline, the merge base, the journal and
\* the queue (all on disk) and drops the barrier in flight.
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

\* A sync, between barriers: it takes what is owed, moves the merge base
\* there, and prunes the queue where it did (L-123, `sync.rs` step 6).
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

\* The trees' memory: every version a tree's baseline or tree has held at a
\* path.  No action writes it; the step does.
TookUpdate ==
  took' = [s \in Writers |-> [p \in Paths |->
             took[s][p]
               \cup (IF w'[s].baseline[p] # Nil /\ w'[s].baseline[p] # w[s].baseline[p]
                     THEN {Gen(w'[s].baseline[p])} ELSE {})
               \cup (IF w'[s].local[p] # Nil /\ w'[s].local[p] # w[s].local[p]
                     THEN {Gen(w'[s].local[p])} ELSE {})]]

Next == (GatewayStep \/ WriterStep) /\ TookUpdate
Spec == Init /\ [][Next]_vars

\* G1, behaviourally: fair to the gateway's CAS only.  A writer may stop
\* anywhere, the lease holder included.
LSpec == Spec /\ \A p \in Paths : WF_vars(GCas(p) /\ TookUpdate)

------------------------------------------------------------------------------
(* The claims.                                                              *)

Inv_CitationsLive == \A p \in Paths : doc[p] # Nil => doc[p] \in live
Inv_OneName == \A p, q \in Paths : p # q /\ doc[p] # Nil => doc[p] # doc[q]

\* An acknowledged handle that is gone is accounted for by something in the
\* state that names it (`LeanCore`'s, plus the acknowledged delete).
Accounted(p, h) ==
  \/ h \in live
  \/ Preserved(h)
  \/ \E q \in Paths :
       /\ q = p \/ (p = h[1] /\ <<q, h>> \in acked)
       /\ \/ doc[q] # Nil /\ Derives(doc[q], h)
          \/ tomb[q] # Nil /\ Derives(tomb[q], h)
          \/ \E s \in Writers : Gen(h) \in took[s][q] /\ w[s].local[q] # h
          \/ \E d \in udel : d[1] = q /\ Derives(d[2], h)
  \/ \E b \in acked : b[1] = p /\ Later(b[2], h)
Inv_AckedNamed == \A a \in acked : Accounted(a[1], a[2])

RECURSIVE Supersedes(_, _, _)
Supersedes(k, h, p) ==
  \/ k = h
  \/ k # Nil /\ h # Nil /\ Content(k) = Content(h)
  \/ k # Nil /\ base[k] # Nil /\ Supersedes(base[k], h, p)
  \/ <<p, k>> \in acked /\ <<p, h>> \in acked /\ Later(k, h)
\* A published version replaced with no record, no move, and not by an
\* edit on top of it or by the committing tree's own edit over what it held.
SilentRevert(p) ==
  /\ doc[p] # Nil /\ doc'[p] # Nil /\ doc'[p] # doc[p]
  /\ ~Supersedes(doc'[p], doc[p], p)
  /\ ~\E c \in conflicts' : c[2] = doc[p]
  /\ ~\E q \in Paths \ {p} : doc'[q] = doc[p]
  /\ ~\E s \in Writers : w[s].pc = "claimed"
                         /\ (w[s].baseline[p] = doc[p] \/ Gen(doc[p]) \in took[s][p])
Prop_NoSilentRevert == [][\A p \in Paths : ~SilentRevert(p)]_vars

Inv_OneHolder == \A s \in Writers : w[s].pc \in {"claimed", "cased"} => holder = s
Inv_NoRegress == ~regressed

\* G1: every save the gateway starts is answered.  Checked under LSpec.
Prop_UISaveCompletes == \A p \in Paths : (gw[p] # Nil) ~> (gw[p] = Nil)

------------------------------------------------------------------------------
(* Probes (non-vacuity): a world that violates one has taken the step.     *)
ProbeStalledSave == ~(holder # "none" /\ \E p \in Paths : gw[p] # Nil)
ProbeSavedUnderLease ==
  [][~(holder # "none" /\ \E p \in Paths : gw[p] # Nil /\ gw'[p] = Nil /\ doc'[p] = gw[p])]_vars
\* A save refused at its CAS: the document moved off the version it read.
ProbeSaveRefused ==
  [][~\E p \in Paths : gw[p] # Nil /\ gw'[p] = Nil /\ doc'[p] # gw[p]]_vars
ProbeRenamed == ~\E p, q \in Paths : p # q /\ doc[q] # Nil /\ doc[q][1] = p
ProbeSurfaced == conflicts = {}
ProbeDeleteOverridden ==
  ~\E s \in Writers, p \in Paths :
     /\ w[s].pc = "claimed" /\ holder = s /\ w[s].verified
     /\ p \in (w[s].uploads \cap w[s].upDone) \ w[s].gone
     /\ Foreign(s, p) /\ doc[p] = Nil /\ MergeBase(s)[p] # Nil
ProbeRepublish ==
  [][~\E s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased"
        /\ \E p \in (w[s].uploads \cap w[s].upDone) \ w'[s].gone :
              doc[p] # Nil /\ doc[p] # w[s].snap[p] /\ Derives(doc[p], w[s].snap[p])]_vars
ProbeRestartAfterCas == [][~\E s \in Writers : w[s].pc = "cased" /\ restarts' = restarts + 1]_vars
=============================================================================
