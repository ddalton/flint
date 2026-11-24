# Derive FlintCatchupS.tla (the membership stamp, design doc rev 8 §12) from FlintCatchupT.tla rev 15.
import os, sys
D = os.path.expanduser('~/github/flint-rebuild-proof/formal')
s = open(f'{D}/FlintCatchupT.tla').read()
def rep(o, n, c=1):
    global s
    k = s.count(o)
    assert k == c, (k, o[:100]); s = s.replace(o, n)

rep('---------------------------- MODULE FlintCatchupT ----------------------------',
'''---------------------------- MODULE FlintCatchupS ----------------------------
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
(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)
(*     them); the monitor (MarkStale) and per-epoch trust are unchanged.  *)
(* Stamp = FALSE is FlintCatchupT exactly (the stamp variables frozen).   *)
(***************************************************************************)
(* FlintCatchupT's header follows.                                         *)''')
rep('''  CutStampsAll     \\* TRUE = the code: the cut's record write stamps last_epoch''',
'''  Stamp,           \\* the membership stamp (§12); FALSE = FlintCatchupT
  MaxAsm,          \\* bound: assemblies (stamp seq) per behaviour
  RecordUp,        \\* the API server is reachable for the minority record write
  MajorityRead,    \\* step 2 fix: a stamp read is conclusive only with a majority of
                   \\* its size attached, or the record vouching: "count" (§12's rule:
                   \\* attached in_sync legs), "member" (stricter: attached legs the
                   \\* view names), "off" (rev 8 step 2 as first written)
  Decree,          \\* step 4: the old consumer's unattachable leg goes stale
  DeferExpires,    \\* the code's bound: F36c's defer expires (FLINT_F36C_DEFER_SECS) and
                   \\* the stage serves the attachable legs with an inconclusive read
                   \\* (GateDecision::ServeWithRisk), stamping AckedTailRisk
  MonitorSkipsHold, \\* the monitor marks nothing while a membership change is pending
                   \\* (get_bdevs reports it); FALSE = §12 as written (the monitor marks)
  DecreeGated,     \\* fix: the decree runs only when the read concludes WITHOUT counting
                   \\* z stale (majority count, or a confirmed sole continuation on an
                   \\* attached leg), and "no unattached leg in_sync" counts z in_sync
  DecreeSparesLast, \\* fix: the decree spares z when it is the record's only in_sync
                   \\* leg (FALSE = step 4 as written: the volume can be stranded)
  RelView,         \\* fix: a stamp also carries the last RELEASED generation, read
                   \\* when the stamp itself is not valid (an unconfirmed minority)
  CutStampsAll     \\* TRUE = the code: the cut's record write stamps last_epoch''')
rep('''  ackedEver  \\* [Blocks -> SUBSET Vers] ghost: every LEGAL version: acked (and
             \\* 0), or torn by a crash after landing on some leg (rev 11)
''','''  ackedEver, \\* [Blocks -> SUBSET Vers] ghost: every LEGAL version: acked (and
             \\* 0), or torn by a crash after landing on some leg (rev 11)
  stamp,     \\* [R -> StampRec] the stamp at each leg's tail
  nseq,      \\* the last assembly seq allocated
  aseq, agen, asize, \\* the live raid's assembly seq, generation, size at gen 0
  rgenA, rmemA, \\* the live raid's last released generation and its members
  hold,      \\* the barrier: a membership change pending, completions held
  due,       \\* the barrier's stamp has not landed yet
  sole,      \\* SUBSET (seq, gen): record-confirmed minority continuations
  zseq, zgen, zsize, zm0, \\* the zombie raid's assembly at the move, its members
  zrgen, zrmem, \\* the zombie raid's last released generation and members
  zst,       \\* the zombie has stamped its own leg
  badAck,    \\* ghost: an ack while some member's stamp was not the raid's
  zAcked,    \\* ghost: the zombie acknowledged a write
  zfreed,    \\* ghost: the leg the last ZombieHygiene freed (0 = none)
  zLeg,      \\* ghost: the last zombie's leg (kept after hygiene)
  zjv,       \\* ghost: the versions the zombie wrote and never got acknowledged
  zexcl,     \\* ghost: an assembly after the move stamped without zLeg
  postMove,  \\* ghost: the first assembly after the move has formed
  legalJ,    \\* ghost: [Blocks -> SUBSET Vers] the zombie's writes that first assembly
             \\* took in (the write-hole class: legal from then on)
  riskTail   \\* ghost: AckedTailRisk, set (for good) by a serve-with-risk stage
''')
rep('''          tainted, floor, mustRev, restamp, cfgAt, conf, tr, htr, zx, cutAck>>
''','''          tainted, floor, mustRev, restamp, cfgAt, conf, tr, htr, zx, cutAck,
          stamp, nseq, aseq, agen, asize, rgenA, rmemA, hold, due, sole, zseq, zgen, zsize, zm0,
          zrgen, zrmem, zst, badAck, zAcked, zfreed, zLeg, zjv, zexcl, postMove, legalJ, riskTail>>
''')
rep('''trustVars == <<cfgAt, conf, tr, htr, cutAck>>
''','''trustVars == <<cfgAt, conf, tr, htr, cutAck>>
stampVars == <<stamp, nseq, aseq, agen, asize, rgenA, rmemA, hold, due, sole, zseq, zgen, zsize, zm0,
               zrgen, zrmem, zst>>
sGhost    == <<badAck, zAcked, zfreed, zLeg, zjv, zexcl, postMove, legalJ, riskTail>>
NoStamp   == UNCHANGED <<stampVars, sGhost>>
NR        == Cardinality(Reps)
StampRec  == [seq : 0..MaxAsm, gen : 0..(NR + 1), size : 0..NR, mem : SUBSET Reps,
              rgen : 0..(NR + 1), rmem : SUBSET Reps]
''')
rep('''  /\\ ackedEver \\in [Blocks -> SUBSET Vers]
''','''  /\\ ackedEver \\in [Blocks -> SUBSET Vers]
  /\\ stamp \\in [R -> StampRec]
  /\\ nseq \\in 1..MaxAsm /\\ aseq \\in 1..MaxAsm /\\ agen \\in 0..(NR + 1) /\\ asize \\in 0..NR
  /\\ rgenA \\in 0..(NR + 1) /\\ rmemA \\in SUBSET R /\\ zrgen \\in 0..(NR + 1) /\\ zrmem \\in SUBSET R
  /\\ hold \\in BOOLEAN /\\ due \\in BOOLEAN
  /\\ sole \\subseteq (1..MaxAsm) \\X (0..(NR + 1))
  /\\ zseq \\in 0..MaxAsm /\\ zgen \\in 0..(NR + 1) /\\ zsize \\in 0..NR /\\ zm0 \\in SUBSET R /\\ zst \\in BOOLEAN
  /\\ badAck \\in BOOLEAN /\\ zAcked \\in BOOLEAN /\\ zfreed \\in R \\cup {0}
  /\\ zLeg \\in R \\cup {0} /\\ zjv \\subseteq Vers /\\ zexcl \\in BOOLEAN
  /\\ postMove \\in BOOLEAN /\\ legalJ \\in [Blocks -> SUBSET Vers] /\\ riskTail \\in BOOLEAN
''')
rep('''  /\\ ackedEver = [x \\in Blocks |-> {0}]
''','''  /\\ ackedEver = [x \\in Blocks |-> {0}]
  /\\ stamp = [r \\in R |-> [seq |-> 1, gen |-> 0, size |-> NR, mem |-> R, rgen |-> 0, rmem |-> R]]
  /\\ nseq = 1 /\\ aseq = 1 /\\ agen = 0 /\\ asize = NR /\\ rgenA = 0 /\\ rmemA = R
  /\\ zrgen = 0 /\\ zrmem = {}
  /\\ hold = FALSE /\\ due = FALSE /\\ sole = {}
  /\\ zseq = 0 /\\ zgen = 0 /\\ zsize = 0 /\\ zm0 = {} /\\ zst = FALSE
  /\\ badAck = FALSE /\\ zAcked = FALSE /\\ zfreed = 0
  /\\ zLeg = 0 /\\ zjv = {} /\\ zexcl = FALSE
  /\\ postMove = FALSE /\\ legalJ = [x \\in Blocks |-> {}] /\\ riskTail = FALSE
''')
# Complete: no ack under the barrier
rep('''  /\\ AckAfterMark /\\ Cardinality(w.landed) >= WriteQuorum => InSyncR \\subseteq members
''','''  /\\ AckAfterMark /\\ Cardinality(w.landed) >= WriteQuorum => InSyncR \\subseteq members
  \\* the stamp (§12): no completion while the barrier holds
  /\\ Stamp => ~hold
''')
# ZombieMove: under the stamp the move only detaches; record the zombie raid
rep('''          /\\ IF StageDefer
               THEN /\\ attached' = FALSE /\\ members' = {}''','''          /\\ IF StageDefer \\/ Stamp
               THEN /\\ attached' = FALSE /\\ members' = {}''')
