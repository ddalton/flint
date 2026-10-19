--------------------------- MODULE LeanScanRacy ------------------------------
(***************************************************************************)
(* What the scan can SEE, against what LeanP1 assumes it sees.            *)
(*                                                                         *)
(* LeanP1's `Scan` and consume judge dirt by comparing VERSIONS:           *)
(*   dirty == {p \in Paths : W.local[p] # W.baseline[p]}                   *)
(* a perfect oracle. The code cannot see versions. It compares the file's *)
(* stat (size, mtime) to the stat its baseline recorded, and mtime comes  *)
(* from a coarse kernel clock: a same-size write inside the tick of a     *)
(* recorded stat leaves it identical. L-138 (2026-10-19) was exactly that *)
(* gap: such a write was never published, and a consume installed a      *)
(* peer's bytes over it. Every LeanP1 proof and gate world inherited the  *)
(* oracle, so none could see it (FINDINGS Table 3b).                      *)
(*                                                                         *)
(* This module models the code's judgment for one path and checks that,   *)
(* with git's racy-clean rule as shipped (scan.rs, barrier::local_dirty), *)
(* it never calls a file clean that the oracle calls dirty:              *)
(*   Inv_NoMissedChange == OracleDirty => CodeDirty                       *)
(* That is the refinement LeanP1's `Scan` relies on. The converse is not  *)
(* claimed: a touch with no change is code-dirty and oracle-clean, and    *)
(* republishes the same bytes, which the barrier already handles          *)
(* (finding 13, the same-content rewrite).                               *)
(*                                                                         *)
(* One file, one syncer: the judgment is per path and per tree, so more   *)
(* paths or writers add nothing it can see. Time is in clock TICKS; the   *)
(* code's 2 s window is many ticks, and Window = 1 tick is the tightest   *)
(* value the rule must survive.                                           *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
  Sizes,             \* the sizes a write may leave (two suffice)
  MaxVer,            \* writes bound
  MaxTime,           \* ticks bound
  Window,            \* racy window, in ticks (>= 1)
  \* The rule and its three load-bearing details. Each FALSE/TRUE below
  \* the shipped value is a mutation that must break Inv_NoMissedChange.
  RacyClean,         \* shipped TRUE: a racy entry is believed only if its bytes agree
  MeasureAtRead,     \* shipped TRUE: racy is measured from when the bytes were READ
                     \*   (the scan's stat on the upload path), not from when
                     \*   the entry was built after a long upload
  ClearOutsideWindow \* shipped TRUE: the flag is cleared only once the mtime is
                     \*   outside the window

VARIABLES
  clock,      \* the kernel's coarse clock, in ticks
  ver,        \* the file's content version (what a CRC tells apart)
  size,
  mtime,      \* the tick of the file's last write
  read,       \* an upload in flight: [ver, size, mtime, at] read at tick `at`, or NoRead
  base        \* the baseline entry: [ver, size, mtime, racy]

vars == <<clock, ver, size, mtime, read, base>>

NoRead == [ver |-> 0, size |-> 0, mtime |-> 0, at |-> 0, on |-> FALSE]
IsRacy(m, at) == at - m < Window

TypeOK ==
  /\ clock \in 0..MaxTime /\ ver \in 0..MaxVer /\ size \in Sizes /\ mtime \in 0..MaxTime
  /\ read \in [ver: 0..MaxVer, size: Sizes \cup {0}, mtime: 0..MaxTime, at: 0..MaxTime, on: BOOLEAN]
  /\ base \in [ver: 0..MaxVer, size: Sizes, mtime: 0..MaxTime, racy: BOOLEAN]

Init ==
  /\ clock = 0 /\ ver = 0 /\ size \in Sizes /\ mtime = 0
  /\ read = NoRead
  \* A checkout writes the file and records it in the same tick: racy.
  /\ base = [ver |-> 0, size |-> size, mtime |-> 0, racy |-> RacyClean]

\* The agent writes: new bytes, any size, stamped with the CURRENT tick.
Write ==
  /\ ver < MaxVer
  /\ ver' = ver + 1 /\ \E s \in Sizes : size' = s /\ mtime' = clock
  /\ UNCHANGED <<clock, read, base>>

Tick == clock < MaxTime /\ clock' = clock + 1 /\ UNCHANGED <<ver, size, mtime, read, base>>

\* What the code judges, and what LeanP1 assumes.
StatChanged == size # base.size \/ mtime # base.mtime
CodeDirty   == StatChanged \/ (RacyClean /\ base.racy /\ ver # base.ver)
OracleDirty == ver # base.ver

\* The upload path, in two steps as the code takes them: the scan's stat
\* and the read of the bytes; then, after the upload, the baseline entry.
\* (A consume install or a convergence records in one step: ReadUpload
\* immediately followed by Record, with no tick between, is that case.)
ReadUpload ==
  /\ ~read.on /\ CodeDirty
  /\ read' = [ver |-> ver, size |-> size, mtime |-> mtime, at |-> clock, on |-> TRUE]
  /\ UNCHANGED <<clock, ver, size, mtime, base>>

Record ==
  /\ read.on
  /\ LET at == IF MeasureAtRead THEN read.at ELSE clock IN
     base' = [ver |-> read.ver, size |-> read.size, mtime |-> read.mtime,
              racy |-> RacyClean /\ IsRacy(read.mtime, at)]
  /\ read' = NoRead
  /\ UNCHANGED <<clock, ver, size, mtime>>

\* confirm_racy's clear: the hash vouched (the bytes are the baseline's)
\* and the mtime is outside the window, so the flag goes.
ClearRacy ==
  /\ base.racy /\ ~StatChanged /\ ver = base.ver
  /\ (~ClearOutsideWindow \/ ~IsRacy(mtime, clock))
  /\ base' = [base EXCEPT !.racy = FALSE]
  /\ UNCHANGED <<clock, ver, size, mtime, read>>

Next == Write \/ Tick \/ ReadUpload \/ Record \/ ClearRacy

Spec == Init /\ [][Next]_vars

\* THE REFINEMENT: the code never calls clean what the oracle calls dirty.
Inv_NoMissedChange == OracleDirty => CodeDirty

\* Probes (must be violated): the case the rule exists for is reached --
\* a same-size write that leaves the recorded stat unchanged.
ProbeNoSameTickRewrite == ~(OracleDirty /\ ~StatChanged)
==============================================================================
