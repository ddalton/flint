---------------------------- MODULE FlintCatchupS ----------------------------
(***************************************************************************)
(* FlintCatchupT rev 15 (8ac3cf00) plus the MEMBERSHIP STAMP of design    *)
(* doc rev 8 §12 (record-before-ack in the on-disk shape), Stamp = TRUE:  *)
(*                                                                         *)
(*   - every leg carries a stamp (seq, gen, size, mem); an assembly        *)
(*     allocates seq and stamps its members (seq, 0, |M|, M);              *)
(*   - a base the raid loses (a dropped write, a path out) sets the       *)
(*     BARRIER: no completion is released (no ack) until StampLand has    *)
(*     put (seq, gen+1, size, survivors) on every survivor; a survivor    *)
(*     whose path is failing may fail the stamp write and leave too (the  *)
(*     chain); a majority of size releases at once; a minority waits for  *)
(*     RecordConfirm, the node agent's record write (the lost legs stale, *)
(*     the sole continuation noted) gated on the VolumeAttachment, so      *)
(*     never the zombie's, and only while RecordUp; held writes may time  *)
(*     out (HeldFail: no ack, their landed values legal);                 *)
(*   - the stage reads the stamps of the attached record-in_sync legs:    *)
(*     the newest VALID one (a majority of its size, or record-confirmed) *)
(*     names the members; attached legs it does not name go stale in the  *)
(*     assembly write; no named leg attached = defer (F36c); the zombie's *)
(*     unattachable leg goes stale by DECREE (step 4) unless the record    *)
(*     shows the old assembly continuing on it alone; MajorityRead (the   *)
(*     fix to step 2, found by flint-e4 ahead of the run): the read is    *)
(*     conclusive only when the attached legs hold a majority of the      *)
(*     stamp's size, or the record vouches, else defer; RelView (the fix  *)
(*     the first run found): a stamp carries the last released           *)
(*     generation, which a reader takes for a stamp that is not valid;    *)
(*   - the zombie: its raid's writes to fenced legs fail, so it stamps its *)
(*     own leg (seq, gen+1, size, {z}) (ZombieStamp, in any order with    *)
(*     its writes) and holds forever; a zombie whose raid was ALREADY a   *)
(*     record-confirmed sole continuation on z has no failure to see and  *)
(*     acks its writes (zAcked);                                          *)
(*   - EvenVouch (rev 9, step 2 as amended in PR #43): the read is also   *)
(*     conclusive when the attached in_sync legs number more than the     *)
(*     stamp's size less a majority (one of two, two of four) and the     *)
(*     record confirms no continuation, newer than the stamp read, on a   *)
(*     leg outside them; the record keeps each confirmed continuation's   *)
(*     legs (sole holds (seq, gen, mem)); "decree" also lets that read    *)
(*     write step 4's decree;                                             *)
(*   - TornExcluded (rev 10): an attached leg whose own stamp is an       *)
(*     unconfirmed continuation (not valid) is TORN; it counts for the    *)
(*     read (its released view), but the assembly leaves it out, and      *)
(*     its write marks it stale, whenever a clean member of the newest    *)
(*     valid stamp is attached; it serves only as the sole survivor;      *)
(*   - ZombiePreStampLegal (rev 10): a zombie write that lands on z       *)
(*     before the zombie's stamp (the write that finds the fence, in      *)
(*     flight when it is found) is the write hole: a legal value, as a    *)
(*     crash-torn write is;                                               *)
(*   - TornNeverSole (rev 11): with TornExcluded, an ordinary stage never *)
(*     serves when every attached member of the newest valid stamp is     *)
(*     torn: it waits for a clean one; a torn leg serves alone only by    *)
(*     the expiry action (StageRisk: node-gone, the operator's cut),      *)
(*     which marks the record (AckedTailRisk) and, with TornExcluded,     *)
(*     leaves a torn leg out while a clean attachable leg is there;       *)
(*   - Inv_TornJunkMarked (rev 12): while AckedTailRisk is unset, no      *)
(*     member of the live assembly holds a write the fenced writer (the   *)
(*     zombie) made AFTER its stamp (ghost zpost, cumulative over moves); *)
(*     the write that lands before the stamp is the write hole and is     *)
(*     not in zpost;                                                      *)
(*   - rev 13: the replica ids need not be naturals (model values, for    *)
(*     SYMMETRY over them); 0 stays the "none" of zx, zfreed and zLeg;    *)
(*     First is used only with CoverageWalk FALSE;                        *)
(*   - rev 14: LastFails (FailBegin without its other-healthy-member      *)
(*     guard) and WholeMark (the monitor marks every in_sync leg the      *)
(*     live raid lacks in one write), each FALSE = rev 13;                *)
(*   - rev 15: OperatorMark (the drain: an in_sync member marked stale    *)
(*     and removed, record floor only), InWriteGuard (a member other      *)
(*     than it stays in_sync), DrainStamps (the planned removal stamps    *)
(*     and holds as a loss); each FALSE = rev 14;                         *)
(*   - rev 16: MonitorGuard (the in-write rule on the monitor: its        *)
(*     write keeps a member of the live raid in_sync); FALSE = rev 15;    *)
(*   - rev 17: ZombieTriple (the zombie acks only on the confirmed        *)
(*     triple <<zseq, zgen, {zx}>>, not the (seq, gen) pair); FALSE =     *)
(*     rev 16;                                                            *)
(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)
(*     them); the monitor (MarkStale) and per-epoch trust are unchanged.  *)
(* Stamp = FALSE is FlintCatchupT exactly (the stamp variables frozen).   *)
(***************************************************************************)
(* FlintCatchupT's header follows.                                         *)
(***************************************************************************)
(* Stage-admission catch-up at N replicas with PER-EPOCH TRUST, against   *)
(* the shipped code and the source-tie fix, with the record's observation *)
(* modelled as the code does it (docs/plans/catchup-design-review-        *)
(* 2026-10-28.md, flint-cb).                                               *)
(*                                                                         *)
(* FlintCatchupN (rev 7, md5 05918a21) checked the tie with an ORACLE     *)
(* stage: Stage and StageAdmit took the in-sync legs with path = "ok", and *)
(* path was per leg, so a leg that dropped writes could never re-attach   *)
(* while its record still said in_sync.  In the code the admission set is *)
(* the attached in-sync legs, a leg attaches fine from a new consumer, and *)
(* the record learns a failure only from the monitor while the raid runs  *)
(* (or, with flint-91's fix, from a capture at NodeUnstage).  Here:        *)
(*                                                                         *)
(*   - path is relative to the CURRENT raid: a stage attaches every        *)
(*     reachable record-in_sync leg with a fresh path (StageOracle = TRUE *)
(*     keeps rev 7's oracle as a control);                                 *)
(*   - MarkStale is the monitor: it needs a live raid; an unstage before   *)
(*     its tick LOSES the mark unless Capture writes it at NodeUnstage;    *)
(*   - a consumer may crash (Crash) mid write: no capture, torn writes;    *)
(*   - a consumer may become a ZOMBIE (ZombieMoves): the pod restages on   *)
(*     another node, the zombie keeps its co-located leg (which cannot     *)
(*     attach at the new consumer) and may write it; its late monitor or   *)
(*     unstage marks every leg its raid lacks stale unless MarkGate gates  *)
(*     it on being the VolumeAttachment's node;                            *)
(*   - LocalLegGuard (FlintLocalLeg: catchup.rs:2224 before the revert,    *)
(*     :2505 every chase tick): catch-up defers on a leg while a raid on   *)
(*     its node holds it as a configured base, so never on the zombie's    *)
(*     leg until ZombieHygiene ends that raid; FALSE is the control (a     *)
(*     rebuild runs while the zombie still writes the leg; junk admitted); *)
(*   - FenceMark (flint-cb's third write, not in the code): the fence flip *)
(*     marks the zombie's co-located leg stale in the same write; without  *)
(*     it a second move before the monitor's tick assembles that leg, and  *)
(*     the zombie's writes on it, into the next raid (rev 8);              *)
(*   - CatchupWaitsCut: TRUE (revs 1-7) makes the catch-up's steps wait    *)
(*     for no cut in flight; the code runs the epoch scheduler and the     *)
(*     catch-up orchestrator as independent tasks (main.rs:287, :312) that *)
(*     meet only through the record and the epoch names: FALSE (rev 8);    *)
(*   - AssemblyMark (the code: driver.rs:3099 record_assembly_sync_state,  *)
(*     best effort, before the raid takes a write): every assembly marks   *)
(*     the in-sync legs it excludes stale in the same step; FALSE leaves   *)
(*     them to the monitor, which also covers the write failing (rev 9);   *)
(*   - StageDefer (the code's F36c last-writer defer, bounded 180 s): a    *)
(*     zombie move only detaches; the new stage comes later, before        *)
(*     hygiene (serve with risk: z excluded, AssemblyMark marks it) or     *)
(*     after it, z attaching IN_SYNC with no mark (kind 10-28; rev 10);    *)
(*   - WriteQuorum / AssemblyQuorum (option (a) for the residual of a      *)
(*     consumer leaving inside a failure window; NOT the code, whose raid1 *)
(*     acks on one base and stages with any one leg): a write that landed  *)
(*     on fewer than WriteQuorum legs completes FAILED, and a stage needs  *)
(*     at least AssemblyQuorum attachable in-sync legs (rev 12);           *)
(*     Rev 13: the quorum holds while the raid RUNS, too: a raid below     *)
(*     AssemblyQuorum takes no write and writes no stale mark (monitor,    *)
(*     capture, the zombie's late mark): it is no authority on who is      *)
(*     stale (rev 12 let it mark the leg holding the acked write);         *)
(*   - AckAfterMark (option (b), record-before-ack, NOT the code): no      *)
(*     write is acked while an in-sync leg is out of the raid, i.e. until  *)
(*     the record durably marks every base the raid failed (rev 13);       *)
(*   - Rev 14 (cutGone): a cut's plan addresses each target's head by     *)
(*     uuid (epoch_scheduler.rs plan_cut, live_lvol_uuid), and the revert  *)
(*     re-creates the head with a new one (catchup.rs revert_head :1339,   *)
(*     revert_head_to_empty :1469), so a target reverted while the cut is  *)
(*     in flight fails its snapshot (No such device) and the cut aborts.   *)
(*     Rev 13 let the cut snapshot the reverted head, and a copy then      *)
(*     stacked an OLDER epoch on that snapshot (a lineage that ascends);   *)
(*   - CutStampsAll (rev 14): the cut's record write stamps last_epoch,    *)
(*     the standby's mark, on every leg it cut; a leg cut before its       *)
(*     revert that went standby mid-cut is stamped past the epoch its head *)
(*     holds, and its final delta then starts too late;                    *)
(*   - Rev 15 (trust only, design rules the proof found): a base is never  *)
(*     above the copy's target (the target can fall when the legs that     *)
(*     trusted the newest epoch go stale: the session fails and reselects),*)
(*     and a demoted standby re-reverts instead of resuming from its base; *)
(*   - Policy picks the catch-up's trust: "shipped" (record in_sync),     *)
(*     "tie" (rev 7's fix: a write-ahead source list checked at admission, *)
(*     must_revert, the base floor), or "trust": a leg's epoch is trusted *)
(*     iff the leg was configured in the raid at the cut and still at the *)
(*     first observation after every write submitted before the cut       *)
(*     completed (a monitor tick, or the drained unstage capture); a       *)
(*     copied epoch inherits its source epoch's trust; sources and bases  *)
(*     must be trusted, a final delta's source must have a trusted head.  *)
(*                                                                         *)
(* Lineage is a ghost (anc: each present snapshot's ancestors, itself      *)
(* included), not a recursive walk: tlapm refuses RECURSIVE.               *)
(* FlintCatchupTAnc.tla checks anc against the walk.  Hot rejoin is out of *)
(* scope (FlintCatchupN models it).                                        *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANTS
  Reps,            \* the replicas, numbered; record order = number order
  Blocks, MaxVer, MaxEpoch, MaxFails, Retain,
  Backoff, BaseInclusive, PinRetention, CoverageWalk,
  Policy,          \* "shipped", "tie", "trust"
  ZombieWhere,     \* "lag": a zombie writes a leg in its detection lag
                   \* (configured, failing); "stale": design §3, only a record-
                   \* stale leg before its revert; "colocated": the zombie
                   \* consumer's own leg, not configured in the live raid
  StageOracle,     \* TRUE = rev 7's admission set (in_sync and path "ok")
  Capture,         \* NodeUnstage reads the raid's base list and marks every
                   \* leg it lacks stale (the drained unstage also confirms
                   \* trust); FALSE = the code (no record write at unstage)
  Crash,           \* the consumer may crash mid write
  ZombieMoves,     \* the consumer may become a zombie as the pod restages
  MarkGate,        \* a zombie's late monitor / unstage marks nothing (gated
                   \* on the attached VolumeAttachment's node)
  RaidFailsAtDrop, \* the raid leaves a base at its first dropped write
  ReadFails,       \* trust: a cut's membership read may fail; the cut then
                   \* records nothing (a failed cut, as a failed snapshot)
  LocalLegGuard,   \* the code (FlintLocalLeg): no catch-up step on a leg a raid
                   \* on its own node holds (the zombie's, until ZombieHygiene)
  FenceMark,       \* the fence flip marks the zombie's co-located leg stale
                   \* (flint-cb's third write); FALSE = the code today
  CatchupWaitsCut, \* TRUE = SelectBase, BulkCopy and Chase wait for no cut in
                   \* flight (revs 1-7); FALSE = the code (no exclusion)
  AssemblyMark,    \* a stage marks the in-sync legs it excludes stale in the
                   \* same write (the code, when that best-effort write lands)
  StageDefer,      \* a zombie move detaches; the ordinary Stage assembles later
                   \* (F36c waits for the last writer z; hygiene may free it)
  WriteQuorum,     \* a write is acked only if it landed on this many legs (1 =
                   \* the code); fewer = the write FAILS (its value is legal)
  AssemblyQuorum,  \* a stage needs this many attachable in-sync legs, and (rev
                   \* 13) a raid below it neither writes nor marks (1 = the code)
  AckAfterMark,    \* option (b): no ack while an in-sync leg is not a member
                   \* (the record has not yet marked a failed base); FALSE = code
  Stamp,           \* the membership stamp (§12); FALSE = FlintCatchupT
  MaxAsm,          \* bound: assemblies (stamp seq) per behaviour
  RecordUp,        \* the API server is reachable for the minority record write
  MajorityRead,    \* step 2 fix: a stamp read is conclusive only with a majority of
                   \* its size attached, or the record vouching: "count" (§12's rule:
                   \* attached in_sync legs), "member" (stricter: attached legs the
                   \* view names), "off" (rev 8 step 2 as first written)
  Decree,          \* step 4: the old consumer's unattachable leg goes stale
  DeferExpires,    \* the code's bound: F36c's defer expires (FLINT_F36C_DEFER_SECS) and
                   \* the stage serves the attachable legs with an inconclusive read
                   \* (GateDecision::ServeWithRisk), stamping AckedTailRisk
  MonitorSkipsHold, \* the monitor marks nothing while a membership change is pending
                   \* (get_bdevs reports it); FALSE = §12 as written (the monitor marks)
  DecreeGated,     \* fix: the decree runs only when the read concludes WITHOUT counting
                   \* z stale (majority count, or a confirmed sole continuation on an
                   \* attached leg), and "no unattached leg in_sync" counts z in_sync
  DecreeSparesLast, \* fix: the decree spares z when it is the record's only in_sync
                   \* leg (FALSE = step 4 as written: the volume can be stranded)
  RelView,         \* fix: a stamp also carries the last RELEASED generation, read
                   \* when the stamp itself is not valid (an unconfirmed minority)
  TornExcluded,    \* rev 10: a torn leg (own stamp an unconfirmed continuation) is left out
                   \* of the assembly and marked stale while a clean member is attached
  TornNeverSole,   \* rev 11: with TornExcluded, an ordinary stage never serves torn legs
                   \* alone; only the expiry action (StageRisk) does, marking the record
  ZombiePreStampLegal, \* rev 10: a zombie write landed before the zombie's stamp is the
                   \* write hole (joins ackedEver, as a crash-torn write does)
  EvenVouch,       \* step 2's even-size vouch (PR #43): "off"; "read" (the read is also
                   \* conclusive when the attached in_sync legs number more than size
                   \* less a majority and the record confirms no continuation newer
                   \* than the stamp read on a leg outside them); "decree" (that read
                   \* also writes step 4's decree)
  LastFails,       \* rev 14: the last healthy member may begin to fail (FALSE = rev 13)
  WholeMark,       \* rev 14: the monitor marks every in_sync leg the raid lacks at once
  OperatorMark,    \* rev 15: the maintenance drain marks an in_sync member stale and removes it
  InWriteGuard,    \* rev 15: the drain's write keeps a member other than its leg in_sync
  DrainStamps,     \* rev 15: a planned removal stamps and holds as a loss (the hook on the shared path)
  MonitorGuard,    \* rev 16: the monitor's write keeps a member of the live raid in_sync (the in-write rule)
  ZombieTriple,    \* rev 17: the zombie acks only on the confirmed triple <<zseq, zgen, {zx}>> (FALSE = the pair)
  CutStampsAll     \* TRUE = the code: the cut's record write stamps last_epoch
                   \* (the standby's mark) on every leg the cut planned, even
                   \* one that went standby mid-cut (replica_sync.rs
                   \* apply_epoch_cut); FALSE = only legs still in_sync (rev 14)

ASSUME Retain >= 1
ASSUME 0 \notin Reps   \* rev 13: ids may be model values (SYMMETRY); 0 = none
ASSUME Policy \in {"shipped", "tie", "trust"}
ASSUME ZombieWhere \in {"lag", "stale", "colocated"}
ASSUME WriteQuorum \in 1..Cardinality(Reps) /\ AssemblyQuorum \in 1..Cardinality(Reps)

R      == Reps
Epochs == 1..MaxEpoch
Vers   == 0..MaxVer
Zero   == [x \in Blocks |-> 0]
NoRisk == MaxEpoch + 1
Max(T) == CHOOSE t \in T : \A u \in T : u <= t
Min(T) == CHOOSE t \in T : \A u \in T : t <= u

VARIABLES
  ver, head, hAl, hPar, sPres, sCon, sAl, sPar,
  anc,       \* [R -> [Epochs -> SUBSET Epochs]] ghost lineage of a present snapshot
  recorded, nextSeq, cutK, cutTgt, cutDone, cutGone,
  members,   \* the live raid's configured bases
  attached,  \* a raid exists (the consumer staged)
  path,      \* [R -> {"ok", "failing", "out", "back"}], relative to the live raid
  dState, mark, reverted, sb, pin,
  riskRec,   \* A2's ghost (shipped/tie back-off)
  pend, acked, fails, badAdmit,
  used, tainted, floor, mustRev,   \* the tie
  restamp,   \* shipped: a demotion re-stamped `since`
  cfgAt,     \* [Epochs -> SUBSET R] trust: the raid's configured legs at the cut
  conf,      \* [Epochs -> BOOLEAN] trust: the cut's observation is done
  tr,        \* [R -> SUBSET Epochs] trust: the leg's trusted epochs
  htr,       \* [R -> BOOLEAN] trust: the leg's head is complete (drained unstage)
  zx,        \* R \cup {0}: the zombie consumer's co-located leg (0 = none)
  cutAck,    \* [R -> [Epochs -> [Blocks -> Vers]]] trust's ghost: the acked map
             \* when the snapshot was cut (a copy inherits its source epoch's)
  badLoss,   \* an admission lost an acked write its final source held, or
             \* brought a value nobody acked and no member of the assembly holds
  ackedEver, \* [Blocks -> SUBSET Vers] ghost: every LEGAL version: acked (and
             \* 0), or torn by a crash after landing on some leg (rev 11)
  stamp,     \* [R -> StampRec] the stamp at each leg's tail
  nseq,      \* the last assembly seq allocated
  aseq, agen, asize, \* the live raid's assembly seq, generation, size at gen 0
  rgenA, rmemA, \* the live raid's last released generation and its members
  hold,      \* the barrier: a membership change pending, completions held
  due,       \* the barrier's stamp has not landed yet
  sole,      \* SUBSET (seq, gen, mem): record-confirmed minority continuations
  zseq, zgen, zsize, zm0, \* the zombie raid's assembly at the move, its members
  zrgen, zrmem, \* the zombie raid's last released generation and members
  zst,       \* the zombie has stamped its own leg
  badAck,    \* ghost: an ack while some member's stamp was not the raid's
  zAcked,    \* ghost: the zombie acknowledged a write
  zfreed,    \* ghost: the leg the last ZombieHygiene freed (0 = none)
  zLeg,      \* ghost: the last zombie's leg (kept after hygiene)
  zjv,       \* ghost: the versions the zombie wrote and never got acknowledged
  zexcl,     \* ghost: an assembly after the move stamped without zLeg
  postMove,  \* ghost: the first assembly after the move has formed
  legalJ,    \* ghost: [Blocks -> SUBSET Vers] the zombie's writes that first assembly
             \* took in (the write-hole class: legal from then on)
  riskTail,  \* ghost: AckedTailRisk, set (for good) by a serve-with-risk stage
  vouched,   \* ghost: a stage served on the even-size vouch alone (rev 9)
  tornMarked, \* ghost: a stage left a torn leg out (rev 10)
  tornServed, \* ghost: a stage served a torn leg (with TornExcluded: as the sole survivor)
  tornBare,  \* ghost: an ORDINARY stage served torn legs alone (no AckedTailRisk) (rev 11)
  zpost      \* ghost: the versions a zombie wrote AFTER its stamp, every move (rev 12)

vars == <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
          cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
          reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, used,
          tainted, floor, mustRev, restamp, cfgAt, conf, tr, htr, zx, cutAck,
          stamp, nseq, aseq, agen, asize, rgenA, rmemA, hold, due, sole, zseq, zgen, zsize, zm0,
          zrgen, zrmem, zst, badAck, zAcked, zfreed, zLeg, zjv, zexcl, postMove, legalJ, riskTail, vouched, tornMarked, tornServed, tornBare, zpost>>

tieVars   == <<used, tainted, floor, mustRev>>
trustVars == <<cfgAt, conf, tr, htr, cutAck>>
stampVars == <<stamp, nseq, aseq, agen, asize, rgenA, rmemA, hold, due, sole, zseq, zgen, zsize, zm0,
               zrgen, zrmem, zst>>
sGhost    == <<badAck, zAcked, zfreed, zLeg, zjv, zexcl, postMove, legalJ, riskTail, vouched, tornMarked, tornServed, tornBare, zpost>>
NoStamp   == UNCHANGED <<stampVars, sGhost>>
NR        == Cardinality(Reps)
StampRec  == [seq : 0..MaxAsm, gen : 0..(NR + 1), size : 0..NR, mem : SUBSET Reps,
              rgen : 0..(NR + 1), rmem : SUBSET Reps]

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
  /\ anc \in [R -> [Epochs -> SUBSET Epochs]]
  /\ recorded \in SUBSET Epochs
  /\ nextSeq \in 1..(MaxEpoch + 1)
  /\ cutK \in 0..MaxEpoch
  /\ cutTgt \in SUBSET R /\ cutDone \in SUBSET R /\ cutGone \in SUBSET R
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
  /\ mustRev \in [R -> BOOLEAN]
  /\ restamp \in [R -> BOOLEAN]
  /\ cfgAt \in [Epochs -> SUBSET R]
  /\ conf \in [Epochs -> BOOLEAN]
  /\ tr \in [R -> SUBSET Epochs]
  /\ htr \in [R -> BOOLEAN]
  /\ zx \in R \cup {0}
  /\ cutAck \in [R -> [Epochs -> [Blocks -> Vers]]]
  /\ badLoss \in BOOLEAN
  /\ ackedEver \in [Blocks -> SUBSET Vers]
  /\ stamp \in [R -> StampRec]
  /\ nseq \in 1..MaxAsm /\ aseq \in 1..MaxAsm /\ agen \in 0..(NR + 1) /\ asize \in 0..NR
  /\ rgenA \in 0..(NR + 1) /\ rmemA \in SUBSET R /\ zrgen \in 0..(NR + 1) /\ zrmem \in SUBSET R
  /\ hold \in BOOLEAN /\ due \in BOOLEAN
  /\ sole \subseteq (1..MaxAsm) \X (0..(NR + 1)) \X SUBSET R
  /\ zseq \in 0..MaxAsm /\ zgen \in 0..(NR + 1) /\ zsize \in 0..NR /\ zm0 \in SUBSET R /\ zst \in BOOLEAN
  /\ badAck \in BOOLEAN /\ zAcked \in BOOLEAN /\ zfreed \in R \cup {0}
  /\ zLeg \in R \cup {0} /\ zjv \subseteq Vers /\ zexcl \in BOOLEAN
  /\ postMove \in BOOLEAN /\ legalJ \in [Blocks -> SUBSET Vers] /\ riskTail \in BOOLEAN /\ vouched \in BOOLEAN /\ tornMarked \in BOOLEAN /\ tornServed \in BOOLEAN /\ tornBare \in BOOLEAN /\ zpost \subseteq 1..MaxVer

Init ==
  /\ ver = 0
  /\ head = [r \in R |-> Zero] /\ hAl = [r \in R |-> {}] /\ hPar = [r \in R |-> 0]
  /\ sPres = [r \in R |-> {}]
  /\ sCon = [r \in R |-> [e \in Epochs |-> Zero]]
  /\ sAl = [r \in R |-> [e \in Epochs |-> {}]]
  /\ sPar = [r \in R |-> [e \in Epochs |-> 0]]
  /\ anc = [r \in R |-> [e \in Epochs |-> {}]]
  /\ recorded = {} /\ nextSeq = 1
  /\ cutK = 0 /\ cutTgt = {} /\ cutDone = {} /\ cutGone = {}
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
  /\ floor = [r \in R |-> -1] /\ mustRev = [r \in R |-> FALSE]
  /\ restamp = [r \in R |-> FALSE]
  /\ cfgAt = [e \in Epochs |-> {}] /\ conf = [e \in Epochs |-> TRUE]
  /\ tr = [r \in R |-> {}] /\ htr = [r \in R |-> FALSE]
  /\ zx = 0
  /\ cutAck = [r \in R |-> [e \in Epochs |-> Zero]] /\ badLoss = FALSE
  /\ ackedEver = [x \in Blocks |-> {0}]
  /\ stamp = [r \in R |-> [seq |-> 1, gen |-> 0, size |-> NR, mem |-> R, rgen |-> 0, rmem |-> R]]
  /\ nseq = 1 /\ aseq = 1 /\ agen = 0 /\ asize = NR /\ rgenA = 0 /\ rmemA = R
  /\ zrgen = 0 /\ zrmem = {}
  /\ hold = FALSE /\ due = FALSE /\ sole = {}
  /\ zseq = 0 /\ zgen = 0 /\ zsize = 0 /\ zm0 = {} /\ zst = FALSE
  /\ badAck = FALSE /\ zAcked = FALSE /\ zfreed = 0
  /\ zLeg = 0 /\ zjv = {} /\ zexcl = FALSE
  /\ postMove = FALSE /\ legalJ = [x \in Blocks |-> {}] /\ riskTail = FALSE /\ vouched = FALSE /\ tornMarked = FALSE /\ tornServed = FALSE /\ tornBare = FALSE /\ zpost = {}

(***************************************************************************)
(* Helpers                                                                 *)
(***************************************************************************)

Latest == nextSeq - 1
Pending(x) == \E w \in pend : w.b = x
Reachable(r) == path[r] # "out"
InSyncR == {r \in R : dState[r] = "insync"}
Healthy == {r \in members : path[r] = "ok"}

\* the legs a stage attaches: the code's attached in-sync set (reachable, and
\* not the zombie's own leg, whose claim the zombie raid holds), or rev 7's
\* oracle
Attachable == IF StageOracle THEN {r \in R : dState[r] = "insync" /\ path[r] = "ok"}
                             ELSE {r \in InSyncR : Reachable(r) /\ r # zx}

\* The raid holds its quorum (rev 13); always, at AssemblyQuorum = 1.
QuorumOK == AssemblyQuorum = 1 \/ Cardinality(members) >= AssemblyQuorum

\* the in-sync legs an assembly of T excludes, marked stale by its own write
AsmExcl(T) == IF AssemblyMark THEN InSyncR \ T ELSE {}

\* --- the membership stamp (§12) ---------------------------------------------
Newer(s1, s2) == s1.seq > s2.seq \/ (s1.seq = s2.seq /\ s1.gen > s2.gen)
Majority(st) == 2 * Cardinality(st.mem) > st.size
SolePairs == {<<c[1], c[2]>> : c \in sole}
Valid(st) == Majority(st) \/ <<st.seq, st.gen>> \in SolePairs
CurStamp == [seq |-> aseq, gen |-> agen, size |-> asize, mem |-> members, rgen |-> rgenA, rmem |-> rmemA]
Core(st) == [seq |-> st.seq, gen |-> st.gen, size |-> st.size, mem |-> st.mem]
\* what a reader takes from a stamp: itself, or (RelView) for a stamp that is
\* not valid, the last released generation it carries
View(st) == IF RelView /\ ~Valid(st)
              THEN [seq |-> st.seq, gen |-> st.rgen, size |-> st.size, mem |-> st.rmem,
                    rgen |-> st.rgen, rmem |-> st.rmem]
              ELSE st
\* the record shows the old assembly continuing on z alone
SoleOn(z) == <<stamp[z].seq, stamp[z].gen>> \in SolePairs /\ stamp[z].mem = {z}
\* step 2: the newest valid stamp among the attached record-in_sync legs
VAtt == {q \in Attachable : Valid(View(stamp[q]))}
NewestS == CHOOSE st \in {View(stamp[q]) : q \in VAtt} : \A q \in VAtt : ~Newer(View(stamp[q]), st)
\* the fix: conclusive only with a majority of its size attached, or the
\* record vouching (a confirmed sole continuation on an attached leg, or no
\* record-in_sync leg left unattached)
CountOK ==
  \/ MajorityRead = "off"
  \/ MajorityRead = "count" /\ 2 * Cardinality(Attachable) > NewestS.size
  \/ MajorityRead = "member" /\ 2 * Cardinality(Attachable \cap NewestS.mem) > NewestS.size
SoleOK == <<NewestS.seq, NewestS.gen>> \in SolePairs /\ NewestS.mem \cap Attachable # {}
\* rev 9, step 2's even-size vouch: no majority of the stamp's size fits among the
\* legs outside the attached in_sync set, and the record confirms no continuation
\* newer than the stamp read on a leg outside it (VouchCond: the situation, the
\* knob aside; the probes use it)
MajSize(n) == n \div 2 + 1
VouchCond ==
  /\ Cardinality(Attachable) > NewestS.size - MajSize(NewestS.size)
  /\ ~\E c \in sole : Newer([seq |-> c[1], gen |-> c[2]], NewestS) /\ c[3] \ Attachable # {}
VouchOK == EvenVouch # "off" /\ VouchCond
\* step 4: the old consumer's leg, unattachable, goes stale by decree
DecreeSet == IF Decree /\ zx # 0 /\ zx \in InSyncR /\ ~SoleOn(zx)
                  /\ (DecreeSparesLast => InSyncR # {zx})
                  /\ (DecreeGated => VAtt # {} /\ (CountOK \/ SoleOK \/ (EvenVouch = "decree" /\ VouchOK)))
               THEN {zx} ELSE {}
\* conclusive before the vouch (rev 8)
PlainConclusive ==
  \/ CountOK
  \/ SoleOK
  \/ IF DecreeGated THEN InSyncR \subseteq Attachable ELSE InSyncR \ DecreeSet \subseteq Attachable
Conclusive == PlainConclusive \/ VouchOK
StampMem == NewestS.mem \cap Attachable
\* rev 10: a torn leg's own stamp is an unconfirmed continuation: writes may have
\* landed on it after its last release; the assembly leaves it out while a clean
\* member of the newest valid stamp is attached
TornAtt == {q \in Attachable : ~Valid(stamp[q])}
CleanMem == StampMem \ TornAtt
CanStampStage == VAtt # {} /\ Conclusive /\ StampMem # {} /\ Cardinality(StampMem) >= AssemblyQuorum /\ nseq < MaxAsm
                 /\ (TornExcluded /\ TornNeverSole => CleanMem # {})
\* the members an assembly takes, and the in-sync legs its write marks stale
AsmSet == IF Stamp THEN (IF TornExcluded /\ CleanMem # {} THEN CleanMem ELSE StampMem) ELSE Attachable
AsmMarks(T) == IF Stamp THEN AsmExcl(T) \cup (Attachable \ T) \cup DecreeSet ELSE AsmExcl(T)
\* a stamped stage's ghosts: served on the vouch alone (rev 9), a torn leg left
\* out or served (rev 10)
StageGhosts ==
  /\ UNCHANGED riskTail
  /\ vouched' = (vouched \/ ~PlainConclusive)
  /\ tornMarked' = (tornMarked \/ ((StampMem \cap TornAtt) \ AsmSet) # {})
  /\ tornServed' = (tornServed \/ (AsmSet \cap TornAtt) # {})
  /\ tornBare' = (tornBare \/ (AsmSet # {} /\ AsmSet \subseteq TornAtt))
  /\ UNCHANGED zpost
\* an assembly of M stamps it (seq, 0) with a fresh seq
NewAsm(M) ==
  IF Stamp
    THEN /\ nseq' = nseq + 1 /\ aseq' = nseq + 1 /\ agen' = 0 /\ asize' = Cardinality(M)
         /\ stamp' = [q \in R |-> IF q \in M THEN [seq |-> nseq + 1, gen |-> 0, size |-> Cardinality(M), mem |-> M,
                                                    rgen |-> 0, rmem |-> M]
                                              ELSE stamp[q]]
         /\ rgenA' = 0 /\ rmemA' = M
         /\ hold' = FALSE /\ due' = FALSE /\ zfreed' = 0
         /\ zexcl' = (zexcl \/ (zLeg # 0 /\ zLeg \notin M))
         /\ postMove' = (postMove \/ zLeg # 0)
         /\ legalJ' = IF zLeg # 0 /\ ~postMove
                        THEN [x \in Blocks |-> legalJ[x] \cup ({head'[q][x] : q \in M} \cap zjv)]
                        ELSE legalJ
         /\ UNCHANGED <<sole, zseq, zgen, zsize, zm0, zrgen, zrmem, zst, badAck, zAcked, zLeg, zjv>>
    ELSE NoStamp

\* lineage: the ghost ancestor set of the head's parent
AncOf(r, p) == IF p = 0 THEN {} ELSE anc[r][p]
Lin(r) == AncOf(r, hPar[r])
NewAnc(r, e) == {e} \cup AncOf(r, hPar[r])     \* a snapshot cut from r's head

CopyElems(s, lo, target) == {e \in Lin(s) : lo <= e /\ e <= target}
Copied(s, lo, target) == {x \in Blocks : \E e \in CopyElems(s, lo, target) : x \in sAl[s][e]}
AfterCopy(start, s, lo, target) ==
  [x \in Blocks |->
     LET hits == {e \in CopyElems(s, lo, target) : x \in sAl[s][e]}
     IN IF hits = {} THEN start[x] ELSE sCon[s][Max(hits)][x]]

\* the record's view of a leg going stale (MarkStale, the capture, a zombie's
\* marks): the tie's bookkeeping for every leg in M
StaleMarks(M) ==
  /\ dState' = [q \in R |-> IF q \in M THEN "stale" ELSE dState[q]]
  /\ IF Policy = "tie"
       THEN /\ tainted' = [q \in R |-> tainted[q] \/ used[q] \cap M # {}]
            /\ floor' = [q \in R |-> IF q \in M THEN -1 ELSE floor[q]]
       ELSE UNCHANGED <<tainted, floor>>
  /\ restamp' = [q \in R |-> IF q \in M THEN FALSE ELSE restamp[q]]

\* Source selection.  Trust: the copy's target epoch is trusted on the source.
Sources(r) == {s \in R \ {r} : dState[s] = "insync" /\ Reachable(s)}
First(S) == Min(S)
LinCovers(s, b) == b <= 0 \/ b \in Lin(s)
Covers(s, t, b) ==
  /\ t \in Lin(s) /\ LinCovers(s, b)
  /\ Policy = "trust" => t \in tr[s]
\* A session's target: the newest recorded epoch, or under trust the newest
\* one an in-sync leg trusts (a copy waits for its epoch's observation; 0 =
\* none yet).
TrustedRec == {e \in recorded : \E s \in InSyncR : e \in tr[s]}
Tgt == IF Policy = "trust" THEN (IF TrustedRec = {} THEN 0 ELSE Max(TrustedRec))
                           ELSE (IF recorded = {} THEN 0 ELSE Max(recorded))
CoverSet(r, t, b) ==
  IF CoverageWalk THEN {s \in Sources(r) : Covers(s, t, b)}
  ELSE IF Sources(r) # {} /\ Covers(First(Sources(r)), t, b)
       THEN {First(Sources(r))} ELSE {}

(***************************************************************************)
(* Data plane                                                              *)
(***************************************************************************)

Submit(x) ==
  /\ attached /\ members # {} /\ QuorumOK
  /\ ver < MaxVer
  /\ ~Pending(x)
  /\ ver' = ver + 1
  /\ pend' = pend \cup {[b |-> x, v |-> ver + 1, to |-> members, landed |-> {},
                         dropped |-> {}, recAt |-> Latest, cutAt |-> nextSeq]}
  /\ riskRec' = [r \in R |-> IF r \in members THEN riskRec[r]
                                               ELSE Min({riskRec[r], Latest})]
  /\ UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
                 reverted, sb, pin, acked, fails, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

Land(w, r) ==
  /\ w \in pend
  /\ r \in w.to \ (w.landed \cup w.dropped)
  /\ path[r] \in {"ok", "failing"}
  /\ head' = [head EXCEPT ![r][w.b] = w.v]
  /\ hAl' = [hAl EXCEPT ![r] = @ \cup {w.b}]
  /\ pend' = (pend \ {w}) \cup {[w EXCEPT !.landed = @ \cup {r}]}
  /\ UNCHANGED <<ver, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq, cutK,
                 cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
                 reverted, sb, pin, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

Drop(w, r) ==
  /\ w \in pend
  /\ r \in w.to \ (w.landed \cup w.dropped)
  /\ path[r] \in {"failing", "out", "back"}
  /\ pend' = (pend \ {w}) \cup {[w EXCEPT !.dropped = @ \cup {r}]}
  /\ riskRec' = [riskRec EXCEPT ![r] = Min({@, w.recAt})]
  \* the raid's base list, not the leg's reachability
  /\ members' = IF RaidFailsAtDrop THEN members \ {r} ELSE members
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, attached, path,
                 dState, mark, reverted, sb, pin, acked, fails, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

Complete(w) ==
  /\ w \in pend
  /\ w.landed \cup w.dropped = w.to
  \* option (b): an ack waits until every base the raid failed is marked stale
  /\ AckAfterMark /\ Cardinality(w.landed) >= WriteQuorum => InSyncR \subseteq members
  \* the stamp (§12): no completion while the barrier holds
  /\ Stamp => ~hold
  /\ pend' = pend \ {w}
  \* acked on a quorum of landings; a write that landed on fewer FAILS, and
  \* what it left on the legs it reached is a legal outcome of a failed write
  /\ acked' = IF Cardinality(w.landed) >= WriteQuorum THEN [acked EXCEPT ![w.b] = w.v] ELSE acked
  /\ ackedEver' = IF w.landed # {} THEN [ackedEver EXCEPT ![w.b] = @ \cup {w.v}] ELSE ackedEver
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, path,
                 dState, mark, reverted, sb, pin, fails, riskRec, badAdmit, badLoss, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

Zombie(r, x) ==
  /\ CASE ZombieWhere = "lag"   -> path[r] = "failing"
       [] ZombieWhere = "stale" -> dState[r] = "stale" /\ reverted[r] = -1
       [] OTHER                 -> r = zx /\ r \notin members
  /\ ver < MaxVer
  /\ ~Pending(x)
  /\ ver' = ver + 1
  /\ head' = [head EXCEPT ![r][x] = ver + 1]
  /\ hAl' = [hAl EXCEPT ![r] = @ \cup {x}]
  /\ riskRec' = [riskRec EXCEPT ![r] = Min({@, Latest})]
  \* rev 10: before the zombie's stamp its landed write is the write hole
  /\ ackedEver' = IF Stamp /\ ZombiePreStampLegal /\ ~zst THEN [ackedEver EXCEPT ![x] = @ \cup {ver + 1}]
                                                         ELSE ackedEver
  /\ UNCHANGED <<hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq, cutK, cutTgt,
                 cutDone, cutGone, members, attached, path, dState, mark, reverted, sb,
                 pin, pend, acked, fails, badAdmit, badLoss, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

(***************************************************************************)
(* Failure and return; the monitor                                         *)
(***************************************************************************)

FailBegin(r) ==
  /\ path[r] = "ok" /\ r \in members /\ dState[r] = "insync"
  /\ (LastFails \/ Healthy \ {r} # {})
  /\ fails < MaxFails
  /\ path' = [path EXCEPT ![r] = "failing"]
  /\ fails' = fails + 1
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, dState,
                 mark, reverted, sb, pin, pend, acked, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

FaultOut(r) ==
  /\ path[r] = "failing"
  /\ path' = [path EXCEPT ![r] = "out"]
  /\ members' = members \ {r}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, attached, dState, mark,
                 reverted, sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

\* monitor_raid_health: a leg the live raid lacks goes stale.  The oracle
\* model (rev 7) marked without a raid; the code's monitor needs one.
MarkStale(r) ==
  /\ r \notin members /\ dState[r] = "insync"
  /\ IF StageOracle THEN path[r] \in {"out", "back"} ELSE attached /\ QuorumOK
  /\ MonitorGuard => (InSyncR \ (IF WholeMark THEN {q \in InSyncR : q \notin members} ELSE {r})) \cap members # {}
  /\ StaleMarks(IF WholeMark THEN {q \in InSyncR : q \notin members} ELSE {r})
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, path,
                 mark, reverted, sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\ UNCHANGED <<used, mustRev, trustVars>>

\* rev 15: the maintenance drain (maint_roll.rs drain_leg): the record marks r stale (the record floor: another
\* in_sync leg), then the planned removal takes r out of the raid
Drain(r) ==
  /\ OperatorMark /\ attached /\ r \in members /\ dState[r] = "insync"
  /\ InSyncR \ {r} # {}
  /\ InWriteGuard => (InSyncR \ {r}) \cap members # {}
  /\ StaleMarks({r})
  /\ members' = members \ {r}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, attached, path,
                 mark, reverted, sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\ UNCHANGED <<used, mustRev, trustVars>>

Return(r) ==
  /\ path[r] = "out"
  /\ path' = [path EXCEPT ![r] = "back"]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, dState,
                 mark, reverted, sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

\* Trust: an observation of the live raid (a monitor tick, or the next cut)
\* after every write submitted before epoch e's cut has completed confirms e
\* on the legs still configured.
Observe(e) ==
  /\ Policy = "trust"
  /\ attached /\ ~conf[e] /\ e \in recorded
  /\ \A w \in pend : w.cutAt > e
  /\ conf' = [conf EXCEPT ![e] = TRUE]
  /\ tr' = [q \in R |-> IF q \in cfgAt[e] \cap members /\ e \in sPres[q]
                         THEN tr[q] \cup {e} ELSE tr[q]]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, path, dState,
                 mark, reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver,
                 restamp, zx, cfgAt, htr, cutAck>>
  /\ UNCHANGED tieVars

(***************************************************************************)
(* The epoch scheduler                                                     *)
(***************************************************************************)

CutBegin ==
  /\ attached /\ cutK = 0
  /\ nextSeq <= MaxEpoch
  /\ \A w \in pend : w.cutAt >= nextSeq
  /\ cutK' = nextSeq
  /\ cutTgt' = InSyncR
  /\ cutDone' = {} /\ cutGone' = {}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, members, attached, path, dState, mark, reverted, sb,
                 pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

CutOne(r) ==
  /\ cutK # 0
  /\ r \in cutTgt \ cutDone
  /\ Reachable(r)
  \* the plan addresses the head by uuid; a revert re-created it (rev 14)
  /\ r \notin cutGone
  /\ IF cutK \in sPres[r]
       THEN UNCHANGED <<sPres, sCon, sAl, sPar, hPar, hAl, anc>>
       ELSE /\ sPres' = [sPres EXCEPT ![r] = @ \cup {cutK}]
            /\ sCon' = [sCon EXCEPT ![r][cutK] = head[r]]
            /\ sAl' = [sAl EXCEPT ![r][cutK] = hAl[r]]
            /\ sPar' = [sPar EXCEPT ![r][cutK] = hPar[r]]
            /\ anc' = [anc EXCEPT ![r][cutK] = NewAnc(r, cutK)]
            /\ hPar' = [hPar EXCEPT ![r] = cutK]
            /\ hAl' = [hAl EXCEPT ![r] = {}]
  /\ cutDone' = cutDone \cup {r}
  /\ tr' = IF Policy = "trust" THEN [tr EXCEPT ![r] = @ \ {cutK}] ELSE tr
  /\ cutAck' = IF Policy = "trust" /\ cutK \notin sPres[r]
                THEN [cutAck EXCEPT ![r][cutK] = acked] ELSE cutAck
  /\ UNCHANGED <<ver, head, recorded, nextSeq, cutK, cutTgt, cutGone, members, attached,
                 path, dState, mark, reverted, sb, pin, pend, acked, fails,
                 riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, cfgAt, conf, htr>>

CutRecord ==
  /\ cutK # 0 /\ cutDone = cutTgt
  /\ recorded' = recorded \cup {cutK}
  /\ nextSeq' = nextSeq + 1
  /\ mark' = [r \in R |-> IF r \in cutTgt /\ (CutStampsAll \/ dState[r] = "insync")
                         THEN cutK ELSE mark[r]]
  /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {} /\ cutGone' = {}
  \* trust: the raid's base list read at the cut
  /\ IF Policy = "trust"
       THEN /\ cfgAt' = [cfgAt EXCEPT ![cutK] = cutTgt \cap members]
            /\ conf' = [conf EXCEPT ![cutK] = FALSE]
       ELSE UNCHANGED <<cfgAt, conf>>
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, members,
                 attached, path, dState, reverted, sb, pin, pend, acked, fails,
                 riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, tr, htr, cutAck>>

CutAbort ==
  /\ cutK # 0
  /\ \E r \in cutTgt \ cutDone : ~Reachable(r) \/ r \in cutGone
  /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {} /\ cutGone' = {}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, members, attached, path, dState, mark, reverted, sb,
                 pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

\* Trust: the membership read on the consumer node fails after every target
\* cut: the scheduler records nothing (the leftovers are adopted by the next
\* cut's execute_cut, as a failed snapshot's are).
CutAbortRead ==
  /\ ReadFails /\ Policy = "trust"
  /\ cutK # 0 /\ cutDone = cutTgt
  /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {} /\ cutGone' = {}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, members, attached, path, dState, mark, reverted, sb,
                 pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

Retire ==
  /\ Cardinality(recorded) > Retain
  /\ LET e == Min(recorded) IN
       /\ PinRetention => \A r \in R : pin[r] # 0 => e < pin[r]
       /\ recorded' = recorded \ {e}
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, nextSeq, cutK,
                 cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
                 reverted, sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

Children(r, e) == {c \in sPres[r] : sPar[r][c] = e}
GC(r, e) ==
  /\ recorded # {} /\ e \in sPres[r] /\ e < Min(recorded)
  /\ Reachable(r)
  /\ LET kids == Children(r, e)
         headKid == hPar[r] = e
     IN /\ Cardinality(kids) + (IF headKid THEN 1 ELSE 0) <= 1
        /\ sPres' = [sPres EXCEPT ![r] = @ \ {e}]
        /\ anc' = [anc EXCEPT ![r] = [c \in Epochs |-> IF c = e THEN {} ELSE anc[r][c] \ {e}]]
        /\ IF headKid
             THEN /\ hAl' = [hAl EXCEPT ![r] = @ \cup sAl[r][e]]
                  /\ hPar' = [hPar EXCEPT ![r] = sPar[r][e]]
                  /\ UNCHANGED <<sAl, sPar>>
             ELSE /\ sAl' = [sAl EXCEPT ![r] = [c \in Epochs |->
                               IF c \in kids THEN sAl[r][c] \cup sAl[r][e] ELSE sAl[r][c]]]
                  /\ sPar' = [sPar EXCEPT ![r] = [c \in Epochs |->
                               IF c \in kids THEN sPar[r][e] ELSE sPar[r][c]]]
                  /\ UNCHANGED <<hAl, hPar>>
  /\ tr' = IF Policy = "trust" THEN [tr EXCEPT ![r] = @ \ {e}] ELSE tr
  /\ cutAck' = IF Policy = "trust" THEN [cutAck EXCEPT ![r][e] = Zero] ELSE cutAck
  /\ UNCHANGED <<ver, head, sCon, recorded, nextSeq, cutK, cutTgt, cutDone, cutGone,
                 members, attached, path, dState, mark, reverted, sb, pin, pend,
                 acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, cfgAt, conf, htr>>

(***************************************************************************)
(* The catch-up, per stale replica r                                       *)
(***************************************************************************)

BaseCandidates(r) ==
  {e \in recorded \cap sPres[r] :
     CASE Policy = "trust" -> e \in tr[r] /\ e <= Tgt     \* never above the target (rev 15)
       [] Policy = "tie"   -> /\ Backoff /\ ~restamp[r] => e <= riskRec[r]
                              /\ floor[r] # -1 => e <= floor[r]
       [] OTHER            -> Backoff /\ ~restamp[r] => e <= riskRec[r]}

SelectBase(r) ==
  /\ dState[r] = "stale" /\ path[r] = "back" /\ sb[r] = -1 /\ recorded # {}
  /\ LocalLegGuard => r # zx      \* catchup.rs:2224, before the pin and revert
  /\ (CatchupWaitsCut => cutK = 0) /\ Tgt # 0
  /\ Sources(r) # {}
  /\ \E b \in BaseCandidates(r) \cup {0} :
       LET b2 == IF CoverageWalk /\ b # 0 /\ CoverSet(r, Tgt, b) = {}
                   THEN 0 ELSE b
       IN /\ sb' = [sb EXCEPT ![r] = b2]
          \* catchup.rs:2201: a base equal to reverted_to resumes (the tie's
          \* must_revert forbids it); any other base re-reverts
          /\ reverted' = [reverted EXCEPT ![r] = IF @ = b2 /\ ~mustRev[r] THEN @ ELSE -1]
  /\ pin' = [pin EXCEPT ![r] = IF ~PinRetention THEN pin[r]
                               ELSE IF sb'[r] = 0 THEN Min(recorded) ELSE sb'[r]]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, path,
                 dState, mark, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

\* §5 step 0 + F75, or the full build's empty head with its reap.
RevertD(r) ==
  /\ dState[r] = "stale" /\ path[r] = "back" /\ sb[r] # -1 /\ reverted[r] = -1
  /\ sb[r] > 0 => sb[r] \in sPres[r]
  /\ LET keep == IF sb[r] = 0 THEN {e \in sPres[r] : e < Min(recorded)}
                              ELSE {e \in sPres[r] : e <= sb[r]}
     IN /\ sPres' = [sPres EXCEPT ![r] = keep]
        /\ anc' = [anc EXCEPT ![r] = [c \in Epochs |-> IF c \in keep THEN anc[r][c] ELSE {}]]
        /\ tr' = IF Policy = "trust" THEN [tr EXCEPT ![r] = @ \cap keep] ELSE tr
        /\ cutAck' = IF Policy = "trust"
                      THEN [cutAck EXCEPT ![r] = [c \in Epochs |-> IF c \in keep THEN @[c] ELSE Zero]]
                      ELSE cutAck
  /\ head' = [head EXCEPT ![r] = IF sb[r] = 0 THEN Zero ELSE sCon[r][sb[r]]]
  /\ hAl' = [hAl EXCEPT ![r] = {}]
  /\ hPar' = [hPar EXCEPT ![r] = sb[r]]
  /\ reverted' = [reverted EXCEPT ![r] = sb[r]]
  /\ IF Policy = "tie"
       THEN /\ used' = [used EXCEPT ![r] = {}]
            /\ tainted' = [tainted EXCEPT ![r] = FALSE]
            /\ floor' = [floor EXCEPT ![r] = IF @ = -1 THEN sb[r] ELSE Min({@, sb[r]})]
            /\ mustRev' = [mustRev EXCEPT ![r] = FALSE]
       ELSE UNCHANGED tieVars
  \* the head is re-created (revert_head / revert_head_to_empty: a new uuid),
  \* so a cut planned before it can no longer snapshot this leg (rev 14)
  /\ cutGone' = IF cutK # 0 /\ r \in cutTgt \ cutDone THEN cutGone \cup {r} ELSE cutGone
  /\ UNCHANGED <<ver, sCon, sAl, sPar, recorded, nextSeq, cutK, cutTgt,
                 cutDone, members, attached, path, dState, mark, sb, pin,
                 pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<cfgAt, conf, htr>>

\* copy_chain_and_align onto r's head from source s's lineage, then align.
\* Trust: the aligned copy inherits the source's target epoch's trust (the
\* base was trusted, or empty).
CopyAlign(r, s, from, target) ==
  LET lo == IF BaseInclusive \/ from = 0 THEN from ELSE from + 1
      newHead == AfterCopy(head[r], s, lo, target)
      newAl == hAl[r] \cup Copied(s, lo, target)
  IN /\ head' = [head EXCEPT ![r] = newHead]
     /\ IF from = target \/ target \in sPres[r]
          THEN /\ hAl' = [hAl EXCEPT ![r] = newAl]
               /\ UNCHANGED <<sPres, sCon, sAl, sPar, hPar, anc, tr, cutAck>>
          ELSE /\ sPres' = [sPres EXCEPT ![r] = @ \cup {target}]
               /\ sCon' = [sCon EXCEPT ![r][target] = newHead]
               /\ sAl' = [sAl EXCEPT ![r][target] = newAl]
               /\ sPar' = [sPar EXCEPT ![r][target] = hPar[r]]
               /\ anc' = [anc EXCEPT ![r][target] = NewAnc(r, target)]
               /\ hPar' = [hPar EXCEPT ![r] = target]
               /\ hAl' = [hAl EXCEPT ![r] = {}]
               /\ tr' = IF Policy = "trust" THEN [tr EXCEPT ![r] = @ \cup {target}] ELSE tr
               /\ cutAck' = IF Policy = "trust"
                             THEN [cutAck EXCEPT ![r][target] = cutAck[s][target]] ELSE cutAck

BulkCopy(r) ==
  /\ dState[r] = "stale" /\ path[r] = "back" /\ reverted[r] # -1 /\ sb[r] = reverted[r]
  /\ recorded # {} /\ (CatchupWaitsCut => cutK = 0) /\ Tgt # 0
  \* trust: a copy never targets an epoch below its base (rev 15)
  /\ Policy = "trust" => sb[r] <= Tgt
  /\ LET t == Tgt IN
       \E s \in CoverSet(r, t, sb[r]) :
         /\ CopyAlign(r, s, sb[r], t)
         /\ used' = IF Policy = "tie" THEN [used EXCEPT ![r] = @ \cup {s}] ELSE used
         /\ mark' = [mark EXCEPT ![r] = t]
         /\ pin' = [pin EXCEPT ![r] = IF PinRetention THEN t ELSE pin[r]]
  /\ dState' = [dState EXCEPT ![r] = "standby"]
  /\ sb' = [sb EXCEPT ![r] = -1]
  /\ UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, cutGone, members,
                 attached, path, reverted, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver,
                 restamp, zx>>
  /\ UNCHANGED <<tainted, floor, mustRev, cfgAt, conf, htr>>

SessionFail(r) ==
  /\ dState[r] = "stale" /\ sb[r] # -1
  /\ \/ sb[r] > 0 /\ reverted[r] = -1 /\ sb[r] \notin sPres[r]
     \/ Tgt # 0 /\ CoverSet(r, Tgt, sb[r]) = {}
     \/ Policy = "trust" /\ Tgt # 0 /\ sb[r] > Tgt    \* the target fell below the base (rev 15)
  /\ sb' = [sb EXCEPT ![r] = -1]
  /\ reverted' = [reverted EXCEPT ![r] = -1]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, path,
                 dState, mark, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, restamp, zx>>
  /\ UNCHANGED <<tieVars, trustVars>>

\* A standby's demotion: the shipped mark_stale re-stamps `since` (the back-
\* off is lost) and keeps reverted_to; the tie sets must_revert; trust
\* forgets the revert (rev 15): the head is a copy through the old mark, and
\* a resume from the base after the target fell below that mark would stamp
\* the newer content with an older epoch's name.  A re-revert is one clone.
Demote(r) ==
  /\ reverted' = IF Policy = "trust" THEN [reverted EXCEPT ![r] = -1] ELSE reverted
  /\ IF Policy = "shipped" THEN restamp' = [restamp EXCEPT ![r] = TRUE]
                           ELSE UNCHANGED restamp
  /\ IF Policy = "tie" THEN mustRev' = [mustRev EXCEPT ![r] = TRUE]
                       ELSE UNCHANGED mustRev

Chase(r) ==
  /\ dState[r] = "standby" /\ path[r] = "back" /\ (CatchupWaitsCut => cutK = 0)
  /\ LocalLegGuard => r # zx      \* catchup.rs:2505, every chase tick
  /\ Tgt # 0 /\ Tgt > mark[r]
  /\ Sources(r) # {}
  /\ LET t == Tgt IN
       IF CoverSet(r, t, mark[r]) # {}
         THEN /\ \E s \in CoverSet(r, t, mark[r]) :
                   /\ CopyAlign(r, s, mark[r], t)
                   /\ used' = IF Policy = "tie" THEN [used EXCEPT ![r] = @ \cup {s}] ELSE used
                   /\ mark' = [mark EXCEPT ![r] = t]
                   /\ pin' = [pin EXCEPT ![r] = IF PinRetention THEN t ELSE pin[r]]
              /\ UNCHANGED <<dState, reverted, restamp, mustRev>>
         ELSE /\ CoverageWalk
              /\ dState' = [dState EXCEPT ![r] = "stale"]
              /\ Demote(r)
              /\ UNCHANGED <<head, hAl, hPar, sPres, sCon, sAl, sPar, anc, mark, pin, used, tr, cutAck>>
  /\ UNCHANGED <<ver, recorded, nextSeq, cutK, cutTgt, cutDone, cutGone, members,
                 attached, path, sb, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\ UNCHANGED <<tainted, floor, cfgAt, conf, htr>>

(***************************************************************************)
(* The consumer                                                            *)
(***************************************************************************)

\* NodeUnstage, drained.  Capture: the raid's base list read before
\* bdev_raid_delete; every in-sync leg it lacks goes stale in the same
\* write, and (trust) the drained raid confirms its pending epochs and the
\* configured legs' heads.  Without it nothing is written and the pending
\* epochs are never confirmed.
Unstage ==
  /\ attached /\ pend = {} /\ cutK = 0
  /\ attached' = FALSE
  /\ members' = {}
  /\ IF Capture /\ QuorumOK THEN StaleMarks({q \in InSyncR : q \notin members})
                            ELSE UNCHANGED <<dState, tainted, floor, restamp>>
  /\ IF Policy = "trust"
       THEN /\ conf' = [e \in Epochs |-> TRUE]
            /\ tr' = IF Capture
                       THEN [q \in R |-> tr[q] \cup {e \in Epochs :
                               ~conf[e] /\ q \in cfgAt[e] \cap members /\ e \in sPres[q]}]
                       ELSE tr
            /\ htr' = [q \in R |-> Capture /\ q \in members]
       ELSE UNCHANGED <<conf, tr, htr>>
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, path, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\ UNCHANGED <<used, mustRev, cfgAt, cutAck>>

\* The consumer crashes mid write: no capture, writes in flight torn (never
\* acked), the cut in progress abandoned, nothing confirmed.  A torn write
\* that landed on some leg is a legal outcome (a block device promises
\* nothing for an unacked write), so its value joins ackedEver; that the
\* legs then disagree is survivor divergence, Inv_MembersAgree's business.
CrashUnstage ==
  /\ Crash /\ attached
  /\ attached' = FALSE
  /\ members' = {}
  /\ pend' = {}
  /\ ackedEver' = [x \in Blocks |-> ackedEver[x] \cup
                     {w.v : w \in {u \in pend : u.b = x /\ u.landed # {}}}]
  /\ cutK' = 0 /\ cutTgt' = {} /\ cutDone' = {} /\ cutGone' = {}
  /\ IF Policy = "trust"
       THEN /\ conf' = [e \in Epochs |-> TRUE]
            /\ htr' = [q \in R |-> FALSE]
       ELSE UNCHANGED <<conf, htr>>
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, path, dState, mark, reverted, sb, pin, acked, fails,
                 riskRec, badAdmit, badLoss, restamp, zx>>
  /\ UNCHANGED <<tieVars, cfgAt, tr, cutAck>>

\* NodeStage with no admission: the attachable legs, each with a fresh path
\* to this consumer.
Stage ==
  /\ ~attached
  /\ Attachable # {} /\ Cardinality(Attachable) >= AssemblyQuorum
  /\ Stamp => CanStampStage
  /\ attached' = TRUE
  /\ members' = AsmSet
  /\ path' = [q \in R |-> IF q \in AsmSet THEN "ok" ELSE path[q]]
  /\ htr' = IF Policy = "trust" THEN [q \in R |-> FALSE] ELSE htr
  /\ StaleMarks(AsmMarks(AsmSet))
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\ UNCHANGED <<used, mustRev, cfgAt, conf, tr, cutAck>>
  /\ NewAsm(AsmSet)
  /\ IF Stamp THEN StageGhosts ELSE TRUE

\* The pod moves while its old consumer lives on as a ZOMBIE holding its
\* co-located leg z (whose claim keeps z out of the new raid); the fence flip
\* keeps the zombie off every other leg (and, FenceMark, marks z stale in
\* the same write).  The new consumer stages at once, or (StageDefer) later
\* through Stage, F36c having waited for z, the last writer; the old raid's
\* pending epochs are never confirmed.
ZombieMove ==
  /\ ZombieMoves /\ attached /\ zx = 0 /\ pend = {} /\ cutK = 0
  /\ \E z \in members :
       LET A == IF StageOracle THEN {r \in R : dState[r] = "insync" /\ path[r] = "ok" /\ r # z}
                               ELSE {r \in InSyncR : Reachable(r) /\ r # z}
       IN /\ (Stamp \/ A # {})
          /\ zx' = z
          /\ IF StageDefer \/ Stamp
               THEN /\ attached' = FALSE /\ members' = {}
                    /\ path' = [path EXCEPT ![z] = "back"]
                    /\ StaleMarks(IF FenceMark THEN {z} ELSE {})
               ELSE /\ Cardinality(A) >= AssemblyQuorum
                    /\ attached' = attached /\ members' = A
                    /\ path' = [q \in R |-> IF q \in A THEN "ok" ELSE IF q = z THEN "back" ELSE path[q]]
                    /\ StaleMarks((IF FenceMark THEN {z} ELSE {}) \cup AsmExcl(A))
  /\ IF Policy = "trust"
       THEN /\ conf' = [e \in Epochs |-> TRUE]
            /\ htr' = [q \in R |-> FALSE]
       ELSE UNCHANGED <<conf, htr>>
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver>>
  /\ UNCHANGED <<used, mustRev, cfgAt, tr, cutAck>>

\* The zombie's late monitor or unstage capture, ungated: every in-sync leg
\* its raid lacks (all but its own) goes stale.
ZombieMark ==
  /\ ~MarkGate /\ zx # 0
  /\ AssemblyQuorum = 1          \* the zombie's raid holds one leg: below any quorum
  /\ \E q \in InSyncR : q # zx
  /\ StaleMarks({q \in InSyncR : q # zx})
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, path, mark,
                 reverted, sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\ UNCHANGED <<used, mustRev, trustVars>>

\* Design §3 hygiene: the zombie's raid goes away; the zombie writes its
\* co-located leg no more, and LocalLegGuard lets catch-up take the leg.
ZombieHygiene ==
  /\ LocalLegGuard /\ zx # 0
  /\ zx' = 0
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, path, dState,
                 mark, reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss,
                 ackedEver, restamp>>
  /\ UNCHANGED <<tieVars, trustVars>>

\* NodeStage admitting standby r (admit_one_standby): the strict final cut on
\* every attached in-sync leg, one more base-inclusive session from the mark
\* through it from a covering attached source (its head IS the final epoch:
\* no writer), in_sync, then the raid.  The tie checks the standby's sources;
\* trust needs the source's head and the standby's mark trusted.
StageAdmit(r) ==
  /\ ~attached /\ cutK = 0
  /\ dState[r] = "standby" /\ path[r] = "back"
  /\ nextSeq <= MaxEpoch /\ recorded # {}
  /\ Stamp => CanStampStage
  /\ LET k == nextSeq
         T == AsmSet
     IN /\ T # {} /\ Cardinality(T) >= AssemblyQuorum
        /\ Policy = "tie" => ~tainted[r] /\ used[r] \subseteq (IF Stamp THEN NewestS.mem ELSE T)
        /\ Policy = "trust" => mark[r] \in tr[r]   \* the standby's own copy is trusted
        /\ \A t \in T : k \notin sPres[t]          \* the strict final cut
        /\ k \notin sPres[r]
        \* trust: any member of the assembly being formed that covers the mark
        \* (the standby joins at that member's standing)
        /\ \E s \in (IF Policy = "trust"
                      THEN {q \in T : LinCovers(q, mark[r])}
                      ELSE CoverSet(r, Max(recorded), mark[r]) \cap T) :
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
                /\ anc' = [q \in R |-> IF q \in T \cup {r} THEN [anc[q] EXCEPT ![k] = NewAnc(q, k)]
                                       ELSE anc[q]]
                /\ hPar' = [q \in R |-> IF q \in T \cup {r} THEN k ELSE hPar[q]]
                /\ hAl' = [q \in R |-> IF q \in T \cup {r} THEN {} ELSE hAl[q]]
                /\ head' = [head EXCEPT ![r] = dHead]
                /\ badAdmit' = (badAdmit \/ dHead # head[s])
                /\ badLoss' = (badLoss \/ \E x \in Blocks :
                                  \/ head[s][x] >= acked[x] /\ dHead[x] < acked[x]
                                  \/ dHead[x] \notin ackedEver[x] \cup {head[q][x] : q \in T})
                \* trust: the final epoch is trusted where the head was (no
                \* writer since a drained unstage); the admitted copy inherits
                \* its source's standing
                /\ IF Policy = "trust"
                     THEN /\ tr' = [q \in R |-> IF (q \in T /\ htr[q]) \/ (q = r /\ htr[s])
                                                THEN tr[q] \cup {k} ELSE tr[q] \ {k}]
                          /\ cutAck' = [q \in R |-> IF q \in T \cup {r} THEN [cutAck[q] EXCEPT ![k] = acked]
                                                    ELSE cutAck[q]]
                          /\ htr' = [q \in R |-> FALSE]
                     ELSE UNCHANGED <<tr, htr, cutAck>>
        /\ recorded' = recorded \cup {k}
        /\ nextSeq' = nextSeq + 1
        /\ mark' = [q \in R |-> IF q \in T \cup {r} THEN k ELSE mark[q]]
        /\ members' = T \cup {r}
        /\ path' = [q \in R |-> IF q \in T \cup {r} THEN "ok" ELSE path[q]]
  \* the assembly's own write: r in_sync, the excluded in-sync legs stale
  /\ LET M == AsmMarks(AsmSet)
     IN /\ dState' = [q \in R |-> IF q = r THEN "insync" ELSE IF q \in M THEN "stale" ELSE dState[q]]
        /\ IF Policy = "tie"
             THEN /\ used' = [used EXCEPT ![r] = {}]
                  /\ tainted' = [q \in R |-> IF q = r THEN FALSE ELSE tainted[q] \/ used[q] \cap M # {}]
                  /\ floor' = [q \in R |-> IF q = r \/ q \in M THEN -1 ELSE floor[q]]
                  /\ mustRev' = [mustRev EXCEPT ![r] = FALSE]
             ELSE UNCHANGED tieVars
        /\ restamp' = [q \in R |-> IF q = r \/ q \in M THEN FALSE ELSE restamp[q]]
  /\ reverted' = [reverted EXCEPT ![r] = -1]
  /\ pin' = [pin EXCEPT ![r] = 0]
  /\ attached' = TRUE
  /\ riskRec' = [riskRec EXCEPT ![r] = NoRisk]
  /\ UNCHANGED <<ver, cutK, cutTgt, cutDone, cutGone, sb, pend, acked, fails, cfgAt, conf, zx, ackedEver>>

\* The tie's refusal: a listed source is not attachable now, or was marked
\* stale since; the standby goes back to stale with must_revert.
AdmitRefuse(r) ==
  /\ Policy = "tie"
  /\ ~attached /\ cutK = 0
  /\ dState[r] = "standby"
  /\ tainted[r] \/ (IF Stamp THEN VAtt # {} /\ ~(used[r] \subseteq NewestS.mem)
                              ELSE ~(used[r] \subseteq Attachable))
  /\ dState' = [dState EXCEPT ![r] = "stale"]
  /\ mustRev' = [mustRev EXCEPT ![r] = TRUE]
  /\ used' = [used EXCEPT ![r] = {}]
  /\ tainted' = [tainted EXCEPT ![r] = FALSE]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, members, attached, path, mark,
                 reverted, sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, floor,
                 restamp, zx>>
  /\ UNCHANGED trustVars

(***************************************************************************)
(* The stamp: the barrier, its release, the record, the zombie            *)
(***************************************************************************)

\* a base lost (a dropped write, a path out): the barrier and a stamp due
LossS(r) ==
  IF Stamp /\ attached /\ r \in members
    THEN /\ hold' = TRUE /\ due' = TRUE
         /\ UNCHANGED <<stamp, nseq, aseq, agen, asize, rgenA, rmemA, sole, zseq, zgen, zsize, zm0,
                        zrgen, zrmem, zst, sGhost>>
    ELSE NoStamp

\* the survivors' stamp lands; a failing survivor may fail it and leave too
StampLand ==
  /\ Stamp /\ attached /\ hold /\ due
  /\ LET Must == {q \in members : path[q] \in {"out", "back"}}
         May  == {q \in members : path[q] = "failing"}
     IN \E G \in SUBSET May :
          LET S == members \ (Must \cup G)
          IN /\ members' = S
             /\ IF S = {}
                  THEN \* no base left: the held writes complete as failed
                       /\ pend' = {}
                       /\ ackedEver' = [x \in Blocks |-> ackedEver[x] \cup
                                          {w.v : w \in {u \in pend : u.b = x /\ u.landed # {}}}]
                       /\ UNCHANGED stamp
                  ELSE /\ stamp' = [q \in R |-> IF q \in S THEN [seq |-> aseq, gen |-> agen + 1,
                                                                 size |-> asize, mem |-> S,
                                                                 rgen |-> rgenA, rmem |-> rmemA]
                                                          ELSE stamp[q]]
                       /\ UNCHANGED <<pend, ackedEver>>
  /\ agen' = agen + 1 /\ due' = FALSE
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, attached, path, dState, mark,
                 reverted, sb, pin, riskRec, acked, fails, badAdmit, badLoss, zx>>
  /\ UNCHANGED <<tieVars, restamp, trustVars>>
  /\ UNCHANGED <<nseq, aseq, asize, rgenA, rmemA, hold, sole, zseq, zgen, zsize, zm0, zrgen, zrmem, zst, sGhost>>

\* a majority of the assembly's size survives: release
Release ==
  /\ Stamp /\ attached /\ hold /\ ~due /\ Majority(CurStamp)
  /\ hold' = FALSE /\ rgenA' = agen /\ rmemA' = members
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
                 reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, zx>>
  /\ UNCHANGED <<tieVars, restamp, trustVars>>
  /\ UNCHANGED <<stamp, nseq, aseq, agen, asize, due, sole, zseq, zgen, zsize, zm0, zrgen, zrmem, zst, sGhost>>

\* a minority: the node agent writes the record (the lost legs stale, the
\* continuation noted), gated on the VolumeAttachment (the live consumer,
\* never the zombie), then confirms
RecordConfirm ==
  /\ Stamp /\ RecordUp /\ attached /\ hold /\ ~due /\ members # {} /\ ~Majority(CurStamp)
  /\ StaleMarks(InSyncR \ members)
  /\ sole' = sole \cup {<<aseq, agen, members>>}
  /\ hold' = FALSE /\ rgenA' = agen /\ rmemA' = members
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, members, attached, path, mark,
                 reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, zx>>
  /\ UNCHANGED <<used, mustRev, trustVars>>
  /\ UNCHANGED <<stamp, nseq, aseq, agen, asize, due, zseq, zgen, zsize, zm0, zrgen, zrmem, zst, sGhost>>

\* held writes reach the initiator's I/O timeout: they fail (no ack; what
\* landed is a legal outcome of a failed write)
HeldFail ==
  /\ Stamp /\ attached /\ hold /\ ~due /\ ~Majority(CurStamp) /\ pend # {}
  /\ pend' = {}
  /\ ackedEver' = [x \in Blocks |-> ackedEver[x] \cup {w.v : w \in {u \in pend : u.b = x /\ u.landed # {}}}]
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
                 reverted, sb, pin, riskRec, acked, fails, badAdmit, badLoss, zx>>
  /\ UNCHANGED <<tieVars, restamp, trustVars>>
  /\ UNCHANGED <<stampVars, sGhost>>

\* the zombie's raid sees its fenced legs fail and stamps its own leg
\* (minority, never confirmed); in any order with its writes
ZombieStamp ==
  /\ Stamp /\ zx # 0 /\ ~zst /\ zm0 # {zx}
  /\ stamp' = [stamp EXCEPT ![zx] = [seq |-> zseq, gen |-> zgen + 1, size |-> zsize, mem |-> {zx},
                                        rgen |-> zrgen, rmem |-> zrmem]]
  /\ zst' = TRUE
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
                 reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, zx>>
  /\ UNCHANGED <<tieVars, restamp, trustVars>>
  /\ UNCHANGED <<nseq, aseq, agen, asize, rgenA, rmemA, hold, due, sole, zseq, zgen, zsize, zm0,
                 zrgen, zrmem, sGhost>>

\* a stage attempt that defers still writes step 4's decree
StageDecree ==
  /\ Stamp /\ ~attached /\ DecreeSet # {}
  /\ StaleMarks(DecreeSet)
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, members, attached, path, mark,
                 reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, zx>>
  /\ UNCHANGED <<used, mustRev, trustVars>>
  /\ UNCHANGED <<stampVars, sGhost>>

\* a zombie whose raid was already a confirmed sole continuation on z sees no
\* failure: its write lands on z and is acknowledged (to the old consumer)
ZombieReleases == Stamp /\ zx # 0 /\ zm0 = {zx}
                  /\ IF ZombieTriple
                       THEN \/ Majority([seq |-> zseq, gen |-> zgen, size |-> zsize, mem |-> {zx}, rgen |-> zrgen, rmem |-> zrmem])
                            \/ <<zseq, zgen, {zx}>> \in sole
                       ELSE Valid([seq |-> zseq, gen |-> zgen, size |-> zsize, mem |-> {zx}, rgen |-> zrgen, rmem |-> zrmem])
ZombieAck(x) ==
  /\ ZombieReleases
  /\ ver < MaxVer /\ ~Pending(x)
  /\ ver' = ver + 1
  /\ head' = [head EXCEPT ![zx][x] = ver + 1]
  /\ hAl' = [hAl EXCEPT ![zx] = @ \cup {x}]
  /\ acked' = [acked EXCEPT ![x] = ver + 1]
  /\ ackedEver' = [ackedEver EXCEPT ![x] = @ \cup {ver + 1}]
  /\ zAcked' = TRUE
  /\ UNCHANGED <<hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq, cutK, cutTgt,
                 cutDone, cutGone, members, attached, path, dState, mark, reverted, sb,
                 pin, pend, fails, riskRec, badAdmit, badLoss, zx>>
  /\ UNCHANGED <<tieVars, restamp, trustVars>>
  /\ UNCHANGED <<stampVars, badAck, zfreed, zLeg, zjv, zexcl, postMove, legalJ, riskTail, vouched, tornMarked, tornServed, tornBare, zpost>>

\* the raid goes away (unstage, crash): no barrier survives it
EndRaidS ==
  /\ hold' = FALSE /\ due' = FALSE
  /\ UNCHANGED <<stamp, nseq, aseq, agen, asize, rgenA, rmemA, sole, zseq, zgen, zsize, zm0, zrgen, zrmem,
                 zst, sGhost>>

\* the zombie move: the zombie raid keeps the old assembly
MoveS ==
  IF Stamp
    THEN /\ zseq' = aseq /\ zgen' = agen /\ zsize' = asize /\ zm0' = members /\ zst' = FALSE
         /\ zrgen' = rgenA /\ zrmem' = rmemA
         /\ hold' = FALSE /\ due' = FALSE
         /\ zLeg' = zx' /\ zjv' = {} /\ zexcl' = FALSE /\ postMove' = FALSE /\ legalJ' = [x \in Blocks |-> {}]
         /\ UNCHANGED <<stamp, nseq, aseq, agen, asize, rgenA, rmemA, sole, badAck, zAcked, zfreed, riskTail, vouched, tornMarked, tornServed, tornBare, zpost>>
    ELSE NoStamp

\* the defer's bound expires: the stage serves the attachable in-sync legs on a
\* read that is not conclusive (or names no attached leg), the assembly write
\* marks the in-sync legs it lacks stale as any assembly does, and the record
\* carries AckedTailRisk from then on (freshness_gate.rs ServeWithRisk)
\* rev 11: the expiry serves the attachable legs, a torn one left out (and marked
\* stale by the assembly write) while a clean attachable leg is there
RiskSet == IF TornExcluded /\ Attachable \ TornAtt # {} THEN Attachable \ TornAtt ELSE Attachable
StageRisk ==
  /\ Stamp /\ DeferExpires /\ ~attached /\ ~CanStampStage /\ nseq < MaxAsm
  /\ Attachable # {} /\ Cardinality(RiskSet) >= AssemblyQuorum
  /\ attached' = TRUE
  /\ members' = RiskSet
  /\ path' = [q \in R |-> IF q \in RiskSet THEN "ok" ELSE path[q]]
  /\ htr' = IF Policy = "trust" THEN [q \in R |-> FALSE] ELSE htr
  /\ StaleMarks(AsmExcl(RiskSet))
  /\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\ UNCHANGED <<used, mustRev, cfgAt, conf, tr, cutAck>>
  /\ NewAsm(RiskSet)
  /\ riskTail' = TRUE /\ UNCHANGED <<vouched, tornBare, zpost>>
  /\ tornMarked' = (tornMarked \/ ((Attachable \cap TornAtt) \ RiskSet) # {})
  /\ tornServed' = (tornServed \/ (RiskSet \cap TornAtt) # {})

Next ==
  \/ \E x \in Blocks : Submit(x) /\ NoStamp
  \/ \E w \in pend : \/ Complete(w) /\ badAck' = (badAck \/ (Stamp /\ \E q \in members : Core(stamp[q]) # Core(CurStamp)))
                                  /\ UNCHANGED <<stampVars, zAcked, zfreed, zLeg, zjv, zexcl, postMove, legalJ, riskTail, vouched, tornMarked, tornServed, tornBare, zpost>>
                     \/ \E r \in R : \/ Land(w, r) /\ NoStamp
                                     \/ Drop(w, r) /\ LossS(r)
  \/ \E r \in R : \E x \in Blocks : Zombie(r, x) /\ ~ZombieReleases /\ zjv' = zjv \cup {ver + 1}
                                  /\ zpost' = (IF Stamp /\ zst THEN zpost \cup {ver + 1} ELSE zpost)
                                  /\ UNCHANGED <<stampVars, badAck, zAcked, zfreed, zLeg, zexcl, postMove, legalJ, riskTail, vouched, tornMarked, tornServed, tornBare>>
  \/ \E x \in Blocks : ZombieAck(x)
  \/ \E r \in R : \/ FailBegin(r) /\ NoStamp
                  \/ FaultOut(r) /\ LossS(r)
                  \/ MarkStale(r) /\ ~(Stamp /\ MonitorSkipsHold /\ hold) /\ NoStamp
                  \/ Drain(r) /\ (IF DrainStamps THEN LossS(r) ELSE NoStamp)
                  \/ Return(r) /\ NoStamp
  \/ \E e \in Epochs : Observe(e) /\ NoStamp
  \/ (CutBegin \/ CutRecord \/ CutAbort \/ CutAbortRead \/ Retire) /\ NoStamp
  \/ \E r \in R : (CutOne(r) \/ \E e \in Epochs : GC(r, e)) /\ NoStamp
  \/ \E r \in R : (SelectBase(r) \/ RevertD(r) \/ BulkCopy(r) \/ SessionFail(r) \/ Chase(r)) /\ NoStamp
  \/ (Unstage \/ CrashUnstage) /\ EndRaidS
  \/ Stage
  \/ ZombieMove /\ MoveS
  \/ ZombieMark /\ NoStamp
  \/ ZombieHygiene /\ zfreed' = zx /\ UNCHANGED <<stampVars, badAck, zAcked, zLeg, zjv, zexcl, postMove, legalJ, riskTail, vouched, tornMarked, tornServed, tornBare, zpost>>
  \/ \E r \in R : StageAdmit(r) /\ NewAsm(members')
                     /\ (IF Stamp THEN StageGhosts ELSE TRUE)
  \/ \E r \in R : AdmitRefuse(r) /\ NoStamp
  \/ StampLand \/ Release \/ RecordConfirm \/ HeldFail \/ ZombieStamp \/ StageDecree
  \/ StageRisk

Fairness ==
  /\ \A r \in R : /\ WF_vars(MarkStale(r)) /\ WF_vars(Return(r))
                  /\ WF_vars(SelectBase(r)) /\ WF_vars(RevertD(r))
                  /\ WF_vars(BulkCopy(r)) /\ WF_vars(SessionFail(r))
                  /\ WF_vars(CutOne(r))
  /\ WF_vars(CutRecord) /\ WF_vars(CutAbort)
  /\ \A e \in Epochs : WF_vars(Observe(e))
  /\ WF_vars(StampLand) /\ WF_vars(Release) /\ WF_vars(RecordConfirm)

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* Theorems                                                                *)
(***************************************************************************)

\* Every admission left the admitted head equal to its final source's.
Inv_AdmitFaithful == ~badAdmit

\* Healthy members agree on every block with no write in flight.
Inv_MembersAgree ==
  \A r1, r2 \in Healthy : \A x \in Blocks : ~Pending(x) => head[r1][x] = head[r2][x]

\* Every configured member holds every acked write (no write in flight).
Inv_MembersHold ==
  \A r \in members : \A x \in Blocks : ~Pending(x) => head[r][x] >= acked[x]

\* The live raid can serve every acked write: some configured member holds it
\* (rev 12).  Between Inv_AckedHeld (some in-sync leg ANYWHERE holds it, maybe
\* one the raid lacks) and Inv_MembersHold (EVERY member holds it, so no read
\* can miss it).  A raid with no member left, or (rev 13) below its quorum,
\* serves nothing: unavailable, not lost, so it is out of scope here.
Inv_RaidHolds ==
  attached /\ members # {} /\ QuorumOK =>
    \A x \in Blocks : ~Pending(x) => \E r \in members : head[r][x] >= acked[x]

\* Every acked write is held by a record-in_sync replica.
Inv_AckedHeld == \A x \in Blocks : \E r \in R : dState[r] = "insync" /\ head[r][x] >= acked[x]

\* At N replicas the admitted head need not EQUAL its final source's: with a
\* lagging source (a lost mark, a crash) it can hold more -- an acked write
\* the source dropped before the mark, copied from a trusted epoch.  What an
\* admission must not do is lose an acked write its source holds, or bring a
\* value that is not legal (acked, or torn by a crash: ackedEver) and that no
\* member of the assembly holds (a zombie's).
Inv_AdmitNoLoss == ~badLoss

\* Trust's claim: a trusted epoch holds every write acked before its cut.
Inv_TrustComplete ==
  \A q \in R : \A e \in tr[q] : \A x \in Blocks : sCon[q][e][x] >= cutAck[q][e][x]

\* The catch-up's own safety (what a copy can break), where the record is
\* right at every assembly (the capture, no crash, no zombie move): trusted
\* epochs are complete.
Inv_Catchup == TypeOK /\ Inv_AdmitNoLoss /\ Inv_AckedHeld /\ Inv_TrustComplete

\* Where a mark can be lost (no capture, a crash, a zombie move), a lagging
\* leg rejoins the next assembly and its epochs pass the rule while missing
\* a write acked before that assembly ("complete since the assembly began").
\* What the catch-up still owes: no NEW loss -- no admission loses an acked
\* write its source holds or brings unacked junk, and every acked write
\* stays on an in-sync leg.
Inv_NoNewLoss == TypeOK /\ Inv_AdmitNoLoss /\ Inv_AckedHeld

Inv == Inv_Catchup /\ Inv_AdmitFaithful /\ Inv_MembersAgree /\ Inv_MembersHold

\* --- the stamp's claims (§12) ----------------------------------------------------
\* every ack happened with every member stamped with the raid's membership
Inv_AckStamped == ~badAck
\* the zombie never acknowledges (a confirmed sole continuation on its own leg
\* is the one case the design lets it: zAcked records it)
Inv_ZombieNoAck == ~zAcked
\* cleanliness: a member holds only legal values (acked, torn by a failed
\* write, or in flight): never a zombie's unacknowledged write
Inv_MembersClean ==
  \A r \in members : \A x \in Blocks : head[r][x] \in ackedEver[x] \cup {w.v : w \in {u \in pend : u.b = x}}
\* a leg hygiene freed during the defer is stale (unless the record shows the
\* old assembly continuing on it alone)
Inv_FreedStale == zfreed # 0 /\ ~attached => dState[zfreed] # "insync" \/ SoleOn(zfreed)
\* flint-cb's line: a zombie's unacknowledged write on z at the new consumer's
\* FIRST assembly is the write-hole class (legal); once an assembly after the
\* move has stamped WITHOUT z, no member may ever hold the zombie's writes
\* (rev 3; too strict: junk the first assembly took in persists legally)
Inv_NoLateJunk == zexcl => \A r \in members : \A x \in Blocks : head[r][x] \notin zjv
\* rev 4: no member ever holds a zombie write the first assembly after the move
\* did not take in (junk entering later, or z rejoining dirty after it was excluded)
\* the bound's honest claim: no acknowledged write is absent from every in_sync
\* leg unless a serve-with-risk stage has marked the record (AckedTailRisk)
Inv_LossMarked == ~riskTail => Inv_AckedHeld
\* rev 11: torn legs serve alone only through the expiry, which marks the record
Inv_TornMarked == ~tornBare
\* rev 12: the torn junk itself: while the record carries no AckedTailRisk, no member
\* holds a write the fenced writer made after its stamp (the pre-stamp write, the
\* write hole, is not in zpost)
Inv_TornJunkMarked == ~riskTail => \A r \in members : \A x \in Blocks : head[r][x] \notin zpost
\* flint-cb: a confirmed sole continuation leaves the record with that leg alone in_sync
Inv_SoleOnly == \A q \in R : SoleOn(q) /\ dState[q] = "insync" => InSyncR = {q}
Inv_NoLateJunk2 ==
  \A r \in members : \A x \in Blocks : head[r][x] \in zjv => postMove /\ head[r][x] \in legalJ[x]
\* liveness: a barrier ends (the stamp lands and a majority releases; a
\* minority's record is written while RecordUp)
Prop_BarrierEnds == [](attached /\ hold => <>(~hold \/ ~attached))

\* Rev 7's Inv (FlintCatchupN): where a base may stay configured after
\* dropping writes (RaidFailsAtDrop = FALSE), Inv_MembersHold fails by premise.
Inv7 == Inv_Catchup /\ Inv_AdmitFaithful /\ Inv_MembersAgree

\* Trust: once an in-sync leg trusts a recorded epoch, a stale leg that is
\* back with a source leaves Stale (a copy waits for its epoch's observation,
\* so progress is owed only from then).
Prop_TrustProgress ==
  \A r \in R : (dState[r] = "stale" /\ path[r] = "back" /\ Sources(r) # {} /\ Tgt # 0)
                 ~> (dState[r] # "stale" \/ path[r] # "back" \/ Sources(r) = {} \/ Tgt = 0)

Prop_StaleProgress ==
  \A r \in R : (dState[r] = "stale" /\ path[r] = "back" /\ Sources(r) # {} /\ recorded # {})
                 ~> (dState[r] # "stale" \/ path[r] # "back" \/ Sources(r) = {})

=============================================================================