# ZombieMove: under the stamp a move only detaches, so it needs no other attachable leg
# (the old raid may be a confirmed sole continuation on z; the new stage then defers)
rep('''       IN /\\ A # {}
          /\\ zx' = z''','''       IN /\\ (Stamp \\/ A # {})
          /\\ zx' = z''')
# ---- helpers: stamps, the stage's read
rep('''\\* the in-sync legs an assembly of T excludes, marked stale by its own write
AsmExcl(T) == IF AssemblyMark THEN InSyncR \\ T ELSE {}
''','''\\* the in-sync legs an assembly of T excludes, marked stale by its own write
AsmExcl(T) == IF AssemblyMark THEN InSyncR \\ T ELSE {}

\\* --- the membership stamp (§12) ---------------------------------------------
Newer(s1, s2) == s1.seq > s2.seq \\/ (s1.seq = s2.seq /\\ s1.gen > s2.gen)
Majority(st) == 2 * Cardinality(st.mem) > st.size
Valid(st) == Majority(st) \\/ <<st.seq, st.gen>> \\in sole
CurStamp == [seq |-> aseq, gen |-> agen, size |-> asize, mem |-> members, rgen |-> rgenA, rmem |-> rmemA]
Core(st) == [seq |-> st.seq, gen |-> st.gen, size |-> st.size, mem |-> st.mem]
\\* what a reader takes from a stamp: itself, or (RelView) for a stamp that is
\\* not valid, the last released generation it carries
View(st) == IF RelView /\\ ~Valid(st)
              THEN [seq |-> st.seq, gen |-> st.rgen, size |-> st.size, mem |-> st.rmem,
                    rgen |-> st.rgen, rmem |-> st.rmem]
              ELSE st
\\* the record shows the old assembly continuing on z alone
SoleOn(z) == <<stamp[z].seq, stamp[z].gen>> \\in sole /\\ stamp[z].mem = {z}
\\* step 2: the newest valid stamp among the attached record-in_sync legs
VAtt == {q \\in Attachable : Valid(View(stamp[q]))}
NewestS == CHOOSE st \\in {View(stamp[q]) : q \\in VAtt} : \\A q \\in VAtt : ~Newer(View(stamp[q]), st)
\\* the fix: conclusive only with a majority of its size attached, or the
\\* record vouching (a confirmed sole continuation on an attached leg, or no
\\* record-in_sync leg left unattached)
CountOK ==
  \\/ MajorityRead = "off"
  \\/ MajorityRead = "count" /\\ 2 * Cardinality(Attachable) > NewestS.size
  \\/ MajorityRead = "member" /\\ 2 * Cardinality(Attachable \\cap NewestS.mem) > NewestS.size
SoleOK == <<NewestS.seq, NewestS.gen>> \\in sole /\\ NewestS.mem \\cap Attachable # {}
\\* step 4: the old consumer's leg, unattachable, goes stale by decree
DecreeSet == IF Decree /\\ zx # 0 /\\ zx \\in InSyncR /\\ ~SoleOn(zx)
                  /\\ (DecreeSparesLast => InSyncR # {zx})
                  /\\ (DecreeGated => VAtt # {} /\\ (CountOK \\/ SoleOK))
               THEN {zx} ELSE {}
Conclusive ==
  \\/ CountOK
  \\/ SoleOK
  \\/ IF DecreeGated THEN InSyncR \\subseteq Attachable ELSE InSyncR \\ DecreeSet \\subseteq Attachable
StampMem == NewestS.mem \\cap Attachable
CanStampStage == VAtt # {} /\\ Conclusive /\\ StampMem # {} /\\ Cardinality(StampMem) >= AssemblyQuorum /\\ nseq < MaxAsm
\\* the members an assembly takes, and the in-sync legs its write marks stale
AsmSet == IF Stamp THEN StampMem ELSE Attachable
AsmMarks(T) == IF Stamp THEN AsmExcl(T) \\cup (Attachable \\ T) \\cup DecreeSet ELSE AsmExcl(T)
\\* an assembly of M stamps it (seq, 0) with a fresh seq
NewAsm(M) ==
  IF Stamp
    THEN /\\ nseq' = nseq + 1 /\\ aseq' = nseq + 1 /\\ agen' = 0 /\\ asize' = Cardinality(M)
         /\\ stamp' = [q \\in R |-> IF q \\in M THEN [seq |-> nseq + 1, gen |-> 0, size |-> Cardinality(M), mem |-> M,
                                                    rgen |-> 0, rmem |-> M]
                                              ELSE stamp[q]]
         /\\ rgenA' = 0 /\\ rmemA' = M
         /\\ hold' = FALSE /\\ due' = FALSE /\\ zfreed' = 0
         /\\ zexcl' = (zexcl \\/ (zLeg # 0 /\\ zLeg \\notin M))
         /\\ postMove' = (postMove \\/ zLeg # 0)
         /\\ legalJ' = IF zLeg # 0 /\\ ~postMove
                        THEN [x \\in Blocks |-> legalJ[x] \\cup ({head'[q][x] : q \\in M} \\cap zjv)]
                        ELSE legalJ
         /\\ UNCHANGED <<sole, zseq, zgen, zsize, zm0, zrgen, zrmem, zst, badAck, zAcked, zLeg, zjv>>
    ELSE NoStamp
''')
# Stage: the stamp's read decides the members
rep('''Stage ==
  /\\ ~attached
  /\\ Attachable # {} /\\ Cardinality(Attachable) >= AssemblyQuorum
  /\\ attached' = TRUE
  /\\ members' = Attachable
  /\\ path' = [q \\in R |-> IF q \\in Attachable THEN "ok" ELSE path[q]]
  /\\ htr' = IF Policy = "trust" THEN [q \\in R |-> FALSE] ELSE htr
  /\\ StaleMarks(AsmExcl(Attachable))''','''Stage ==
  /\\ ~attached
  /\\ Attachable # {} /\\ Cardinality(Attachable) >= AssemblyQuorum
  /\\ Stamp => CanStampStage
  /\\ attached' = TRUE
  /\\ members' = AsmSet
  /\\ path' = [q \\in R |-> IF q \\in AsmSet THEN "ok" ELSE path[q]]
  /\\ htr' = IF Policy = "trust" THEN [q \\in R |-> FALSE] ELSE htr
  /\\ StaleMarks(AsmMarks(AsmSet))
  /\\ NewAsm(AsmSet)''')
