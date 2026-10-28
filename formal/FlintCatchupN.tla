---------------------------- MODULE FlintCatchupN ----------------------------
(***************************************************************************)
(* FlintCatchup at N replicas: the §5 catch-up when ANY replica can fail, *)
(* sources are chosen per session among the in-sync replicas, and a       *)
(* source can itself have been rebuilt (spdk-csi-driver/src/catchup.rs).  *)
(*                                                                         *)
(* FlintCatchup.tla fixes R = {S, D}: S never fails and is every          *)
(* session's source.  Its TLAPS proof (FlintCatchupProof.tla) is therefore *)
(* unbounded in writes, epochs and failures but fixed at two replicas.    *)
(* Two places in the code rest on more:                                    *)
(*                                                                         *)
(*   SOURCE SWITCH (pick_source's doc, catchup.rs:936): "Common epochs    *)
(*   make any in-sync replica an equivalent source — the base-inclusive   *)
(*   chain re-copy makes even switching sources between sessions safe."  *)
(*   Cuts are skewed per replica, so S1's epoch k and S2's epoch k can    *)
(*   hold different write sets; the base agreed with S1, the chase copies *)
(*   from S2's lineage.  Here every session (bulk, chase, final delta)    *)
(*   picks ANY covering in-sync source, independently.                    *)
(*                                                                         *)
(*   COVERAGE (select_covering_source, catchup.rs:996): a source rebuilt  *)
(*   from a newer base roots its chain there and cannot reach an older    *)
(*   base (the 2026-07-03 3-replica drill wedge).  The walk tries every   *)
(*   in-sync candidate; none covering licenses the full build (bulk) or   *)
(*   demotes a standby (chase); admission defers.  CoverageWalk = FALSE   *)
(*   is the code before it: pick_source's first candidate only, and a     *)
(*   session it cannot serve fails and retries.                           *)
(*                                                                         *)
(* Because a rebuilt source's present snapshots are NOT its lineage (a    *)
(* full build leaves its old snapshots below the record, outside the new  *)
(* head's chain), lineage is walked by parent links (Lin), as             *)
(* lineage_chain does.  Two stale legs at once, leg-by-leg raid1 writes   *)
(* across N legs, zombies and the detection lag are all in play.  The     *)
(* guards FlintCatchup proved load-bearing at two replicas (Revert,       *)
(* ReapNewer, FullBuildReaps, FinalDelta, FinalCutStrict) are fixed at    *)
(* their shipped values; BaseInclusive, Backoff, PinRetention and         *)
(* CoverageWalk remain knobs.  The forced-stale fallback is              *)
(* FlintLocalLeg's and FlintReplication's (it serves a stale leg; F36     *)
(* keeps the catch-up off a served head).                                 *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANTS
  Reps,          \* the replicas, numbered: e.g. {1, 2, 3}; pick_source
                 \* prefers the lowest-numbered in-sync one (non-consumer
                 \* first, record order: any fixed order)
  Blocks, MaxVer, MaxEpoch, MaxFails, Retain,
  Backoff, BaseInclusive, PinRetention, CoverageWalk,
  SourceMustBeHealthy, \* TRUE = a copy source must be a HEALTHY leg (path ok:
                       \* a configured raid base without errors, or cleanly
                       \* unstaged), not merely record-in_sync; FALSE = shipped
                       \* (in_sync_sources: the record alone)
  ZombieExcluded       \* the zombie premise.  FALSE = FlintCatchup's: a zombie
                       \* writes a leg in its detection lag, still record-
                       \* in_sync (and cut, and a source).  TRUE = design §3:
                       \* the replica co-located with a zombie consumer is
                       \* "treated as contaminated and excluded (stale) until
                       \* hygiene runs there" -- zombie writes land only on a
                       \* record-stale leg before its revert
  , SourceTie          \* TRUE = the admission is tied to the standby's own
                       \* sources: every replica a session copied from since
                       \* the revert must, at admission, be an attached
                       \* in-sync healthy member never marked stale since;
                       \* otherwise the standby is demoted.  FALSE =
                       \* shipped (admission checks only the final source)
  , DemoteAsCode       \* how a standby is demoted (the chase's NoneCovering,
                       \* SourceTie's refusal).  TRUE = today's mark_stale
                       \* (replica_sync.rs:447): reverted_to survives (a base
                       \* equal to it RESUMES with no re-revert, catchup.rs:
                       \* 2201) and `since` is re-stamped (select_base_epoch's
                       \* back-off moves to the demotion: any present recorded
                       \* epoch is eligible).  FALSE = the fix: reverted_to is
                       \* cleared, `since` kept, and the base stays at or below
                       \* the stale episode's FIRST base (everything above it
                       \* on the replica is a copy), as a must_revert flag
                       \* (reverted_to kept: head_not_built stays true for the
                       \* forced-stale guard; a revert clears the flag)
  , FloorFirstOnly     \* control: the floor is the episode's FIRST base only
                       \* (FALSE = the fix: MIN over the episode's reverts)
  , RaidFailsAtDrop    \* TRUE = the raid fails a base at its first dropped
                       \* write (raid1's write-error path; the repro saw the
                       \* raid drop the leg as the stuck write completed);
                       \* FALSE = a base may stay configured after dropping
                       \* writes (the detection lag reaches the raid too)
  , AdmitPaths         \* "stage" (Tier 1), "hot" (Tier 2 hot rejoin), "both"
  , HotFix             \* the hot-rejoin fix, by part (SUBSET {"win", "pad",
                       \* "tie", "reap"}): "win" = the window's source is a CONFIGURED
                       \* base of the live raid read under the quiesce (not
                       \* pick_source's record); "pad" = the landing pad's base
                       \* honours the back-off and the floor (not its chain
                       \* top); "tie" = the window parent, backfill and chase
                       \* sources are checked at the flip; "reap" = the pad's
                       \* revert reaps like §5's (epochs newer than its base;
                       \* for the full build, every retained one), which
                       \* localize() does not.  {} = the code

ASSUME Retain >= 1
ASSUME Reps \subseteq Nat /\ 0 \notin Reps
ASSUME AdmitPaths \in {"stage", "hot", "both"}
ASSUME HotFix \subseteq {"win", "pad", "tie", "reap"}

R      == Reps
Epochs == 1..MaxEpoch
Vers   == 0..MaxVer
Zero   == [x \in Blocks |-> 0]
NoRisk == MaxEpoch + 1
Max(T) == CHOOSE t \in T : \A u \in T : u <= t
Min(T) == CHOOSE t \in T : \A u \in T : t <= u

VARIABLES
  ver, head, hAl, hPar, sPres, sCon, sAl, sPar,
  recorded, nextSeq, cutK, cutTgt, cutDone,
  members, attached,
  path,      \* [R -> {"ok", "failing", "out", "back"}]
  dState,    \* [R -> {"insync", "stale", "standby"}] the record
  mark,      \* [R -> 0..MaxEpoch] last_epoch
  reverted,  \* [R -> -1..MaxEpoch] reverted_to
  sb,        \* [R -> -1..MaxEpoch] the bulk session's selected base
  pin,       \* [R -> 0..MaxEpoch] retention pins
  riskRec,   \* [R -> 0..NoRisk] A2's ghost, per replica
  pend, acked, fails, badAdmit,
  used,      \* [R -> SUBSET R] the sources r's sessions copied from since its revert
  tainted,   \* [R -> BOOLEAN] one of them has been marked stale since
  floor,     \* [R -> -1..MaxEpoch] the stale episode's first revert base
  restamp,   \* [R -> BOOLEAN] `since` was re-stamped by a demotion
  mustRev,   \* [R -> BOOLEAN] the fix's must_revert flag
  hot,       \* [R -> 0..MaxEpoch] the hot-rejoin marker (E_f), 0 = none
  hotSrc,    \* [R -> R \cup {0}] the esnap parent (0 = inline window)
  padTop     \* [R -> 0..MaxEpoch] the landing pad's chain top at the window

vars == <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq,
          cutK, cutTgt, cutDone, members, attached, path, dState, mark,
          reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

Write == [b : Blocks, v : 1..MaxVer, to : SUBSET R, landed : SUBSET R,
          dropped : SUBSET R, recAt : 0..MaxEpoch, cutAt : 1..(MaxEpoch + 1)]

TypeOK ==
  /\ ver \in Vers
  /\ head \in [R -> [Blocks -> Vers]]
  /\ hAl \in [R -> SUBSET Blocks]
  /\ hPar \in [R -> 0..MaxEpoch]
  /\ sPres \in [R -> SUBSET Epochs]
  /\ sCon \in [R -> [Epochs -> [Blocks -> Vers]]]
  /\ sAl \in [R -> [Epochs -> SUBSET Blocks]]
  /\ sPar \in [R -> [Epochs -> 0..MaxEpoch]]
  /\ recorded \in SUBSET Epochs
  /\ nextSeq \in 1..(MaxEpoch + 1)
  /\ cutK \in 0..MaxEpoch
  /\ cutTgt \in SUBSET R /\ cutDone \in SUBSET R
  /\ members \in SUBSET R
  /\ attached \in BOOLEAN
  /\ path \in [R -> {"ok", "failing", "out", "back"}]
  /\ dState \in [R -> {"insync", "stale", "standby"}]
  /\ mark \in [R -> 0..MaxEpoch]
  /\ reverted \in [R -> -1..MaxEpoch]
  /\ sb \in [R -> -1..MaxEpoch]
  /\ pin \in [R -> 0..MaxEpoch]
  /\ riskRec \in [R -> 0..NoRisk]
  /\ pend \subseteq Write
  /\ acked \in [Blocks -> Vers]
  /\ fails \in 0..MaxFails
  /\ badAdmit \in BOOLEAN
  /\ used \in [R -> SUBSET R]
  /\ tainted \in [R -> BOOLEAN]
  /\ floor \in [R -> -1..MaxEpoch]
  /\ restamp \in [R -> BOOLEAN]
  /\ mustRev \in [R -> BOOLEAN]
  /\ hot \in [R -> 0..MaxEpoch]
  /\ hotSrc \in [R -> R \cup {0}]
  /\ padTop \in [R -> 0..MaxEpoch]

Init ==
  /\ ver = 0
  /\ head = [r \in R |-> Zero] /\ hAl = [r \in R |-> {}] /\ hPar = [r \in R |-> 0]
  /\ sPres = [r \in R |-> {}]
  /\ sCon = [r \in R |-> [e \in Epochs |-> Zero]]
  /\ sAl = [r \in R |-> [e \in Epochs |-> {}]]
  /\ sPar = [r \in R |-> [e \in Epochs |-> 0]]
  /\ recorded = {} /\ nextSeq = 1
  /\ cutK = 0 /\ cutTgt = {} /\ cutDone = {}
  /\ members = R /\ attached = TRUE
  /\ path = [r \in R |-> "ok"]
  /\ dState = [r \in R |-> "insync"]
  /\ mark = [r \in R |-> 0]
  /\ reverted = [r \in R |-> -1]
  /\ sb = [r \in R |-> -1]
  /\ pin = [r \in R |-> 0]
  /\ riskRec = [r \in R |-> NoRisk]
  /\ pend = {} /\ acked = Zero /\ fails = 0 /\ badAdmit = FALSE
  /\ used = [r \in R |-> {}] /\ tainted = [r \in R |-> FALSE]
  /\ floor = [r \in R |-> -1] /\ restamp = [r \in R |-> FALSE]
  /\ mustRev = [r \in R |-> FALSE]
  /\ hot = [r \in R |-> 0] /\ hotSrc = [r \in R |-> 0] /\ padTop = [r \in R |-> 0]

(***************************************************************************)
(* Helpers                                                                 *)
(***************************************************************************)

Latest == nextSeq - 1
Pending(x) == \E w \in pend : w.b = x
Reachable(r) == path[r] # "out"
InSyncR == {r \in R : dState[r] = "insync"}
Healthy == {r \in members : path[r] = "ok"}

\* r's live lineage: the snapshots reachable from its head by parent links
\* (lineage_chain).  Parents name older epochs: MaxEpoch steps reach the root.
RECURSIVE Anc(_, _, _)
Anc(r, e, n) == IF e = 0 \/ n = 0 THEN {} ELSE {e} \cup Anc(r, sPar[r][e], n - 1)
Lin(r) == Anc(r, hPar[r], MaxEpoch)

\* A session copying source s's lineage from lo to target onto a head.
CopyElems(s, lo, target) == {e \in Lin(s) : lo <= e /\ e <= target}
Copied(s, lo, target) == {x \in Blocks : \E e \in CopyElems(s, lo, target) : x \in sAl[s][e]}
AfterCopy(start, s, lo, target) ==
  [x \in Blocks |->
     LET hits == {e \in CopyElems(s, lo, target) : x \in sAl[s][e]}
     IN IF hits = {} THEN start[x] ELSE sCon[s][Max(hits)][x]]

SnapHead(r, e) ==
  /\ sPres' = [sPres EXCEPT ![r] = @ \cup {e}]
  /\ sCon' = [sCon EXCEPT ![r][e] = head[r]]
  /\ sAl' = [sAl EXCEPT ![r][e] = hAl[r]]
  /\ sPar' = [sPar EXCEPT ![r][e] = hPar[r]]
  /\ hPar' = [hPar EXCEPT ![r] = e]
  /\ hAl' = [hAl EXCEPT ![r] = {}]

\* Source selection (in_sync_sources / select_covering_source).  The walk
\* returns the first covering candidate in preference order; the model lets
\* a session take ANY covering one, which contains every order the code can
\* see as replicas fail and return — and so every source switch.
Sources(r) == {s \in R \ {r} : dState[s] = "insync" /\ Reachable(s)
                                 /\ (SourceMustBeHealthy => path[s] = "ok")}
First(S) == Min(S)
Covers(s, t, b) == t \in Lin(s) /\ (b <= 0 \/ b \in Lin(s))
CoverSet(r, t, b) ==
  IF CoverageWalk THEN {s \in Sources(r) : Covers(s, t, b)}
  ELSE IF Sources(r) # {} /\ Covers(First(Sources(r)), t, b)
       THEN {First(Sources(r))} ELSE {}

(***************************************************************************)
(* Data plane                                                              *)
(***************************************************************************)

Submit(x) ==
  /\ attached /\ members # {}
  /\ ver < MaxVer
  /\ ~Pending(x)
  /\ ver' = ver + 1
  /\ pend' = pend \cup {[b |-> x, v |-> ver + 1, to |-> members, landed |-> {},
                         dropped |-> {}, recAt |-> Latest, cutAt |-> nextSeq]}
  /\ riskRec' = [r \in R |-> IF r \in members THEN riskRec[r]
                                               ELSE Min({riskRec[r], Latest})]
  /\ UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq,
                 cutK, cutTgt, cutDone, members, attached, path, dState, mark,
                 reverted, sb, pin, acked, fails, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

Land(w, r) ==
  /\ w \in pend
  /\ r \in w.to \ (w.landed \cup w.dropped)
  /\ path[r] \in {"ok", "failing"}
  /\ head' = [head EXCEPT ![r][w.b] = w.v]
  /\ hAl' = [hAl EXCEPT ![r] = @ \cup {w.b}]
  /\ pend' = (pend \ {w}) \cup {[w EXCEPT !.landed = @ \cup {r}]}
  /\ UNCHANGED <<ver, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq, cutK,
                 cutTgt, cutDone, members, attached, path, dState, mark,
                 reverted, sb, pin, acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

Drop(w, r) ==
  /\ w \in pend
  /\ r \in w.to \ (w.landed \cup w.dropped)
  /\ path[r] \in {"failing", "out", "back"}
  /\ pend' = (pend \ {w}) \cup {[w EXCEPT !.dropped = @ \cup {r}]}
  /\ riskRec' = [riskRec EXCEPT ![r] = Min({@, w.recAt})]
  \* the raid's base list, not the leg's reachability: the scheduler and the
  \* catch-up still reach a dropped leg over the control plane
  /\ members' = IF RaidFailsAtDrop THEN members \ {r} ELSE members
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, attached, path,
                 dState, mark, reverted, sb, pin, acked, fails, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

Complete(w) ==
  /\ w \in pend
  /\ w.landed \cup w.dropped = w.to
  /\ pend' = pend \ {w}
  /\ acked' = IF w.landed # {} THEN [acked EXCEPT ![w.b] = w.v] ELSE acked
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, path,
                 dState, mark, reverted, sb, pin, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

Zombie(r, x) ==
  /\ IF ZombieExcluded THEN dState[r] = "stale" /\ reverted[r] = -1
                       ELSE path[r] = "failing"
  /\ ver < MaxVer
  /\ ~Pending(x)
  /\ ver' = ver + 1
  /\ head' = [head EXCEPT ![r][x] = ver + 1]
  /\ hAl' = [hAl EXCEPT ![r] = @ \cup {x}]
  /\ riskRec' = [riskRec EXCEPT ![r] = Min({@, Latest})]
  /\ UNCHANGED <<hPar, sPres, sCon, sAl, sPar, recorded, nextSeq, cutK, cutTgt,
                 cutDone, members, attached, path, dState, mark, reverted, sb,
                 pin, pend, acked, fails, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

(***************************************************************************)
(* Failure and return (a healthy member always remains: raid1 cannot      *)
(* survive losing every leg, and the model does not ask it to)            *)
(***************************************************************************)

FailBegin(r) ==
  /\ path[r] = "ok" /\ r \in members /\ dState[r] = "insync"
  /\ Healthy \ {r} # {}
  /\ fails < MaxFails
  /\ path' = [path EXCEPT ![r] = "failing"]
  /\ fails' = fails + 1
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, dState,
                 mark, reverted, sb, pin, pend, acked, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

FaultOut(r) ==
  /\ path[r] = "failing"
  /\ path' = [path EXCEPT ![r] = "out"]
  /\ members' = members \ {r}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, attached, dState, mark,
                 reverted, sb, pin, pend, acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

MarkStale(r) ==
  /\ r \notin members /\ dState[r] = "insync" /\ path[r] \in {"out", "back"}
  /\ dState' = [dState EXCEPT ![r] = "stale"]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, path,
                 mark, reverted, sb, pin, pend, acked, fails, riskRec, badAdmit>>
  /\ tainted' = [q \in R |-> tainted[q] \/ r \in used[q]]
  /\ floor' = [floor EXCEPT ![r] = -1]
  /\ restamp' = [restamp EXCEPT ![r] = FALSE]
  /\ UNCHANGED <<used, mustRev, hot, hotSrc, padTop>>

Return(r) ==
  /\ path[r] = "out"
  /\ path' = [path EXCEPT ![r] = "back"]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, dState,
                 mark, reverted, sb, pin, pend, acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

(***************************************************************************)
(* The epoch scheduler                                                     *)
(***************************************************************************)

CutBegin ==
  /\ attached /\ cutK = 0
  /\ nextSeq <= MaxEpoch
  /\ \A w \in pend : w.cutAt >= nextSeq
  /\ cutK' = nextSeq
  /\ cutTgt' = InSyncR
  /\ cutDone' = {}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, members, attached, path, dState, mark, reverted, sb,
                 pin, pend, acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

CutOne(r) ==
  /\ cutK # 0
  /\ r \in cutTgt \ cutDone
  /\ Reachable(r)
  /\ IF cutK \in sPres[r] THEN UNCHANGED <<sPres, sCon, sAl, sPar, hPar, hAl>>
                          ELSE SnapHead(r, cutK)
  /\ cutDone' = cutDone \cup {r}
  /\ UNCHANGED <<ver, head, recorded, nextSeq, cutK, cutTgt, members, attached,
                 path, dState, mark, reverted, sb, pin, pend, acked, fails,
                 riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

CutRecord ==
  /\ cutK # 0 /\ cutDone = cutTgt
  /\ recorded' = recorded \cup {cutK}
  /\ nextSeq' = nextSeq + 1
  /\ mark' = [r \in R |-> IF r \in cutTgt THEN cutK ELSE mark[r]]
  /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, members,
                 attached, path, dState, reverted, sb, pin, pend, acked, fails,
                 riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

CutAbort ==
  /\ cutK # 0
  /\ \E r \in cutTgt \ cutDone : ~Reachable(r)
  /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, members, attached, path, dState, mark, reverted, sb,
                 pin, pend, acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

\* retire_epochs: never an epoch at or above any replica's pin.
Retire ==
  /\ Cardinality(recorded) > Retain
  /\ LET e == Min(recorded) IN
       /\ PinRetention => \A r \in R : pin[r] # 0 => e < pin[r]
       /\ recorded' = recorded \ {e}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, nextSeq, cutK,
                 cutTgt, cutDone, members, attached, path, dState, mark,
                 reverted, sb, pin, pend, acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

Children(r, e) == {c \in sPres[r] : sPar[r][c] = e}
GC(r, e) ==
  /\ recorded # {} /\ e \in sPres[r] /\ e < Min(recorded) /\ hot[r] = 0
  /\ Reachable(r)
  /\ LET kids == Children(r, e)
         headKid == hPar[r] = e
     IN /\ Cardinality(kids) + (IF headKid THEN 1 ELSE 0) <= 1
        /\ sPres' = [sPres EXCEPT ![r] = @ \ {e}]
        /\ IF headKid
             THEN /\ hAl' = [hAl EXCEPT ![r] = @ \cup sAl[r][e]]
                  /\ hPar' = [hPar EXCEPT ![r] = sPar[r][e]]
                  /\ UNCHANGED <<sAl, sPar>>
             ELSE /\ sAl' = [sAl EXCEPT ![r] = [c \in Epochs |->
                               IF c \in kids THEN sAl[r][c] \cup sAl[r][e] ELSE sAl[r][c]]]
                  /\ sPar' = [sPar EXCEPT ![r] = [c \in Epochs |->
                               IF c \in kids THEN sPar[r][e] ELSE sPar[r][c]]]
                  /\ UNCHANGED <<hAl, hPar>>
  /\ UNCHANGED <<ver, head, sCon, recorded, nextSeq, cutK, cutTgt, cutDone,
                 members, attached, path, dState, mark, reverted, sb, pin, pend,
                 acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

(***************************************************************************)
(* The catch-up, per stale replica r                                       *)
(***************************************************************************)

BaseCandidates(r) == {e \in recorded \cap sPres[r] :
                        /\ Backoff /\ ~restamp[r] => e <= riskRec[r]
                        /\ ~DemoteAsCode /\ floor[r] # -1 => e <= floor[r]}

\* select_base_epoch + select_covering_source: any candidate or none (T_back
\* is wall-clock); a base no in-sync lineage covers demotes the session to
\* the full build when the walk is on — pick_source-only keeps it, and the
\* copy then fails and re-selects.
SelectBase(r) ==
  /\ dState[r] = "stale" /\ path[r] = "back" /\ sb[r] = -1 /\ recorded # {} /\ hot[r] = 0
  /\ cutK = 0
  /\ Sources(r) # {}
  /\ \E b \in BaseCandidates(r) \cup {0} :
       LET b2 == IF CoverageWalk /\ b # 0 /\ CoverSet(r, Max(recorded), b) = {}
                   THEN 0 ELSE b
       IN /\ sb' = [sb EXCEPT ![r] = b2]
          \* catchup.rs:2201: the head is "write-virgin" for reverted_to, so a
          \* base equal to it resumes; any other base re-reverts
          /\ reverted' = [reverted EXCEPT ![r] = IF @ = b2 /\ ~mustRev[r] THEN @ ELSE -1]
  /\ pin' = [pin EXCEPT ![r] = IF ~PinRetention THEN pin[r]
                               ELSE IF sb'[r] = 0 THEN Min(recorded) ELSE sb'[r]]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, path,
                 dState, mark, riskRec, pend, acked, fails, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

\* §5 step 0 + F75, or the full build's empty head with its reap (shipped).
RevertD(r) ==
  /\ dState[r] = "stale" /\ path[r] = "back" /\ sb[r] # -1 /\ reverted[r] = -1 /\ hot[r] = 0
  /\ IF sb[r] = 0
       THEN /\ head' = [head EXCEPT ![r] = Zero]
            /\ hAl' = [hAl EXCEPT ![r] = {}]
            /\ hPar' = [hPar EXCEPT ![r] = 0]
            /\ sPres' = [sPres EXCEPT ![r] = {e \in @ : e < Min(recorded)}]
       ELSE /\ sb[r] \in sPres[r]
            /\ head' = [head EXCEPT ![r] = sCon[r][sb[r]]]
            /\ hAl' = [hAl EXCEPT ![r] = {}]
            /\ hPar' = [hPar EXCEPT ![r] = sb[r]]
            /\ sPres' = [sPres EXCEPT ![r] = {e \in @ : e <= sb[r]}]
  /\ reverted' = [reverted EXCEPT ![r] = sb[r]]
  /\ UNCHANGED <<ver, sCon, sAl, sPar, recorded, nextSeq, cutK, cutTgt,
                 cutDone, members, attached, path, dState, mark, sb, pin,
                 pend, acked, fails, riskRec, badAdmit>>
  /\ used' = [used EXCEPT ![r] = {}]
  /\ tainted' = [tainted EXCEPT ![r] = FALSE]
  /\ floor' = [floor EXCEPT ![r] = IF @ = -1 THEN sb[r]
                                   ELSE IF FloorFirstOnly THEN @ ELSE Min({@, sb[r]})]
  /\ mustRev' = [mustRev EXCEPT ![r] = FALSE]
  /\ UNCHANGED <<restamp, hot, hotSrc, padTop>>

\* copy_chain_and_align onto r's head from source s's lineage, then align.
CopyAlign(r, s, from, target) ==
  LET lo == IF BaseInclusive \/ from = 0 THEN from ELSE from + 1
      newHead == AfterCopy(head[r], s, lo, target)
      newAl == hAl[r] \cup Copied(s, lo, target)
  IN /\ head' = [head EXCEPT ![r] = newHead]
     /\ IF from = target \/ target \in sPres[r]
          THEN /\ hAl' = [hAl EXCEPT ![r] = newAl]
               /\ UNCHANGED <<sPres, sCon, sAl, sPar, hPar>>
          ELSE /\ sPres' = [sPres EXCEPT ![r] = @ \cup {target}]
               /\ sCon' = [sCon EXCEPT ![r][target] = newHead]
               /\ sAl' = [sAl EXCEPT ![r][target] = newAl]
               /\ sPar' = [sPar EXCEPT ![r][target] = hPar[r]]
               /\ hPar' = [hPar EXCEPT ![r] = target]
               /\ hAl' = [hAl EXCEPT ![r] = {}]

BulkCopy(r) ==
  /\ dState[r] = "stale" /\ path[r] = "back" /\ reverted[r] # -1 /\ sb[r] = reverted[r]
  /\ hot[r] = 0
  /\ recorded # {} /\ cutK = 0
  /\ LET t == Max(recorded) IN
       \E s \in CoverSet(r, t, sb[r]) :
         /\ CopyAlign(r, s, sb[r], t)
         /\ used' = [used EXCEPT ![r] = @ \cup {s}]
         /\ mark' = [mark EXCEPT ![r] = t]
         /\ pin' = [pin EXCEPT ![r] = IF PinRetention THEN t ELSE pin[r]]
  /\ dState' = [dState EXCEPT ![r] = "standby"]
  /\ sb' = [sb EXCEPT ![r] = -1]
  /\ UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, members,
                 attached, path, reverted, pend, acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

\* A session that cannot run drops its selection: the base gone from r, or
\* no source (the walk's, or pick_source's one) covers it now.
SessionFail(r) ==
  /\ dState[r] = "stale" /\ sb[r] # -1 /\ hot[r] = 0
  /\ \/ sb[r] > 0 /\ reverted[r] = -1 /\ sb[r] \notin sPres[r]
     \/ recorded # {} /\ CoverSet(r, Max(recorded), sb[r]) = {}
  /\ sb' = [sb EXCEPT ![r] = -1]
  /\ reverted' = [reverted EXCEPT ![r] = -1]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, path,
                 dState, mark, pin, pend, acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

\* A standby's demotion (DemoteAsCode: today's mark_stale; else the fix).
Demote(r) ==
  IF DemoteAsCode
    THEN /\ restamp' = [restamp EXCEPT ![r] = TRUE]
         /\ UNCHANGED <<reverted, mustRev>>
    ELSE /\ mustRev' = [mustRev EXCEPT ![r] = TRUE]
         /\ UNCHANGED <<reverted, restamp>>

\* chase_standby: base-inclusive from the mark, from any covering source;
\* none covering demotes the standby to stale (the bulk path owns it).
Chase(r) ==
  /\ dState[r] = "standby" /\ path[r] = "back" /\ cutK = 0 /\ hot[r] = 0
  /\ recorded # {} /\ Max(recorded) > mark[r]
  /\ Sources(r) # {}
  /\ LET t == Max(recorded) IN
       IF CoverSet(r, t, mark[r]) # {}
         THEN /\ \E s \in CoverSet(r, t, mark[r]) :
                   /\ CopyAlign(r, s, mark[r], t)
                   /\ used' = [used EXCEPT ![r] = @ \cup {s}]
                   /\ mark' = [mark EXCEPT ![r] = t]
                   /\ pin' = [pin EXCEPT ![r] = IF PinRetention THEN t ELSE pin[r]]
              /\ UNCHANGED <<dState, reverted, restamp, mustRev>>
         ELSE /\ CoverageWalk
              /\ dState' = [dState EXCEPT ![r] = "stale"]
              /\ Demote(r)
              /\ UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, mark, pin, used>>
  /\ UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, members,
                 attached, path, sb, pend, acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<tainted, floor, hot, hotSrc, padTop>>

(***************************************************************************)
(* The consumer                                                            *)
(***************************************************************************)

Unstage ==
  /\ attached /\ pend = {} /\ cutK = 0
  /\ \A q \in R : hot[q] = 0     \* no consumer restart mid hot rejoin (not modelled)
  /\ attached' = FALSE
  /\ members' = {}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, path, dState, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

\* NodeStage with no admission (or deferring one): the healthy in-sync legs.
Stage ==
  /\ ~attached
  /\ {r \in R : dState[r] = "insync" /\ path[r] = "ok"} # {}
  /\ attached' = TRUE
  /\ members' = {r \in R : dState[r] = "insync" /\ path[r] = "ok"}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, path, dState, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit>>
  /\ UNCHANGED <<used, tainted, floor, restamp, mustRev, hot, hotSrc, padTop>>

\* NodeStage admitting standby r (admit_one_standby): a covering attached
\* source s, the strict final cut on every attached in-sync replica, one
\* more base-inclusive session from the mark through it (s's head IS the
\* final epoch: no writer), in_sync, then the raid.
StageAdmit(r) ==
  /\ AdmitPaths \in {"stage", "both"}
  /\ ~attached /\ cutK = 0 /\ hot[r] = 0
  /\ dState[r] = "standby" /\ path[r] = "back"
  /\ nextSeq <= MaxEpoch /\ recorded # {}
  /\ LET k == nextSeq
         T == {t \in R : dState[t] = "insync" /\ path[t] = "ok"}
     IN /\ T # {}
        /\ SourceTie => ~tainted[r] /\ used[r] \subseteq T
        /\ \A t \in T : k \notin sPres[t]          \* the strict final cut
        /\ k \notin sPres[r]
        /\ \E s \in CoverSet(r, Max(recorded), mark[r]) \cap T :
             LET lineS == Lin(s) \cup {k}
                 elems == {e \in lineS : (IF BaseInclusive THEN mark[r] ELSE mark[r] + 1) <= e /\ e <= k}
                 alOf(e) == IF e = k THEN hAl[s] ELSE sAl[s][e]
                 conOf(e) == IF e = k THEN head[s] ELSE sCon[s][e]
                 dHead == [x \in Blocks |->
                            LET hits == {e \in elems : x \in alOf(e)}
                            IN IF hits = {} THEN head[r][x] ELSE conOf(Max(hits))[x]]
                 dAl == hAl[r] \cup {x \in Blocks : \E e \in elems : x \in alOf(e)}
             IN /\ sPres' = [q \in R |-> IF q \in T \cup {r} THEN sPres[q] \cup {k} ELSE sPres[q]]
                /\ sCon' = [q \in R |-> IF q \in T THEN [sCon[q] EXCEPT ![k] = head[q]]
                                        ELSE IF q = r THEN [sCon[q] EXCEPT ![k] = dHead]
                                        ELSE sCon[q]]
                /\ sAl' = [q \in R |-> IF q \in T THEN [sAl[q] EXCEPT ![k] = hAl[q]]
                                       ELSE IF q = r THEN [sAl[q] EXCEPT ![k] = dAl]
                                       ELSE sAl[q]]
                /\ sPar' = [q \in R |-> IF q \in T \cup {r} THEN [sPar[q] EXCEPT ![k] = hPar[q]]
                                        ELSE sPar[q]]
                /\ hPar' = [q \in R |-> IF q \in T \cup {r} THEN k ELSE hPar[q]]
                /\ hAl' = [q \in R |-> IF q \in T \cup {r} THEN {} ELSE hAl[q]]
                /\ head' = [head EXCEPT ![r] = dHead]
                /\ badAdmit' = (badAdmit \/ dHead # head[s])
        /\ recorded' = recorded \cup {k}
        /\ nextSeq' = nextSeq + 1
        /\ mark' = [q \in R |-> IF q \in T \cup {r} THEN k ELSE mark[q]]
        /\ members' = T \cup {r}
  /\ dState' = [dState EXCEPT ![r] = "insync"]
  /\ reverted' = [reverted EXCEPT ![r] = -1]
  /\ path' = [path EXCEPT ![r] = "ok"]
  /\ pin' = [pin EXCEPT ![r] = 0]
  /\ attached' = TRUE
  /\ riskRec' = [riskRec EXCEPT ![r] = NoRisk]
  /\ used' = [used EXCEPT ![r] = {}]
  /\ tainted' = [tainted EXCEPT ![r] = FALSE]
  /\ floor' = [floor EXCEPT ![r] = -1]
  /\ restamp' = [restamp EXCEPT ![r] = FALSE]
  /\ mustRev' = [mustRev EXCEPT ![r] = FALSE]
  /\ UNCHANGED <<ver, cutK, cutTgt, cutDone, sb, pend, acked, fails, hot, hotSrc, padTop>>

