---------------------------- MODULE FlintCatchup ----------------------------
(***************************************************************************)
(* The §5 catch-up of a returning replica, at BLOCK-CONTENT level, with    *)
(* EACH replica's own epoch snapshots (spdk-csi-driver/src/catchup.rs,     *)
(* epoch_scheduler.rs; docs/incremental-replica-rebuild.md §5-§6).         *)
(*                                                                         *)
(* FlintReplication.tla models content as a write-set and an epoch cut as  *)
(* one event; FlintSnapshots.tla gives the target no snapshots of its own  *)
(* and makes a session atomic from a SHARED chain.  Neither can state the  *)
(* §5 argument, and F75's write-up (§7) says both "neither prove nor       *)
(* refute" it.  Here:                                                      *)
(*                                                                         *)
(*   - two replicas: S, the source, never fails; D fails, returns, is      *)
(*     caught up, admitted, and may fail again (F75's corner needs that);  *)
(*   - every replica has a HEAD (content + the clusters allocated in the   *)
(*     head blob since its last snapshot) and its OWN epoch snapshots,     *)
(*     each with content, allocated clusters and a parent;                 *)
(*   - raid1 writes land LEG BY LEG; a write completes when every member   *)
(*     leg finished, acked if one succeeded (raid1.c: one good leg flips   *)
(*     the completion to SUCCESS); a failing D drops writes, and zombie    *)
(*     writes land on D alone and are never acked;                         *)
(*   - epochs are cut PER REPLICA (skewed against the writes), recorded    *)
(*     only when every in-sync replica took the cut, "already exists"     *)
(*     counts as cut (execute_cut), retention retires the oldest epoch    *)
(*     and the GC deletes rolled-out snapshots with blobstore semantics:  *)
(*     a deleted snapshot's clusters are absorbed by its one child, and a *)
(*     snapshot with two children cannot be deleted (EBUSY);              *)
(*   - the catch-up exactly as catchup_stale runs it: select the backed-  *)
(*     off base present on D, check S's lineage covers it, pin, revert D  *)
(*     to its OWN base, reap D's epochs newer than the base (F75), copy   *)
(*     S's lineage base-INCLUSIVE to the newest recorded epoch, align     *)
(*     (snapshot D's head under that name, "already exists" = converged), *)
(*     record standby; the chase repeats from the mark; admission at the  *)
(*     next assembly cuts a FINAL epoch with no writer, copies it, and    *)
(*     only then records in_sync.  No shared history = the full build.    *)
(*                                                                         *)
(* THE THEOREMS.  Inv_AdmitFaithful: every admission leaves D's head equal *)
(* to S's.  Inv_MemberAgrees: while D is a healthy raid member its head    *)
(* equals S's on every block with no write in flight.                     *)
(*                                                                         *)
(* THE TIMING ASSUMPTIONS, stated as guards (the design's own premises):   *)
(*   A1  an I/O outlives at most one cut boundary: a cut may begin only   *)
(*       when every write submitted before the previous cut was recorded  *)
(*       has completed (I/O timeout < T_snap);                            *)
(*   A2  the T_back back-off (Backoff): the base was recorded before ANY  *)
(*       write that missed D or landed on D alone was submitted — what    *)
(*       "recorded at least T_back (I/O timeout + detection lag + skew)   *)
(*       before the stale mark" buys.  riskRec is that bound, kept as a   *)
(*       ghost: the newest epoch recorded at the submission of the        *)
(*       earliest such write.                                             *)
(*                                                                         *)
(* MUTATIONS, each a piece §5 says is load-bearing (scripts/check-tla.sh): *)
(*   Backoff = FALSE        base = newest common epoch on D (the failure   *)
(*                          window: a write acked via S alone, inside S's *)
(*                          copy of an epoch D also holds, is never copied)*)
(*   Revert = FALSE         D keeps its own tail (unacked or zombie writes)*)
(*   BaseInclusive = FALSE  the copy skips the base's own delta (cut skew) *)
(*   ReapNewer = FALSE      F75: pre-failure epochs keep live names; the   *)
(*                          align takes "already exists" as converged and *)
(*                          a second failure bases on a leftover           *)
(*   FinalDelta = FALSE     admission on copy completion alone             *)
(*   PinRetention = FALSE   NOT a safety mutation: retention may retire   *)
(*                          the session's base, and the lineage-coverage  *)
(*                          check (LINEAGE_NOT_COVERED) fails the session *)
(*                          instead — the pin buys progress, not safety   *)
(*   FullBuildReaps = FALSE F75 through the full build: a dirty epoch cut  *)
(*                          on D in the detection lag keeps its live name, *)
(*                          the align takes it as converged, a second      *)
(*                          failure's time back-off picks it as the base   *)
(*   FinalCutStrict = FALSE the final cut takes a leftover (an aborted     *)
(*                          scheduler cut at the same seq) as the cut: it  *)
(*                          is OLDER than S's head, so the final delta     *)
(*                          misses S's writes after it                    *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANTS
  Blocks,        \* logical blocks (clusters), e.g. {b1, b2}
  MaxVer,        \* write budget: versions 1..MaxVer
  MaxEpoch,      \* epoch sequence budget: 1..MaxEpoch
  MaxFails,      \* failure budget for D
  Retain,        \* K: epochs the scheduler retains (>= 1)
  Backoff, Revert, BaseInclusive, ReapNewer, FinalDelta, PinRetention,
  FullBuildReaps, \* TRUE = the §9-5 full build also reaps D's epoch
                 \* snapshots still under retained (live) names; FALSE =
                 \* shipped revert_head_to_empty, which leaves them all
  FinalCutStrict \* TRUE = admission's final cut refuses a name that already
                 \* exists (hot rejoin's cut_ef_strict); FALSE = shipped
                 \* admit_one_standby, whose execute_cut takes EEXIST as cut

ASSUME Retain >= 1

R      == {"S", "D"}
Epochs == 1..MaxEpoch
Vers   == 0..MaxVer
Zero   == [x \in Blocks |-> 0]
NoRisk == MaxEpoch + 1
Max(T) == CHOOSE t \in T : \A u \in T : u <= t
Min(T) == CHOOSE t \in T : \A u \in T : t <= u

VARIABLES
  ver,       \* versions issued (every write and zombie write is fresh)
  head,      \* [R -> [Blocks -> Vers]] live head content
  hAl,       \* [R -> SUBSET Blocks] clusters allocated in the head blob
  hPar,      \* [R -> 0..MaxEpoch] the head's parent snapshot (0 = none)
  sPres,     \* [R -> SUBSET Epochs] epoch snapshots on r's lvstore, by name
  sCon,      \* [R -> [Epochs -> [Blocks -> Vers]]] their content
  sAl,       \* [R -> [Epochs -> SUBSET Blocks]] clusters allocated in each
  sPar,      \* [R -> [Epochs -> 0..MaxEpoch]] each snapshot's parent
  recorded,  \* SUBSET Epochs — the record's retained common epochs
  nextSeq,   \* the next epoch's sequence number (never reused)
  cutK,      \* epoch being cut (0 = no cut in progress)
  cutTgt,    \* SUBSET R — the replicas this cut targets (in-sync at begin)
  cutDone,   \* SUBSET R — targets already cut
  members,   \* SUBSET R — legs of the serving raid ({} = no raid)
  attached,  \* a consumer exists: writes flow, the scheduler cuts
  dPath,     \* "ok" | "failing" | "out" | "back" — D's data path
  dState,    \* "insync" | "stale" | "standby" — D's record sync_state
  mark,      \* D's last_epoch: the standby's chase mark (0 = none)
  reverted,  \* D's reverted_to: -1 none, 0 empty (full build), else a base
  sb,        \* the bulk session's selected base: -1 none, 0 full build
  pin,       \* the retention pin (0 = none)
  pend,      \* writes in flight
  acked,     \* [Blocks -> Vers] the newest acked version of each block
  fails,     \* D failures spent
  riskRec,   \* ghost (A2): see the header
  badAdmit   \* ghost: an admission left D unequal to S

vars == <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq,
          cutK, cutTgt, cutDone, members, attached, dPath, dState, mark,
          reverted, sb, pin, pend, acked, fails, riskRec, badAdmit>>

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
  /\ dPath \in {"ok", "failing", "out", "back"}
  /\ dState \in {"insync", "stale", "standby"}
  /\ mark \in 0..MaxEpoch
  /\ reverted \in -1..MaxEpoch
  /\ sb \in -1..MaxEpoch
  /\ pin \in 0..MaxEpoch
  /\ pend \subseteq Write
  /\ acked \in [Blocks -> Vers]
  /\ fails \in 0..MaxFails
  /\ riskRec \in 0..NoRisk
  /\ badAdmit \in BOOLEAN

Init ==
  /\ ver = 0
  /\ head = [r \in R |-> Zero]
  /\ hAl = [r \in R |-> {}]
  /\ hPar = [r \in R |-> 0]
  /\ sPres = [r \in R |-> {}]
  /\ sCon = [r \in R |-> [e \in Epochs |-> Zero]]
  /\ sAl = [r \in R |-> [e \in Epochs |-> {}]]
  /\ sPar = [r \in R |-> [e \in Epochs |-> 0]]
  /\ recorded = {}
  /\ nextSeq = 1
  /\ cutK = 0 /\ cutTgt = {} /\ cutDone = {}
  /\ members = R
  /\ attached = TRUE
  /\ dPath = "ok"
  /\ dState = "insync"
  /\ mark = 0
  /\ reverted = -1
  /\ sb = -1
  /\ pin = 0
  /\ pend = {}
  /\ acked = Zero
  /\ fails = 0
  /\ riskRec = NoRisk
  /\ badAdmit = FALSE

(***************************************************************************)
(* Helpers                                                                 *)
(***************************************************************************)

Latest == nextSeq - 1                      \* newest epoch ever recorded
Pending(x) == \E w \in pend : w.b = x

\* r's snapshot lineage: the snapshots reachable from its head by parent
\* links (lineage_chain walks exactly these).  Parents always name an older
\* epoch, so MaxEpoch steps reach the root.
RECURSIVE Anc(_, _, _)
Anc(r, e, n) == IF e = 0 \/ n = 0 THEN {} ELSE {e} \cup Anc(r, sPar[r][e], n - 1)
Line(r) == Anc(r, hPar[r], MaxEpoch)

\* S never reverts, so its lineage IS its set of present snapshots
\* (Inv_SLinear checks the two agree on every state TLC visits).  The
\* actions read SLine, which keeps recursion out of the next-state relation
\* and so out of the TLAPS proof.
SLine == sPres["S"]

\* One copy session: S's lineage elements in lo..target, each shallow-copied
\* (its allocated clusters, at its content) onto D's head, oldest first — so
\* for each block the newest element that allocated it wins (FlintSnapshots'
\* faithful walk).  Copy writes allocate in D's head blob.
CopyElems(lo, target) == {e \in SLine : lo <= e /\ e <= target}
Copied(lo, target) == {x \in Blocks : \E e \in CopyElems(lo, target) : x \in sAl["S"][e]}
AfterCopy(start, lo, target) ==
  [x \in Blocks |->
     LET hits == {e \in CopyElems(lo, target) : x \in sAl["S"][e]}
     IN IF hits = {} THEN start[x] ELSE sCon["S"][Max(hits)][x]]

\* Snapshot r's head under name e (bdev_lvol_snapshot): the head's clusters
\* move to the new blob, which becomes the head's parent.
SnapHead(r, e) ==
  /\ sPres' = [sPres EXCEPT ![r] = @ \cup {e}]
  /\ sCon' = [sCon EXCEPT ![r][e] = head[r]]
  /\ sAl' = [sAl EXCEPT ![r][e] = hAl[r]]
  /\ sPar' = [sPar EXCEPT ![r][e] = hPar[r]]
  /\ hPar' = [hPar EXCEPT ![r] = e]
  /\ hAl' = [hAl EXCEPT ![r] = {}]

Reachable(r) == r = "S" \/ dPath # "out"

(***************************************************************************)
(* Data plane                                                              *)
(***************************************************************************)

\* A write to block x, one write in flight per block (concurrent writes to
\* one LBA have no defined order in raid1 or in the filesystem above it).
Submit(x) ==
  /\ attached /\ members # {}
  /\ ver < MaxVer
  /\ ~Pending(x)
  /\ ver' = ver + 1
  /\ pend' = pend \cup {[b |-> x, v |-> ver + 1, to |-> members, landed |-> {},
                         dropped |-> {}, recAt |-> Latest, cutAt |-> nextSeq]}
  \* A write the raid no longer sends to D misses D as surely as a dropped
  \* one (A2's risk set): D may still read in_sync for a detection lag, and
  \* the scheduler cuts it as a common epoch meanwhile.
  /\ riskRec' = IF "D" \in members THEN riskRec ELSE Min({riskRec, Latest})
  /\ UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq,
                 cutK, cutTgt, cutDone, members, attached, dPath, dState, mark,
                 reverted, sb, pin, acked, fails, badAdmit>>

Land(w, r) ==
  /\ w \in pend
  /\ r \in w.to \ (w.landed \cup w.dropped)
  /\ r = "S" \/ dPath \in {"ok", "failing"}
  /\ head' = [head EXCEPT ![r][w.b] = w.v]
  /\ hAl' = [hAl EXCEPT ![r] = @ \cup {w.b}]
  /\ pend' = (pend \ {w}) \cup {[w EXCEPT !.landed = @ \cup {r}]}
  /\ UNCHANGED <<ver, hPar, sPres, sCon, sAl, sPar, recorded, nextSeq, cutK,
                 cutTgt, cutDone, members, attached, dPath, dState, mark,
                 reverted, sb, pin, acked, fails, riskRec, badAdmit>>

\* D's leg errors the write: it misses D (A2's risk set).  A write still in
\* flight to D when its node returns fails on the old connection too.
Drop(w) ==
  /\ w \in pend
  /\ "D" \in w.to \ (w.landed \cup w.dropped)
  /\ dPath \in {"failing", "out", "back"}
  /\ pend' = (pend \ {w}) \cup {[w EXCEPT !.dropped = @ \cup {"D"}]}
  /\ riskRec' = Min({riskRec, w.recAt})
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, dPath,
                 dState, mark, reverted, sb, pin, acked, fails, badAdmit>>

\* Every member leg finished: raid1 acks if one succeeded (S always does).
Complete(w) ==
  /\ w \in pend
  /\ w.landed \cup w.dropped = w.to
  /\ pend' = pend \ {w}
  /\ acked' = IF w.landed # {} THEN [acked EXCEPT ![w.b] = w.v] ELSE acked
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, dPath,
                 dState, mark, reverted, sb, pin, fails, riskRec, badAdmit>>

\* A zombie loopback path (§3) writes D alone while it is failing; never acked.
Zombie(x) ==
  /\ dPath = "failing"
  /\ ver < MaxVer
  /\ ~Pending(x)
  /\ ver' = ver + 1
  /\ head' = [head EXCEPT !["D"][x] = ver + 1]
  /\ hAl' = [hAl EXCEPT !["D"] = @ \cup {x}]
  /\ riskRec' = Min({riskRec, Latest})
  /\ UNCHANGED <<hPar, sPres, sCon, sAl, sPar, recorded, nextSeq, cutK, cutTgt,
                 cutDone, members, attached, dPath, dState, mark, reverted, sb,
                 pin, pend, acked, fails, badAdmit>>

(***************************************************************************)
(* D's failure and return                                                  *)
(***************************************************************************)

FailBegin ==
  /\ dPath = "ok" /\ "D" \in members /\ dState = "insync"
  /\ fails < MaxFails
  /\ dPath' = "failing"
  /\ fails' = fails + 1
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, dState,
                 mark, reverted, sb, pin, pend, acked, riskRec, badAdmit>>

\* The raid faults D out; the node is unreachable.
FaultOut ==
  /\ dPath = "failing"
  /\ dPath' = "out"
  /\ members' = members \ {"D"}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, attached, dState, mark,
                 reverted, sb, pin, pend, acked, fails, riskRec, badAdmit>>

\* monitor_raid_health / mark_stale: a leg out of the raid stopped receiving
\* writes.  last_epoch stays frozen at the last cut D took.
MarkStale ==
  /\ "D" \notin members /\ dState = "insync" /\ dPath \in {"out", "back"}
  /\ dState' = "stale"
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, dPath,
                 mark, reverted, sb, pin, pend, acked, fails, riskRec, badAdmit>>

\* The node comes back: reachable over RPC, not in any raid.
Return ==
  /\ dPath = "out"
  /\ dPath' = "back"
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, dState,
                 mark, reverted, sb, pin, pend, acked, fails, riskRec, badAdmit>>

(***************************************************************************)
(* The epoch scheduler (execute_cut, apply_epoch_cut, retire_epochs, GC)  *)
(***************************************************************************)

InSyncR == {"S"} \cup (IF dState = "insync" THEN {"D"} ELSE {})

\* A1: no write submitted before the previous epoch was recorded may still
\* be in flight when the next cut begins.
CutBegin ==
  /\ attached /\ cutK = 0
  /\ nextSeq <= MaxEpoch
  /\ \A w \in pend : w.cutAt >= nextSeq
  /\ cutK' = nextSeq
  /\ cutTgt' = InSyncR
  /\ cutDone' = {}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, members, attached, dPath, dState, mark, reverted, sb,
                 pin, pend, acked, fails, riskRec, badAdmit>>

\* One replica's snapshot.  "Already exists" counts as cut (EEXIST): a
\* leftover of an aborted attempt was cut from the same head, earlier.
CutOne(r) ==
  /\ cutK # 0
  /\ r \in cutTgt \ cutDone
  /\ Reachable(r)
  /\ IF cutK \in sPres[r] THEN UNCHANGED <<sPres, sCon, sAl, sPar, hPar, hAl>>
                          ELSE SnapHead(r, cutK)
  /\ cutDone' = cutDone \cup {r}
  /\ UNCHANGED <<ver, head, recorded, nextSeq, cutK, cutTgt, members, attached,
                 dPath, dState, mark, reverted, sb, pin, pend, acked, fails,
                 riskRec, badAdmit>>

\* All targets cut: the epoch is recorded common; last_epoch moves on
\* exactly the replicas that were cut.
CutRecord ==
  /\ cutK # 0 /\ cutDone = cutTgt
  /\ recorded' = recorded \cup {cutK}
  /\ nextSeq' = nextSeq + 1
  /\ mark' = IF "D" \in cutTgt THEN cutK ELSE mark
  /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, members,
                 attached, dPath, dState, reverted, sb, pin, pend, acked, fails,
                 riskRec, badAdmit>>

\* A target unreachable: nothing is recorded; snapshots already taken stay
\* (best-effort rollback may fail) and a retry converges on them by EEXIST.
CutAbort ==
  /\ cutK # 0
  /\ \E r \in cutTgt \ cutDone : ~Reachable(r)
  /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, members, attached, dPath, dState, mark, reverted, sb,
                 pin, pend, acked, fails, riskRec, badAdmit>>

\* retire_epochs: the oldest beyond K leaves the record — never the pinned
\* epoch or anything newer.
Retire ==
  /\ Cardinality(recorded) > Retain
  /\ LET e == Min(recorded) IN
       /\ (PinRetention /\ pin # 0) => e < pin
       /\ recorded' = recorded \ {e}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, nextSeq, cutK,
                 cutTgt, cutDone, members, attached, dPath, dState, mark,
                 reverted, sb, pin, pend, acked, fails, riskRec, badAdmit>>

\* gc_candidates / execute_gc: delete an epoch snapshot older than the
\* oldest retained one.  Blobstore: one child absorbs its clusters and is
\* re-parented; a leaf just goes; two children = EBUSY (no step).
Children(r, e) == {c \in sPres[r] : sPar[r][c] = e}
GC(r, e) ==
  /\ recorded # {} /\ e \in sPres[r] /\ e < Min(recorded)
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
                 members, attached, dPath, dState, mark, reverted, sb, pin, pend,
                 acked, fails, riskRec, badAdmit>>

(***************************************************************************)
(* The catch-up orchestrator (catchup_stale, chase_standby)                *)
(***************************************************************************)

\* select_base_epoch + select_covering_source + pin_retention: the newest
\* recorded epoch present on D (by NAME) that S's lineage covers and that
\* was recorded at least T_back before the stale mark; none = the §9-5 full
\* build (base 0), pinned at the oldest recorded epoch.  A resume
\* (reverted_to set) keeps its base.  T_back is WALL-CLOCK: the premise A2
\* (riskRec) only bounds it from one side, so the time cut-off may pass over
\* any number of newer safe epochs — the selector is modelled as picking any
\* candidate, or none (F75's corner is a time-backed-off pick of an older
\* epoch, which "newest safe" never makes).
BaseCandidates ==
  {e \in recorded \cap sPres["D"] : (Backoff => e <= riskRec) /\ e \in SLine}
SelectBase ==
  /\ dState = "stale" /\ dPath = "back" /\ sb = -1 /\ recorded # {}
  /\ cutK = 0
  /\ IF reverted # -1
       THEN sb' = reverted
       ELSE sb' \in BaseCandidates \cup {0}
  /\ pin' = IF ~PinRetention THEN pin
            ELSE IF sb' = 0 THEN Min(recorded) ELSE sb'
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, dPath,
                 dState, mark, reverted, pend, acked, fails, riskRec, badAdmit>>

\* §5 step 0 (revert_head) + F75 (reap_epochs_newer_than_base), before the
\* record marks the head reverted; or the full build's empty head.  The
\* base must still exist on D (else the revert errors and the tick retries
\* with a fresh selection — modelled as the session being dropped).
RevertD ==
  /\ dState = "stale" /\ dPath = "back" /\ sb # -1 /\ reverted = -1
  /\ IF sb = 0
       THEN /\ head' = [head EXCEPT !["D"] = Zero]
            /\ hAl' = [hAl EXCEPT !["D"] = {}]
            /\ hPar' = [hPar EXCEPT !["D"] = 0]
            \* Shipped: "the old chain's snapshots are NOT ours to reap" —
            \* true below the oldest retained epoch (the GC's), false for a
            \* snapshot cut in the detection lag, which holds a live name.
            /\ sPres' = IF FullBuildReaps
                          THEN [sPres EXCEPT !["D"] = {e \in @ : e < Min(recorded)}]
                          ELSE sPres
       ELSE /\ sb \in sPres["D"]
            /\ IF Revert
                 THEN /\ head' = [head EXCEPT !["D"] = sCon["D"][sb]]
                      /\ hAl' = [hAl EXCEPT !["D"] = {}]
                      /\ hPar' = [hPar EXCEPT !["D"] = sb]
                 ELSE UNCHANGED <<head, hAl, hPar>>
            /\ sPres' = IF ReapNewer /\ Revert
                          THEN [sPres EXCEPT !["D"] = {e \in @ : e <= sb}]
                          ELSE sPres
  /\ reverted' = sb
  /\ UNCHANGED <<ver, sCon, sAl, sPar, recorded, nextSeq, cutK, cutTgt,
                 cutDone, members, attached, dPath, dState, mark, sb, pin,
                 pend, acked, fails, riskRec, badAdmit>>

\* copy_chain_and_align from `lo` to the newest recorded epoch, then align
\* (EEXIST = converged, the F75 hazard); the degenerate base = target case
\* skips the align (the base IS that name on D).
CopyAlign(from, target) ==
  LET lo == IF BaseInclusive \/ from = 0 THEN from ELSE from + 1
      newHead == AfterCopy(head["D"], lo, target)
      newAl == hAl["D"] \cup Copied(lo, target)
  IN /\ IF from = target
          THEN /\ head' = [head EXCEPT !["D"] = newHead]
               /\ hAl' = [hAl EXCEPT !["D"] = newAl]
               /\ UNCHANGED <<sPres, sCon, sAl, sPar, hPar>>
          ELSE IF target \in sPres["D"]
          THEN /\ head' = [head EXCEPT !["D"] = newHead]
               /\ hAl' = [hAl EXCEPT !["D"] = newAl]
               /\ UNCHANGED <<sPres, sCon, sAl, sPar, hPar>>
          ELSE /\ head' = [head EXCEPT !["D"] = newHead]
               /\ sPres' = [sPres EXCEPT !["D"] = @ \cup {target}]
               /\ sCon' = [sCon EXCEPT !["D"][target] = newHead]
               /\ sAl' = [sAl EXCEPT !["D"][target] = newAl]
               /\ sPar' = [sPar EXCEPT !["D"][target] = hPar["D"]]
               /\ hPar' = [hPar EXCEPT !["D"] = target]
               /\ hAl' = [hAl EXCEPT !["D"] = {}]

\* The bulk session's copy + align + record_standby.  S's lineage must
\* still hold the base (a retired-and-collected base = LINEAGE_NOT_COVERED:
\* the session fails and re-selects next tick).
BulkCopy ==
  /\ dState = "stale" /\ dPath = "back" /\ reverted # -1 /\ sb = reverted
  /\ recorded # {} /\ cutK = 0
  /\ LET target == Max(recorded) IN
       /\ target \in SLine
       /\ sb = 0 \/ sb \in SLine
       /\ CopyAlign(sb, target)
       /\ mark' = target
  /\ dState' = "standby"
  /\ sb' = -1
  /\ pin' = IF PinRetention THEN mark' ELSE pin
  /\ UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, members,
                 attached, dPath, reverted, pend, acked, fails, riskRec, badAdmit>>

\* A session that cannot run (base gone from D or from S's lineage) drops
\* its selection; the next tick selects afresh (reverted_to cleared only by
\* re-selection when the base is gone everywhere).
SessionFail ==
  /\ dState = "stale" /\ sb # -1
  /\ \/ sb > 0 /\ reverted = -1 /\ sb \notin sPres["D"]
     \/ sb > 0 /\ sb \notin SLine
  /\ sb' = -1
  /\ reverted' = -1
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, members, attached, dPath,
                 dState, mark, pin, pend, acked, fails, riskRec, badAdmit>>

\* chase_standby: base-inclusive from the mark to the newest epoch.
Chase ==
  /\ dState = "standby" /\ dPath = "back" /\ cutK = 0
  /\ recorded # {} /\ Max(recorded) > mark
  /\ mark \in SLine
  /\ LET target == Max(recorded) IN
       /\ CopyAlign(mark, target)
       /\ mark' = target
  /\ pin' = IF PinRetention THEN mark' ELSE pin
  /\ UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, members,
                 attached, dPath, dState, reverted, sb, pend, acked, fails,
                 riskRec, badAdmit>>

(***************************************************************************)
(* The consumer: unstage, and the next assembly (admit_standbys_at_stage) *)
(***************************************************************************)

\* NodeUnstage: the raid goes (its writes completed first).
Unstage ==
  /\ attached /\ pend = {} /\ cutK = 0
  /\ attached' = FALSE
  /\ members' = {}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, dPath, dState, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit>>

\* NodeStage, no standby to admit: assemble the in-sync legs (D's leg only
\* if reachable and in_sync).
Stage ==
  /\ ~attached
  /\ ~(dState = "standby" /\ dPath = "back")
  /\ attached' = TRUE
  /\ members' = {"S"} \cup (IF dState = "insync" /\ dPath = "ok" THEN {"D"} ELSE {})
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, dPath, dState, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit>>

\* NodeStage admitting the standby: with no writer anywhere, cut a FINAL
\* epoch on S, one more base-inclusive chase session to it, record in_sync
\* (clearing reverted_to), then D joins the raid.  The final epoch is
\* latest_epoch_seq + 1 — the very name an aborted scheduler cut leaves
\* behind (gc_candidates keeps one-past-newest as live state).  Shipped,
\* execute_cut takes that leftover as the cut: S's head is NOT snapshotted
\* and the "final" epoch is older than the head.  D's side aligns the same
\* way (EEXIST = converged, F75).  FinalDelta = FALSE: admit on the last
\* chase alone.
StageAdmit ==
  /\ ~attached /\ cutK = 0
  /\ dState = "standby" /\ dPath = "back"
  /\ mark \in SLine
  /\ IF FinalDelta
       THEN /\ nextSeq <= MaxEpoch
            /\ LET k == nextSeq
                   leftS == k \in sPres["S"]
                   leftD == k \in sPres["D"]
                   sHead == head["S"]
                   lineS == SLine \cup {k}
                   elems == {e \in lineS : (IF BaseInclusive THEN mark ELSE mark + 1) <= e /\ e <= k}
                   alOf(e) == IF e = k /\ ~leftS THEN hAl["S"] ELSE sAl["S"][e]
                   conOf(e) == IF e = k /\ ~leftS THEN sHead ELSE sCon["S"][e]
                   dHead == [x \in Blocks |->
                              LET hits == {e \in elems : x \in alOf(e)}
                              IN IF hits = {} THEN head["D"][x] ELSE conOf(Max(hits))[x]]
                   dAl == hAl["D"] \cup {x \in Blocks : \E e \in elems : x \in alOf(e)}
               IN /\ ~(leftS /\ FinalCutStrict)
                  /\ sPres' = [sPres EXCEPT !["S"] = @ \cup {k}, !["D"] = @ \cup {k}]
                  /\ sCon' = [sCon EXCEPT !["S"][k] = conOf(k),
                                         !["D"][k] = IF leftD THEN @ ELSE dHead]
                  /\ sAl' = [sAl EXCEPT !["S"][k] = alOf(k),
                                        !["D"][k] = IF leftD THEN @ ELSE dAl]
                  /\ sPar' = [sPar EXCEPT !["S"][k] = IF leftS THEN @ ELSE hPar["S"],
                                         !["D"][k] = IF leftD THEN @ ELSE hPar["D"]]
                  /\ hPar' = [hPar EXCEPT !["S"] = IF leftS THEN @ ELSE k,
                                         !["D"] = IF leftD THEN @ ELSE k]
                  /\ hAl' = [hAl EXCEPT !["S"] = IF leftS THEN @ ELSE {},
                                        !["D"] = IF leftD THEN dAl ELSE {}]
                  /\ head' = [head EXCEPT !["D"] = dHead]
                  /\ recorded' = recorded \cup {k}
                  /\ nextSeq' = nextSeq + 1
                  /\ mark' = k
                  /\ badAdmit' = (badAdmit \/ dHead # sHead)
       ELSE /\ UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                           nextSeq, mark>>
            /\ badAdmit' = (badAdmit \/ head["D"] # head["S"])
  /\ dState' = "insync"
  /\ reverted' = -1
  /\ dPath' = "ok"
  /\ pin' = 0
  /\ attached' = TRUE
  /\ members' = R
  \* T_back is measured from the NEXT stale mark: an admitted D starts with
  \* no risk set.
  /\ riskRec' = NoRisk
  /\ UNCHANGED <<ver, cutK, cutTgt, cutDone, sb, pend, acked, fails>>

\* The strict final cut refuses the leftover: StandbyAdmissionDeferred, the
\* stage proceeds with the in-sync legs and the standby keeps chasing (the
\* scheduler's next cut records the leftover by EEXIST, which only widens
\* the following delta).
StageDefer ==
  /\ ~attached /\ cutK = 0
  /\ dState = "standby" /\ dPath = "back"
  /\ FinalDelta /\ FinalCutStrict /\ nextSeq \in sPres["S"]
  /\ attached' = TRUE
  /\ members' = {"S"}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, recorded,
                 nextSeq, cutK, cutTgt, cutDone, dPath, dState, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit>>

Next ==
  \/ \E x \in Blocks : Submit(x) \/ Zombie(x)
  \/ \E w \in pend : Drop(w) \/ Complete(w) \/ \E r \in R : Land(w, r)
  \/ FailBegin \/ FaultOut \/ MarkStale \/ Return
  \/ CutBegin \/ CutRecord \/ CutAbort \/ Retire
  \/ \E r \in R : CutOne(r) \/ \E e \in Epochs : GC(r, e)
  \/ SelectBase \/ RevertD \/ BulkCopy \/ SessionFail \/ Chase
  \/ Unstage \/ Stage \/ StageAdmit \/ StageDefer

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Theorems                                                                *)
(***************************************************************************)

\* Every admission left D's head equal to S's.
Inv_AdmitFaithful == ~badAdmit

\* While D is a healthy raid member it agrees with S on every block that has
\* no write in flight — no lost acked write, no divergent tail.
Inv_MemberAgrees ==
  ("D" \in members /\ dPath = "ok") =>
    \A x \in Blocks : ~Pending(x) => head["D"][x] = head["S"][x]

\* S holds every acked write (it never fails).
Inv_SourceHoldsAcked == \A x \in Blocks : head["S"][x] >= acked[x]

Inv == TypeOK /\ Inv_AdmitFaithful /\ Inv_MemberAgrees /\ Inv_SourceHoldsAcked

(***************************************************************************)
(* Candidate strengthenings for the inductive proof (checked by TLC as     *)
(* plain invariants first; FlintCatchupProbe*.cfg).                        *)
(***************************************************************************)

\* Blobstore structure: a blob's unallocated clusters read through to its
\* parent; parents are present and older.
FallThrough(r, par, al, con) ==
  \A x \in Blocks : x \notin al => con[x] = (IF par = 0 THEN 0 ELSE sCon[r][par][x])
ChainOK(r) ==
  /\ \A e \in sPres[r] : sPar[r][e] \in sPres[r] \cup {0} /\ sPar[r][e] < e
  /\ hPar[r] \in sPres[r] \cup {0}
  /\ \A e \in sPres[r] : FallThrough(r, sPar[r][e], sAl[r][e], sCon[r][e])
  /\ FallThrough(r, hPar[r], hAl[r], head[r])
Inv_Chain == ChainOK("S") /\ ChainOK("D")

\* S never reverts: its snapshots are exactly its head's lineage, and every
\* recorded epoch is one of them.
Inv_SLinear == sPres["S"] = Line("S") /\ recorded \subseteq sPres["S"]

\* S's clusters written at or after its cut e (resp. strictly after m): the
\* blocks a base-inclusive copy from e (resp. a chase from m) still moves.
AllocFrom(e) == hAl["S"] \cup UNION {sAl["S"][f] : f \in {g \in sPres["S"] : g >= e}}
AllocAfter(m) == hAl["S"] \cup UNION {sAl["S"][f] : f \in {g \in sPres["S"] : g > m}}
InFlightS(x) == \E w \in pend : w.b = x /\ "S" \notin w.landed

\* §5's base lemma as a state property: every base the selector may pick
\* agrees with S's same-named epoch on every block the copy will not move.
Inv_BaseSafe ==
  dState = "stale" =>
    \A e \in BaseCandidates : \A x \in Blocks :
      x \notin AllocFrom(e) => (sCon["D"][e][x] = sCon["S"][e][x] \/ InFlightS(x))

\* A standby is S at its mark, except where S wrote after the mark or a
\* write that reached D is still in flight to S.
Inv_StandbyTracks ==
  dState = "standby" =>
    /\ mark \in sPres["S"]
    /\ \A x \in Blocks : x \notin AllocAfter(mark) =>
         (head["D"][x] = sCon["S"][mark][x] \/ InFlightS(x))

=============================================================================
