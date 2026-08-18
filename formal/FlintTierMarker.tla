---------------------------- MODULE FlintTierMarker ----------------------------
(***************************************************************************)
(* The flint-lite eviction-marker visibility protocol — L2 steps 10/11     *)
(* (src/tier/evict.rs evict_file + reconcile, src/tier/hydrate.rs          *)
(* restore_once, and the un-gated consult lanes of ioops.rs).  Two of the  *)
(* chaos campaign's six real bugs were interleaving violations of ONE      *)
(* invariant — no reader ever observes stub or partial bytes as content:   *)
(*                                                                         *)
(*   - BUG 3 (mid-read evict race): a READ passing the marker consult      *)
(*     then racing the truncate served short bytes; fixed by the           *)
(*     post-I/O re-consult (ioops.rs, after pread, before serving).        *)
(*   - BUG 4 (marker after truncate — the root cause of every              *)
(*     git-under-churn failure, found by strace after days): evict_file    *)
(*     inserted the RAM marker AFTER truncate+fsync — a multi-ms window    *)
(*     where GETATTR served size 0 and READs served the bare stub.         *)
(*     READs and GETATTRs deliberately take no gate ticket, so the         *)
(*     marker's visibility is their ONLY protection.                       *)
(*                                                                         *)
(* Both are mutation runs here: the model must rediscover each             *)
(* counterexample forever.  The drills sampled these interleavings; this   *)
(* module enumerates them.                                                 *)
(*                                                                         *)
(* THE MODEL: one file identity that starts published-and-clean            *)
(* (eviction-eligible; the A4 precondition ladder — dirty bits, CRC        *)
(* verify, open-writer probe — is evict_file's own test matrix, not an     *)
(* interleaving question).  State per lane, decomposed at the points the   *)
(* code can actually interleave:                                           *)
(*                                                                         *)
(*   - bytes: full | stub | partial — the physical file.  Truncate and    *)
(*     each restore milestone are single steps (ftruncate; the restore's   *)
(*     final pwrite+fsync).                                                *)
(*   - ram: the consult-map marker.  drow: the durable tier_evicted row.   *)
(*     hyd: the durable hydrating flag (restore_once flips it BEFORE the   *)
(*     first byte — the reconciler's disambiguation hinge).                *)
(*   - The evictor's commit sequence is a CONSTANT order (EvictOrder):     *)
(*     "safe"     = row, mark, truncate   (shipped: C2 durable-first,      *)
(*                  RAM marker before the truncate)                        *)
(*     "marklate" = row, truncate, mark   (BUG 4's shipped order)          *)
(*     "rowlate"  = mark, truncate, row   (the C2 violation: a crash       *)
(*                  before the row leaves a 0-byte file with NO durable    *)
(*                  evidence — the restart serves the stub as content)     *)
(*   - The reader decomposes consult / pread / serve; PostReadCheck        *)
(*     models the bug-3 fix (re-consult between pread and serve).          *)
(*   - Crash wipes RAM (marker, all in-flight lanes), keeps durable        *)
(*     state and bytes; Reconcile (pre-listener, so reads are down until   *)
(*     it runs) implements evict.rs's arms verbatim: flag+partial =>       *)
(*     truncate back; flag+full => commit; row+full => finish the          *)
(*     half-eviction; row => re-arm the RAM marker.  Without the           *)
(*     hydrating flag (NoHydFlag mutation) a crashed restore's partial     *)
(*     falls into the C2 diverged-rollback arm — "local wins" keeps the    *)
(*     PARTIAL as the file, and the next read serves it.                   *)
(*                                                                         *)
(* Writers are OUT OF SCOPE: every write lane holds a gate ticket and the  *)
(* eviction takes the A4 exclusion (drain + refuse), a different           *)
(* protection with its own unit drills.  So are CRC content checks         *)
(* (numbers, not interleavings) and the epoch (FlintTierEpoch).            *)
(***************************************************************************)
EXTENDS Naturals, Sequences

CONSTANTS
  EvictOrder,    \* "safe" | "marklate" | "rowlate"
  PostReadCheck, \* TRUE = the bug-3 fix: re-consult after pread
  CycleCheck,    \* TRUE = the fix THIS MODULE forced: the re-consult
                 \* also requires the marker CYCLE counter unchanged
                 \* since before the consult — a complete evict+hydrate
                 \* cycle inside the read window clears the marker
                 \* before the re-consult looks, and the counter is the
                 \* only evidence it ever happened
  HydFlagOn,     \* TRUE = restore_once's durable hydrating flag
  MaxEvicts, MaxHyds, MaxHydFails, MaxReads, MaxCrashes

VARIABLES
  bytes,      \* "full" | "stub" | "partial"
  ram,        \* consult-map marker
  drow,       \* durable tier_evicted row
  hyd,        \* durable hydrating flag
  up,         \* process serving (FALSE between Crash and Reconcile)
  ev,         \* evictor progress: 0..3 into Order
  hy,         \* hydrator: "idle" | "flagged" | "partial" | "done"
  rd,         \* reader: "idle" | "checked" | "got"
  rdData,     \* what the pread captured
  cyc,        \* marker cycle counter (bumped on insert AND clear)
  rdCyc,      \* the reader's sample of cyc, taken before its consult
  evicts, hyds, hydFails, reads, crashes,
  badServe,   \* WITNESS: stub/partial served as content
  badAttr     \* WITNESS: GETATTR reported the stub while unmarked

vars == <<bytes, ram, drow, hyd, up, ev, hy, rd, rdData, cyc, rdCyc,
          evicts, hyds, hydFails, reads, crashes, badServe, badAttr>>

Order ==
  IF EvictOrder = "safe" THEN <<"row", "mark", "trunc">>
  ELSE IF EvictOrder = "marklate" THEN <<"row", "trunc", "mark">>
  ELSE <<"mark", "trunc", "row">>

TypeOK ==
  /\ bytes \in {"full", "stub", "partial"}
  /\ ram \in BOOLEAN /\ drow \in BOOLEAN /\ hyd \in BOOLEAN /\ up \in BOOLEAN
  /\ ev \in 0..3
  /\ hy \in {"idle", "flagged", "partial", "done"}
  /\ rd \in {"idle", "checked", "got"}
  /\ rdData \in {"none", "full", "stub", "partial"}
  /\ cyc \in Nat /\ rdCyc \in Nat
  /\ evicts \in 0..MaxEvicts /\ hyds \in 0..MaxHyds
  /\ hydFails \in 0..MaxHydFails /\ reads \in 0..MaxReads
  /\ crashes \in 0..MaxCrashes
  /\ badServe \in BOOLEAN /\ badAttr \in BOOLEAN

Init ==
  /\ bytes = "full" /\ ram = FALSE /\ drow = FALSE /\ hyd = FALSE /\ up = TRUE
  /\ ev = 0 /\ hy = "idle" /\ rd = "idle" /\ rdData = "none"
  /\ cyc = 0 /\ rdCyc = 0
  /\ evicts = 0 /\ hyds = 0 /\ hydFails = 0 /\ reads = 0 /\ crashes = 0
  /\ badServe = FALSE /\ badAttr = FALSE

unchangedW == UNCHANGED <<badServe, badAttr>>

(***************************************************************************)
(* The evictor — one commit step at a time, in the configured order.       *)
(* Starting requires eligibility; each step is what the code does          *)
(* between awaits/syscalls.                                                *)
(***************************************************************************)

EvApply(op) ==
  IF op = "row" THEN /\ drow' = TRUE /\ UNCHANGED <<ram, bytes, cyc>>
  ELSE IF op = "mark" THEN /\ ram' = TRUE /\ cyc' = cyc + 1
                           /\ UNCHANGED <<drow, bytes>>
  ELSE /\ bytes' = "stub" /\ UNCHANGED <<ram, drow, cyc>>

EvStep ==
  /\ up
  /\ ev < 3
  /\ (ev = 0 => (bytes = "full" /\ ~ram /\ ~drow /\ hy = "idle"
                 /\ evicts < MaxEvicts))
  /\ EvApply(Order[ev + 1])
  /\ ev' = ev + 1
  /\ evicts' = IF ev = 0 THEN evicts + 1 ELSE evicts
  /\ UNCHANGED <<hyd, up, hy, rd, rdData, rdCyc, hyds, hydFails, reads,
                 crashes>>
  /\ unchangedW

EvDone ==
  /\ ev = 3
  /\ ev' = 0
  /\ UNCHANGED <<bytes, ram, drow, hyd, up, hy, rd, rdData, cyc, rdCyc,
                 evicts, hyds, hydFails, reads, crashes>>
  /\ unchangedW

(***************************************************************************)
(* The hydrator (restore_once): durable flag BEFORE the first byte;        *)
(* bytes land (partial, then complete+fsync), CRC verifies, then rows      *)
(* first and the RAM marker LAST.  Failure truncates back to the stub.     *)
(***************************************************************************)

HyFlag ==
  /\ up /\ hy = "idle" /\ ram /\ drow /\ bytes = "stub" /\ ev = 0
  /\ hyds < MaxHyds
  /\ hyd' = IF HydFlagOn THEN TRUE ELSE hyd
  /\ hy' = "flagged"
  /\ hyds' = hyds + 1
  /\ UNCHANGED <<bytes, ram, drow, up, ev, rd, rdData, cyc, rdCyc, evicts,
                 hydFails, reads, crashes>>
  /\ unchangedW

HyRestore ==
  /\ up /\ hy = "flagged"
  /\ bytes' = "partial"
  /\ hy' = "partial"
  /\ UNCHANGED <<ram, drow, hyd, up, ev, rd, rdData, cyc, rdCyc, evicts,
                 hyds, hydFails, reads, crashes>>
  /\ unchangedW

HyBytesDone ==
  /\ up /\ hy = "partial"
  /\ bytes' = "full"
  /\ hy' = "done"
  /\ UNCHANGED <<ram, drow, hyd, up, ev, rd, rdData, cyc, rdCyc, evicts,
                 hyds, hydFails, reads, crashes>>
  /\ unchangedW

\* Completion order (step 11): durable rows first, RAM marker LAST —
\* the moment the map clears, ops serve, and everything observable is
\* already consistent.  Decomposed exactly there.
HyCommitRows ==
  /\ up /\ hy = "done" /\ drow
  /\ drow' = FALSE /\ hyd' = FALSE
  /\ UNCHANGED <<bytes, ram, up, ev, hy, rd, rdData, cyc, rdCyc, evicts,
                 hyds, hydFails, reads, crashes>>
  /\ unchangedW

HyClearMarker ==
  /\ up /\ hy = "done" /\ ~drow
  /\ ram' = FALSE
  /\ cyc' = cyc + 1
  /\ hy' = "idle"
  /\ UNCHANGED <<bytes, drow, hyd, up, ev, rd, rdData, rdCyc, evicts, hyds,
                 hydFails, reads, crashes>>
  /\ unchangedW

HyFail ==
  /\ up /\ hy \in {"flagged", "partial"}
  /\ hydFails < MaxHydFails
  /\ bytes' = "stub" /\ hyd' = FALSE /\ hy' = "idle"
  /\ hydFails' = hydFails + 1
  /\ UNCHANGED <<ram, drow, up, ev, rd, rdData, cyc, rdCyc, evicts, hyds,
                 reads, crashes>>
  /\ unchangedW

(***************************************************************************)
(* The un-gated lanes.  A consult finding the marker parks (DELAY) — a     *)
(* no-op here.  The reader decomposes consult / pread / serve; the serve   *)
(* re-consults iff PostReadCheck (the bug-3 fix).  GETATTR answers the     *)
(* marker's logical size when marked, else the file's — reporting the      *)
(* stub while unmarked is the witness (bug 4's "empty git object").        *)
(***************************************************************************)

RdConsult ==
  /\ up /\ rd = "idle" /\ ~ram
  /\ reads < MaxReads
  /\ rd' = "checked"
  /\ rdCyc' = cyc          \* sampled BEFORE the consult in the code
  /\ reads' = reads + 1
  /\ UNCHANGED <<bytes, ram, drow, hyd, up, ev, hy, rdData, cyc, evicts,
                 hyds, hydFails, crashes>>
  /\ unchangedW

RdRead ==
  /\ up /\ rd = "checked"
  /\ rdData' = bytes
  /\ rd' = "got"
  /\ UNCHANGED <<bytes, ram, drow, hyd, up, ev, hy, cyc, rdCyc, evicts,
                 hyds, hydFails, reads, crashes>>
  /\ unchangedW

RdServe ==
  /\ up /\ rd = "got"
  /\ IF PostReadCheck /\ (ram \/ (CycleCheck /\ cyc # rdCyc))
       THEN \* the re-consult catches the race: discard, answer DELAY.
            \* Without CycleCheck a COMPLETE evict+hydrate cycle inside
            \* the read window clears the marker before this look — the
            \* counter is the only evidence it happened (the strict
            \* run's first counterexample, and the code fix it forced).
            /\ UNCHANGED badServe
       ELSE badServe' = (badServe \/ rdData # "full")
  /\ rd' = "idle"
  /\ rdData' = "none"
  /\ UNCHANGED <<bytes, ram, drow, hyd, up, ev, hy, cyc, rdCyc, evicts,
                 hyds, hydFails, reads, crashes, badAttr>>

Getattr ==
  /\ up
  /\ badAttr' = (badAttr \/ (~ram /\ bytes # "full"))
  /\ UNCHANGED <<bytes, ram, drow, hyd, up, ev, hy, rd, rdData, cyc, rdCyc,
                 evicts, hyds, hydFails, reads, crashes, badServe>>

(***************************************************************************)
(* Crash and the pre-listener reconciler (evict.rs reconcile + the         *)
(* step-11 hydration arms).  RAM vanishes; durable state and bytes stay;   *)
(* nothing serves until Reconcile has run.                                 *)
(***************************************************************************)

Crash ==
  /\ crashes < MaxCrashes
  /\ up' = FALSE
  /\ ram' = FALSE
  /\ cyc' = IF ram THEN cyc + 1 ELSE cyc   \* the wipe clears markers too
  /\ ev' = 0 /\ hy' = "idle" /\ rd' = "idle" /\ rdData' = "none"
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<bytes, drow, hyd, rdCyc, evicts, hyds, hydFails, reads>>
  /\ unchangedW

Reconcile ==
  /\ ~up
  /\ up' = TRUE
  /\ IF drow /\ hyd
       THEN IF bytes = "full"
              THEN \* restore had completed: commit (rows clear; no marker)
                   /\ drow' = FALSE /\ hyd' = FALSE /\ ram' = FALSE
                   /\ UNCHANGED bytes
              ELSE \* crashed mid-restore: back to the stub, marker re-armed
                   /\ bytes' = "stub" /\ hyd' = FALSE /\ ram' = TRUE
                   /\ UNCHANGED drow
     ELSE IF drow /\ bytes = "full"
       THEN \* C2 half-eviction: row landed, truncate did not — finish it
            /\ bytes' = "stub" /\ ram' = TRUE
            /\ UNCHANGED <<drow, hyd>>
     ELSE IF drow /\ bytes = "partial"
       THEN \* only reachable with NoHydFlag: the flagless partial falls
            \* into the diverged-rollback arm — "local wins" keeps the
            \* PARTIAL as the file (the mutation's loss)
            /\ drow' = FALSE /\ ram' = FALSE
            /\ UNCHANGED <<bytes, hyd>>
     ELSE IF drow
       THEN /\ ram' = TRUE
            /\ UNCHANGED <<bytes, drow, hyd>>
       ELSE /\ UNCHANGED <<bytes, ram, drow, hyd>>
  /\ cyc' = IF ram' # ram THEN cyc + 1 ELSE cyc
  /\ UNCHANGED <<ev, hy, rd, rdData, rdCyc, evicts, hyds, hydFails, reads,
                 crashes>>
  /\ unchangedW

Next ==
  \/ EvStep \/ EvDone
  \/ HyFlag \/ HyRestore \/ HyBytesDone \/ HyCommitRows \/ HyClearMarker
  \/ HyFail
  \/ RdConsult \/ RdRead \/ RdServe \/ Getattr
  \/ Crash \/ Reconcile

\* Protocol machinery is weakly fair; Crash, HyFail and the probes-only
\* Getattr are the environment.
Fairness ==
  /\ WF_vars(EvStep) /\ WF_vars(EvDone)
  /\ WF_vars(HyFlag) /\ WF_vars(HyRestore) /\ WF_vars(HyBytesDone)
  /\ WF_vars(HyCommitRows) /\ WF_vars(HyClearMarker)
  /\ WF_vars(RdConsult) /\ WF_vars(RdRead) /\ WF_vars(RdServe)
  /\ WF_vars(Reconcile)

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* THE INVARIANT both drill bugs violated, and the mutations must          *)
(* re-violate: content served is content published.  Plus the GETATTR      *)
(* face of the same truth (bug 4's visible symptom), and liveness: a       *)
(* marked file eventually has its bytes back (the parked reader's          *)
(* DELAY-retry converges once budgets settle).                             *)
(***************************************************************************)

Inv_NoStubServed  == ~badServe
Inv_TruthfulAttrs == ~badAttr

Inv == TypeOK /\ Inv_NoStubServed /\ Inv_TruthfulAttrs

MarkedEventuallyRestores ==
  []((ram /\ up) => <>(bytes = "full" \/ ~up \/ hyds = MaxHyds))

================================================================================
