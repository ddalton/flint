# gen-FlintCatchupSProof.py: build FlintCatchupSProof.tla (the stamp proof, gate item 2) from FlintCatchupTProof.tla
# (cf71b06a): T's preamble and type layer (LEMMA MinProp .. the frames) are copied with TypeOK -> TypeOKT (T's own
# TypeOK, the T variables of S) and Clean -> Full (c27's setting); the S parts are written here.  Never hand-edit the
# output: edit this file or the S part files it reads.
import os, re, hashlib, glob
D = os.path.expanduser('~/github/flint-rebuild-proof/formal')
t = open(f'{D}/FlintCatchupTProof.tla').read()
tm = open(f'{D}/FlintCatchupT.tla').read()
assert hashlib.md5(t.encode()).hexdigest().startswith('cf71b06a'), 'T proof md5'
i = t.index('LEMMA MinProp ==')
j = t.index('(* Frames: a part of the invariant carries over when what it reads does.')
j = t.rindex('-----', 0, j)
body = t[i:j]
body = re.sub(r'\bTypeOK\b', 'TypeOKT', body)
body = re.sub(r'\bClean\b', 'Full', body)

# --- S fixes to T's type layer (t1, 2026-10-31: 6/1513 failed, all here) -------------------------------------
def block(txt, start):
    a = txt.index(start); b = txt.find('\nLEMMA ', a + 1); c = txt.find('\n-----', a + 1)
    b = min(x for x in (b, c) if x != -1)
    return a, b + 1
# Unstage: Full has Capture FALSE, so the record's view keeps dState (T's Clean took the StaleMarks branch)
a, b = block(body, 'LEMMA T_Unstage ==')
body = body[:a] + r"""LEMMA T_Unstage ==
  ASSUME TypeOKT, Unstage PROVE TypeOKT'
<1>0. TyScalar /\ TySets /\ TyLeg /\ TyHead /\ TySnap /\ TyPend /\ TyTie /\ TyTrust BY Groups
<1>1. /\ UNCHANGED <<dState, tainted, floor, restamp>>
      /\ conf' = [e \in Epochs |-> TRUE] /\ tr' = tr /\ htr' = [q \in R |-> FALSE]
  BY Full DEF Unstage
<1>u. UNCHANGED <<ver, nextSeq, cutK, fails, badAdmit, badLoss, zx>>
      /\ UNCHANGED <<recorded, cutTgt, cutDone, cutGone>>
      /\ UNCHANGED <<path, mark, reverted, sb, pin, riskRec, used, mustRev>>
      /\ UNCHANGED <<head, hAl, hPar>> /\ UNCHANGED <<sPres, sCon, sAl, sPar, anc>>
      /\ UNCHANGED <<pend, acked, ackedEver>> /\ UNCHANGED <<cfgAt, cutAck>>
  BY DEF Unstage
<1>2. TyLeg' /\ TyTie' BY <1>0, <1>1, <1>u DEF TyLeg, TyTie
<1>3. TyScalar' BY <1>0, <1>u DEF Unstage, TyScalar
<1>4. TySets' BY <1>0, <1>u DEF Unstage, TySets
<1>5. TyTrust' BY <1>0, <1>1, <1>u DEF TyTrust
<1>. QED BY <1>0, <1>u, <1>2, <1>3, <1>4, <1>5, C_Head, C_Snap, C_Pend, GroupsP
""" + body[b:]
# Stage: S assembles AsmSet (the stamp's members) and marks AsmMarks(AsmSet)
a, b = block(body, 'LEMMA T_Stage ==')
st = body[a:b]
for o, n in (('StaleMarks(AsmExcl(Attachable)) BY DEF Stage', 'StaleMarks(AsmMarks(AsmSet)) BY DEF Stage'),
             ("TySets' BY <1>0, <1>u DEF Stage, TySets, Attachable, InSyncR", "TySets' BY <1>0, <1>u, AsmSetSub DEF Stage, TySets")):
    assert st.count(o) == 1, o; st = st.replace(o, n)