\* SourceTie's refusal: a source the standby copied from is not an attached
\* in-sync healthy member now, or was marked stale since.  The standby goes
\* back to stale (Demote: as today's mark_stale, or the fix).
AdmitRefuse(r) ==
  /\ SourceTie /\ AdmitPaths \in {"stage", "both"}
  /\ ~attached /\ cutK = 0 /\ hot[r] = 0
  /\ dState[r] = "standby"
  /\ tainted[r] \/ ~(used[r] \subseteq {t \in R : dState[t] = "insync" /\ path[t] = "ok"})
  /\ dState' = [dState EXCEPT ![r] = "stale"]
  /\ Demote(r)
  /\ used' = [used EXCEPT ![r] = {}]
  /\ tainted' = [tainted EXCEPT ![r] = FALSE]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, path, mark,
                 sb, pin, pend, acked, fails, riskRec, badAdmit, floor, hot, hotSrc,
                 padTop>>

(***************************************************************************)
(* Tier 2: hot rejoin into the LIVE raid (hot_rejoin.rs, design §7).      *)
(* The esnap window quiesces the raid, cuts E_f strictly on every record-  *)
(* in_sync survivor, esnap-clones a NEW head from the window source's E_f  *)
(* and adds it with skip_rebuild: the leg serves at once, reading its non- *)
(* local clusters through the remote E_f.  Localization later replays the  *)
(* §5 chain onto the OLD head (the landing pad) from a backfill source,    *)
(* snapshots it as the local E_f and re-roots the head onto it: the        *)
(* head's non-local clusters switch from the remote E_f to the pad.  The   *)
(* inline window (a small-delta standby) equalizes the leg itself to E_f   *)
(* under the quiesce and adds it; promotion later flips the record.        *)
(* localize() reverts the pad with revert_head / revert_head_to_empty and  *)
(* reaps nothing (catchup_stale reaps at its revert: ReapNewer,           *)
(* FullBuildReaps).                                                        *)
(* Not modelled: crash points (the marker's reconcile arms), consumer      *)
(* restarts or leg failures mid rejoin, the resume shortcut that skips the *)
(* backfill when a local E_f exists.                                       *)
(***************************************************************************)

HotOn == AdmitPaths \in {"hot", "both"}
NoHot == \A q \in R : hot[q] = 0
\* the window's source: pick_source (the record), or with the fix a
\* configured base of the live raid read under the quiesce
WinSources(Sv) == IF "win" \in HotFix THEN {s \in Sv : s \in members}
                            ELSE {s \in Sv : Reachable(s)}
\* the tie at the flip, hot-rejoin flavour: configured bases of the live raid
HotTieOK(r, extra) ==
  ~tainted[r] /\ (used[r] \cup extra) \subseteq {t \in R : dState[t] = "insync" /\ t \in members}

HotWindow(r) ==
  /\ HotOn /\ attached /\ cutK = 0 /\ pend = {} /\ NoHot
  /\ dState[r] \in {"stale", "standby"} /\ path[r] = "back" /\ sb[r] = -1
  /\ recorded # {} /\ nextSeq <= MaxEpoch
  /\ LET k == nextSeq
         Sv == InSyncR
     IN /\ Sv # {}
        /\ \A t \in Sv : Reachable(t) /\ k \notin sPres[t]      \* cut_ef_strict
        /\ \E s \in WinSources(Sv) :
             /\ sPres' = [q \in R |-> IF q \in Sv THEN sPres[q] \cup {k} ELSE sPres[q]]
             /\ sCon' = [q \in R |-> IF q \in Sv THEN [sCon[q] EXCEPT ![k] = head[q]] ELSE sCon[q]]
             /\ sAl' = [q \in R |-> IF q \in Sv THEN [sAl[q] EXCEPT ![k] = hAl[q]] ELSE sAl[q]]
             /\ sPar' = [q \in R |-> IF q \in Sv THEN [sPar[q] EXCEPT ![k] = hPar[q]] ELSE sPar[q]]
             /\ hPar' = [q \in R |-> IF q \in Sv THEN k ELSE IF q = r THEN 0 ELSE hPar[q]]
             /\ hAl' = [q \in R |-> IF q \in Sv \cup {r} THEN {} ELSE hAl[q]]
             /\ head' = [head EXCEPT ![r] = head[s]]               \* reads via the remote E_f
             /\ hotSrc' = [hotSrc EXCEPT ![r] = s]
             /\ used' = [used EXCEPT ![r] = @ \cup {s}]
        /\ recorded' = recorded \cup {k}
        /\ nextSeq' = nextSeq + 1
        /\ mark' = [q \in R |-> IF q \in Sv THEN k ELSE mark[q]]
        /\ hot' = [hot EXCEPT ![r] = k]
  /\ padTop' = [padTop EXCEPT ![r] = hPar[r]]
  /\ members' = members \cup {r}
  /\ path' = [path EXCEPT ![r] = "ok"]
  /\ dState' = [dState EXCEPT ![r] = "standby"]                  \* the flip, with the marker
  /\ UNCHANGED <<ver, cutK, cutTgt, cutDone, attached, reverted, sb, pin, pend,
                 acked, fails, riskRec, badAdmit, tainted, floor, restamp, mustRev>>

\* The flip's bookkeeping: in_sync, every per-episode field cleared.
HotDone(r, k) ==
  /\ dState' = [dState EXCEPT ![r] = "insync"]
  /\ mark' = [mark EXCEPT ![r] = k]
  /\ reverted' = [reverted EXCEPT ![r] = -1]
  /\ floor' = [floor EXCEPT ![r] = -1]
  /\ used' = [used EXCEPT ![r] = {}]
  /\ tainted' = [tainted EXCEPT ![r] = FALSE]
  /\ mustRev' = [mustRev EXCEPT ![r] = FALSE]
  /\ restamp' = [restamp EXCEPT ![r] = FALSE]
  /\ riskRec' = [riskRec EXCEPT ![r] = NoRisk]
  /\ pin' = [pin EXCEPT ![r] = 0]
  /\ hot' = [hot EXCEPT ![r] = 0]
  /\ hotSrc' = [hotSrc EXCEPT ![r] = 0]

\* localize(): the pad's base (the code: its chain top when an epoch below
\* E_f, else the oldest recorded epoch present, else the full build; the
\* fix: the back-off and the floor), a covering backfill source, the base-
\* inclusive replay, the local E_f, the re-root.
HotLocalize(r) ==
  /\ hot[r] # 0 /\ hotSrc[r] # 0 /\ r \in members
  /\ LET k == hot[r]
         own == {e \in sPres[r] : e < k}
         cands == IF "pad" \in HotFix
                    THEN {e \in recorded \cap own :
                            /\ Backoff /\ ~restamp[r] => e <= riskRec[r]
                            /\ floor[r] # -1 => e <= floor[r]} \cup {0}
                    ELSE {IF padTop[r] \in own THEN padTop[r]
                          ELSE IF recorded \cap own # {} THEN Min(recorded \cap own) ELSE 0}
     IN \E b \in cands : \E s2 \in Sources(r) :
          /\ Covers(s2, k, b)
          /\ "tie" \in HotFix => HotTieOK(r, {s2})
          /\ LET lo == IF BaseInclusive \/ b = 0 THEN b ELSE b + 1
                 L == AfterCopy(IF b = 0 THEN Zero ELSE sCon[r][b], s2, lo, k)
             IN /\ sPres' = [sPres EXCEPT ![r] =
                              (IF "reap" \notin HotFix THEN @
                               ELSE IF b = 0 THEN {e \in @ : e < Min(recorded)}
                               ELSE {e \in @ : e <= b}) \cup {k}]
                /\ sCon' = [sCon EXCEPT ![r][k] = L]
                /\ sAl' = [sAl EXCEPT ![r][k] = Copied(s2, lo, k)]
                /\ sPar' = [sPar EXCEPT ![r][k] = b]
                /\ head' = [head EXCEPT ![r] = [x \in Blocks |->
                              IF x \in hAl[r] THEN head[r][x] ELSE L[x]]]
                /\ hPar' = [hPar EXCEPT ![r] = k]
          /\ HotDone(r, k)
  /\ UNCHANGED <<ver, hAl, recorded, nextSeq, cutK, cutTgt, cutDone, members,
                 attached, path, sb, pend, acked, fails, badAdmit, padTop>>

\* The fix's refusal at the flip: the leg leaves the raid (a degrade) and is
\* demoted; the code instead waits for the leg-gone reconcile arm.
HotRefuse(r) ==
  /\ "tie" \in HotFix
  /\ hot[r] # 0 /\ r \in members
  /\ ~HotTieOK(r, {})
  /\ members' = members \ {r}
  /\ path' = [path EXCEPT ![r] = "back"]
  /\ dState' = [dState EXCEPT ![r] = "stale"]
  /\ Demote(r)
  /\ used' = [used EXCEPT ![r] = {}]
  /\ tainted' = [tainted EXCEPT ![r] = FALSE]
  /\ hot' = [hot EXCEPT ![r] = 0]
  /\ hotSrc' = [hotSrc EXCEPT ![r] = 0]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq,
                 cutK, cutTgt, cutDone, attached, mark, sb, pin, pend, acked,
                 fails, riskRec, badAdmit, floor, padTop>>

\* The esnap parent's node is lost before localization: the head's reads
\* fail, the raid drops the leg, the leg-gone arm demotes it.
HotParentLost(r) ==
  /\ hot[r] # 0 /\ hotSrc[r] # 0 /\ r \in members
  /\ path[hotSrc[r]] \in {"failing", "out"}
  /\ members' = members \ {r}
  /\ path' = [path EXCEPT ![r] = "back"]
  /\ dState' = [dState EXCEPT ![r] = "stale"]
  /\ Demote(r)
  /\ used' = [used EXCEPT ![r] = {}]
  /\ tainted' = [tainted EXCEPT ![r] = FALSE]
  /\ hot' = [hot EXCEPT ![r] = 0]
  /\ hotSrc' = [hotSrc EXCEPT ![r] = 0]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq,
                 cutK, cutTgt, cutDone, attached, mark, sb, pin, pend, acked,
                 fails, riskRec, badAdmit, floor, padTop>>

\* window_inline(): a chased standby is equalized to E_f under the quiesce
\* (base-inclusive from its mark, from the window source) and added.
HotInline(r) ==
  /\ HotOn /\ attached /\ cutK = 0 /\ pend = {} /\ NoHot
  /\ dState[r] = "standby" /\ path[r] = "back"
  /\ recorded # {} /\ nextSeq <= MaxEpoch
  /\ LET k == nextSeq
         Sv == InSyncR
     IN /\ Sv # {}
        /\ \A t \in Sv : Reachable(t) /\ k \notin sPres[t]
        /\ k \notin sPres[r]
        /\ \E s \in WinSources(Sv) :
             /\ mark[r] <= 0 \/ mark[r] \in Lin(s)
             /\ "tie" \in HotFix => HotTieOK(r, {s})
             /\ LET elems == {e \in Lin(s) \cup {k} :
                                (IF BaseInclusive THEN mark[r] ELSE mark[r] + 1) <= e /\ e <= k}
                    alOf(e) == IF e = k THEN hAl[s] ELSE sAl[s][e]
                    conOf(e) == IF e = k THEN head[s] ELSE sCon[s][e]
                    dHead == [x \in Blocks |->
                               LET hits == {e \in elems : x \in alOf(e)}
                               IN IF hits = {} THEN head[r][x] ELSE conOf(Max(hits))[x]]
                    dAl == hAl[r] \cup {x \in Blocks : \E e \in elems : x \in alOf(e)}
                IN /\ sPres' = [q \in R |-> IF q \in Sv \cup {r} THEN sPres[q] \cup {k} ELSE sPres[q]]
                   /\ sCon' = [q \in R |-> IF q \in Sv THEN [sCon[q] EXCEPT ![k] = head[q]]
                                           ELSE IF q = r THEN [sCon[q] EXCEPT ![k] = dHead]
                                           ELSE sCon[q]]
                   /\ sAl' = [q \in R |-> IF q \in Sv THEN [sAl[q] EXCEPT ![k] = hAl[q]]
                                          ELSE IF q = r THEN [sAl[q] EXCEPT ![k] = dAl]
                                          ELSE sAl[q]]
                   /\ sPar' = [q \in R |-> IF q \in Sv \cup {r} THEN [sPar[q] EXCEPT ![k] = hPar[q]]
                                           ELSE sPar[q]]
                   /\ hPar' = [q \in R |-> IF q \in Sv \cup {r} THEN k ELSE hPar[q]]
                   /\ hAl' = [q \in R |-> IF q \in Sv \cup {r} THEN {} ELSE hAl[q]]
                   /\ head' = [head EXCEPT ![r] = dHead]
                   /\ used' = [used EXCEPT ![r] = @ \cup {s}]
        /\ recorded' = recorded \cup {k}
        /\ nextSeq' = nextSeq + 1
        /\ mark' = [q \in R |-> IF q \in Sv \cup {r} THEN k ELSE mark[q]]
        /\ hot' = [hot EXCEPT ![r] = k]
  /\ members' = members \cup {r}
  /\ path' = [path EXCEPT ![r] = "ok"]
  /\ UNCHANGED <<ver, cutK, cutTgt, cutDone, attached, dState, reverted, sb, pin,
                 pend, acked, fails, riskRec, badAdmit, tainted, floor, restamp,
                 mustRev, hotSrc, padTop>>

