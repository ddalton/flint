------------------------------ MODULE TraceCore ------------------------------
(***************************************************************************)
(* Trace validation against LeanCore.tla: the shipped shape, not the       *)
(* history model.  TraceLean.tla does this for LeanSubtree.tla and its     *)
(* 70-odd constants; this module does it for the one page of rules the     *)
(* protocol reasoning is now written on, so a trace the code produced is   *)
(* checked against the model the claims are proved in.                    *)
(*                                                                         *)
(* P5 stage A, 2026-09-24.  What exists:                                *)
(*   - the event -> action map (Match), one LeanCore action per event;     *)
(*   - the merge's per-path decision as DATA: the code's `merge` trace     *)
(*     event names the deletes theirs outranked (`manifest::              *)
(*     delete_outcome`), and Install must agree with `Foreign(s, p)` —     *)
(*     the fact M5/L-124 showed the code and the model could disagree on;  *)
(*   - a hand-written smoke trace (MCTraceCore.tla) and its mutation.      *)
(*   - ndjson2core.py: a conformance test's NDJSON -> TraceCoreData.tla   *)
(*     (the step map is in its docstring), run by trace-check-core.sh.     *)
(*                                                                         *)
(* Accepted = TraceIncomplete VIOLATED (the cursor reached the end).       *)
(***************************************************************************)
EXTENDS LeanCore, Sequences, TLC

CONSTANT Trace

VARIABLE l

TraceInit == Init /\ l = 1

\* The deletes this writer's install leaves cited because theirs moved off
\* the merge base: Install's `IF Foreign(s, p) THEN doc[p]` arm, for a
\* declared delete.  The code's name for the same set is the `merge`
\* event's `outranked`.
Outranked(s) == {p \in w[s].deletes : Foreign(s, p)}

\* What a consume did to the tree, read off the step itself: the versions
\* the tree took in, WHERE.  Pairs, not handles: a handle names the path it
\* was minted at, not the one it sits at after a rename, and a set of
\* handles could not tell one adoption of a moved write from two (the
\* RenameMovesEntry control accepted with the rule off, 2026-09-24).
AdoptedBy(s) ==
  {<<p, w'[s].local[p]>> : p \in {q \in Paths : w'[s].local[q] # w[s].local[q] /\ w'[s].local[q] # Nil}}
RemovedBy(s) == {p \in Paths : w[s].local[p] # Nil /\ w'[s].local[p] = Nil}

\* Each check is taken exactly when the step carries the field: the
\* converter emits what the code's trace says, a hand trace may say less.
Has(e, f) == f \in DOMAIN e

Match(e) ==
  \/ e.ev = "checkout" /\ Checkout(e.w)
  \/ e.ev = "agent_write" /\ nextGen = e.g /\ Edit(e.w, e.p)
  \/ e.ev = "agent_delete" /\ Delete(e.w, e.p)
  \/ e.ev = "hitl_write" /\ nextGen = e.g /\ UIWrite(e.p)
  \/ e.ev = "hitl_delete" /\ UIDelete(e.p)
  \/ e.ev = "hitl_rename" /\ UIRename(e.p, e.q)
  \/ /\ e.ev = "consume"
     /\ Consume(e.w)
     /\ Has(e, "adopted") => AdoptedBy(e.w) = e.adopted
     /\ Has(e, "removed") => RemovedBy(e.w) = e.removed
     \* A preserved copy <<path, version>>: the record names the version,
     \* the baseline advances to it (the tree publishes over it knowingly).
     \* The pair, not the handle alone: a renamed version sits at a path
     \* other than the one it was minted under.
     /\ Has(e, "dirty") => \A c \in e.dirty : c \in conflicts' /\ w'[e.w].baseline[c[1]] = c[2]
     \* The declared removals this tree refused, and no others.
     /\ Has(e, "refused") => {r.path : r \in removals \ removals'} = e.refused
     /\ Has(e, "kept") => \A p \in e.kept : <<p, Nil>> \in conflicts'
  \/ /\ e.ev = "scan"
     /\ Scan(e.w)
     /\ Has(e, "uploads") => w'[e.w].uploads = e.uploads
     /\ Has(e, "deletes") => w'[e.w].deletes = e.deletes
     /\ Has(e, "nuploads") => Cardinality(w'[e.w].uploads) = e.nuploads
     /\ Has(e, "ndeletes") => Cardinality(w'[e.w].deletes) = e.ndeletes
  \/ e.ev = "fastpath" /\ Skip(e.w)
  \/ e.ev = "upload" /\ Upload(e.w, e.p) /\ (Has(e, "h") => w[e.w].snap[e.p] = e.h)
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
     \* L-119: the destination adoptions this install declines for a move
     \* and keeps PENDING (the code's `repair` event, action "pending").
     /\ Has(e, "pending") => {p \in Paths : PendingAdoption(e.w, p)} = e.pending
     /\ Install(e.w)
     \* R7: the versions this install published over and recorded — the
     \* install's new records that name theirs' citation, or the version a
     \* delete of theirs removed where mine re-creates the path.
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
  \* NOTHING.  The core retires per path and spares a handle still cited
  \* elsewhere (a rename's destination); the code's collector subtracts
  \* the new document's handles from the old one's, finds nothing, and
  \* emits no `gc`.  Only when the model's collect removes nothing.
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