body = body[:a] + st + body[b:]
# StageAdmit: T == AsmSet, M == AsmMarks(AsmSet)
a = body.index("(* StageAdmit's LET, named"); b = body.index('(* A snapshot chain, in general')
sa = body[a:b]
for o, n in (('AdmT == Attachable\n', 'AdmT == AsmSet\nAdmM == AsmMarks(AsmSet)\n'),
             ('AsmExcl(AdmT)', 'AdmM'),
             ('<1>3. s \\in R BY DEF Attachable, InSyncR', '<1>3. s \\in R BY <1>2, AsmSetSub'),
             ('<1>4. AdmT \\subseteq R BY DEF AdmT, Attachable, InSyncR', '<1>4. AdmT \\subseteq R BY AsmSetSub DEF AdmT'),
             ('DEF StageAdmit, AdmRest, AdmT, AdmK, tieVars', 'DEF StageAdmit, AdmRest, AdmT, AdmM, AdmK, tieVars')):
    assert sa.count(o) >= 1, o; sa = sa.replace(o, n)
sa = re.sub(r'\bAttachable\b', 'AsmSet', sa)
# StageAdmitFacts' PICK: S's StageAdmit is T's plus the stamp guard, and every backend fails T's PICK on it (s1/s2,
# p1-p3 on c33: SMT timeout at 180 s, zenon out of memory, isabelle false).  So: <1>2a PICKs s with the model's own
# body (k := nextSeq, T := AsmSet; a syntactic match), and <1>2 states T's simplified body of that s from it.
ms = open(f'{D}/sproof/model/FlintCatchupS-r17.tla').read()   # the proof's model: rev 17, pinned
assert hashlib.md5(ms.encode()).hexdigest().startswith('61ae6829'), 'S model md5 (the StageAdmit body is read from it)'
a0 = ms.index('\nStageAdmit(r) =='); b0 = ms.index('\n\n', a0)
sad = ms[a0:b0]
i0 = sad.index('        /\\ \\E s \\in (IF Policy = "trust"'); j0 = sad.index('        /\\ recorded\' = recorded')
ex = '\n'.join(l for l in sad[i0:j0].split('\n') if not l.strip().startswith('\\*'))
ex = re.sub(r'\bk\b', 'nextSeq', ex); ex = re.sub(r'\bT\b', 'AsmSet', ex)
ex = ex.replace('        /\\ \\E s \\in', '<1>2a. PICK s \\in', 1)
ex = re.sub(r'\) :\n', ') :\n', ex, count=1)
o1 = "<1>2. PICK s \\in {q \\in AsmSet : LinCovers(q, mark[r])} :\n"
assert sa.count(o1) == 1; sa = sa.replace(o1, ex.rstrip() + "\n  BY DEF StageAdmit\n<1>2. s \\in {q \\in AsmSet : LinCovers(q, mark[r])} /\\\n")
o2 = "  BY <1>1 DEF StageAdmit\n<1>3. s \\in R"
assert sa.count(o2) == 1; sa = sa.replace(o2, "  BY <1>1, <1>2a\n<1>3. s \\in R")
body = body[:a] + sa + body[b:]
k = tm.index('\nTypeOK ==') + 1
ty = tm[k:tm.index('\n\n', k)]
assert ty.startswith('TypeOK ==')
tyT = 'TypeOKT ==' + ty[len('TypeOK =='):]
head = r'''------------------------- MODULE FlintCatchupSProof -------------------------
(***************************************************************************)
(* TLAPS proof of the membership stamp (FlintCatchupS rev 17, design doc  *)
(* rev 8 section 12) for every bound, in c27's setting with the consumer  *)
(* crash and the zombie move ON (gate item 2; plan:                       *)
(* FlintCatchupSProof-PLAN.md).  Generated by gen-FlintCatchupSProof.py: *)
(* FlintCatchupT's preamble and type layer (FlintCatchupTProof.tla        *)
(* cf71b06a) with TypeOK -> TypeOKT, Clean -> Full; the stamp's parts     *)
(* after it.  WORK IN PROGRESS: nothing here is cited until a record run. *)
(***************************************************************************)
EXTENDS FlintCatchupSInd, TLAPS, NaturalsInduction, FiniteSetTheorems

\* (the model's own unnamed ASSUMEs, restated so a proof can cite them)
ASSUME Const ==
  /\ MaxVer \in Nat /\ MaxEpoch \in Nat /\ MaxFails \in Nat /\ MaxAsm \in Nat /\ MaxAsm >= 1
  /\ Retain \in Nat /\ Retain >= 1
  /\ Reps \subseteq Nat /\ 0 \notin Reps /\ IsFiniteSet(Reps) /\ Reps # {}

\* c27's setting (the gate's arms): every knob as FlintCatchupX*.cfg sets it
ASSUME Full ==
  /\ Policy = "trust" /\ Capture = FALSE /\ Crash = TRUE /\ ZombieMoves = TRUE
  /\ ZombieWhere = "colocated" /\ RaidFailsAtDrop = TRUE /\ StageOracle = FALSE
  /\ WriteQuorum = 1 /\ AssemblyQuorum = 1 /\ BaseInclusive = TRUE
  /\ CutStampsAll = FALSE /\ PinRetention = TRUE
  /\ Backoff = TRUE /\ CoverageWalk = TRUE /\ MarkGate = TRUE /\ ReadFails = FALSE /\ LocalLegGuard = TRUE
  /\ FenceMark = FALSE /\ CatchupWaitsCut = FALSE /\ AssemblyMark = TRUE /\ StageDefer = TRUE /\ AckAfterMark = FALSE
  /\ Stamp = TRUE /\ RecordUp = TRUE /\ MajorityRead = "count" /\ Decree = TRUE /\ DeferExpires = FALSE
  /\ MonitorSkipsHold = FALSE /\ DecreeGated = TRUE /\ DecreeSparesLast = TRUE /\ RelView = TRUE
  /\ EvenVouch = "decree" /\ TornExcluded = TRUE /\ TornNeverSole = TRUE /\ ZombiePreStampLegal = TRUE
  \* the in-write rule (the user, 10-31: "adopt the in-write rule and the whole mark"), in place of the premise that
  \* the last healthy member never fails; the drain with its stamp (the user, 10-31: "adopt the planned-removal stamp")
  /\ LastFails = TRUE /\ WholeMark = TRUE /\ InWriteGuard = TRUE /\ MonitorGuard = TRUE
  /\ OperatorMark = TRUE /\ DrainStamps = TRUE
  \* the zombie acks only on the continuation confirmed for it alone (rev 17; c33b pz1's four-replica trace)
  /\ ZombieTriple = TRUE

\* S's TypeOK = FlintCatchupT's TypeOK (the T variables) /\ the stamp's
''' + tyT + r'''
TyStampS ==
  /\ stamp \in [R -> StampRec]
  /\ nseq \in 1..MaxAsm /\ aseq \in 1..MaxAsm /\ agen \in 0..(NR + 1) /\ asize \in 0..NR
  /\ rgenA \in 0..(NR + 1) /\ rmemA \in SUBSET R /\ zrgen \in 0..(NR + 1) /\ zrmem \in SUBSET R
  /\ hold \in BOOLEAN /\ due \in BOOLEAN
  /\ sole \subseteq (1..MaxAsm) \X (0..(NR + 1)) \X SUBSET R
  /\ zseq \in 0..MaxAsm /\ zgen \in 0..(NR + 1) /\ zsize \in 0..NR /\ zm0 \in SUBSET R /\ zst \in BOOLEAN
  /\ badAck \in BOOLEAN /\ zAcked \in BOOLEAN /\ zfreed \in R \cup {0}
  /\ zLeg \in R \cup {0} /\ zjv \subseteq Vers /\ zexcl \in BOOLEAN
  /\ postMove \in BOOLEAN /\ legalJ \in [Blocks -> SUBSET Vers] /\ riskTail \in BOOLEAN /\ vouched \in BOOLEAN
  /\ tornMarked \in BOOLEAN /\ tornServed \in BOOLEAN /\ tornBare \in BOOLEAN /\ zpost \subseteq 1..MaxVer

LEMMA TypeOKParts == TypeOK <=> TypeOKT /\ TyStampS
BY DEF TypeOK, TypeOKT, TyStampS

-----------------------------------------------------------------------------
(* FlintCatchupT's preamble and type layer, for the T variables (copied).  *)

'''
# T_Path's MarkStale case: rev 14+ marks every in_sync leg the live raid lacks under WholeMark (inv-path, 10-31)
a, b = block(body, 'LEMMA T_Path ==')
tp = body[a:b]
o1 = "          /\\ StaleMarks({r})\n      BY <2>3 DEF MarkStale"
n1 = "          /\\ StaleMarks(IF WholeMark THEN {q \\in InSyncR : q \\notin members} ELSE {r})\n      BY <2>3 DEF MarkStale"
assert tp.count(o1) == 1, 'T_Path MarkStale'
body = body[:a] + tp.replace(o1, n1) + body[b:]
# two copied steps only Isabelle proves in S's context (SMT cannot instantiate SmallestNatural's P(_); Zenon gives up
# on both): IsaT, not SMTT.  Isabelle fails at once on the EC2 instances (c31, c33b: never proved one obligation), so
# these two are checked on the box (full1 on c31: lines 131, 1262 under SMTT(120))
for lem, o1, n1 in (('LEMMA MinProp ==', '    BY <2>0, SmallestNatural\n', '    BY <2>0, SmallestNatural, IsaT(300)\n'),
                    ('LEMMA ReadAt ==', '  BY ReadThrough\n', '  BY ReadThrough, IsaT(300)\n')):
    a, b = block(body, lem); seg = body[a:b]
    assert seg.count(o1) == 1, lem
    body = body[:a] + seg.replace(o1, n1) + body[b:]
