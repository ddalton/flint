------------------------------ MODULE TraceCore ------------------------------
(***************************************************************************)
(* Trace validation against LeanP2.tla: the protocol as the code runs it   *)
(* after P2 (step 5, slices 1-3: the gateway commits).  A trace the code   *)
(* produced must be a behaviour of the model, with the model's safety      *)
(* claims holding along it.                                                *)
(*                                                                         *)
(* Until 2026-09-25 this module targeted LeanCore.tla (the cell's          *)
(* entries, consumed by writers); that version and its last run (44/44)    *)
(* are in ../results/2026-09-25-tracecore-on-leancore/.                    *)
(*                                                                         *)
(* ndjson2core.py turns a conformance test's NDJSON into TraceCoreData.tla *)
(* (the step map is in its docstring); trace-check-core.sh runs it.        *)
(* Accepted = TraceIncomplete VIOLATED (the cursor reached the end).       *)
(***************************************************************************)
EXTENDS LeanP2, Sequences, TLC

CONSTANT Trace

VARIABLE l

TraceInit == Init /\ l = 1

\* The deletes this writer's install leaves cited because theirs moved off
\* the merge base: the code's `merge` event names the same set (`outranked`).
Outranked(s) == {p \in w[s].deletes : Foreign(s, p)}

\* What a consume did to the tree, read off the step itself: the versions
\* the tree took in, WHERE (pairs: a renamed version sits at a path other
\* than the one it was minted under).
AdoptedBy(s) ==
  {<<p, w'[s].local[p]>> : p \in {q \in Paths : w'[s].local[q] # w[s].local[q] /\ w'[s].local[q] # Nil}}
RemovedBy(s) == {p \in Paths : w[s].local[p] # Nil /\ w'[s].local[p] = Nil}

\* Each check is taken exactly when the step carries the field.
Has(e, f) == f \in DOMAIN e

Match(e) ==
  \/ e.ev = "checkout" /\ Checkout(e.w)
  \/ e.ev = "agent_write" /\ nextGen = e.g /\ Edit(e.w, e.p)
  \/ e.ev = "agent_delete" /\ Delete(e.w, e.p)
  \* The gateway: a save's bytes, then its commit, which installs the
  \* generation the code's commit did.
  \/ e.ev = "ui_put" /\ nextGen = e.g /\ GPut(e.p)
  \/ /\ e.ev = "ui_commit" /\ GCas(e.p)
     /\ doc'[e.p] = gw[e.p]
     /\ Has(e, "seq") => seq' = e.seq
  \/ e.ev = "ui_delete" /\ GDelete(e.p) /\ (Has(e, "seq") => seq' = e.seq)
  \/ e.ev = "ui_rename" /\ GRename(e.p, e.q) /\ (Has(e, "seq") => seq' = e.seq)
  \/ /\ e.ev = "consume"
     /\ Consume(e.w)
     /\ Has(e, "adopted") => AdoptedBy(e.w) = e.adopted
     /\ Has(e, "removed") => RemovedBy(e.w) = e.removed
     \* A preserved copy <<path, version>>: the record names the version,
     \* the baseline advances to it (the tree publishes over it knowingly).
     /\ Has(e, "dirty") => \A c \in e.dirty : c \in conflicts' /\ w'[e.w].baseline[c[1]] = c[2]
     /\ Has(e, "kept") => \A p \in e.kept : <<p, Nil>> \in conflicts'
  \/ /\ e.ev = "scan"
     /\ Scan(e.w)
     /\ Has(e, "uploads") => w'[e.w].uploads = e.uploads
     /\ Has(e, "deletes") => w'[e.w].deletes = e.deletes
     /\ Has(e, "nuploads") => Cardinality(w'[e.w].uploads) = e.nuploads
     /\ Has(e, "ndeletes") => Cardinality(w'[e.w].deletes) = e.ndeletes
  \/ e.ev = "fastpath" /\ Skip(e.w)
  \* The version uploaded, read BEFORE the step: a re-upload of bytes PUT
  \* once lands at a copy, which the step names.
  \/ /\ e.ev = "upload" /\ Upload(e.w, e.p)
     /\ Has(e, "h") => w[e.w].snap[e.p] = e.h
     /\ Has(e, "at") => w'[e.w].snap[e.p] = e.at
  \/ /\ e.ev = "pullonly"
     /\ Has(e, "foreign") => Cardinality(MergeForeign(e.w)) = e.foreign
     /\ Has(e, "gone") => Cardinality(MergeGone(e.w)) = e.gone
     /\ PullOnly(e.w)
  \/ e.ev = "claim" /\ Claim(e.w)
  \/ e.ev = "verify" /\ Verify(e.w) /\ (Has(e, "withheld") => w'[e.w].gone = e.withheld)
  \/ /\ e.ev = "install"
     \* Read BEFORE the step: the merge decides against the document it
     \* merges onto.
     /\ Outranked(e.w) = e.outranked
     /\ Has(e, "foreign") => Cardinality(MergeForeign(e.w)) = e.foreign
     /\ Has(e, "gone") => Cardinality(MergeGone(e.w)) = e.gone
     /\ Install(e.w)
     \* R7: the versions this install published over and recorded — theirs'
     \* citation, or the version a delete of theirs removed where mine
     \* re-creates the path.
     /\ Has(e, "surfaced") =>
          e.surfaced = {c \in conflicts' \ conflicts :
                          /\ c[2] # Nil
                          /\ \/ c[2] = doc[c[1]]
                             \/ doc[c[1]] = Nil /\ c[2] = DeletedAt(e.w, c[1])}
     \* No seq: the code's merge added nothing, so neither may the model's.
     /\ IF Has(e, "seq") THEN seq' = e.seq /\ seq' # seq ELSE doc' = doc
  \/ /\ e.ev = "collect"
     /\ Has(e, "retired") => Cardinality(w[e.w].retire) = e.retired
     /\ Collect(e.w)
  \/ e.ev = "sweep" /\ Sweep(e.w, e.h)
  \/ e.ev = "finish" /\ Finish(e.w)

TraceNext ==
  \/ /\ l <= Len(Trace)
     /\ Match(Trace[l])
     /\ TookUpdate
     /\ l' = l + 1
  \* The one step the code takes without an event: a collect that takes
  \* NOTHING (the code's collector finds nothing and emits no `gc`).
  \/ /\ \E s \in Writers : Collect(s) /\ live' = live
     /\ TookUpdate
     /\ UNCHANGED l

TraceSpec == TraceInit /\ [][TraceNext]_<<vars, l>>

ASSUME TLCSet(1, 0)
\* Always TRUE; prints each new furthest point.  Run with -workers 1 (TLC
\* registers are per worker).
TraceProgress ==
  \/ TLCGet(1) >= l
  \/ /\ PrintT(<<"TRACE-REACHED", l - 1, IF l <= Len(Trace) THEN Trace[l] ELSE "end",
                 [s \in Writers |-> w[s].pc], holder>>)
     /\ TLCSet(1, l)

\* VIOLATED means the whole trace was followed: accepted.
TraceIncomplete == l <= Len(Trace)
==============================================================================