# StageAdmit: the stamp's members; the tie's conjunct = named in the newest valid stamp
rep('''  /\\ LET k == nextSeq
         T == Attachable
     IN /\\ T # {} /\\ Cardinality(T) >= AssemblyQuorum
        /\\ Policy = "tie" => ~tainted[r] /\\ used[r] \\subseteq T''','''  /\\ Stamp => CanStampStage
  /\\ LET k == nextSeq
         T == AsmSet
     IN /\\ T # {} /\\ Cardinality(T) >= AssemblyQuorum
        /\\ Policy = "tie" => ~tainted[r] /\\ used[r] \\subseteq (IF Stamp THEN NewestS.mem ELSE T)''')
rep('''  /\\ LET M == AsmExcl(Attachable)
     IN /\\ dState' = [q \\in R |-> IF q = r THEN "insync" ELSE IF q \\in M THEN "stale" ELSE dState[q]]''',
'''  /\\ LET M == AsmMarks(AsmSet)
     IN /\\ dState' = [q \\in R |-> IF q = r THEN "insync" ELSE IF q \\in M THEN "stale" ELSE dState[q]]''')
rep('''  /\\ tainted[r] \\/ ~(used[r] \\subseteq Attachable)
''','''  /\\ tainted[r] \\/ (IF Stamp THEN VAtt # {} /\\ ~(used[r] \\subseteq NewestS.mem)
                              ELSE ~(used[r] \\subseteq Attachable))
''')
# ---- the stamp's own actions, before Next
rep('''Next ==
  \\/ \\E x \\in Blocks : Submit(x)''','''(***************************************************************************)
(* The stamp: the barrier, its release, the record, the zombie            *)
(***************************************************************************)

\\* a base lost (a dropped write, a path out): the barrier and a stamp due
LossS(r) ==
  IF Stamp /\\ attached /\\ r \\in members
    THEN /\\ hold' = TRUE /\\ due' = TRUE
         /\\ UNCHANGED <<stamp, nseq, aseq, agen, asize, rgenA, rmemA, sole, zseq, zgen, zsize, zm0,
                        zrgen, zrmem, zst, sGhost>>
    ELSE NoStamp

\\* the survivors' stamp lands; a failing survivor may fail it and leave too
StampLand ==
  /\\ Stamp /\\ attached /\\ hold /\\ due
  /\\ LET Must == {q \\in members : path[q] \\in {"out", "back"}}
         May  == {q \\in members : path[q] = "failing"}
     IN \\E G \\in SUBSET May :
          LET S == members \\ (Must \\cup G)
          IN /\\ members' = S
             /\\ IF S = {}
                  THEN \\* no base left: the held writes complete as failed
                       /\\ pend' = {}
                       /\\ ackedEver' = [x \\in Blocks |-> ackedEver[x] \\cup
                                          {w.v : w \\in {u \\in pend : u.b = x /\\ u.landed # {}}}]
                       /\\ UNCHANGED stamp
                  ELSE /\\ stamp' = [q \\in R |-> IF q \\in S THEN [seq |-> aseq, gen |-> agen + 1,
                                                                 size |-> asize, mem |-> S,
                                                                 rgen |-> rgenA, rmem |-> rmemA]
                                                          ELSE stamp[q]]
                       /\\ UNCHANGED <<pend, ackedEver>>
  /\\ agen' = agen + 1 /\\ due' = FALSE
  /\\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, attached, path, dState, mark,
                 reverted, sb, pin, riskRec, acked, fails, badAdmit, badLoss, zx>>
  /\\ UNCHANGED <<tieVars, restamp, trustVars>>
  /\\ UNCHANGED <<nseq, aseq, asize, rgenA, rmemA, hold, sole, zseq, zgen, zsize, zm0, zrgen, zrmem, zst, sGhost>>

\\* a majority of the assembly's size survives: release
Release ==
  /\\ Stamp /\\ attached /\\ hold /\\ ~due /\\ Majority(CurStamp)
  /\\ hold' = FALSE /\\ rgenA' = agen /\\ rmemA' = members
  /\\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
                 reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, zx>>
  /\\ UNCHANGED <<tieVars, restamp, trustVars>>
  /\\ UNCHANGED <<stamp, nseq, aseq, agen, asize, due, sole, zseq, zgen, zsize, zm0, zrgen, zrmem, zst, sGhost>>

\\* a minority: the node agent writes the record (the lost legs stale, the
\\* continuation noted), gated on the VolumeAttachment (the live consumer,
\\* never the zombie), then confirms
RecordConfirm ==
  /\\ Stamp /\\ RecordUp /\\ attached /\\ hold /\\ ~due /\\ members # {} /\\ ~Majority(CurStamp)
  /\\ StaleMarks(InSyncR \\ members)
  /\\ sole' = sole \\cup {<<aseq, agen>>}
  /\\ hold' = FALSE /\\ rgenA' = agen /\\ rmemA' = members
  /\\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, members, attached, path, mark,
                 reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, zx>>
  /\\ UNCHANGED <<used, mustRev, trustVars>>
  /\\ UNCHANGED <<stamp, nseq, aseq, agen, asize, due, zseq, zgen, zsize, zm0, zrgen, zrmem, zst, sGhost>>

\\* held writes reach the initiator's I/O timeout: they fail (no ack; what
\\* landed is a legal outcome of a failed write)
HeldFail ==
  /\\ Stamp /\\ attached /\\ hold /\\ ~due /\\ ~Majority(CurStamp) /\\ pend # {}
  /\\ pend' = {}
  /\\ ackedEver' = [x \\in Blocks |-> ackedEver[x] \\cup {w.v : w \\in {u \\in pend : u.b = x /\\ u.landed # {}}}]
  /\\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
                 reverted, sb, pin, riskRec, acked, fails, badAdmit, badLoss, zx>>
  /\\ UNCHANGED <<tieVars, restamp, trustVars>>
  /\\ UNCHANGED <<stampVars, sGhost>>

\\* the zombie's raid sees its fenced legs fail and stamps its own leg
\\* (minority, never confirmed); in any order with its writes
ZombieStamp ==
  /\\ Stamp /\\ zx # 0 /\\ ~zst /\\ zm0 # {zx}
  /\\ stamp' = [stamp EXCEPT ![zx] = [seq |-> zseq, gen |-> zgen + 1, size |-> zsize, mem |-> {zx},
                                        rgen |-> zrgen, rmem |-> zrmem]]
  /\\ zst' = TRUE
  /\\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
                 reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, zx>>
  /\\ UNCHANGED <<tieVars, restamp, trustVars>>
  /\\ UNCHANGED <<nseq, aseq, agen, asize, rgenA, rmemA, hold, due, sole, zseq, zgen, zsize, zm0,
                 zrgen, zrmem, sGhost>>

\\* a stage attempt that defers still writes step 4's decree
StageDecree ==
  /\\ Stamp /\\ ~attached /\\ DecreeSet # {}
  /\\ StaleMarks(DecreeSet)
  /\\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
                 cutK, cutTgt, cutDone, cutGone, members, attached, path, mark,
                 reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, zx>>
  /\\ UNCHANGED <<used, mustRev, trustVars>>
  /\\ UNCHANGED <<stampVars, sGhost>>

\\* a zombie whose raid was already a confirmed sole continuation on z sees no
\\* failure: its write lands on z and is acknowledged (to the old consumer)
ZombieReleases == Stamp /\\ zx # 0 /\\ zm0 = {zx}
                  /\\ Valid([seq |-> zseq, gen |-> zgen, size |-> zsize, mem |-> {zx}, rgen |-> zrgen, rmem |-> zrmem])
ZombieAck(x) ==
  /\\ ZombieReleases
  /\\ ver < MaxVer /\\ ~Pending(x)
  /\\ ver' = ver + 1
  /\\ head' = [head EXCEPT ![zx][x] = ver + 1]
  /\\ hAl' = [hAl EXCEPT ![zx] = @ \\cup {x}]
  /\\ acked' = [acked EXCEPT ![x] = ver + 1]
  /\\ ackedEver' = [ackedEver EXCEPT ![x] = @ \\cup {ver + 1}]
  /\\ zAcked' = TRUE
  /\\ UNCHANGED <<hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq, cutK, cutTgt,
                 cutDone, cutGone, members, attached, path, dState, mark, reverted, sb,
                 pin, pend, fails, riskRec, badAdmit, badLoss, zx>>
  /\\ UNCHANGED <<tieVars, restamp, trustVars>>
  /\\ UNCHANGED <<stampVars, badAck, zfreed, zLeg, zjv, zexcl, postMove, legalJ, riskTail>>

\\* the raid goes away (unstage, crash): no barrier survives it
EndRaidS ==
  /\\ hold' = FALSE /\\ due' = FALSE
  /\\ UNCHANGED <<stamp, nseq, aseq, agen, asize, rgenA, rmemA, sole, zseq, zgen, zsize, zm0, zrgen, zrmem,
                 zst, sGhost>>

\\* the zombie move: the zombie raid keeps the old assembly
MoveS ==
  IF Stamp
    THEN /\\ zseq' = aseq /\\ zgen' = agen /\\ zsize' = asize /\\ zm0' = members /\\ zst' = FALSE
         /\\ zrgen' = rgenA /\\ zrmem' = rmemA
         /\\ hold' = FALSE /\\ due' = FALSE
         /\\ zLeg' = zx' /\\ zjv' = {} /\\ zexcl' = FALSE /\\ postMove' = FALSE /\\ legalJ' = [x \\in Blocks |-> {}]
         /\\ UNCHANGED <<stamp, nseq, aseq, agen, asize, rgenA, rmemA, sole, badAck, zAcked, zfreed, riskTail>>
    ELSE NoStamp

\\* the defer's bound expires: the stage serves the attachable in-sync legs on a
\\* read that is not conclusive (or names no attached leg), the assembly write
\\* marks the in-sync legs it lacks stale as any assembly does, and the record
\\* carries AckedTailRisk from then on (freshness_gate.rs ServeWithRisk)
StageRisk ==
  /\\ Stamp /\\ DeferExpires /\\ ~attached /\\ ~CanStampStage /\\ nseq < MaxAsm
  /\\ Attachable # {} /\\ Cardinality(Attachable) >= AssemblyQuorum
  /\\ attached' = TRUE
  /\\ members' = Attachable
  /\\ path' = [q \\in R |-> IF q \\in Attachable THEN "ok" ELSE path[q]]
  /\\ htr' = IF Policy = "trust" THEN [q \\in R |-> FALSE] ELSE htr
  /\\ StaleMarks(AsmExcl(Attachable))
  /\\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\\ UNCHANGED <<used, mustRev, cfgAt, conf, tr, cutAck>>
  /\\ NewAsm(Attachable)
  /\\ riskTail' = TRUE

Next ==
  \\/ \\E x \\in Blocks : Submit(x) /\\ NoStamp''')
