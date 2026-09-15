------------------------------ MODULE TraceLean ------------------------------
(***************************************************************************)
(* Trace validation: is a trace the code produced a behaviour of the       *)
(* model?  After Cirstea, Kuppe, Loillier, Merz, Ranzato, "Validating      *)
(* Traces of Distributed Programs Against TLA+ Specifications" (2024).     *)
(*                                                                         *)
(* `TraceData.tla` (from ndjson2tla.py) is a sequence of model steps, each *)
(* read off the syncer's event trace and the test harness's record of what *)
(* that trace cannot see.  The cursor `l` advances only through the model  *)
(* action a step names, with the values the code reported bound — so TLC   *)
(* either reaches the end (TraceIncomplete is VIOLATED: accepted), or      *)
(* exhausts every way of following the trace and stops short of it         *)
(* (rejected, and TraceProgress printed how far it got).  A rejection is a *)
(* disagreement between the model and the code at a known event: one of   *)
(* the two is wrong, and the trace says where.                             *)
(*                                                                         *)
(* This does NOT show the model complete — only that it can do what the    *)
(* code did on these runs.  It is the check the formal gate cannot make:   *)
(* the gate asks whether the model is internally sound, and nothing in it  *)
(* notices the Rust changing underneath.                                   *)
(***************************************************************************)
EXTENDS LeanSubtree, TraceData, TLC

VARIABLE l

TraceInit == Init /\ l = 1

\* Under ~GatedCitation `Next` freezes these; the trace steps do too.
Frame == versions' = versions /\ UNCHANGED gatedVars

\* What a consume did to the tree, read off the step itself.
AdoptedBy(w) ==
  {<<p, sc'[w].local[p]>> : p \in {q \in Paths : sc'[w].local[q] # sc[w].local[q] /\ sc'[w].local[q] # 0}}
RemovedBy(w) == {p \in Paths : sc[w].local[p] # 0 /\ sc'[w].local[p] = 0}

Match(e) ==
  \/ e.ev = "start" /\ StartLease(e.w)
  \/ e.ev = "agent_write" /\ ~e.same /\ gh.nextGen = e.g /\ AgentWrite(e.w, e.p)
  \/ e.ev = "agent_write" /\ e.same /\ AgentWriteSame(e.w, e.p) /\ sc'[e.w].local[e.p] = e.g
  \/ e.ev = "agent_delete" /\ AgentDelete(e.w, e.p)
  \/ e.ev = "hitl_write" /\ gh.nextGen = e.g /\ HitlWrite(e.p)
  \/ e.ev = "touch" /\ Touch(e.w)
  \/ e.ev = "take" /\ TakeSentinel(e.w)
  \/ /\ e.ev = "consume"
     /\ Consume(e.w)
     /\ AdoptedBy(e.w) = e.adopted
     /\ RemovedBy(e.w) = e.removed
     /\ \A pr \in e.dirty : pr \in conflicts' /\ sc'[e.w].baseline[pr[1]] = pr[2]
     /\ \A p \in e.kept : <<p, 0>> \in conflicts'
     /\ sc'[e.w].fq = {}
  \/ /\ e.ev = "scan"
     /\ Scan(e.w)
     /\ Cardinality(sc'[e.w].scanU) = e.uploads
     /\ Cardinality(sc'[e.w].scanD) = e.deletes
  \/ e.ev = "fastpath" /\ FastPath(e.w)
  \/ /\ e.ev = "upload"
     /\ Upload(e.w, e.p)
     /\ e.outcome = "put" => e.p \in sc'[e.w].upDone \ sc'[e.w].adopted /\ objects'[e.p] = e.g
     /\ e.outcome = "adopted" => e.p \in sc'[e.w].adopted
     /\ e.outcome = "parked" => e.p \in sc'[e.w].parked
  \/ e.ev = "claim" /\ (Claim(e.w) \/ SkipDeadHandoff(e.w))
  \/ e.ev = "wait" /\ IF sc[e.w].pc = "waiting" THEN UNCHANGED vars ELSE Enqueue(e.w)
  \/ /\ e.ev = "pullonly"
     /\ Cardinality(MergeForeign(e.w)) = e.foreign
     /\ Cardinality(MergeGone(e.w)) = e.gone
     /\ PullOnly(e.w)
  \/ /\ e.ev = "install"
     /\ Cardinality(MergeForeign(e.w)) = e.foreign
     /\ Cardinality(MergeGone(e.w)) = e.gone
     /\ CASInstall(e.w)
     /\ IF e.nothing THEN manSeq' = manSeq ELSE manSeq' = e.seq
     /\ sc[e.w].upDone \ sc'[e.w].upDone = e.withheld
  \/ e.ev = "cas_lost" /\ CASMiss(e.w)
  \/ /\ e.ev = "gc"
     /\ GCDelete(e.w, e.p)
     /\ IF e.result = "deleted" THEN objects'[e.p] = 0 ELSE objects' = objects
  \/ e.ev = "finish" /\ Finish(e.w)
  \/ e.ev = "ack" /\ IF e.status = "ok" THEN AckOk(e.w) ELSE AckPartial(e.w)
  \/ e.ev = "retire" /\ RetirePending(e.w)

TraceNext ==
  \/ /\ l <= Len(Trace)
     /\ Match(Trace[l])
     /\ Frame
     /\ l' = l + 1
  \* The one step the code takes without an event: the GC loop skips a
  \* delete the merge outranked (`barrier.rs`, `continue` before any trace),
  \* which the model's GCDelete records as done because the new manifest
  \* still cites the path.
  \/ /\ \E s \in Syncers, p \in Paths :
          /\ p \in sc[s].scanD \ sc[s].gcDone
          /\ manifest[p] # 0
          /\ GCDelete(s, p)
     /\ Frame
     /\ UNCHANGED l

ASSUME TLCSet(1, 0)
\* Always TRUE; prints each new furthest point.  Run with -workers 1 (TLC
\* registers are per worker).
TraceProgress ==
  \/ TLCGet(1) >= l
  \/ /\ PrintT(<<"TRACE-REACHED", l - 1, IF l <= Len(Trace) THEN Trace[l] ELSE "end">>)
     /\ TLCSet(1, l)
\* VIOLATED means the whole trace was followed: accepted.
TraceIncomplete == l <= Len(Trace)
==============================================================================
