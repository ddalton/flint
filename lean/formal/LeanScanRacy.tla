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
(*                                                                         *)
(* THE GATE (latency plan §17, 2026-10-23). A checkout writes the whole   *)
(* tree and stats each file in the tick it wrote it, so every entry is    *)
(* racy and the first scan hashes the whole tree (105 s at 1M files). But *)
(* no agent can write before the checkout's marker exists (the CSI node   *)
(* plugin binds the tree into the pod only then: `node.rs` resume_lean).  *)
(* So the checkout measures its entries' racy flags when it opens that    *)
(* gate (`MeasureAtGate`): an entry whose mtime is a window before the    *)
(* gate cannot hide a write, which can only come after it. The gate is    *)
(* load-bearing: `WritesWaitForGate = FALSE` must break the refinement.   *)
(*                                                                         *)
(* THE AGENT'S CLOCK (2026-10-26). `Write` stamps the current tick, as an  *)
(* editor or a compiler does; but `touch -d`, `cp -p` and `rsync -t` set   *)
(* mtime to anything (`SetMtime`, under AgentSetsMtime). AGENTS.md forbade *)
(* it, because a same-size rewrite under the recorded mtime left the stat  *)
(* unchanged, and this model had no such action, so the rule was an       *)
(* assumption no proof could see (FINDINGS Table 3b). ctime is the stamp   *)
(* no agent can set: every write and every SetMtime moves it to now. The  *)
(* code records it and, when it moved and nothing else did, CONFIRMS by    *)
(* the bytes (CtimeConfirm), as the racy rule does; a confirm that vouches *)
(* refreshes the recorded ctime. And the racy window is measured from the *)
(* LATER of mtime and ctime (RacyOnChange): measured from an mtime the     *)
(* agent set into the past, a same-tick second `cp -p` hides.              *)
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
  ClearOutsideWindow, \* shipped TRUE: the flag is cleared only once the mtime is
                     \*   outside the window
  MeasureAtGate,     \* shipped TRUE: the checkout re-measures its entries' racy
                     \*   flags when it opens the gate (FALSE: the shape before)
  WritesWaitForGate, \* the assumption: no agent write before the gate opens
                     \*   (FALSE: the gate not honoured, which must break it)
  AgentSetsMtime,    \* the world: TRUE, the agent may set mtime to any tick
                     \*   (FALSE: AGENTS.md's old rule, assumed)
  CtimeConfirm,      \* shipped TRUE: a ctime that moved alone is confirmed by
                     \*   the bytes (FALSE: the code before 2026-10-26)
  RacyOnChange       \* shipped TRUE: the racy window is measured from the later
                     \*   of mtime and ctime (FALSE: from mtime alone)

VARIABLES
  clock,      \* the kernel's coarse clock, in ticks
  ver,        \* the file's content version (what a CRC tells apart)
  size,
  mtime,      \* the file's mtime: the tick of its last write, or what the agent set
  ctime,      \* the tick of its last change, which only the kernel sets
  read,       \* an upload in flight: [ver, size, mtime, ctime] read at tick `at`, or NoRead
  base,       \* the baseline entry: [ver, size, mtime, ctime, racy]
  gate        \* the checkout's marker is written: the agent may run

vars == <<clock, ver, size, mtime, ctime, read, base, gate>>

NoRead == [ver |-> 0, size |-> 0, mtime |-> 0, ctime |-> 0, at |-> 0, on |-> FALSE]
IsRacy(m, at) == at - m < Window
\* The stamp the racy window is measured from: the later of the two, or
\* mtime alone as before.
Later(m, c) == IF RacyOnChange /\ c > m THEN c ELSE m

TypeOK ==
  /\ clock \in 0..MaxTime /\ ver \in 0..MaxVer /\ size \in Sizes
  /\ mtime \in 0..MaxTime /\ ctime \in 0..MaxTime
  /\ read \in [ver: 0..MaxVer, size: Sizes \cup {0}, mtime: 0..MaxTime, ctime: 0..MaxTime,
               at: 0..MaxTime, on: BOOLEAN]
  /\ base \in [ver: 0..MaxVer, size: Sizes, mtime: 0..MaxTime, ctime: 0..MaxTime, racy: BOOLEAN]
  /\ gate \in BOOLEAN

Init ==
  /\ clock = 0 /\ ver = 0 /\ size \in Sizes /\ mtime = 0 /\ ctime = 0
  /\ read = NoRead
  \* A checkout writes the file and records it in the same tick: racy.
  /\ base = [ver |-> 0, size |-> size, mtime |-> 0, ctime |-> 0, racy |-> RacyClean]
  \* ... and the checkout is still running: the gate is shut.
  /\ gate = FALSE

\* The checkout ends: it re-measures its entry against the clock now, then
\* writes the marker. (Ticks before this are the rest of the checkout.)
Open ==
  /\ ~gate /\ gate' = TRUE
  /\ base' = IF MeasureAtGate THEN [base EXCEPT !.racy = @ /\ IsRacy(Later(base.mtime, base.ctime), clock)]
                              ELSE base
  /\ UNCHANGED <<clock, ver, size, mtime, ctime, read>>

\* The agent writes: new bytes, any size, stamped with the CURRENT tick;
\* only once the gate is open.
Write ==
  /\ WritesWaitForGate => gate
  /\ ver < MaxVer
  /\ ver' = ver + 1 /\ \E s \in Sizes : size' = s /\ mtime' = clock /\ ctime' = clock
  /\ UNCHANGED <<clock, read, base, gate>>

\* The agent sets the mtime to any tick, past or future (`touch -d`; after
\* a Write, `cp -p` and `rsync -t`). The kernel stamps ctime now.
SetMtime ==
  /\ AgentSetsMtime
  /\ WritesWaitForGate => gate
  /\ \E m \in 0..MaxTime : mtime' = m
  /\ ctime' = clock
  /\ UNCHANGED <<clock, ver, size, read, base, gate>>

Tick == clock < MaxTime /\ clock' = clock + 1 /\ UNCHANGED <<ver, size, mtime, ctime, read, base, gate>>

\* What the code judges, and what LeanP1 assumes. An entry is UNSURE when
\* its stat cannot vouch for its bytes: recorded racy, or its ctime moved
\* while size and mtime did not. An unsure entry is dirty exactly when its
\* bytes differ (the code hashes it: `confirm_racy`, `local_dirty`).
StatChanged == size # base.size \/ mtime # base.mtime
StampMoved  == ctime # base.ctime
Unsure      == (RacyClean /\ base.racy) \/ (CtimeConfirm /\ StampMoved)
CodeDirty   == StatChanged \/ (Unsure /\ ver # base.ver)
OracleDirty == ver # base.ver

\* The upload path, in two steps as the code takes them: the scan's stat
\* and the read of the bytes; then, after the upload, the baseline entry.
\* (A consume install or a convergence records in one step: ReadUpload
\* immediately followed by Record, with no tick between, is that case.)
ReadUpload ==
  /\ ~read.on /\ CodeDirty
  /\ read' = [ver |-> ver, size |-> size, mtime |-> mtime, ctime |-> ctime, at |-> clock, on |-> TRUE]
  /\ UNCHANGED <<clock, ver, size, mtime, ctime, base, gate>>

Record ==
  /\ read.on
  /\ LET at == IF MeasureAtRead THEN read.at ELSE clock IN
     base' = [ver |-> read.ver, size |-> read.size, mtime |-> read.mtime, ctime |-> read.ctime,
              racy |-> RacyClean /\ IsRacy(Later(read.mtime, read.ctime), at)]
  /\ read' = NoRead
  /\ UNCHANGED <<clock, ver, size, mtime, ctime, gate>>

\* confirm_racy: an unsure entry whose hash vouched (the bytes are the
\* baseline's). The recorded ctime is refreshed, and the racy flag is
\* measured again: it goes once the later stamp is outside the window
\* (ClearOutsideWindow; FALSE, it goes at once).
Confirm ==
  /\ Unsure /\ ~StatChanged /\ ver = base.ver
  /\ base' = [base EXCEPT !.ctime = ctime,
                          !.racy = RacyClean /\ ClearOutsideWindow /\ IsRacy(Later(mtime, ctime), clock)]
  /\ base' # base
  /\ UNCHANGED <<clock, ver, size, mtime, ctime, read, gate>>

Next == Write \/ SetMtime \/ Tick \/ ReadUpload \/ Record \/ Confirm \/ Open

Spec == Init /\ [][Next]_vars

\* THE REFINEMENT: the code never calls clean what the oracle calls dirty.
Inv_NoMissedChange == OracleDirty => CodeDirty

\* Probes (must be violated): the case the rule exists for is reached --
\* a same-size write that leaves the recorded stat unchanged.
ProbeNoSameTickRewrite == ~(OracleDirty /\ ~StatChanged)
\* ... and the gate clears a checkout's racy flag (the point of §17).
ProbeGateClears == [][~(~gate /\ gate' /\ base.racy /\ ~base'.racy)]_vars
\* ... and the case ctime exists for: new bytes under the recorded size and
\* mtime, in an entry not flagged racy (the agent set the mtime back).
ProbeRestoredMtime == ~(OracleDirty /\ ~StatChanged /\ ~base.racy)
==============================================================================
