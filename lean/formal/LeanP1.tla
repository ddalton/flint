------------------------------- MODULE LeanP1 ---------------------------------
(* Lean's handles protocol as the code runs it after P2 AND P1-lite (step
   5, slices 1-4, 2026-09-25): the gateway commits, and a writer's baseline
   IS its merge base, so what the tree is owed is DERIVED from the document
   at each consume.  `LeanP2.tla` is the shape before slice 4 (a separate
   merge base, a writer-local queue, the journal's installed document).
   The trace check (`trace/TraceCore.tla`) targets this module.

   From `LeanP2`, removed: `instBase`, `queue`, `jSeq`, `jParked` and every
   rule that guarded them (L-126's parked base, L-123's prune).  From the
   sandbox `pending/p2/LeanCoreP1.tla`, taken: the derived owed set, content
   convergence, M3 (a delete over theirs applies; theirs is recorded).  And
   one thing neither had, because the code's mutations showed it is
   load-bearing: the consume's CHEAP PATH, as the SCAN TRIGGER: a consume
   derives nothing when the pointer names the document it last derived
   against (`derived`) and every path it skipped as the agent's work
   (`skipped`) is still dirty (`RecheckSkipped`).  Only the consume records
   that; a commit advances it only when its CAS replaced exactly that
   document (`CommitAdvanceGuarded`).  `Inv_ShortcutSound` is the claim.

   SCOPE AND RESCOPE (2026-10-02): a checkout admits a set of paths (one of
   `Scopes`; `{Paths}` is the unscoped tree), and a consume or a sync owes
   only what the tree HOLDS (its baseline cites it) or its scope covers
   (`barrier.rs::consume_owed`, `sync.rs`).  The narrow / widen verb
   (`checkout.rs::rescope`) is a durable INTENT applied in two halves, the
   uncite and then the unlink with the widen; a restart replays it from the
   recorded drop set before any consume.  With `MaxRescopes = 0` and
   `Scopes = {Paths}` every state is the unscoped model's.

   A FAILED FETCH (2026-10-02): a consume, a sync or a widen may fail to
   fetch or write any path it would take, up to `MaxFetchFails` in a run (a
   lost race with a newer document, a checksum refusal, a full disk).  The
   path stays as it was and stays owed; the consume and the sync then record
   nothing as derived (`left` in the code, L-128), and the consume does not
   move the pointer it integrated.  With `MaxFetchFails = 0` every state is
   the model's without it.

   A READER (2026-10-02, `reader.rs`): a tree in `Readers` never publishes.
   Its tick reads the pointer, and syncs the whole tree only when the
   pointer moved since the last pull (`memo`) or its baseline says
   something is still owed (L-129); it then remembers the pointer it read
   BEFORE the sync, so a document that moved in between is pulled again.

   THE BOUNDARY VERBS' ANSWER, THE RESTART, DEPOSAL (2026-10-14, the plan
   `docs/plans/lean-row15-boundary-verbs-model-plan-2026-10-14.md`, SAFETY
   rows 15-17): an agent's or the gateway's request folds into a pending
   record (`Touch`), any barrier that begins while it stands carries it and
   journals what its CAS published (the carrier), and the answer is the
   ack (`Honor`), `ok` or `partial`, retired by the restart rule
   (`RetireByAck`).  A takeover deposes a holder judged dead (`Depose`,
   with the rotation), and the straggler learns it at its next cell read
   (`Fence`).  All of it is per-writer state in `Writer`, written only when
   a touch or a deposal exists: with `MaxTouches = 0` and `MaxDeposals = 0`
   every state is the model's without it.

   Differences from the code the model keeps on purpose: a scoped SYNC (D4,
   a request scope narrower than the workspace's) is not modelled; the
   reader's second etag (the request cell) is not either; nor the sync
   verb's ack (D1), a touch without a nonce (D8), or the budget's
   arithmetic (a barrier's clock is a choice).                          *)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Nil, Paths, Free, Writers,
  MaxMint,      \* generations 2..MaxMint (1 is the seed)
  MaxUI,        \* UI saves
  MaxRemovals,  \* UI deletes and renames
  MaxBarriers,  \* writer barriers
  MaxRestarts, MaxSyncs,
  MaxAges,      \* times the retire age G may elapse (the ghost clock's bound)
  MaxCopies,    \* re-uploads of bytes already PUT once (a withheld upload's,
                \*   a restarted writer's): each goes to a fresh key
  \* Writer rules.  TRUE = the code; FALSE = the mutation.
  CommitSurfacesForeign,        \* R7
  CommitVerifiesUploads,        \* R4a
  SweepUnderLease,              \* R4b
  CollectorSparesCited,         \* step 6 spares what the install cites
  CommitRecordsDeleteOverride,  \* mine re-creating a path theirs deleted is recorded
  DeleteWinsPreserved,          \* M3: mine deleting over theirs applies; theirs recorded
  ContentConverges,             \* the consume adopts dirty bytes that ARE the document's
  RecheckSkipped,               \* the cheap path re-checks the paths it skipped as dirty
  CommitAdvanceGuarded,         \* a commit advances `derived` only over the derived document
  RetireAge,                    \* M1: what a commit stops citing is collected only
                                \*   after the retire age G, not at once
  \* Gateway rules.
  GatewayIgnoresLease,          \* G1
  GatewayJudgesRead,            \* a save lands only over the version it read
  GatewaySweepGrace,            \* G3: the sweep spares a save in flight
  RenameAtomic,                 \* one CAS (FALSE: two)
  \* Scope and rescope.
  Scopes,                       \* the admitted sets a checkout or a rescope may name
  MaxRescopes,
  ConsumeHonorsScope,           \* a path neither held nor covered is not owed
  RescopeUnciteFirst,           \* the uncite lands before the unlink
  RescopeKeepsDirty,            \* the apply (and its replay) keeps a still-cited dirty path
  WidenKeepsLocal,              \* L-130: a widen keeps a file the agent made at an admitted path
  UnlinkChecksBytes,            \* the unlink takes only the bytes the uncite dropped
  \* A failed fetch.
  MaxFetchFails,
  ConsumeKeepsLeft,             \* a consume that left a path records nothing as derived
  SyncKeepsLeft,                \* likewise a sync (L-128)
  \* Readers.
  Readers,                      \* the trees with read access (a subset of Writers)
  ReaderRechecksOwed,           \* a reader pulls while its baseline says something is owed (L-129)
  \* The boundary verbs' answer (row 15).
  MaxTouches,                   \* requests per writer: the agent's touch, the gateway's, the door's
  MaxNonces,                    \* the pending record keeps this many nonces (`MAX_NONCES`)
  \* Deposal (row 17).
  MaxDeposals,                  \* takeovers per writer
  InstallFenced,                \* H2: the fence read before the CAS (an install needs the lease)
  DeletesUnderLease             \* L-132: a holder deposed after its CAS deletes nothing more

ASSUME Free \subseteq Paths
ASSUME MaxMint \in Nat /\ MaxMint >= 1
ASSUME Scopes \subseteq SUBSET Paths /\ Scopes # {}
ASSUME Readers \subseteq Writers
ASSUME MaxTouches \in Nat /\ MaxNonces \in Nat /\ MaxNonces >= 1 /\ MaxDeposals \in Nat

Seed == 1
Gens == Seed..(MaxMint + MaxCopies)
Handles == Paths \X Gens
Gen(h) == h[2]
Later(h, k) == Gen(h) > Gen(k)
Opt(S) == S \cup {Nil}
\* The clock a carrying barrier stamps (`boundary_source`) and its ack names.
Clocks == {"sentinel", "sentinel-deferred", "drain"}
NoPend == [id |-> 0, fold |-> 0, nonces |-> {}]
NoAck == [st |-> "none", nonces |-> {}, seq |-> 0, src |-> "none", dropped |-> {}, inst |-> FALSE]
NoCar == [id |-> 0, paths |-> {}, deletes |-> {}, dropped |-> {}, uncited |-> {}, pub |-> [p \in Paths |-> Nil]]

VARIABLES
  live, minted, doc, seq, tomb, base, acked, conflicts, holder,
  nextGen, ui, reqs, barriers, w, took,
  gw,     \* per path, the save in flight: its fresh handle, not yet cited
  mv,     \* a two-CAS rename between its CASes (only when ~RenameAtomic)
  udel,   \* the ACKNOWLEDGED deletes, the gateway's and a commit's: <<path, version deleted>>
  restarts, syncs, regressed, rescopes, fails,
  upped, copies, orig,  \* every handle PUT; the copies made; a copy's original
  \* RETIRE-AGE G (M1, slice 5).  `retiring`: what a commit stopped citing,
  \* less than G ago (the code's young retire logs; the sweeps spare it).
  \* `aged`: retired at least G ago, for a writer to reap.  `ages`: how often
  \* G has elapsed (the bound).  A READER ghost: `rdoc`, the document it
  \* loaded, and `rlag`, whether G has elapsed since.
  retiring, aged, ages, rdoc, rlag

bucket == <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel>>
aux == <<restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
ret == <<retiring, aged, ages, rdoc, rlag>>
vars == <<live, minted, doc, seq, tomb, base, acked, conflicts, holder,
          nextGen, ui, reqs, barriers, w, took, gw, mv, udel, aux, ret>>

Writer ==
  [st : {"off", "on"},
   pc : {"idle", "consumed", "scanned", "claimed", "cased", "pulling"},
   local : [Paths -> Opt(Handles)],     \* the tree
   baseline : [Paths -> Opt(Handles)],  \* what the tree integrated — and the merge base
   integrated : SUBSET Gens,
   uploads : SUBSET Paths, deletes : SUBSET Paths,
   snap : [Paths -> Opt(Handles)],
   upDone : SUBSET Paths, gone : SUBSET Paths, verified : BOOLEAN,
   inst : [Paths -> Opt(Handles)],
   retire : SUBSET Handles,
   collected : BOOLEAN,
   adv : BOOLEAN,                       \* this install replaced the derived document
   synced : Nat,                        \* the pointer seq it last left the tree at
                                        \*   (`baseline.manifest_etag`)
   derived : Nat,                       \* the seq the last derive read (0: none)
   skipped : SUBSET Paths,              \* what it left as the agent's work
   scope : SUBSET Paths,                \* the admitted set (`scope.json`)
   \* A rescope in flight (`scope-intent.json`): its target and the paths it
   \* drops, durable before the first mutation; "saved" until the first
   \* half lands, "mid" between the halves.  `sKeep`: the still-cited dirty
   \* paths this apply keeps.  `sHeld`: what the uncite dropped, per path.
   sStage : {"none", "saved", "mid"},
   sTgt : SUBSET Paths, sDrop : SUBSET Paths, sKeep : SUBSET Paths,
   sHeld : [Paths -> Opt(Handles)],
   \* A ghost: paths a rescope unlinked that the agent has not touched since.
   unlinked : SUBSET Paths,
   \* A reader: the pointer its last completed pull synced against
   \* (`reader.json`, 0: none), and the one this pull read.
   memo : Nat, rnow : Nat,
   \* THE ANSWER (row 15).  `nt`: the requests so far.  `pend`: the pending
   \* record (`publish.pending.json`; id 0: none): its id, minted when the
   \* record is created and kept across folds (L-133), the request that last
   \* folded into it, and its nonces.  `ack`: `.flint/publish.ack` (st
   \* "none": none).  `car`: the carrier (`IntentJournal::carrier`; id 0:
   \* none).  The barrier in flight: the record it carries (`cId`, 0: none;
   \* read at its start), the fold it read (`cFold`), its clock (`clk`), the
   \* seq its CAS installed and whether it did (`iseq`, `instd`).  `bound`:
   \* the last carrying barrier, ended: the document it ended at and its own
   \* withheld uploads.  All files on disk: a restart keeps them and drops
   \* the barrier's.
   nt : Nat,
   pend : [id : Nat, fold : Nat, nonces : SUBSET Nat],
   ack : [st : {"none", "ok", "partial", "refused-read-only"}, nonces : SUBSET Nat, seq : Nat,
          src : Clocks \cup {"none"}, dropped : SUBSET Paths, inst : BOOLEAN],
   car : [id : Nat, paths : SUBSET Paths, deletes : SUBSET Paths,
          dropped : SUBSET Paths, uncited : SUBSET Paths, pub : [Paths -> Opt(Handles)]],
   cId : Nat, cFold : Nat, clk : Clocks \cup {"none"}, iseq : Nat, instd : BOOLEAN,
   bound : [id : Nat, fold : Nat, doc : [Paths -> Opt(Handles)], seq : Nat,
            wd : SUBSET Paths, inst : BOOLEAN, clk : Clocks \cup {"none"}],
   \* GHOSTS the answer's claims read; no guard reads them.  Every nonce
   \* consumed, acked, or dropped by the cap; what the record's requests
   \* declared (`dset`: the paths dirty at a fold; `decl`: their versions
   \* then; `dMint`: the mint counter then); the declared paths the agent
   \* wrote or deleted since (`moved`); what each carrying CAS stamped.
   cN : SUBSET Nat, aN : SUBSET Nat, capped : SUBSET Nat,
   dset : SUBSET Paths, decl : [Paths -> Opt(Handles)], dMint : [Paths -> Nat],
   \* (`stamps` and `lost` are typed loosely, a clock or "none" and a handle
   \* or Nil: what they hold is narrower, but only an invariant says so, and
   \* no guard or claim reads their types.)
   moved : SUBSET Paths, stamps : SUBSET (Nat \X (Clocks \cup {"none"})),
   \* `lost`: this writer's uploads another writer swept before its CAS
   \* (`gone`).  Their conflict record names bytes no copy keeps; every
   \* other conflict's version is copied to `conflicts/` first
   \* (`preserve_conflict_copy`), and the copy outlives a reap of the handle.
   lost : SUBSET Opt(Handles),
   \* DEPOSAL (row 17): a takeover deposed this writer and it has not yet
   \* learned so; `nd`: the takeovers it made.
   deposed : BOOLEAN, nd : Nat]

WriterInit ==
  [st |-> "off", pc |-> "idle",
   local |-> [p \in Paths |-> Nil], baseline |-> [p \in Paths |-> Nil],
   integrated |-> {},
   uploads |-> {}, deletes |-> {},
   snap |-> [p \in Paths |-> Nil], upDone |-> {}, gone |-> {},
   verified |-> FALSE, inst |-> [p \in Paths |-> Nil], retire |-> {},
   collected |-> FALSE, adv |-> FALSE, synced |-> 0, derived |-> 0, skipped |-> {},
   scope |-> {}, sStage |-> "none", sTgt |-> {}, sDrop |-> {}, sKeep |-> {},
   sHeld |-> [p \in Paths |-> Nil], unlinked |-> {}, memo |-> 0, rnow |-> 0,
   nt |-> 0, pend |-> NoPend, ack |-> NoAck, car |-> NoCar,
   cId |-> 0, cFold |-> 0, clk |-> "none", iseq |-> 0, instd |-> FALSE,
   bound |-> [id |-> 0, fold |-> 0, doc |-> [p \in Paths |-> Nil], seq |-> 0,
              wd |-> {}, inst |-> FALSE, clk |-> "none"],
   cN |-> {}, aN |-> {}, capped |-> {},
   dset |-> {}, decl |-> [p \in Paths |-> Nil], dMint |-> [p \in Paths |-> 0],
   moved |-> {}, stamps |-> {}, lost |-> {}, deposed |-> FALSE, nd |-> 0]

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
  /\ restarts \in Nat /\ syncs \in Nat /\ regressed \in BOOLEAN /\ rescopes \in Nat /\ fails \in Nat
  /\ retiring \subseteq Handles /\ aged \subseteq Handles /\ ages \in Nat
  /\ rdoc \in [Paths -> Opt(Handles)] /\ rlag \in BOOLEAN

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
\* The barrier in flight carries a standing declaration (`barrier.rs:600-616`).
Carries(s) == w[s].cId # 0
\* The clock a barrier that begins now may stamp: a carrying one's is the
\* honour's (`sentinel`), the floor's or a deferred honour's
\* (`sentinel-deferred`), or the drain's.
CarryClocks(s) == IF w[s].pend.id # 0 THEN Clocks ELSE {"none"}
\* The carried uploads a document does not cite with the bytes they
\* published (`note_carrier_uncited`; L-137, found by the proof's AckRename
\* world: a UI delete then a rename of another file onto the path left the
\* ack `ok`).  `pub`: what each carried upload published (`published`).
Uncited(c, d) == {p \in c.paths : /\ p \notin c.deletes
                                  /\ \/ d[p] = Nil
                                     \/ c.pub[p] # Nil /\ Content(d[p]) # Content(c.pub[p])}
\* The ack already answers the pending record (`ack_matches`, the nonce arm).
Covered(s) == w[s].ack.st # "none" /\ w[s].pend.nonces \subseteq w[s].ack.nonces
Oldest(S) == CHOOSE m \in S : \A x \in S : m <= x

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
                 restarts, syncs, regressed, rescopes, fails, copies, orig>>

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
     \* The tree's newest version at p: its bytes; else, in a barrier that
     \* uploads p, the snapshot it publishes; else what it integrated (E27,
     \* E31, E33: a stale baseline left an edit over the tree's own
     \* published version underived from it).
     /\ base' = [base EXCEPT ![h] = IF w[s].local[p] # Nil THEN w[s].local[p]
                                    ELSE IF p \in w[s].uploads THEN w[s].snap[p]
                                    ELSE w[s].baseline[p]]
     /\ w' = [w EXCEPT ![s].local[p] = h, ![s].integrated = @ \cup {nextGen},
                       ![s].unlinked = @ \ {p},
                       ![s].moved = IF w[s].pend.id # 0 THEN @ \cup {p} ELSE @]
     /\ nextGen' = nextGen + 1
  /\ UNCHANGED <<live, doc, seq, tomb, acked, conflicts, holder, ui, reqs, barriers, gw, mv, udel, aux>>

Delete(s, p) ==
  /\ On(s) /\ w[s].local[p] # Nil
  /\ w' = [w EXCEPT ![s].local[p] = Nil, ![s].unlinked = @ \ {p},
                    ![s].moved = IF w[s].pend.id # 0 THEN @ \cup {p} ELSE @]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

\* A checkout materializes what its scope admits (`checkout_scoped`).
Checkout(s) ==
  /\ w[s].st = "off"
  /\ \E T \in Scopes :
       LET held == [p \in Paths |-> IF p \in T THEN doc[p] ELSE Nil] IN
       w' = [w EXCEPT ![s].st = "on",
                      ![s].local = held, ![s].baseline = held, ![s].inst = doc,
                      ![s].integrated = {Gen(doc[p]) : p \in {q \in T : doc[q] # Nil}},
                      ![s].synced = seq, ![s].derived = seq, ![s].skipped = {},
                      ![s].scope = T]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

------------------------------------------------------------------------------
(* Step 1 (P1-lite): what the tree is OWED, derived from the document and
   taken in one step (`barrier.rs::consume_owed`).  Owed: the document
   differs from the baseline and the tree is clean there.  A dirty path is
   the agent's work and is not owed — unless its bytes ARE the document's
   (content convergence: the baseline follows, nothing is published), or
   the tree and the document both lack it (the baseline drops it).  The
   CHEAP PATH: the pointer where this writer left it and nothing marked
   owed means nothing is derived at all; a consume that leaves a dirty
   path untaken marks it (`left` in the code).                             *)
\* What the tree HOLDS: its baseline cites the path, or its scope covers it.
\* A path neither is not this tree's (what the scope declined stays declined).
Held(s, p) == w[s].baseline[p] # Nil \/ p \in w[s].scope
Owed(s, p) == /\ doc[p] # w[s].baseline[p] /\ w[s].local[p] = w[s].baseline[p]
              /\ ConsumeHonorsScope => Held(s, p)
\* Taking doc[p] would step the tree BACK to a version its own derives from
\* (L-123's class), unless a record names what it steps over.
Back(s, p) == /\ doc[p] # Nil /\ w[s].baseline[p] # Nil /\ Derives(w[s].baseline[p], doc[p])
              /\ Content(doc[p]) # Content(w[s].baseline[p])
              /\ <<p, w[s].baseline[p]>> \notin conflicts
Converges(s, p) ==
  /\ ContentConverges /\ Held(s, p)
  /\ w[s].local[p] # w[s].baseline[p] /\ doc[p] # w[s].baseline[p]
  /\ \/ w[s].local[p] # Nil /\ doc[p] # Nil /\ Content(w[s].local[p]) = Content(doc[p])
     \/ w[s].local[p] = Nil /\ doc[p] = Nil
\* The scan trigger: the document last derived against, and every skipped
\* path still the agent's (`barrier.rs::consume_owed`).
CheapPath(s) ==
  /\ w[s].derived = seq
  /\ RecheckSkipped => \A p \in w[s].skipped : w[s].local[p] # w[s].baseline[p]
Consume(s) ==
  /\ On(s) /\ s \notin Readers /\ w[s].pc = "idle" /\ barriers < MaxBarriers
  \* Step 0 replays a rescope in flight first (`run_barrier`).
  /\ w[s].sStage = "none"
  /\ \E ck \in CarryClocks(s) :
     IF CheapPath(s)
     THEN /\ w' = [w EXCEPT ![s].pc = "consumed",
                   ![s].cId = w[s].pend.id, ![s].cFold = w[s].pend.fold, ![s].clk = ck]
          /\ UNCHANGED <<regressed, fails>>
     ELSE LET owed == {p \in Paths : Owed(s, p)}
              conv == {p \in Paths : Converges(s, p)}
          IN \E fail \in SUBSET owed :
             LET taken == (owed \ fail) \cup conv
                 left == fail # {}
             IN /\ fails + Cardinality(fail) <= MaxFetchFails
                /\ w' = [w EXCEPT ![s].pc = "consumed",
                         ![s].cId = w[s].pend.id, ![s].cFold = w[s].pend.fold, ![s].clk = ck,
                         ![s].local = [p \in Paths |-> IF p \in taken THEN doc[p] ELSE @[p]],
                         ![s].baseline = [p \in Paths |-> IF p \in taken THEN doc[p] ELSE @[p]],
                         ![s].integrated = @ \cup {Gen(doc[p]) : p \in {q \in taken : doc[q] # Nil}},
                         \* Something left owed: the tree is not integrated with
                         \* this document, and nothing is recorded as derived.
                         ![s].synced = IF left THEN @ ELSE seq,
                         ![s].derived = IF left /\ ConsumeKeepsLeft THEN 0 ELSE seq,
                         \* A dirty path left untaken is the agent's work, owed
                         \* again the moment the agent backs out (a revert, or a
                         \* new file deleted unpublished).
                         ![s].skipped = {p \in Paths \ taken : doc[p] # w[s].baseline[p] /\ Held(s, p)
                                                             /\ w[s].local[p] # w[s].baseline[p]}]
                /\ fails' = fails + Cardinality(fail)
                /\ regressed' = (regressed \/ \E p \in owed \ fail : Back(s, p))
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, restarts, syncs, rescopes, upped, copies, orig>>

\* Step 2: what differs from the baseline is an upload or a deletion; a
\* deletion may wait for the next walk (the two-scan guard: any subset).
Scan(s) ==
  /\ On(s) /\ w[s].pc = "consumed"
  /\ LET W == w[s]
         dirty == {p \in Paths : W.local[p] # W.baseline[p]}
         ups == {p \in dirty : W.local[p] # Nil}
         absent == {p \in dirty : W.local[p] = Nil}
         \* A carrying barrier confirms first absences: every absent dirty
         \* path is deleted (`confirm_absences`, the declared form).
     IN \E dels \in IF Carries(s) THEN {absent} ELSE SUBSET absent :
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
  /\ \A p \in Paths : w[s].local[p] = w[s].baseline[p]
  /\ seq = w[s].synced
  /\ LET W == w[s] IN
     w' = [w EXCEPT ![s].pc = "idle",
                    ![s].car = IF W.car.id # 0 THEN [W.car EXCEPT !.uncited = Uncited(W.car, doc)] ELSE @,
                    ![s].bound = IF W.cId # 0
                                 THEN [id |-> W.cId, fold |-> W.cFold, doc |-> doc, seq |-> seq,
                                       wd |-> {}, inst |-> FALSE, clk |-> W.clk]
                                 ELSE @,
                    ![s].cId = 0, ![s].cFold = 0, ![s].clk = "none", ![s].iseq = 0, ![s].instd = FALSE]
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
                 nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails>>

------------------------------------------------------------------------------
(* The merge's vocabulary, read at the CAS against the CURRENT document
   (`barrier.rs::merge_onto`, `manifest::merge`).                           *)

\* The merge base IS the baseline.
Foreign(s, p) == doc[p] # w[s].baseline[p]
\* The version a delete since the baseline removed.
DeletedAt(s, p) == IF tomb[p] # Nil THEN tomb[p] ELSE w[s].baseline[p]
MineInMerge(s) == w[s].uploads \cap w[s].upDone
\* What the code's merge reports as `foreign` and `gone` (the trace's counts),
\* and whether either leaves the tree owed.
MergeForeign(s) == {p \in Paths : p \notin MineInMerge(s) /\ Foreign(s, p) /\ doc[p] # Nil}
MergeGone(s) == {p \in Paths : p \notin MineInMerge(s) \cup w[s].deletes /\ doc[p] = Nil /\ w[s].baseline[p] # Nil}

\* Nothing to publish: no claim, no CAS; the merge base follows the document.
PullOnly(s) ==
  /\ On(s) /\ w[s].pc = "scanned"
  /\ w[s].uploads = {} /\ w[s].deletes = {}
  /\ LET W == w[s] IN
     w' = [w EXCEPT ![s].pc = "idle",
                    ![s].inst = doc, ![s].synced = seq,
                    ![s].snap = [p \in Paths |-> Nil],
                    ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
                    ![s].car = IF W.car.id # 0 THEN [W.car EXCEPT !.uncited = Uncited(W.car, doc)] ELSE @,
                    ![s].bound = IF W.cId # 0
                                 THEN [id |-> W.cId, fold |-> W.cFold, doc |-> doc, seq |-> seq,
                                       wd |-> {}, inst |-> FALSE, clk |-> W.clk]
                                 ELSE @,
                    ![s].cId = 0, ![s].cFold = 0, ![s].clk = "none", ![s].iseq = 0, ![s].instd = FALSE]
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
  /\ On(s) /\ w[s].pc = "claimed" /\ (holder = s \/ w[s].deposed) /\ ~w[s].verified
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
\* deleted since the merge base records the deleted version; mine DELETING
\* over a version theirs changed applies, and records theirs (M3).
Install(s) ==
  /\ On(s) /\ w[s].pc = "claimed" /\ (InstallFenced => holder = s) /\ w[s].verified
  /\ LET W == w[s]
         mine == W.uploads \cap W.upDone
         contested == IF CommitSurfacesForeign
                      THEN {p \in mine \ W.gone : Foreign(s, p) /\ doc[p] # Nil}
                      ELSE {}
         delOverridden == IF CommitRecordsDeleteOverride
                          THEN {p \in mine \ W.gone : Foreign(s, p) /\ doc[p] = Nil /\ W.baseline[p] # Nil}
                          ELSE {}
         delOver == IF DeleteWinsPreserved
                    THEN {p \in W.deletes : Foreign(s, p) /\ doc[p] # Nil}
                    ELSE {}
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
                                 \cup {<<p, DeletedAt(s, p)>> : p \in delOverridden}
                                 \cup {<<p, doc[p]>> : p \in delOver}
       /\ w' = [w EXCEPT ![s].pc = "cased",
                         ![s].upDone = @ \ W.gone,
                         ![s].inst = inst, ![s].retire = retired,
                         \* The CAS replaced exactly the derived document: the
                         \* installed one is it plus this tree's own changes.
                         ![s].adv = IF CommitAdvanceGuarded THEN seq = W.derived ELSE TRUE,
                         ![s].collected = retired = {},
                         \* H10: what a carrying CAS published of the declaration,
                         \* journalled with it (`barrier.rs:1107-1125`): every dirty
                         \* path it took, which were deletes, and what it did not
                         \* carry (withheld uploads, deletes the merge outranked).
                         \* A barrier that adds nothing makes no CAS and no entry.
                         ![s].car = IF Carries(s) /\ ~nothing
                                    THEN LET c0 == IF W.car.id = W.cId THEN W.car ELSE [NoCar EXCEPT !.id = W.cId]
                                         IN [c0 EXCEPT !.paths = @ \cup W.uploads \cup W.deletes,
                                                       \* L-136: a later carry's upload of a path an
                                                       \* earlier one deleted is an upload again.
                                                       !.deletes = (@ \ W.uploads) \cup W.deletes,
                                                       !.dropped = @ \cup W.gone \cup {p \in W.deletes : inst[p] # Nil},
                                                       \* L-137: the bytes each upload published.
                                                       !.pub = [p \in Paths |-> IF p \in W.uploads \ W.gone THEN W.snap[p]
                                                                               ELSE IF p \in W.uploads \cup W.deletes THEN Nil
                                                                               ELSE @[p]]]
                                    ELSE @,
                         ![s].stamps = IF Carries(s) /\ ~nothing THEN @ \cup {<<seq + 1, W.clk>>} ELSE @,
                         ![s].lost = IF MaxTouches > 0 THEN @ \cup {W.snap[p] : p \in W.gone} ELSE @,
                         ![s].iseq = IF Carries(s) THEN (IF nothing THEN seq ELSE seq + 1) ELSE @,
                         ![s].instd = IF Carries(s) THEN ~nothing ELSE @]
       \* A commit's delete is acknowledged as the gateway's is (E21: without
       \* it, a delete then a rename back reads as a regress at a peer).
       /\ udel' = udel \cup {<<p, doc[p]>> : p \in {q \in Paths : doc[q] # Nil /\ inst[q] = Nil}}
  /\ UNCHANGED <<live, minted, base, acked, holder, gw, mv, nextGen, ui, reqs, barriers, aux>>

\* Step 6: the retired set, in one batch, sparing what the installed
\* document still cites — at once only without the retire age; with it,
\* the commit's retirements were logged (`RetUpdate`) and `Reap` takes them.
Collect(s) ==
  /\ On(s) /\ w[s].pc = "cased" /\ ~w[s].collected
  /\ DeletesUnderLease => holder = s
  /\ LET W == w[s]
         taken == {h \in W.retire : ~(CollectorSparesCited /\ \E q \in Paths : W.inst[q] = h)}
     IN /\ live' = IF RetireAge THEN live ELSE live \ taken
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
  /\ RetireAge => h \notin retiring
  /\ ~\E p \in w[s].upDone : w[s].snap[p] = h
  /\ live' = live \ {h}
  /\ UNCHANGED <<minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, w, aux>>

\* Step 7: the baseline follows what this barrier published; the pointer is
\* where this install left it, and what its merge saw untaken is owed.
Finish(s) ==
  /\ On(s) /\ w[s].pc = "cased" /\ w[s].collected
  /\ LET W == w[s] IN
     w' = [w EXCEPT ![s].pc = "idle",
             ![s].baseline = [p \in Paths |->
                                IF p \in W.uploads \cap W.upDone THEN W.snap[p]
                                ELSE IF p \in W.deletes /\ W.inst[p] = Nil THEN Nil
                                ELSE @[p]],
             ![s].synced = IF W.inst = doc THEN seq ELSE @,
             ![s].derived = IF W.adv /\ W.inst = doc THEN seq ELSE @,
             ![s].skipped = IF W.adv /\ W.inst = doc
                              THEN @ \ ((W.uploads \cap W.upDone) \cup {p \in W.deletes : W.inst[p] = Nil})
                              ELSE @,
             ![s].adv = FALSE,
             ![s].uploads = {}, ![s].deletes = {}, ![s].snap = [p \in Paths |-> Nil],
             ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
             ![s].collected = FALSE,
             \* The ack names the document this install left (`note_carrier_uncited`).
             ![s].car = IF W.car.id # 0 THEN [W.car EXCEPT !.uncited = Uncited(W.car, W.inst)] ELSE @,
             ![s].bound = IF W.cId # 0
                          THEN [id |-> W.cId, fold |-> W.cFold, doc |-> W.inst, seq |-> W.iseq,
                                wd |-> W.gone, inst |-> W.instd, clk |-> W.clk]
                          ELSE @,
             ![s].cId = 0, ![s].cFold = 0, ![s].clk = "none", ![s].iseq = 0, ![s].instd = FALSE,
             ![s].deposed = FALSE]
  \* The release is a CAS on this writer's token: a no-op on a cell a
  \* successor holds (`lease::release`).
  /\ holder' = IF holder = s THEN "none" ELSE holder
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                 nextGen, ui, reqs, barriers, aux>>

------------------------------------------------------------------------------
(* The restart and the sync.                                               *)

\* A restart keeps the tree and the baseline (with `synced`, `derived` and `skipped`, all
\* on disk) and drops the barrier in flight.  No journal is read back.
Restart(s) ==
  /\ On(s) /\ restarts < MaxRestarts
  /\ w' = [w EXCEPT ![s].pc = "idle",
                    ![s].uploads = {}, ![s].deletes = {},
                    ![s].snap = [p \in Paths |-> Nil],
                    ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
                    ![s].inst = [p \in Paths |-> Nil], ![s].retire = {},
                    ![s].collected = FALSE, ![s].adv = FALSE,
                    \* The intent is on disk; its replay starts the apply over.
                    ![s].sStage = IF @ = "none" THEN "none" ELSE "saved",
                    ![s].sKeep = {},
                    ![s].cId = 0, ![s].cFold = 0, ![s].clk = "none", ![s].iseq = 0, ![s].instd = FALSE,
                    ![s].deposed = FALSE]
  /\ holder' = IF holder = s THEN "none" ELSE holder
  /\ restarts' = restarts + 1
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                 nextGen, ui, reqs, barriers, regressed, syncs, rescopes, fails, upped, copies, orig>>

\* A sync, between barriers: it takes what is owed, and records what it
\* derived as a consume would: the paths it left are the agent's (dirty,
\* with the document moved) (`sync.rs` step 5).  It does not move `synced`.
\* A sync does not replay a rescope; it can run after a crash left one
\* "saved" (the scope it reads is still the old one).
Sync(s) ==
  /\ On(s) /\ s \notin Readers /\ w[s].pc = "idle" /\ syncs < MaxSyncs /\ w[s].sStage \in {"none", "saved"}
  /\ \E p \in Paths : Owed(s, p)
  /\ LET all == {p \in Paths : Owed(s, p)} IN
     \E fail \in SUBSET all :
     LET owed == all \ fail
         left == fail # {}
         bl == [p \in Paths |-> IF p \in owed THEN doc[p] ELSE w[s].baseline[p]]
     IN /\ fails + Cardinality(fail) <= MaxFetchFails
        /\ w' = [w EXCEPT ![s].local = [p \in Paths |-> IF p \in owed THEN doc[p] ELSE @[p]],
                       ![s].baseline = bl,
                       ![s].integrated = @ \cup {Gen(doc[p]) : p \in {q \in owed : doc[q] # Nil}},
                       ![s].derived = IF left /\ SyncKeepsLeft THEN 0 ELSE seq,
                       ![s].skipped = IF left /\ SyncKeepsLeft THEN {}
                                      ELSE {p \in Paths : doc[p] # bl[p] /\ w[s].local[p] # bl[p] /\ Held(s, p)}]
        /\ fails' = fails + Cardinality(fail)
        /\ regressed' = (regressed \/ \E p \in owed : Back(s, p))
  /\ syncs' = syncs + 1
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, restarts, rescopes, upped, copies, orig>>

------------------------------------------------------------------------------
(* The narrow / widen verb (`checkout.rs::rescope`, scoped-read design §4).
   The DOOR refuses a narrow over a leaving path with unpublished changes;
   the intent (target, drop set) is durable before the first mutation.  The
   APPLY converges instead of refusing: a still-cited dirty path is kept.
   Its halves: the uncite, then the unlink and the widen (shipped order;
   `RescopeUnciteFirst = FALSE` swaps them).  A restart between them sends
   the replay back to the first half, which re-derives what it keeps from
   the RECORDED drop set.                                                  *)

\* What a target drops: the paths the baseline holds that it does not admit.
Leaving(s, T) == {p \in Paths : w[s].baseline[p] # Nil /\ p \notin T}

RescopeBegin(s) ==
  /\ On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "none" /\ rescopes < MaxRescopes
  /\ \E T \in Scopes \ {w[s].scope} :
       /\ \A p \in Leaving(s, T) : w[s].local[p] = w[s].baseline[p]
       /\ w' = [w EXCEPT ![s].sStage = "saved", ![s].sTgt = T,
                         ![s].sDrop = Leaving(s, T), ![s].sKeep = {},
                         ![s].sHeld = [p \in Paths |-> Nil]]
  /\ rescopes' = rescopes + 1
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, restarts, syncs, regressed, fails, upped, copies, orig>>

\* What the apply keeps: still cited and dirty.  A path already uncited
\* reads as dirty in the code (an upload) and is NOT kept: keeping it would
\* undo the very step a crash interrupted.
KeepSet(s) == IF RescopeKeepsDirty
              THEN {p \in w[s].sDrop : w[s].baseline[p] # Nil /\ w[s].local[p] # w[s].baseline[p]}
              ELSE {}

RescopeFirst(s) ==
  /\ On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "saved"
  /\ LET keep == KeepSet(s)
         dd == w[s].sDrop \ keep
     IN w' = [w EXCEPT ![s].sStage = "mid", ![s].sKeep = keep,
                       \* What it uncites, recorded with the intent; a replay
                       \* keeps what an earlier run recorded.
                       ![s].sHeld = [p \in Paths |-> IF p \in dd /\ w[s].baseline[p] # Nil
                                                     THEN w[s].baseline[p] ELSE @[p]],
                       ![s].baseline = IF RescopeUnciteFirst
                                       THEN [p \in Paths |-> IF p \in dd THEN Nil ELSE @[p]]
                                       ELSE @,
                       ![s].local = IF RescopeUnciteFirst
                                    THEN @
                                    ELSE [p \in Paths |-> IF p \in dd THEN Nil ELSE @[p]],
                       ![s].unlinked = IF RescopeUnciteFirst
                                       THEN @
                                       ELSE @ \cup {p \in dd : w[s].local[p] # Nil}]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

\* The second half, the widen, and the new scope; then the intent clears.
\* The widen fetches each admitted citation the tree does not hold, adopts
\* bytes already there that ARE the document's, and keeps a file the agent
\* made there (L-130: uncited, published by the next barrier).
RescopeSecond(s) ==
  /\ On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "mid"
  /\ LET W == w[s]
         dd == W.sDrop \ W.sKeep
         \* The unlink: what the uncite dropped, unless the tree's bytes
         \* there are no longer those (the agent wrote since).
         unlink(p) == /\ p \in dd /\ W.local[p] # Nil
                      /\ \/ ~UnlinkChecksBytes
                         \/ W.sHeld[p] # Nil /\ Content(W.local[p]) = Content(W.sHeld[p])
         local1 == IF RescopeUnciteFirst
                   THEN [p \in Paths |-> IF unlink(p) THEN Nil ELSE W.local[p]]
                   ELSE W.local
         base1 == IF RescopeUnciteFirst
                  THEN W.baseline
                  ELSE [p \in Paths |-> IF p \in dd THEN Nil ELSE W.baseline[p]]
         add == {p \in W.sTgt : base1[p] = Nil /\ doc[p] # Nil}
         kept == IF WidenKeepsLocal
                 THEN {p \in add : local1[p] # Nil /\ Content(local1[p]) # Content(doc[p])}
                 ELSE {}
         fetch0 == add \ kept
     IN \E wfail \in SUBSET {p \in fetch0 : local1[p] = Nil \/ ~WidenKeepsLocal} :
        LET fetch == fetch0 \ wfail IN
        /\ fails + Cardinality(wfail) <= MaxFetchFails
        /\ fails' = fails + Cardinality(wfail)
        /\ w' = [w EXCEPT ![s].sStage = "none",
                       \* Fetched where the tree has nothing (or, unguarded,
                       \* over whatever it has); adopted where its bytes ARE
                       \* the document's.
                       ![s].local = [p \in Paths |->
                                       IF p \in fetch /\ (local1[p] = Nil \/ ~WidenKeepsLocal)
                                       THEN doc[p] ELSE local1[p]],
                       ![s].baseline = [p \in Paths |-> IF p \in fetch THEN doc[p] ELSE base1[p]],
                       ![s].integrated = @ \cup {Gen(doc[p]) : p \in fetch},
                       ![s].scope = W.sTgt,
                       ![s].synced = seq, ![s].derived = 0, ![s].skipped = {},
                       ![s].sTgt = {}, ![s].sDrop = {}, ![s].sKeep = {},
                       ![s].sHeld = [p \in Paths |-> Nil],
                       ![s].unlinked = (@ \cup {p \in Paths : unlink(p) /\ RescopeUnciteFirst}) \ fetch]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers,
                 restarts, syncs, regressed, rescopes, upped, copies, orig>>

------------------------------------------------------------------------------
(* A reader's tick (`reader.rs::reader_pull`).                             *)

\* What the sync last left owed, by its own record: nothing derived (a
\* fetch or a write it could not complete), or a path it skipped as dirty
\* that is clean again (`still_owed`).
StillOwed(s) == w[s].derived = 0 \/ \E p \in w[s].skipped : w[s].local[p] = w[s].baseline[p]
\* The tick does nothing: the pointer is the one last pulled against, and
\* (L-129) nothing is still owed.
ReaderSkips(s) == w[s].memo = seq /\ (ReaderRechecksOwed => ~StillOwed(s))

\* The pointer GET.
RPullRead(s) ==
  /\ On(s) /\ s \in Readers /\ w[s].pc = "idle" /\ w[s].sStage \in {"none", "saved"}
  /\ ~ReaderSkips(s)
  /\ w' = [w EXCEPT ![s].pc = "pulling", ![s].rnow = seq]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

\* The whole-tree sync, against the document as it is NOW (it reads both
\* again), recording what it derived as a consume would; then the memo is
\* the pointer read BEFORE it.
RPullSync(s) ==
  /\ On(s) /\ s \in Readers /\ w[s].pc = "pulling"
  /\ LET all == {p \in Paths : Owed(s, p)} IN
     \E fail \in SUBSET all :
     LET owed == all \ fail
         left == fail # {}
         bl == [p \in Paths |-> IF p \in owed THEN doc[p] ELSE w[s].baseline[p]]
     IN /\ fails + Cardinality(fail) <= MaxFetchFails
        /\ w' = [w EXCEPT ![s].pc = "idle", ![s].memo = w[s].rnow,
                       ![s].local = [p \in Paths |-> IF p \in owed THEN doc[p] ELSE @[p]],
                       ![s].baseline = bl,
                       ![s].integrated = @ \cup {Gen(doc[p]) : p \in {q \in owed : doc[q] # Nil}},
                       ![s].derived = IF left /\ SyncKeepsLeft THEN 0 ELSE seq,
                       ![s].skipped = IF left /\ SyncKeepsLeft THEN {}
                                      ELSE {p \in Paths : doc[p] # bl[p] /\ w[s].local[p] # bl[p] /\ Held(s, p)}]
        /\ fails' = fails + Cardinality(fail)
        /\ regressed' = (regressed \/ \E p \in owed : Back(s, p))
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, restarts, syncs, rescopes, upped, copies, orig>>

------------------------------------------------------------------------------
(* The boundary verbs' answer (row 15; `sentinel.rs`).                     *)

\* A request: the agent's touch of `.flint/publish`, the gateway's
\* `boundary_request`, the UDS door.  Each is consumed and folded into the
\* pending record at once (`consume_sentinel`, `fold_into_pending`): its
\* nonce appended, the oldest dropped past `MaxNonces`.  A new record mints
\* its id; a fold into a standing one keeps it (L-133).  Between barriers:
\* the file and the door are polled between ticks, and a barrier reads the
\* gateway's request BEFORE it decides what it carries (L-135, found here:
\* read after, the barrier that published a requested path did not carry
\* it, and a peer's delete before the honour was answered `ok`).
\* A narrow's leftover: in the rescope's "mid" stage, a dropped path whose
\* uncite already ran and whose tree file the second half will unlink (its
\* `unlink`, RescopeSecond; the code's replay of the intent, `checkout.rs`).
RescopeLeftover(s, p) ==
  /\ w[s].sStage = "mid" /\ RescopeUnciteFirst
  /\ p \in w[s].sDrop \ w[s].sKeep /\ w[s].local[p] # Nil
  /\ \/ ~UnlinkChecksBytes
     \/ w[s].sHeld[p] # Nil /\ Content(w[s].local[p]) = Content(w[s].sHeld[p])
Touch(s) ==
  /\ On(s) /\ w[s].pc = "idle" /\ w[s].nt < MaxTouches
  /\ LET W == w[s]
         n == W.nt + 1
         fresh == W.pend.id = 0
         ns == (IF fresh THEN {} ELSE W.pend.nonces) \cup {n}
         kept == IF Cardinality(ns) > MaxNonces THEN ns \ {Oldest(ns)} ELSE ns
         \* What this request declares: the tree as it stands (the record's
         \* `id` comment): the paths it changed whose bytes the document does
         \* not already hold, at their present versions.  (A path whose bytes
         \* an install already published, with step 7 not yet run, is the
         \* published path the contract lets a peer delete, AGENTS.md: the
         \* gate's AckHolds1, after a restart between a CAS and step 7.)
         \* Not a narrow's leftover (FINDINGS Table 3, 2026-10-16): a file
         \* a mid-rescope uncite left, still holding the held bytes, is
         \* unlinked by the rescope's second half or by the barrier's replay
         \* of the intent before its scan (`barrier.rs` step 0); the code
         \* never declares it.  A path the agent rewrote there is declared.
         dirty == {p \in Paths : /\ W.local[p] # W.baseline[p] /\ Content(W.local[p]) # Content(doc[p])
                                 /\ ~RescopeLeftover(s, p)}
     IN w' = [w EXCEPT ![s].nt = n,
                       ![s].pend = [id |-> IF fresh THEN n ELSE W.pend.id, fold |-> n, nonces |-> kept],
                       ![s].cN = @ \cup {n}, ![s].capped = @ \cup (ns \ kept),
                       ![s].dset = (IF fresh THEN {} ELSE @) \cup dirty,
                       ![s].decl = [p \in Paths |-> IF p \in dirty THEN W.local[p]
                                                   ELSE IF fresh THEN Nil ELSE @[p]],
                       ![s].dMint = [p \in Paths |-> IF p \in dirty THEN nextGen
                                                    ELSE IF fresh THEN 0 ELSE @[p]],
                       ![s].moved = (IF fresh THEN {} ELSE @) \ dirty]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

\* The honour (`honor_pending_as`, `honor_publish`): at idle, once a barrier
\* that began after the latest fold has ended (the honour's own, or the
\* floor's), and the ack does not already answer the record.  What it did
\* not carry: that barrier's withheld uploads and, for this record, the
\* carrier's `dropped` and `uncited`.  `ok` iff nothing; the seq and the
\* clock are the barrier's.  The pending is retired by `RetireByAck`.
Honor(s) ==
  /\ On(s) /\ s \notin Readers /\ w[s].pc = "idle"
  /\ w[s].pend.id # 0 /\ ~Covered(s)
  /\ w[s].bound.id = w[s].pend.id /\ w[s].bound.fold = w[s].pend.fold
  /\ \E src \in {w[s].bound.clk} :
     LET W == w[s]
         carried == IF W.car.id = W.pend.id THEN W.car.dropped \cup W.car.uncited ELSE {}
         dr == W.bound.wd \cup carried
     IN w' = [w EXCEPT ![s].ack = [st |-> IF dr = {} THEN "ok" ELSE "partial",
                                    nonces |-> W.pend.nonces, seq |-> W.bound.seq,
                                    src |-> src, dropped |-> dr, inst |-> W.bound.inst],
                       ![s].aN = @ \cup W.pend.nonces]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

\* A reader answers and publishes nothing (`honor_publish`'s first arm).
HonorReader(s) ==
  /\ On(s) /\ s \in Readers /\ w[s].pc = "idle"
  /\ w[s].pend.id # 0 /\ ~Covered(s)
  /\ w' = [w EXCEPT ![s].ack = [NoAck EXCEPT !.st = "refused-read-only", !.nonces = w[s].pend.nonces],
                    ![s].aN = @ \cup w[s].pend.nonces]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

\* The retire after the ack, and the restart rule: a pending its ack already
\* answers is retired, never re-run (`ack_matches`).
RetireByAck(s) ==
  /\ On(s) /\ w[s].pc = "idle" /\ w[s].pend.id # 0 /\ Covered(s)
  /\ w' = [w EXCEPT ![s].pend = NoPend]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

------------------------------------------------------------------------------
(* Deposal (row 17; `lease.rs::claim_step`, `barrier.rs`).                 *)

\* A claim that finds a held cell standing still past the threshold deposes
\* its holder and ROTATES the manifest: a content-identical CAS, seq + 1,
\* so a CAS the straggler computed on the old document fails.  The model
\* has no clock: any holder, at any point of its section, may be judged
\* dead (D9).
Depose(t) ==
  /\ On(t) /\ w[t].pc = "scanned" /\ w[t].nd < MaxDeposals
  /\ w[t].uploads \subseteq w[t].upDone /\ w[t].uploads \cup w[t].deletes # {}
  /\ holder \in Writers /\ holder # t
  /\ holder' = t
  /\ seq' = seq + 1
  /\ w' = [w EXCEPT ![t].pc = "claimed", ![t].nd = @ + 1, ![holder].deposed = TRUE]
  /\ UNCHANGED <<live, minted, doc, tomb, base, acked, conflicts, gw, mv, udel,
                 nextGen, ui, reqs, barriers, aux>>

\* The straggler's next read of the cell is `Fenced`: before its CAS (the
\* fence read, `verify_not_deposed`) the barrier is abandoned, its uploads
\* stand and the pending stays; after it, the deletes stop (`may_delete`)
\* and step 7 still follows the install.
Fence(s) ==
  /\ On(s) /\ w[s].deposed
  /\ \/ /\ w[s].pc = "claimed"
        /\ w' = [w EXCEPT ![s].pc = "idle", ![s].deposed = FALSE,
                    ![s].uploads = {}, ![s].deletes = {},
                    ![s].snap = [p \in Paths |-> Nil],
                    ![s].upDone = {}, ![s].gone = {}, ![s].verified = FALSE,
                    ![s].inst = [p \in Paths |-> Nil], ![s].retire = {},
                    ![s].collected = FALSE, ![s].adv = FALSE,
                    ![s].cId = 0, ![s].cFold = 0, ![s].clk = "none", ![s].iseq = 0, ![s].instd = FALSE]
     \/ /\ w[s].pc = "cased" /\ ~w[s].collected
        /\ w' = [w EXCEPT ![s].retire = {}, ![s].collected = TRUE]
  /\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>

------------------------------------------------------------------------------
(* The retire age (M1) and a reader of the document.                       *)

\* Every step that moves the document LOGS the handles it stopped citing
\* (`manifest::cas_write_retiring`: Handles(D) \ Handles(result), a writer's
\* commit and the gateway's alike; a rename keeps its handle cited).  Read
\* off the step, as `took` is.  The code's crash between a CAS and its log
\* leaves those handles to the write-age rule; that window is not modelled.
RetUpdate ==
  LET cited(d) == {d[p] : p \in {q \in Paths : d[q] # Nil}}
  IN retiring' = IF RetireAge THEN retiring \cup (cited(doc) \ cited(doc')) ELSE retiring

\* G elapses: everything retiring is now due, and a reader has lagged.
Age ==
  /\ ages < MaxAges
  /\ aged' = aged \cup retiring /\ retiring' = {} /\ rlag' = TRUE /\ ages' = ages + 1
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, w, took, aux, rdoc>>

\* A writer, in its commit section, reaps a handle retired at least G ago
\* (`untracked.rs::reap_retired`; it spares a cited handle).
Reap(s, h) ==
  /\ On(s) /\ holder = s /\ w[s].pc \in {"claimed", "cased"}
  /\ h \in aged /\ h \in live /\ ~Cited(h)
  /\ live' = live \ {h} /\ aged' = aged \ {h}
  /\ UNCHANGED <<minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, w, took, aux, retiring, ages, rdoc, rlag>>

\* A reader (a checkout, a UI read) loads the document.
RLoad ==
  /\ rdoc' = doc /\ rlag' = FALSE
  /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                 nextGen, ui, reqs, barriers, w, took, aux, retiring, aged, ages>>

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
  /\ restarts = 0 /\ syncs = 0 /\ regressed = FALSE /\ rescopes = 0 /\ fails = 0
  /\ upped = live /\ copies = 0 /\ orig = [h \in Handles |-> Nil]
  /\ retiring = {} /\ aged = {} /\ ages = 0
  /\ rdoc = [p \in Paths |-> Nil] /\ rlag = TRUE

GatewayStep ==
  \/ \E p \in Paths : GPut(p) \/ GCas(p) \/ GDelete(p)
  \/ \E p, q \in Paths : GRename(p, q)
  \/ GRenameFinish

WriterStep ==
  \/ \E s \in Writers :
       \/ Checkout(s) \/ Consume(s) \/ Scan(s) \/ Skip(s) \/ PullOnly(s) \/ Claim(s)
       \/ Verify(s) \/ Install(s) \/ Collect(s) \/ Finish(s) \/ Restart(s) \/ Sync(s)
       \/ RescopeBegin(s) \/ RescopeFirst(s) \/ RescopeSecond(s)
       \/ RPullRead(s) \/ RPullSync(s)
       \/ Touch(s) \/ Honor(s) \/ HonorReader(s) \/ RetireByAck(s) \/ Depose(s) \/ Fence(s)
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

Next ==
  \/ (GatewayStep \/ WriterStep) /\ TookUpdate /\ RetUpdate /\ UNCHANGED <<aged, ages, rdoc, rlag>>
  \/ Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)
Spec == Init /\ [][Next]_vars

\* G1, behaviourally: fair to the gateway's CAS only.  A writer may stop
\* anywhere, the lease holder included.
LSpec == Spec /\ \A p \in Paths : WF_vars(GCas(p) /\ TookUpdate /\ RetUpdate /\ UNCHANGED <<aged, ages, rdoc, rlag>>)

------------------------------------------------------------------------------
(* The claims.                                                              *)

Inv_CitationsLive == \A p \in Paths : doc[p] # Nil => doc[p] \in live
Inv_OneName == \A p, q \in Paths : p # q /\ doc[p] # Nil => doc[p] # doc[q]

\* SUPERSEDED (2026-10-14) by AN5 below; kept as the record.  An
\* acknowledged handle that is gone is accounted for by something in the
\* state that names it (`LeanCore`'s, plus the acknowledged delete).  FALSE
\* once the gateway may remove three times (results/2026-10-07-tlaps-m5/
\* NOTES.txt E8: renamed away and back, deleted, a fresh install clears the
\* tomb, reaped); no gate world before 2026-10-14 had MaxRemovals >= 3.
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

\* THE CLAIM (AN5, 2026-10-14; proved: results/2026-10-07-tlaps-m5/m5d/,
\* Spec => []AN5).  An acked <<p, h>> is accounted for along h's RENAME
\* CHAIN -- its own path and every path it was acked at: by a record there
\* (the document, a tombstone, a tree that took it and moved on, an
\* acknowledged delete, a conflict naming a version derived from it), by a
\* later ack there, or by an ACKNOWLEDGED version with other bytes derived
\* from it (a save over it).  AN3 (the chain, without the last two) is FALSE
\* (E38, E39: a copy's acked successor contested away or renamed on, the copy
\* reaped).  An acknowledged delete is the gateway's answer or the commit's
\* report: `udel` is a ghost of the acknowledgements (PROTOCOL.md).
Chain(p, h) == {p, h[1]} \cup {q \in Paths : <<q, h>> \in acked}
AccountedAt(p, h, Q) ==
  \/ h \in live
  \/ Preserved(h)
  \/ \E q \in Q :
       \/ doc[q] # Nil /\ Derives(doc[q], h)
       \/ tomb[q] # Nil /\ Derives(tomb[q], h)
       \/ \E s \in Writers : Gen(h) \in took[s][q] /\ w[s].local[q] # h
       \/ \E d \in udel : d[1] = q /\ Derives(d[2], h)
  \/ \E b \in acked : b[1] = p /\ Later(b[2], h)
AccountedChain(p, h) ==
  \/ AccountedAt(p, h, Chain(p, h))
  \/ \E b \in acked : b[1] \in Chain(p, h) /\ Later(b[2], h)
ConfAt(q, h) == \E c \in conflicts : c[1] = q /\ c[2] # Nil /\ Derives(c[2], h)
Desc(h) == \E b \in acked : Derives(b[2], h) /\ Content(b[2]) # Content(h)
AN5 == \A a \in acked : \/ AccountedChain(a[1], a[2]) \/ Desc(a[2])
                        \/ \E q \in Chain(a[1], a[2]) : ConfAt(q, a[2])

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

\* Restated 2026-10-14 for deposal (row 17): a writer in its commit section
\* holds the lease unless a takeover deposed it, and the holder is in its
\* section, not deposed.  With `MaxDeposals = 0` the first half is the claim
\* as it was.
Inv_OneHolder ==
  /\ \A s \in Writers : w[s].pc \in {"claimed", "cased"} /\ ~w[s].deposed => holder = s
  /\ holder \in Writers => w[holder].pc \in {"claimed", "cased"} /\ ~w[holder].deposed
\* SUPERSEDED (2026-10-14) by NR5 below; kept as the record (`regressed` and
\* `Back` stay as its ghost).  FALSE once the gateway may remove three times
\* (NOTES.txt E6: rename, re-create, delete, rename back, a consume regresses).
Inv_NoRegress == ~regressed

\* THE CLAIM (NR5, 2026-10-14; proved: results/2026-10-07-tlaps-m5/m5c/,
\* Spec => NR5).  A tree never takes the document's version at p while that
\* steps BACK -- to a version its baseline derives from, with other bytes --
\* unless a record at p names a version derived from the baseline: a
\* conflict, an acknowledged delete, or an acknowledgement at another path.
NamedAt(p, b) ==
  \/ \E c \in conflicts : c[1] = p /\ Derives(c[2], b)
  \/ \E d \in udel : d[1] = p /\ Derives(d[2], b)
  \/ \E a \in acked : a[1] # p /\ Derives(a[2], b)
BackR5(s, p) == /\ doc[p] # Nil /\ w[s].baseline[p] # Nil /\ Derives(w[s].baseline[p], doc[p])
                /\ Content(doc[p]) # Content(w[s].baseline[p])
                /\ ~NamedAt(p, w[s].baseline[p])
Takes(s, p) == w'[s].baseline[p] = doc[p]
NR5 == [][\A s \in Writers, p \in Paths : ~(BackR5(s, p) /\ Takes(s, p))]_vars

\* M1: a reader that loaded a document less than G ago can fetch every
\* handle it cites.  (One lagging longer re-resolves the pointer.)
Inv_ReaderFetches == ~rlag => \A p \in Paths : rdoc[p] # Nil => rdoc[p] \in live

\* THE CHEAP PATH IS SOUND: an idle writer the cheap path would let skip is
\* owed nothing.  Two gaps it closed by construction (the `behind` design's
\* gate 2026-09-25: a skipped dirty path, a withheld upload).  A rescope in
\* flight is no such writer: its replay runs before any consume and clears
\* the record (`derived` = 0).
Inv_ShortcutSound ==
  \A s \in Writers :
    (On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "none" /\ CheapPath(s))
      => \A p \in Paths : ~Owed(s, p)

\* A READER THAT SKIPS A TICK IS OWED NOTHING (the reader's cheap path).
Inv_ReaderSound ==
  \A s \in Readers :
    (On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "none" /\ ReaderSkips(s))
      => \A p \in Paths : ~Owed(s, p)

\* M3: a barrier that published a delete leaves the tree and the document
\* agreeing at that path (applied, or the tree took what stands).
Prop_DeleteSettles ==
  [][\A s \in Writers :
       Finish(s) => \A p \in w[s].deletes : w[s].inst[p] = Nil \/ w'[s].local[p] = w[s].inst[p]]_vars

\* The bytes a handle holds after the step (a copy's original is recorded
\* by the step that makes it).
ContentNext(h) == IF h # Nil /\ orig'[h] # Nil THEN orig'[h] ELSE h
\* The agent's own steps.
AgentStep(s) == \E q \in Paths : Edit(s, q) \/ Delete(s, q)

\* THE AGENT'S WORK IS KEPT: bytes the agent wrote that nobody has PUT yet
\* leave its tree only by its own hand.  Nothing else holds them.
Prop_AgentWorkKept ==
  [][\A s \in Writers, p \in Paths :
       LET h == w[s].local[p] IN
       (/\ h # Nil /\ h \notin upped
        /\ w'[s].local[p] # h
        /\ ~(w'[s].local[p] # Nil /\ ContentNext(w'[s].local[p]) = h)) => AgentStep(s)]_vars

\* WHAT THE SCOPE DECLINED STAYS DECLINED: no step but the agent's writes
\* the tree at a path it neither holds nor admits (before or after) — but
\* a narrow's unlink of a path it dropped.
Prop_ScopeRespected ==
  [][\A s \in Writers, p \in Paths :
       (/\ w[s].st = "on"
        /\ p \notin w[s].scope \cup w'[s].scope /\ w[s].baseline[p] = Nil
        /\ w'[s].local[p] # w[s].local[p]
        /\ ~(w'[s].local[p] = Nil /\ p \in w[s].sDrop)
        /\ ~(w'[s].local[p] # Nil /\ w[s].local[p] # Nil
             /\ ContentNext(w'[s].local[p]) = Content(w[s].local[p]))) => AgentStep(s)]_vars

\* A NARROW IS AN UNWATCH, NEVER AN ABSENCE: no barrier's scan chooses to
\* publish the delete of a path a rescope unlinked and the agent has not
\* touched since.
Prop_NarrowNeverDeletes ==
  [][\A s \in Writers :
       (w[s].pc = "consumed" /\ w'[s].pc = "scanned")
         => w'[s].deletes \cap w[s].unlinked = {}]_vars

\* Probes for the new steps.
ProbeNarrowed == [][~\E s \in Writers : w[s].sStage = "mid" /\ w'[s].sStage = "none" /\ w'[s].scope \subseteq w[s].scope /\ w'[s].scope # w[s].scope]_vars
ProbeWidened == [][~\E s \in Writers : w[s].sStage = "mid" /\ w'[s].sStage = "none"
                     /\ \E p \in Paths : w[s].baseline[p] = Nil /\ w'[s].baseline[p] # Nil]_vars
ProbeRescopeReplayed == [][~\E s \in Writers : w[s].sStage = "mid" /\ w'[s].sStage = "saved"]_vars
\* A reader pulled, and a reader skipped a tick it could have taken.
ProbeReaderPulled == [][~\E s \in Readers : w[s].pc = "pulling" /\ w'[s].pc = "idle"]_vars
ProbeReaderSkipped == ~\E s \in Readers : On(s) /\ w[s].pc = "idle" /\ w[s].memo = seq /\ w[s].memo # 0
\* A consume or a sync left a path it owed (a fetch failed).
ProbeFetchFailed == [][fails' = fails]_vars
ProbeOutOfScopePublished ==
  ~\E s \in Writers, p \in Paths : On(s) /\ p \notin w[s].scope /\ w[s].baseline[p] # Nil

\* G1: every save the gateway starts is answered.  Checked under LSpec.
Prop_UISaveCompletes == \A p \in Paths : (gw[p] # Nil) ~> (gw[p] = Nil)

------------------------------------------------------------------------------
(* Rows 15-17 (2026-10-14).                                                 *)

\* THE ANSWER.  A declared path is BROKEN in document `d` when the agent has
\* not moved on from what it declared there, `d` does not hold those bytes,
\* what `d` holds is not a later version (derived from the declared one,
\* or minted after the declaring request; a declared delete answered by any
\* later file), and no record preserves the declared bytes (R7's "never a
\* silent winner": a peer's later boundary may win, loudly).
LaterVersion(s, p, d) ==
  /\ d[p] # Nil
  /\ \/ w[s].decl[p] = Nil
     \/ Derives(d[p], w[s].decl[p])
     \/ Gen(Content(d[p])) >= w[s].dMint[p]
\* A conflict preserves the declared bytes while they are live, or when its
\* version is not a swept upload: the copy keeps those (AckWithheldHolds,
\* 2026-10-14: a reap of the handle after the age is not a loss).
SurfacedDecl(s, p) ==
  \E c \in conflicts : /\ c[1] = p /\ c[2] # Nil /\ Content(c[2]) = Content(w[s].decl[p])
                       /\ (c[2] \in live \/ c[2] \notin UNION {w[t].lost : t \in Writers})
Broken(s, p, d) ==
  /\ p \in w[s].dset /\ p \notin w[s].moved
  /\ Content(d[p]) # Content(w[s].decl[p])
  /\ ~LaterVersion(s, p, d)
  /\ ~SurfacedDecl(s, p)
AckWrite(s) == w'[s].ack # w[s].ack /\ w'[s].ack.st \in {"ok", "partial"}
\* AK1 (S1 rows 1 and 2): the ack names every declared path its document
\* does not carry; `ok` names none.  (It may name more: a path the agent
\* wrote after the request whose upload was withheld is not in the seq
\* either, and saying so is honest; the gate's AckWithheldHolds, DeposeAck.)
AK1 == [][\A s \in Writers :
            AckWrite(s) => {p \in Paths : Broken(s, p, w[s].bound.doc)} \subseteq w'[s].ack.dropped]_vars
\* AK3 (S1 row 4): every consumed request is pending or was answered, but
\* for what the nonce cap dropped (D2; `ProbeCapDropped` shows it).
AK3 == \A s \in Writers : w[s].cN \ w[s].capped \subseteq w[s].pend.nonces \cup w[s].aN
\* AK4 (S1 row 5): an ack whose barrier installed names the clock its CAS
\* stamped on the bucket.
AK4 == \A s \in Writers :
         w[s].ack.st \in {"ok", "partial"} /\ w[s].ack.inst => <<w[s].ack.seq, w[s].ack.src>> \in w[s].stamps

\* NR1 (row 16, S4 row 1): a path the tree deleted and has not published
\* comes back only by the agent's own write.
NR1 == [][\A s \in Writers, p \in Paths :
            w[s].local[p] = Nil /\ w[s].baseline[p] # Nil /\ w'[s].local[p] # Nil
              => w'[s].local[p] = <<p, nextGen>> /\ nextGen' = nextGen + 1]_vars

\* DP1 (row 17, S5 row 3): an install is the lease holder's.
DP1 == [][\A s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased" => holder = s]_vars
\* DP2 (L-132): the collector deletes only under the lease.  (The orphan
\* sweep and the reap are guarded in place.)
DP2 == [][\A s \in Writers : ~w[s].collected /\ w'[s].collected /\ live' # live => holder = s]_vars

\* Probes (each must fire: the step is reachable).
ProbeAckPartial == \A s \in Writers : w[s].ack.st # "partial"
ProbeAckOk == \A s \in Writers : w[s].ack.st # "ok"
ProbeAckDeferred == \A s \in Writers : w[s].ack.src # "sentinel-deferred"
ProbeCapDropped == \A s \in Writers : w[s].capped = {}
ProbeRetiredAfterRestart ==
  [][~\E s \in Writers : restarts > 0 /\ w[s].pend.id # 0 /\ w'[s].pend.id = 0]_vars
ProbeDeleteSurvivesRestart ==
  [][~\E s \in Writers, p \in Paths : restarts' = restarts + 1 /\ w[s].local[p] = Nil
                                     /\ w[s].baseline[p] # Nil /\ w'[s].local[p] = Nil]_vars
ProbeDeposed == \A s \in Writers : ~w[s].deposed
ProbeFencedBeforeCas ==
  [][~\E s \in Writers : w[s].deposed /\ w[s].pc = "claimed" /\ w'[s].pc = "idle" /\ restarts' = restarts]_vars
ProbeFencedAfterCas ==
  [][~\E s \in Writers : w[s].deposed /\ w[s].pc = "cased" /\ w[s].retire # {} /\ w'[s].collected
                          /\ live' = live /\ w'[s].retire = {}]_vars

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
     /\ Foreign(s, p) /\ doc[p] = Nil /\ w[s].baseline[p] # Nil
ProbeRepublish ==
  [][~\E s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased"
        /\ \E p \in (w[s].uploads \cap w[s].upDone) \ w'[s].gone :
              doc[p] # Nil /\ doc[p] # w[s].snap[p] /\ Derives(doc[p], w[s].snap[p])]_vars
ProbeRestartAfterCas == [][~\E s \in Writers : w[s].pc = "cased" /\ restarts' = restarts + 1]_vars
\* The consume converged (the baseline moved to bytes the tree already had).
ProbeConverged ==
  [][~\E s \in Writers, p \in Paths :
        w[s].pc = "idle" /\ w'[s].pc = "consumed" /\ Converges(s, p) /\ w'[s].baseline[p] = doc[p]]_vars
\* A delete met theirs at the CAS (the M3 row taken).
ProbeDeleteOverTheirs ==
  [][~\E s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased"
                         /\ \E p \in w[s].deletes : Foreign(s, p) /\ doc[p] # Nil]_vars
\* A writer reaped a handle retired at least G ago.
ProbeReaped == [][~\E s \in Writers : live' # live /\ aged' # aged /\ holder = s]_vars
\* The cheap path skipped a consume (the pointer where it was left).
ProbeShortcut ==
  [][~\E s \in Writers : w[s].pc = "idle" /\ w'[s].pc = "consumed" /\ CheapPath(s)]_vars
=============================================================================
