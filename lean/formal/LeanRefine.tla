----------------------------- MODULE LeanRefine -------------------------------
(* The refinement: every behaviour of LeanSubtree under its shipped constants
   (`IMPL` + `ImmutableObjects`, crash-free) is a behaviour of LeanCore.

   TLC checks it as a property: `CoreRefinement` is LeanCore's `Spec` with
   every core variable defined as a function of LeanSubtree's state, and
   TLC verifies that each LeanSubtree step is a LeanCore step or a stutter
   under that mapping.  What it buys: whatever LeanCore's state-based
   invariants say, holds of the history model's shipped worlds too — and
   the mapping is itself a census of which LeanSubtree state the shipped
   shape actually reads.

   THE MAPPING
     A handle is <<birth path, gen>>.  A minted generation is unique across
     the run, so its birth path is the path it was minted under (`hminted`);
     the seed generation is one number at every seeded path, and a rename's
     citation move (`gh.renamed`) is the only way it reaches another path,
     so its birth is the source of that move.  `versions[p]` holding g at
     two paths after a rename is ONE core handle, as the collector and the
     sweep already read it (SameHandle).

   THREE HISTORY VARIABLES the core reads and the history model never kept:
     hminted   every handle name ever issued
     hbase     provenance: the version each minted handle was written from
     htook     per writer and path, every version its baseline has named
               there — the core's `took`, written by the same rule (a
               baseline that moved to a new version at p), which is what
               lets `Inv_AckedNamed` ask whether THIS tree took the handle
               in AT THIS PATH.
   The first two are written only at mints, from the unprimed state.  A
   history variable adds no behaviour: `HSpec` has exactly LeanSubtree's
   behaviours, each annotated.

   WORLDS: the crash-free IO worlds (`LeanRefine*.cfg`).  A crash or a stall
   is out of the core's scope (its writers have no "dead" state).         *)
EXTENDS LeanSubtree

CONSTANT CoreNil   \* the core's "no handle", a model value

VARIABLES hbase, hminted, htook
hvars == <<vars, hbase, hminted, htook>>

CoreGens == 1..MaxGen
CoreHandles == Paths \X CoreGens

\* The path a generation was minted under: for the seed, the source of the
\* rename that moved it here, else the path itself; for a mint, its record.
SeedBirth(p) ==
  IF \E r \in gh.renamed : r[2] = p /\ r[4] = 1
  THEN (CHOOSE r \in gh.renamed : r[2] = p /\ r[4] = 1)[1]
  ELSE p
Birth(p, g) ==
  IF g = 1 THEN SeedBirth(p)
  ELSE IF \E h \in hminted : h[2] = g THEN (CHOOSE h \in hminted : h[2] = g)[1]
  ELSE p
H(p, g) == IF g = 0 THEN CoreNil ELSE <<Birth(p, g), g>>
HMap(f) == [p \in Paths |-> H(p, f[p])]

\* The bucket, mapped.
CLive == UNION {{H(q, h) : h \in versions[q]} : q \in Paths}
CDoc == HMap(manifest)
CTomb == HMap(gh.tomb)
CEntries == {<<pr[1], H(pr[1], pr[2])>> : pr \in inbox}
CSaw == {<<t[1], H(t[1], t[2]), H(t[1], t[3])>> : t \in gh.uiBase}
RenameTo(p) ==
  IF \E r \in gh.renamed : r[1] = p THEN (CHOOSE r \in gh.renamed : r[1] = p)[2] ELSE CoreNil
CRec(p) == [path |-> p, handle |-> H(p, gh.remJudged[p]),
            to |-> RenameTo(p), over |-> H(p, gh.remOver[p])]
CRemovals == {CRec(p) : p \in removals}
CRefused == {CRec(p) : p \in gh.answered}
CAcked == {<<pr[1], H(pr[1], pr[2])>> : pr \in hitlAcked}
CConflicts == {<<pr[1], H(pr[1], pr[2])>> : pr \in conflicts}
CHolder == IF CellHeld THEN cellHolder ELSE "none"

\* The writers, mapped.  A waiting writer is a scanned one to the core (the
\* ticket queue is a fairness device); "delDone" is inside the section.
CPc(s) ==
  CASE sc[s].pc = "consumed" -> "consumed"
    [] sc[s].pc \in {"scanned", "waiting"} -> "scanned"
    [] sc[s].pc \in {"claimed", "delDone"} -> "claimed"
    [] sc[s].pc = "cased" -> "cased"
    [] OTHER -> "idle"
CQueue(s) ==
  {[path |-> pr[1], handle |-> H(pr[1], pr[2]),
    retired |-> IF pr[2] = 0 THEN H(pr[1], sc[s].fqRetired[pr[1]]) ELSE CoreNil] :
     pr \in sc[s].fq}