rep('''  \\/ \\E w \\in pend : Complete(w) \\/ \\E r \\in R : Land(w, r) \\/ Drop(w, r)
  \\/ \\E r \\in R : \\E x \\in Blocks : Zombie(r, x)
  \\/ \\E r \\in R : FailBegin(r) \\/ FaultOut(r) \\/ MarkStale(r) \\/ Return(r)
  \\/ \\E e \\in Epochs : Observe(e)
  \\/ CutBegin \\/ CutRecord \\/ CutAbort \\/ CutAbortRead \\/ Retire
  \\/ \\E r \\in R : CutOne(r) \\/ \\E e \\in Epochs : GC(r, e)
  \\/ \\E r \\in R : SelectBase(r) \\/ RevertD(r) \\/ BulkCopy(r) \\/ SessionFail(r) \\/ Chase(r)
  \\/ Unstage \\/ CrashUnstage \\/ Stage \\/ ZombieMove \\/ ZombieMark \\/ ZombieHygiene
  \\/ \\E r \\in R : StageAdmit(r) \\/ AdmitRefuse(r)
''','''  \\/ \\E w \\in pend : \\/ Complete(w) /\\ badAck' = (badAck \\/ (Stamp /\\ \\E q \\in members : Core(stamp[q]) # Core(CurStamp)))
                                  /\\ UNCHANGED <<stampVars, zAcked, zfreed, zLeg, zjv, zexcl, postMove, legalJ, riskTail>>
                     \\/ \\E r \\in R : \\/ Land(w, r) /\\ NoStamp
                                     \\/ Drop(w, r) /\\ LossS(r)
  \\/ \\E r \\in R : \\E x \\in Blocks : Zombie(r, x) /\\ ~ZombieReleases /\\ zjv' = zjv \\cup {ver + 1}
                                  /\\ UNCHANGED <<stampVars, badAck, zAcked, zfreed, zLeg, zexcl, postMove, legalJ, riskTail>>
  \\/ \\E x \\in Blocks : ZombieAck(x)
  \\/ \\E r \\in R : \\/ FailBegin(r) /\\ NoStamp
                  \\/ FaultOut(r) /\\ LossS(r)
                  \\/ MarkStale(r) /\\ ~(Stamp /\\ MonitorSkipsHold /\\ hold) /\\ NoStamp
                  \\/ Return(r) /\\ NoStamp
  \\/ \\E e \\in Epochs : Observe(e) /\\ NoStamp
  \\/ (CutBegin \\/ CutRecord \\/ CutAbort \\/ CutAbortRead \\/ Retire) /\\ NoStamp
  \\/ \\E r \\in R : (CutOne(r) \\/ \\E e \\in Epochs : GC(r, e)) /\\ NoStamp
  \\/ \\E r \\in R : (SelectBase(r) \\/ RevertD(r) \\/ BulkCopy(r) \\/ SessionFail(r) \\/ Chase(r)) /\\ NoStamp
  \\/ (Unstage \\/ CrashUnstage) /\\ EndRaidS
  \\/ Stage
  \\/ ZombieMove /\\ MoveS
  \\/ ZombieMark /\\ NoStamp
  \\/ ZombieHygiene /\\ zfreed' = zx /\\ UNCHANGED <<stampVars, badAck, zAcked, zLeg, zjv, zexcl, postMove, legalJ, riskTail>>
  \\/ \\E r \\in R : StageAdmit(r) /\\ NewAsm(members') /\\ (IF Stamp THEN UNCHANGED riskTail ELSE TRUE)
  \\/ \\E r \\in R : AdmitRefuse(r) /\\ NoStamp
  \\/ StampLand \\/ Release \\/ RecordConfirm \\/ HeldFail \\/ ZombieStamp \\/ StageDecree
  \\/ StageRisk
''')
rep('''  /\\ WF_vars(CutRecord) /\\ WF_vars(CutAbort)
  /\\ \\A e \\in Epochs : WF_vars(Observe(e))
''','''  /\\ WF_vars(CutRecord) /\\ WF_vars(CutAbort)
  /\\ \\A e \\in Epochs : WF_vars(Observe(e))
  /\\ WF_vars(StampLand) /\\ WF_vars(Release) /\\ WF_vars(RecordConfirm)
''')
# invariants
rep('''Inv == Inv_Catchup /\\ Inv_AdmitFaithful /\\ Inv_MembersAgree /\\ Inv_MembersHold
''','''Inv == Inv_Catchup /\\ Inv_AdmitFaithful /\\ Inv_MembersAgree /\\ Inv_MembersHold

\\* --- the stamp's claims (§12) ----------------------------------------------------
\\* every ack happened with every member stamped with the raid's membership
Inv_AckStamped == ~badAck
\\* the zombie never acknowledges (a confirmed sole continuation on its own leg
\\* is the one case the design lets it: zAcked records it)
Inv_ZombieNoAck == ~zAcked
\\* cleanliness: a member holds only legal values (acked, torn by a failed
\\* write, or in flight): never a zombie's unacknowledged write
Inv_MembersClean ==
  \\A r \\in members : \\A x \\in Blocks : head[r][x] \\in ackedEver[x] \\cup {w.v : w \\in {u \\in pend : u.b = x}}
\\* a leg hygiene freed during the defer is stale (unless the record shows the
\\* old assembly continuing on it alone)
Inv_FreedStale == zfreed # 0 /\\ ~attached => dState[zfreed] # "insync" \\/ SoleOn(zfreed)
\\* flint-cb's line: a zombie's unacknowledged write on z at the new consumer's
\\* FIRST assembly is the write-hole class (legal); once an assembly after the
\\* move has stamped WITHOUT z, no member may ever hold the zombie's writes
\\* (rev 3; too strict: junk the first assembly took in persists legally)
Inv_NoLateJunk == zexcl => \\A r \\in members : \\A x \\in Blocks : head[r][x] \\notin zjv
\\* rev 4: no member ever holds a zombie write the first assembly after the move
\\* did not take in (junk entering later, or z rejoining dirty after it was excluded)
\\* the bound's honest claim: no acknowledged write is absent from every in_sync
\\* leg unless a serve-with-risk stage has marked the record (AckedTailRisk)
Inv_LossMarked == ~riskTail => Inv_AckedHeld
\\* flint-cb: a confirmed sole continuation leaves the record with that leg alone in_sync
Inv_SoleOnly == \\A q \\in R : SoleOn(q) /\\ dState[q] = "insync" => InSyncR = {q}
Inv_NoLateJunk2 ==
  \\A r \\in members : \\A x \\in Blocks : head[r][x] \\in zjv => postMove /\\ head[r][x] \\in legalJ[x]
\\* liveness: a barrier ends (the stamp lands and a majority releases; a
\\* minority's record is written while RecordUp)
Prop_BarrierEnds == [](attached /\\ hold => <>(~hold \\/ ~attached))
''')
# NewAsm reads head' (the first post-move assembly's junk): after the UNCHANGED that fixes it
rep("""  /\\ NewAsm(AsmSet)
  /\\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\\ UNCHANGED <<used, mustRev, cfgAt, conf, tr, cutAck>>
""","""  /\\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\\ UNCHANGED <<used, mustRev, cfgAt, conf, tr, cutAck>>
  /\\ NewAsm(AsmSet)
  /\\ IF Stamp THEN UNCHANGED riskTail ELSE TRUE
""")
# --- rev 9: step 2's even-size vouch (flint-cb, PR #43 d5c1b58b) -----------------
W = len('(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)')
def box(lines):
    return ''.join('(*' + t.ljust(W - 4) + '*)\n' for t in lines)