\* localize() for an inline leg: promotion only.
HotPromote(r) ==
  /\ hot[r] # 0 /\ hotSrc[r] = 0 /\ r \in members
  /\ HotDone(r, hot[r])
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq,
                 cutK, cutTgt, cutDone, members, attached, path, sb, pend, acked,
                 fails, badAdmit, padTop>>

Next ==
  \/ \E x \in Blocks : Submit(x)
  \/ \E w \in pend : Complete(w) \/ \E r \in R : Land(w, r) \/ Drop(w, r)
  \/ \E r \in R : \E x \in Blocks : Zombie(r, x)
  \/ \E r \in R : FailBegin(r) \/ FaultOut(r) \/ MarkStale(r) \/ Return(r)
  \/ CutBegin \/ CutRecord \/ CutAbort \/ Retire
  \/ \E r \in R : CutOne(r) \/ \E e \in Epochs : GC(r, e)
  \/ \E r \in R : SelectBase(r) \/ RevertD(r) \/ BulkCopy(r) \/ SessionFail(r) \/ Chase(r)
  \/ Unstage \/ Stage \/ \E r \in R : StageAdmit(r) \/ AdmitRefuse(r)
  \/ \E r \in R : HotWindow(r) \/ HotLocalize(r) \/ HotRefuse(r) \/ HotParentLost(r)
                  \/ HotInline(r) \/ HotPromote(r)