CW ==
  [s \in Syncers |->
    [st |-> IF sc[s].st = "running" THEN "on" ELSE "off",
     pc |-> CPc(s),
     local |-> HMap(sc[s].local), baseline |-> HMap(sc[s].baseline),
     instBase |-> HMap(sc[s].instBase),
     integrated |-> sc[s].known \ {0},
     queue |-> CQueue(s),
     consumed |-> {<<pr[1], H(pr[1], pr[2])>> : pr \in sc[s].consumed},
     declared |-> sc[s].declared, surfaced |-> sc[s].surfaced,
     uploads |-> sc[s].scanU, deletes |-> sc[s].scanD,
     snap |-> HMap(sc[s].scanGen),
     upDone |-> sc[s].upDone, gone |-> sc[s].upGone, verified |-> sc[s].upVerified,
     inst |-> HMap(sc[s].instSnap),
     retire |-> {H(p, sc[s].retire[p]) : p \in {q \in Paths : sc[s].retire[q] # 0}},
     collected |-> sc[s].pc = "cased" /\ \A p \in Paths : sc[s].retire[p] = 0,
     seen |-> sc[s].instSeq,
     \* The intent journal's own generation — set at a CAS and nowhere
     \* else, where `instSeq` also moves on a checkout and a pull-only.
     jSeq |-> sc[s].jSeq,
     \* LeanSubtree journals no parked set (L-126's rule is bound FALSE).
     jParked |-> {}]]

\* The history variables' updates: one mint per step at most.
Mints ==
  IF gh'.nextGen = gh.nextGen THEN {}
  ELSE {<<p, gh.nextGen>> : p \in {q \in Paths :
          \/ \E s \in Syncers : sc'[s].local[q] = gh.nextGen /\ sc[s].local[q] # gh.nextGen
          \/ <<q, gh.nextGen>> \in inbox' /\ <<q, gh.nextGen>> \notin inbox}}
BaseOf(h) ==
  IF \E s \in Syncers : sc'[s].local[h[1]] = h[2] /\ sc[s].local[h[1]] # h[2]
  THEN H(h[1], sc[CHOOSE s \in Syncers : sc'[s].local[h[1]] = h[2] /\ sc[s].local[h[1]] # h[2]].baseline[h[1]])
  ELSE H(h[1], manifest[h[1]])

HInit ==
  /\ Init
  /\ hbase = [h \in CoreHandles |-> CoreNil]
  /\ hminted = {<<p, 1>> : p \in Paths \ FreePaths}
  /\ htook = [s \in Syncers |-> [p \in Paths |-> {}]]
HNext ==
  /\ Next
  /\ hminted' = hminted \cup Mints
  /\ hbase' = [h \in CoreHandles |-> IF h \in Mints THEN BaseOf(h) ELSE hbase[h]]
  \* The core's `took`, by the core's own rule on this model's baselines.
  /\ htook' = [s \in Syncers |-> [p \in Paths |->
                 htook[s][p]
                   \cup (IF sc'[s].baseline[p] # 0 /\ sc'[s].baseline[p] # sc[s].baseline[p]
                         THEN {sc'[s].baseline[p]} ELSE {})
                   \cup (IF sc'[s].local[p] # 0 /\ sc'[s].local[p] # sc[s].local[p]
                         THEN {sc'[s].local[p]} ELSE {})]]
HSpec == HInit /\ [][HNext]_hvars

Core == INSTANCE LeanCore WITH
  Nil <- CoreNil, Free <- FreePaths, Writers <- Syncers,
  MaxMint <- MaxGen, MaxUI <- MaxHitl,
  CollectorSparesCited <- ~RetirePerPath, SweepSparesNamed <- SweepSparesTracked,
  CommitVerifiesUploads <- VerifyUploadedCitations,
  \* Both models judge theirs-or-mine PER PATH since 2026-09-21, the way
  \* the code does (`barrier.rs`'s R7 loop: does my baseline hold THAT
  \* handle AT THIS PATH?), so this maps onto the rule itself rather than
  \* an approximation of it.  Until then `LeanSubtree` used one flat set
  \* per writer (`sc[s].known`) and this substitution was FALSE;
  \* `LeanCoreForeignFlat` keeps that shape as a mutation and it violates
  \* `Inv_AckedNamed` at depth 20 — a peer publishing over a renamed
  \* destination it never took in THERE, surfacing nothing.
  ForeignPerPath <- TRUE,
  \* L-125 (2026-09-24): the code's step 7 drops a declared removal whose
  \* delete was outranked from the baseline; both models carry it under the
  \* same dial, so the refinement maps it straight through.
  OutrankedRemovalLeavesBaseline <- OutrankedRemovalLeavesBaseline,
  \* L-126 (2026-09-25): LeanSubtree has not got the rule yet (its patch
  \* waits in `pending/` while the box gate runs on the current module), so
  \* the refinement maps onto the core's old step 7 until then.
  ParkedKeepsMergeBase <- FALSE,
  \* The delete-override record (2026-09-25): LeanSubtree has not got it.
  CommitRecordsDeleteOverride <- FALSE,
  live <- CLive, minted <- hminted, doc <- CDoc, seq <- manSeq, tomb <- CTomb,
  base <- hbase, entries <- CEntries, saw <- CSaw, removals <- CRemovals,
  refused <- CRefused, acked <- CAcked, conflicts <- CConflicts, holder <- CHolder,
  nextGen <- gh.nextGen, ui <- gh.hitl, reqs <- gh.removals, barriers <- gh.barriers,
  w <- CW, took <- htook

CoreRefinement == Core!Spec
\* The core's invariants, read through the mapping: the same claims, on the
\* history model's state.
CoreCitationsLive == Core!Inv_CitationsLive
CoreOneName == Core!Inv_OneName
CoreAckedNamed == Core!Inv_AckedNamed
\* ...and the core's one ACTION claim (2026-09-23): a published version is
\* never silently reverted.  Through the mapping it asks every history-model
\* step that moves a citation the same question — which is how a feature the
\* core does not have (`sync`, L-123) is still held to it.
CoreNoSilentRevert == Core!Prop_NoSilentRevert
=============================================================================