rep('(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)\n',
    box(['   - EvenVouch (rev 9, step 2 as amended in PR #43): the read is also',
         '     conclusive when the attached in_sync legs number more than the',
         '     stamp\'s size less a majority (one of two, two of four) and the',
         '     record confirms no continuation, newer than the stamp read, on a',
         '     leg outside them; the record keeps each confirmed continuation\'s',
         '     legs (sole holds (seq, gen, mem)); "decree" also lets that read',
         '     write step 4\'s decree;']) +
    '(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)\n')
rep('''  RelView,         \\* fix: a stamp also carries the last RELEASED generation, read
                   \\* when the stamp itself is not valid (an unconfirmed minority)
''', '''  RelView,         \\* fix: a stamp also carries the last RELEASED generation, read
                   \\* when the stamp itself is not valid (an unconfirmed minority)
  EvenVouch,       \\* step 2's even-size vouch (PR #43): "off"; "read" (the read is also
                   \\* conclusive when the attached in_sync legs number more than size
                   \\* less a majority and the record confirms no continuation newer
                   \\* than the stamp read on a leg outside them); "decree" (that read
                   \\* also writes step 4's decree)
''')
rep('''  sole,      \\* SUBSET (seq, gen): record-confirmed minority continuations''',
    '''  sole,      \\* SUBSET (seq, gen, mem): record-confirmed minority continuations''')