\* The catch-up machinery runs (the reconciler ticks); the consumer, the
\* writes, the cuts and the failures are the environment.
Fairness ==
  /\ \A r \in R : /\ WF_vars(MarkStale(r)) /\ WF_vars(Return(r))
                  /\ WF_vars(SelectBase(r)) /\ WF_vars(RevertD(r))
                  /\ WF_vars(BulkCopy(r)) /\ WF_vars(SessionFail(r))
                  /\ WF_vars(CutOne(r))
  \* the scheduler finishes (records or aborts) a cut it began
  /\ WF_vars(CutRecord) /\ WF_vars(CutAbort)

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* Theorems                                                                *)
(***************************************************************************)

\* Every admission left the admitted head equal to its source's.
Inv_AdmitFaithful == ~badAdmit

\* Healthy members agree on every block with no write in flight.
Inv_MembersAgree ==
  \A r1, r2 \in Healthy : \A x \in Blocks : ~Pending(x) => head[r1][x] = head[r2][x]

\* Every acked write is held by a record-in_sync replica.
Inv_AckedHeld == \A x \in Blocks : \E r \in R : dState[r] = "insync" /\ head[r][x] >= acked[x]

Inv == TypeOK /\ Inv_AdmitFaithful /\ Inv_MembersAgree /\ Inv_AckedHeld

\* A stale replica that is back, with an in-sync source and a recorded epoch
\* (no session starts before the first), leaves Stale (the full build is
\* always available, so the walk can always progress).
Prop_StaleProgress ==
  \A r \in R : (dState[r] = "stale" /\ path[r] = "back" /\ Sources(r) # {} /\ recorded # {})
                 ~> (dState[r] # "stale" \/ path[r] # "back" \/ Sources(r) = {})

=============================================================================
