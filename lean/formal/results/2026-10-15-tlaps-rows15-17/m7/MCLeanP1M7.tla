----------------------------- MODULE MCLeanP1M7 -----------------------------
(* M7's candidate auxiliary invariants for AK3 and AK4 (rows 15-17), checked
   before the proof is written (the method of results/2026-10-07-tlaps-m1..m5).
   AK3: a record with no id has no nonces; an ack's nonces were answered.
   AK4: outside the commit section's install-to-finish span nothing is
   installed; an installed barrier's seq and clock, and the bound's, are a
   stamp the CAS wrote.                                                    *)
EXTENDS LeanP1
AK3a == \A s \in Writers : w[s].pend.id = 0 => w[s].pend.nonces = {}
AK3b == \A s \in Writers : w[s].ack.nonces \subseteq w[s].aN
AK4a == \A s \in Writers : w[s].pc \in {"idle", "consumed", "scanned", "claimed", "pulling"} => ~w[s].instd
AK4b == \A s \in Writers : w[s].instd => <<w[s].iseq, w[s].clk>> \in w[s].stamps
AK4c == \A s \in Writers : w[s].bound.inst => <<w[s].bound.seq, w[s].bound.clk>> \in w[s].stamps
\* M1's new conjunct (deposal), checked here too.
Dep == \A s \in Writers : w[s].deposed => w[s].pc \in {"claimed", "cased"} /\ holder # s
\* AK1's state form: were the honour to fire now, its ack would name every
\* broken declared path (the honour's guard and its `dropped`, LeanP1.tla).
Honorable(s) == w[s].pend.id # 0 /\ w[s].bound.id = w[s].pend.id /\ w[s].bound.fold = w[s].pend.fold
HDropped(s) == w[s].bound.wd \cup (IF w[s].car.id = w[s].pend.id THEN w[s].car.dropped \cup w[s].car.uncited ELSE {})
BoundOK == \A s \in Writers : Honorable(s) =>
             \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s)
\* AK1's invariant candidates (with L-137's `pub`).  A declared path the
\* agent has not moved on from: dirty, the tree still holds the declared
\* bytes (DTree); clean, this record's carrier published them -- or the
\* declared delete -- or names the path dropped (CPub).
Decl(s, p) == w[s].pend.id # 0 /\ p \in w[s].dset /\ p \notin w[s].moved
DTree == \A s \in Writers, p \in Paths :
           Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] => Content(w[s].local[p]) = Content(w[s].decl[p])
CPub == \A s \in Writers, p \in Paths :
          Decl(s, p) /\ w[s].local[p] = w[s].baseline[p] =>
            /\ w[s].car.id = w[s].pend.id /\ p \in w[s].car.paths
            /\ \/ p \in w[s].car.dropped
               \/ w[s].decl[p] = Nil /\ p \in w[s].car.deletes
               \/ /\ w[s].decl[p] # Nil /\ p \notin w[s].car.deletes
                  /\ w[s].car.pub[p] # Nil /\ Content(w[s].car.pub[p]) = Content(w[s].decl[p])
==============================================================================