rep('''  /\\ sole \\subseteq (1..MaxAsm) \\X (0..(NR + 1))''',
    '''  /\\ sole \\subseteq (1..MaxAsm) \\X (0..(NR + 1)) \\X SUBSET R''')
rep('''Valid(st) == Majority(st) \\/ <<st.seq, st.gen>> \\in sole''',
    '''SolePairs == {<<c[1], c[2]>> : c \\in sole}
Valid(st) == Majority(st) \\/ <<st.seq, st.gen>> \\in SolePairs''')
rep('''SoleOn(z) == <<stamp[z].seq, stamp[z].gen>> \\in sole /\\ stamp[z].mem = {z}''',
    '''SoleOn(z) == <<stamp[z].seq, stamp[z].gen>> \\in SolePairs /\\ stamp[z].mem = {z}''')
rep('''SoleOK == <<NewestS.seq, NewestS.gen>> \\in sole /\\ NewestS.mem \\cap Attachable # {}''',
    '''SoleOK == <<NewestS.seq, NewestS.gen>> \\in SolePairs /\\ NewestS.mem \\cap Attachable # {}
\\* rev 9, step 2's even-size vouch: no majority of the stamp's size fits among the
\\* legs outside the attached in_sync set, and the record confirms no continuation
\\* newer than the stamp read on a leg outside it (VouchCond: the situation, the
\\* knob aside; the probes use it)
MajSize(n) == n \\div 2 + 1
VouchCond ==
  /\\ Cardinality(Attachable) > NewestS.size - MajSize(NewestS.size)
  /\\ ~\\E c \\in sole : Newer([seq |-> c[1], gen |-> c[2]], NewestS) /\\ c[3] \\ Attachable # {}
VouchOK == EvenVouch # "off" /\\ VouchCond''')
rep('''                  /\\ (DecreeGated => VAtt # {} /\\ (CountOK \\/ SoleOK))''',
    '''                  /\\ (DecreeGated => VAtt # {} /\\ (CountOK \\/ SoleOK \\/ (EvenVouch = "decree" /\\ VouchOK)))''')
rep('''Conclusive ==
  \\/ CountOK
  \\/ SoleOK
  \\/ IF DecreeGated THEN InSyncR \\subseteq Attachable ELSE InSyncR \\ DecreeSet \\subseteq Attachable
''', '''\\* conclusive before the vouch (rev 8)
PlainConclusive ==
  \\/ CountOK
  \\/ SoleOK
  \\/ IF DecreeGated THEN InSyncR \\subseteq Attachable ELSE InSyncR \\ DecreeSet \\subseteq Attachable
Conclusive == PlainConclusive \\/ VouchOK
''')
rep('''  /\\ sole' = sole \\cup {<<aseq, agen>>}''', '''  /\\ sole' = sole \\cup {<<aseq, agen, members>>}''')
# the ghost: a stage served by the vouch alone
rep('''  riskTail   \\* ghost: AckedTailRisk, set (for good) by a serve-with-risk stage''',
    '''  riskTail,  \\* ghost: AckedTailRisk, set (for good) by a serve-with-risk stage
  vouched    \\* ghost: a stage served on the even-size vouch alone (rev 9)''')
rep('''legalJ, riskTail>>''', '''legalJ, riskTail, vouched>>''', 6)
rep('''/\\ riskTail \\in BOOLEAN''', '''/\\ riskTail \\in BOOLEAN /\\ vouched \\in BOOLEAN''')
rep('''/\\ riskTail = FALSE''', '''/\\ riskTail = FALSE /\\ vouched = FALSE''')
rep('''sole, badAck, zAcked, zfreed, riskTail>>''', '''sole, badAck, zAcked, zfreed, riskTail, vouched>>''')
rep('''  /\\ riskTail' = TRUE''', '''  /\\ riskTail' = TRUE /\\ UNCHANGED vouched''')
rep('''  /\\ IF Stamp THEN UNCHANGED riskTail ELSE TRUE''',
    '''  /\\ IF Stamp THEN UNCHANGED riskTail /\\ vouched' = (vouched \\/ ~PlainConclusive) ELSE TRUE''')
rep('''StageAdmit(r) /\\ NewAsm(members') /\\ (IF Stamp THEN UNCHANGED riskTail ELSE TRUE)''',
    '''StageAdmit(r) /\\ NewAsm(members')
                     /\\ (IF Stamp THEN UNCHANGED riskTail /\\ vouched' = (vouched \\/ ~PlainConclusive) ELSE TRUE)''')
# --- rev 10: the TORN leg (flint-cb, after rev 9's S2Z) -----------------------------------
rep('(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)\n',
    box(['   - TornExcluded (rev 10): an attached leg whose own stamp is an',
         '     unconfirmed continuation (not valid) is TORN; it counts for the',
         '     read (its released view), but the assembly leaves it out, and',
         '     its write marks it stale, whenever a clean member of the newest',
         '     valid stamp is attached; it serves only as the sole survivor;',
         '   - ZombiePreStampLegal (rev 10): a zombie write that lands on z',
         '     before the zombie\'s stamp (the write that finds the fence, in',
         '     flight when it is found) is the write hole: a legal value, as a',
         '     crash-torn write is;']) +
    '(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)\n')
