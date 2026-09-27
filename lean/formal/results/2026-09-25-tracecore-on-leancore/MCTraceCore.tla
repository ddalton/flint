---------------------------- MODULE MCTraceCore ------------------------------
(* The skeleton's smoke: a HAND-WRITTEN trace (not the code's) of the
   delete/modify race the merge outranks, and its one-fact mutation.
   B publishes an edit of p1; A, whose merge base still has the seed,
   deletes p1: the merge must report p1 outranked (theirs moved off A's
   base), and the document keeps B's version.  Accept the trace, reject
   the mutant that claims the delete applied.                              *)
EXTENDS TraceCore

CONSTANTS A, B, p1, p2

Smoke(outrankedA) == <<
  [ev |-> "checkout", w |-> A], [ev |-> "checkout", w |-> B],
  [ev |-> "agent_write", w |-> B, p |-> p1, g |-> 2],
  [ev |-> "consume", w |-> B],
  [ev |-> "scan", w |-> B, uploads |-> {p1}, deletes |-> {}],
  [ev |-> "upload", w |-> B, p |-> p1],
  [ev |-> "claim", w |-> B], [ev |-> "verify", w |-> B, withheld |-> {}],
  [ev |-> "install", w |-> B, outranked |-> {}, seq |-> 2],
  [ev |-> "collect", w |-> B], [ev |-> "finish", w |-> B],
  [ev |-> "agent_delete", w |-> A, p |-> p1],
  [ev |-> "consume", w |-> A],
  [ev |-> "scan", w |-> A, uploads |-> {}, deletes |-> {p1}],
  [ev |-> "claim", w |-> A], [ev |-> "verify", w |-> A, withheld |-> {}],
  \* Outranked, A's install changes nothing and retires nothing: no
  \* collect and no seq (the code's merge says `adds_nothing`).
  [ev |-> "install", w |-> A, outranked |-> outrankedA],
  [ev |-> "finish", w |-> A]
>>
SmokeTrace == Smoke({p1})
SmokeMutant == Smoke({})
==============================================================================