# overrides: sproof/ovr/<Lemma>.tla replaces that copied lemma's block (LEMMA <Lemma> == .. the next LEMMA or ----)
for f in sorted(glob.glob(f'{D}/sproof/ovr/*.tla')):
    nm = os.path.basename(f)[:-4]; a, b = block(body, f'LEMMA {nm} ==')
    body = body[:a] + open(f).read().rstrip('\n') + '\n\n' + body[b:]
prelude = r"""
\* (S) the stage's assembly lies in the attachable legs, its marks in R
LEMMA AsmSetSub == AsmSet \subseteq Attachable /\ Attachable \subseteq R /\ AsmMarks(AsmSet) \subseteq R
BY Full DEF AsmSet, CleanMem, StampMem, Attachable, InSyncR, AsmMarks, AsmExcl, DecreeSet

\* (S) TyStampS in two groups, as T's: the stamp variables, the ghosts
TyStA ==
  /\ stamp \in [R -> StampRec]
  /\ nseq \in 1..MaxAsm /\ aseq \in 1..MaxAsm /\ agen \in 0..(NR + 1) /\ asize \in 0..NR
  /\ rgenA \in 0..(NR + 1) /\ rmemA \in SUBSET R /\ zrgen \in 0..(NR + 1) /\ zrmem \in SUBSET R
  /\ hold \in BOOLEAN /\ due \in BOOLEAN
  /\ sole \subseteq (1..MaxAsm) \X (0..(NR + 1)) \X SUBSET R
  /\ zseq \in 0..MaxAsm /\ zgen \in 0..(NR + 1) /\ zsize \in 0..NR /\ zm0 \in SUBSET R /\ zst \in BOOLEAN
TyGh ==
  /\ badAck \in BOOLEAN /\ zAcked \in BOOLEAN /\ zfreed \in R \cup {0}
  /\ zLeg \in R \cup {0} /\ zjv \subseteq Vers /\ zexcl \in BOOLEAN
  /\ postMove \in BOOLEAN /\ legalJ \in [Blocks -> SUBSET Vers] /\ riskTail \in BOOLEAN /\ vouched \in BOOLEAN
  /\ tornMarked \in BOOLEAN /\ tornServed \in BOOLEAN /\ tornBare \in BOOLEAN /\ zpost \subseteq 1..MaxVer
LEMMA TyStampSplit == TyStampS <=> TyStA /\ TyGh BY DEF TyStampS, TyStA, TyGh
LEMMA TyStampSplitP == TyStampS' <=> TyStA' /\ TyGh' BY DEF TyStampS, TyStA, TyGh
LEMMA TypeOKPartsP == TypeOK' <=> TypeOKT' /\ TyStampS' BY DEF TypeOK, TypeOKT, TyStampS
\* the stamp groups an action leaves alone carry over
LEMMA C_StA == TyStA /\ UNCHANGED stampVars => TyStA' BY DEF TyStA, stampVars
LEMMA C_Gh == TyGh /\ UNCHANGED sGhost => TyGh' BY DEF TyGh, sGhost
LEMMA C_NoStamp == TyStampS /\ NoStamp => TyStampS' BY TyStampSplit, TyStampSplitP, C_StA, C_Gh DEF NoStamp

"""
# the S parts (formal/sproof/NN-*.tla-part, in name order; SPROOF_PARTS=a,b keeps only those whose name contains one)
import sys, glob
want = [w for w in os.environ.get('SPROOF_PARTS', '').split(',') if w]
parts = ''
for f in sorted(glob.glob(f'{D}/sproof/*.tla-part')):
    if want and not any(w in os.path.basename(f) for w in want): continue
    parts += '\n-----------------------------------------------------------------------------\n' + open(f).read().rstrip('\n') + '\n'
out = head + prelude + body + parts + """-----------------------------------------------------------------------------
=============================================================================
"""
dst = sys.argv[1] if len(sys.argv) > 1 else f'{D}/FlintCatchupSProof.tla'
open(dst, 'w').write(out)
print('wrote', len(out.splitlines()), 'lines; T section', body.count('\\n'), 'md5', hashlib.md5(out.encode()).hexdigest()[:8])