rep('''  EvenVouch,       \\* step 2's even-size vouch (PR #43): "off"; "read" (the read is also''',
    '''  TornExcluded,    \\* rev 10: a torn leg (own stamp an unconfirmed continuation) is left out
                   \\* of the assembly and marked stale while a clean member is attached
  ZombiePreStampLegal, \\* rev 10: a zombie write landed before the zombie's stamp is the
                   \\* write hole (joins ackedEver, as a crash-torn write does)
  EvenVouch,       \\* step 2's even-size vouch (PR #43): "off"; "read" (the read is also''')
rep('''  vouched    \\* ghost: a stage served on the even-size vouch alone (rev 9)''',
    '''  vouched,   \\* ghost: a stage served on the even-size vouch alone (rev 9)
  tornMarked, \\* ghost: a stage left a torn leg out (rev 10)
  tornServed \\* ghost: a stage served a torn leg (with TornExcluded: as the sole survivor)''')
rep('''riskTail, vouched>>''', '''riskTail, vouched, tornMarked, tornServed>>''', 7)
rep('''/\\ vouched \\in BOOLEAN''', '''/\\ vouched \\in BOOLEAN /\\ tornMarked \\in BOOLEAN /\\ tornServed \\in BOOLEAN''')
rep('''/\\ vouched = FALSE''', '''/\\ vouched = FALSE /\\ tornMarked = FALSE /\\ tornServed = FALSE''')
rep('''  /\\ riskTail' = TRUE /\\ UNCHANGED vouched''', '''  /\\ riskTail' = TRUE /\\ UNCHANGED <<vouched, tornMarked, tornServed>>''')
rep('''UNCHANGED riskTail /\\ vouched' = (vouched \\/ ~PlainConclusive)''', '''StageGhosts''', 2)
rep('''StampMem == NewestS.mem \\cap Attachable
''', '''StampMem == NewestS.mem \\cap Attachable
\\* rev 10: a torn leg's own stamp is an unconfirmed continuation: writes may have
\\* landed on it after its last release; the assembly leaves it out while a clean
\\* member of the newest valid stamp is attached
TornAtt == {q \\in Attachable : ~Valid(stamp[q])}
CleanMem == StampMem \\ TornAtt
''')
rep('''\\* an assembly of M stamps it (seq, 0) with a fresh seq
''', '''\\* a stamped stage's ghosts: served on the vouch alone (rev 9), a torn leg left
\\* out or served (rev 10)
StageGhosts ==
  /\\ UNCHANGED riskTail
  /\\ vouched' = (vouched \\/ ~PlainConclusive)
  /\\ tornMarked' = (tornMarked \\/ ((StampMem \\cap TornAtt) \\ AsmSet) # {})
  /\\ tornServed' = (tornServed \\/ (AsmSet \\cap TornAtt) # {})
\\* an assembly of M stamps it (seq, 0) with a fresh seq
''')
rep('''AsmSet == IF Stamp THEN StampMem ELSE Attachable''',
    '''AsmSet == IF Stamp THEN (IF TornExcluded /\\ CleanMem # {} THEN CleanMem ELSE StampMem) ELSE Attachable''')
rep('''  /\\ riskRec' = [riskRec EXCEPT ![r] = Min({@, Latest})]
  /\\ UNCHANGED <<hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq, cutK, cutTgt,
                 cutDone, cutGone, members, attached, path, dState, mark, reverted, sb,
                 pin, pend, acked, fails, badAdmit, badLoss, ackedEver, restamp, zx>>''',
    '''  /\\ riskRec' = [riskRec EXCEPT ![r] = Min({@, Latest})]
  \\* rev 10: before the zombie's stamp its landed write is the write hole
  /\\ ackedEver' = IF Stamp /\\ ZombiePreStampLegal /\\ ~zst THEN [ackedEver EXCEPT ![x] = @ \\cup {ver + 1}]
                                                         ELSE ackedEver
  /\\ UNCHANGED <<hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq, cutK, cutTgt,
                 cutDone, cutGone, members, attached, path, dState, mark, reverted, sb,
                 pin, pend, acked, fails, badAdmit, badLoss, restamp, zx>>''')
# --- rev 11: a torn leg never serves alone outside the expiry (flint-cb) -----------------------------
rep('(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)\n',
    box(['   - TornNeverSole (rev 11): with TornExcluded, an ordinary stage never',
         '     serves when every attached member of the newest valid stamp is',
         '     torn: it waits for a clean one; a torn leg serves alone only by',
         '     the expiry action (StageRisk: node-gone, the operator\'s cut),',
         '     which marks the record (AckedTailRisk) and, with TornExcluded,',
         '     leaves a torn leg out while a clean attachable leg is there;']) +
    '(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)\n')
rep('''  ZombiePreStampLegal, \\* rev 10: a zombie write landed before the zombie's stamp is the''',
    '''  TornNeverSole,   \\* rev 11: with TornExcluded, an ordinary stage never serves torn legs
                   \\* alone; only the expiry action (StageRisk) does, marking the record
  ZombiePreStampLegal, \\* rev 10: a zombie write landed before the zombie's stamp is the''')
rep('''  tornServed \\* ghost: a stage served a torn leg (with TornExcluded: as the sole survivor)''',
    '''  tornServed, \\* ghost: a stage served a torn leg (with TornExcluded: as the sole survivor)
  tornBare   \\* ghost: an ORDINARY stage served torn legs alone (no AckedTailRisk) (rev 11)''')
rep('''riskTail, vouched, tornMarked, tornServed>>''', '''riskTail, vouched, tornMarked, tornServed, tornBare>>''', 7)
rep('''/\\ tornServed \\in BOOLEAN''', '''/\\ tornServed \\in BOOLEAN /\\ tornBare \\in BOOLEAN''')
rep('''/\\ tornServed = FALSE''', '''/\\ tornServed = FALSE /\\ tornBare = FALSE''')
rep('''CanStampStage == VAtt # {} /\\ Conclusive /\\ StampMem # {} /\\ Cardinality(StampMem) >= AssemblyQuorum /\\ nseq < MaxAsm''',
    '''CanStampStage == VAtt # {} /\\ Conclusive /\\ StampMem # {} /\\ Cardinality(StampMem) >= AssemblyQuorum /\\ nseq < MaxAsm
                 /\\ (TornExcluded /\\ TornNeverSole => CleanMem # {})''')
