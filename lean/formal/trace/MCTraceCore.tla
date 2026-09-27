---------------------------- MODULE MCTraceCore ------------------------------
(* The skeleton's smoke: a HAND-WRITTEN trace (not the code's) of the
   delete/modify race under M3, and its one-fact mutation.  B publishes an
   edit of p1; A, whose baseline still has the seed, deletes p1: the merge
   must report the delete as OVER theirs (B's edit moved off A's base), the
   delete applies, and a new generation is installed.  Accept the trace,
   reject the mutant that claims a plain delete.                           *)
EXTENDS TraceCore

CONSTANTS A, B, p1, p2

Smoke(overA) == <<
  [ev |-> "checkout", w |-> A], [ev |-> "checkout", w |-> B],
  [ev |-> "agent_write", w |-> B, p |-> p1, g |-> 2],
  [ev |-> "consume", w |-> B],
  [ev |-> "scan", w |-> B, uploads |-> {p1}, deletes |-> {}],
  [ev |-> "upload", w |-> B, p |-> p1],
  [ev |-> "claim", w |-> B], [ev |-> "verify", w |-> B, withheld |-> {}],
  [ev |-> "install", w |-> B, outranked |-> {}, over |-> {}, seq |-> 2],
  [ev |-> "collect", w |-> B], [ev |-> "finish", w |-> B],
  [ev |-> "agent_delete", w |-> A, p |-> p1],
  [ev |-> "consume", w |-> A, adopted |-> {}],
  [ev |-> "scan", w |-> A, uploads |-> {}, deletes |-> {p1}],
  [ev |-> "claim", w |-> A], [ev |-> "verify", w |-> A, withheld |-> {}],
  [ev |-> "install", w |-> A, outranked |-> {}, over |-> overA, seq |-> 3],
  [ev |-> "collect", w |-> A], [ev |-> "finish", w |-> A]
>>
SmokeTrace == Smoke({p1})
SmokeMutant == Smoke({})
==============================================================================