rep('''  /\\ tornServed' = (tornServed \\/ (AsmSet \\cap TornAtt) # {})
''', '''  /\\ tornServed' = (tornServed \\/ (AsmSet \\cap TornAtt) # {})
  /\\ tornBare' = (tornBare \\/ (AsmSet # {} /\\ AsmSet \\subseteq TornAtt))
''')
rep('''StageRisk ==
  /\\ Stamp /\\ DeferExpires /\\ ~attached /\\ ~CanStampStage /\\ nseq < MaxAsm
  /\\ Attachable # {} /\\ Cardinality(Attachable) >= AssemblyQuorum
  /\\ attached' = TRUE
  /\\ members' = Attachable
  /\\ path' = [q \\in R |-> IF q \\in Attachable THEN "ok" ELSE path[q]]
  /\\ htr' = IF Policy = "trust" THEN [q \\in R |-> FALSE] ELSE htr
  /\\ StaleMarks(AsmExcl(Attachable))
  /\\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\\ UNCHANGED <<used, mustRev, cfgAt, conf, tr, cutAck>>
  /\\ NewAsm(Attachable)
  /\\ riskTail' = TRUE /\\ UNCHANGED <<vouched, tornMarked, tornServed>>''',
'''\\* rev 11: the expiry serves the attachable legs, a torn one left out (and marked
\\* stale by the assembly write) while a clean attachable leg is there
RiskSet == IF TornExcluded /\\ Attachable \\ TornAtt # {} THEN Attachable \\ TornAtt ELSE Attachable
StageRisk ==
  /\\ Stamp /\\ DeferExpires /\\ ~attached /\\ ~CanStampStage /\\ nseq < MaxAsm
  /\\ Attachable # {} /\\ Cardinality(RiskSet) >= AssemblyQuorum
  /\\ attached' = TRUE
  /\\ members' = RiskSet
  /\\ path' = [q \\in R |-> IF q \\in RiskSet THEN "ok" ELSE path[q]]
  /\\ htr' = IF Policy = "trust" THEN [q \\in R |-> FALSE] ELSE htr
  /\\ StaleMarks(AsmExcl(RiskSet))
  /\\ UNCHANGED <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded,
                 nextSeq, cutK, cutTgt, cutDone, cutGone, mark, reverted,
                 sb, pin, pend, acked, fails, riskRec, badAdmit, badLoss, ackedEver, zx>>
  /\\ UNCHANGED <<used, mustRev, cfgAt, conf, tr, cutAck>>
  /\\ NewAsm(RiskSet)
  /\\ riskTail' = TRUE /\\ UNCHANGED <<vouched, tornBare>>
  /\\ tornMarked' = (tornMarked \\/ ((Attachable \\cap TornAtt) \\ RiskSet) # {})
  /\\ tornServed' = (tornServed \\/ (RiskSet \\cap TornAtt) # {})''')
rep('''Inv_LossMarked == ~riskTail => Inv_AckedHeld
''', '''Inv_LossMarked == ~riskTail => Inv_AckedHeld
\\* rev 11: torn legs serve alone only through the expiry, which marks the record
Inv_TornMarked == ~tornBare
''')
# --- rev 12: the torn junk itself (flint-cb: a data invariant that separates TornNeverSole from its absence) ------
rep('(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)\n',
    box(['   - Inv_TornJunkMarked (rev 12): while AckedTailRisk is unset, no',
         '     member of the live assembly holds a write the fenced writer (the',
         '     zombie) made AFTER its stamp (ghost zpost, cumulative over moves);',
         '     the write that lands before the stamp is the write hole and is',
         '     not in zpost;']) +
    '(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)\n')
rep('''  tornBare   \\* ghost: an ORDINARY stage served torn legs alone (no AckedTailRisk) (rev 11)''',
    '''  tornBare,  \\* ghost: an ORDINARY stage served torn legs alone (no AckedTailRisk) (rev 11)
  zpost      \\* ghost: the versions a zombie wrote AFTER its stamp, every move (rev 12)''')
rep('''tornServed, tornBare>>''', '''tornServed, tornBare, zpost>>''', 7)
rep('''/\\ tornBare \\in BOOLEAN''', '''/\\ tornBare \\in BOOLEAN /\\ zpost \\subseteq 1..MaxVer''')
rep('''/\\ tornBare = FALSE''', '''/\\ tornBare = FALSE /\\ zpost = {}''')
rep('''  /\\ tornBare' = (tornBare \\/ (AsmSet # {} /\\ AsmSet \\subseteq TornAtt))
''', '''  /\\ tornBare' = (tornBare \\/ (AsmSet # {} /\\ AsmSet \\subseteq TornAtt))
  /\\ UNCHANGED zpost
''')
rep('''  /\\ riskTail' = TRUE /\\ UNCHANGED <<vouched, tornBare>>''', '''  /\\ riskTail' = TRUE /\\ UNCHANGED <<vouched, tornBare, zpost>>''')
rep('''Zombie(r, x) /\\ ~ZombieReleases /\\ zjv' = zjv \\cup {ver + 1}
                                  /\\ UNCHANGED <<stampVars, badAck, zAcked, zfreed, zLeg, zexcl, postMove, legalJ, riskTail, vouched, tornMarked, tornServed, tornBare, zpost>>''',
    '''Zombie(r, x) /\\ ~ZombieReleases /\\ zjv' = zjv \\cup {ver + 1}
                                  /\\ zpost' = (IF Stamp /\\ zst THEN zpost \\cup {ver + 1} ELSE zpost)
                                  /\\ UNCHANGED <<stampVars, badAck, zAcked, zfreed, zLeg, zexcl, postMove, legalJ, riskTail, vouched, tornMarked, tornServed, tornBare>>''')
rep('''Inv_TornMarked == ~tornBare
''', '''Inv_TornMarked == ~tornBare
\\* rev 12: the torn junk itself: while the record carries no AckedTailRisk, no member
\\* holds a write the fenced writer made after its stamp (the pre-stamp write, the
\\* write hole, is not in zpost)
Inv_TornJunkMarked == ~riskTail => \\A r \\in members : \\A x \\in Blocks : head[r][x] \\notin zpost
''')
# --- rev 13 (10-31, the gate's item 3): replica ids may be MODEL VALUES, which tla-mc's (and TLC's) SYMMETRY
# needs; every rev 12 run (integer ids) is unchanged.  0 stays the "none" sentinel of zx, zfreed and zLeg (an untyped
# model value is unequal to 0); First (Min over replica ids) is reached only with CoverageWalk FALSE, never in an S arm.
rep('(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)\n',
    box(['   - rev 13: the replica ids need not be naturals (model values, for',
         '     SYMMETRY over them); 0 stays the "none" of zx, zfreed and zLeg;',
         '     First is used only with CoverageWalk FALSE;']) +
    '(*   - Capture, AckAfterMark: off in every stamp arm (the stamp replaces  *)\n')
rep('ASSUME Reps \\subseteq Nat /\\ 0 \\notin Reps', 'ASSUME 0 \\notin Reps   \\* rev 13: ids may be model values (SYMMETRY); 0 = none')
open(f'{D}/FlintCatchupS.tla', 'w').write(s)
print("wrote", len(s.splitlines()))
