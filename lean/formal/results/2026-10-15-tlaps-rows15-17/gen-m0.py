#!/usr/bin/env python3
"""(v4, 2026-10-15: rows 15-17 -- the 21 new Writer fields, the edited steps, the six
new steps; copied from results/2026-10-06-tlaps-typeok/gen-proof.py.)
Emit lean/formal/LeanP1Proof.tla (M0: Spec => []TypeOK) with one uniform
lemma shape per step: hidden facts cited where needed, never the whole
invariant expanded into a step's hypotheses."""
import sys, os, re
OUT = sys.argv[1]
def _split_top(s, sep=','):
    out, d, cur, i = [], 0, '', 0
    while i < len(s):
        if s.startswith('<<', i): d += 1; cur += '<<'; i += 2; continue
        if s.startswith('>>', i): d -= 1; cur += '>>'; i += 2; continue
        c = s[i]
        if c in '([{': d += 1
        if c in ')]}': d -= 1
        if d == 0 and c == sep: out.append(cur); cur = ''; i += 1; continue
        cur += c; i += 1
    out.append(cur); return out
_anc = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'LeanP1Anc.tla')).read()
_m = re.search(r'^Writer ==\n(.*?)\n(?=\S)', _anc, re.S | re.M)
_wtext = re.sub(r'[ \t]*\\\*[^\n]*', '', _m.group(1)).strip()
WRITERT = 'WriterT ==\n' + re.sub(r'[ \t]*\\\*[^\n]*', '', _m.group(1))
WRITERT = '\n'.join(l for l in WRITERT.split('\n') if l.strip())
for _f, _ty in [('pend', 'PendT'), ('ack', 'AckT'), ('car', 'CarT'), ('bound', 'BoundT')]:
    _n = len(re.findall(r'\b%s : \[' % _f, WRITERT)); assert _n == 1, (_f, _n)
    _i = WRITERT.index(_f + ' : ['); _d = 0
    for _j in range(_i + len(_f) + 3, len(WRITERT)):
        _d += {'[': 1, ']': -1}.get(WRITERT[_j], 0)
        if _d == 0: break
    WRITERT = WRITERT[:_i] + _f + ' : ' + _ty + WRITERT[_j + 1:]
# Each Writer field's type, as WriterT names it.
FTYPE = {}
for _kv in _split_top(WRITERT[WRITERT.index('[') + 1:WRITERT.rindex(']')]):
    _k, _, _v = _kv.partition(' : ')
    FTYPE[_k.strip()] = ' '.join(_v.split())

def RECOP(rec):
    """A record `rec` (`XW(args)`) whose changed fields are IF expressions or other
    terms: tlapm lifts a field's IF out of the record, so a 48-field membership with
    k of them is 2^k records (2026-10-15: Skip, PullOnly, Finish, Install timed out
    even alone).  `XRec(args, v...)` is the record with each changed field a
    parameter; `XRecType` its membership from the parameters' types (fast: no IF);
    the step proves each changed field's type, then the membership is an instance.
    Returns (definitions + lemma, [(field, value, type)], instance term)."""
    name = rec.split('(')[0]; args = rec[len(name) + 1:-1]
    m = re.search(r'^' + re.escape(rec) + r' ==\n(.*?)\n(?=\S)', _anc, re.S | re.M)
    body = m.group(1).strip()
    assert body.startswith('[') and body.endswith(']'), rec
    parts = _split_top(body[1:-1])
    fields = []
    for part in parts:
        k, _, v = part.partition('|->')
        fields.append((k.strip(), ' '.join(v.split())))
    assert [f for f, _ in fields] == list(FTYPE), (rec, [f for f, _ in fields][:5])
    # A constant field stays literal in XRec (XRecType types it at no cost):
    # the instance's parameters are the state-dependent fields only (Finish
    # with all 21 changed fields as parameters ran past 60 s, 2026-10-15).
    CONST = re.compile(r'^("[a-z-]+"|\{\}|FALSE|TRUE|\d+|\[p \\in Paths \|-> Nil\])$')
    ch = [(f, v) for f, v in fields if v != f'w[s].{f}' and not CONST.match(v)]
    kept = {f: v for f, v in fields if v != f'w[s].{f}' and CONST.match(v)}
    pn = {f: 'v' + f[0].upper() + f[1:] for f, _ in ch}
    opn = name[:-1] + 'Rec'
    sig = f'{opn}(' + ', '.join([args] + [pn[f] for f, _ in ch]) + ')'
    lines = [f'   {f} |-> ' + (pn[f] if f in pn else kept.get(f, f'w[s].{f}')) for f, _ in fields]
    d = f'{sig} ==\n  [' + ',\n'.join(lines).lstrip() + ']\n'
    asm = ''.join(f', NEW {pn[f]} \\in {FTYPE[f]}' for f, _ in ch)
    d += f'LEMMA {opn}Type ==\n  ASSUME NEW s \\in Writers, w[s] \\in Writer{asm}\n  PROVE  {sig} \\in Writer\nBY WriterFields, WriterTEq DEF {opn}, WriterT, Opt\n\n'
    inst = f'{opn}({args}, ' + ', '.join(v for _, v in ch) + ')'
    return d, [(f, v, FTYPE[f]) for f, v in ch], inst, opn

def RECMEM(rec, n, facts, cites, prefix=None, memlabel=None):
    """The membership of `rec` at step <1>n via RECOP: one step per changed field
    (a barrier-end fact when one states it), then the instance and the equality.
    A field whose value has an IF is NAMED first (a proof-local DEFINE, opaque
    until expanded): the instance then has no IF to lift (2026-10-15: Finish's,
    with five, ran past 60 s).  Returns (text, next n, the membership's step, defs)."""
    d, ch, inst, opn = RECOP(rec)
    st = ''; mine = []; args = []; names = []
    for f, v, ty in ch:
        nm = None
        if 'IF ' in v:
            nm = 'f' + opn[:-3] + f[0].upper() + f[1:]
            st += f'<1> DEFINE {nm} == {v}\n'; names.append(nm)
        a = nm or v; args.append(a)
        hit = [c for (txt, _), c in zip(facts, cites) if txt.startswith(f'({v}) \\in')]
        lab = f'<1>{prefix}{n}' if prefix else f'<1>{n}'
        dn = f', {nm}' if nm else ''
        if re.search(r'\bNo(Pend|Ack|Car)\b', v):   # the empty record and its type
            dn += ', NoPend, NoAck, NoCar, PendT, AckT, CarT'
        if hit:
            if not nm:
                mine.append(hit[0]); continue
            st += f'{lab}. {nm} \\in {ty} BY {hit[0]} DEF {nm}\n'
        else:
            need = '<1>0, <1>d, WriterFields' if ('w[s]' in v or 'doc' in v or 'seq' in v) else '<1>d'
            st += f'{lab}. ({a}) \\in {ty}\n  BY {need}, {", ".join(cites) + ", " if cites else ""}ShippedShape DEF Opt, Shipped{dn}\n'
        mine.append(lab); n += 1
    inst = f'{opn}(' + ', '.join([rec[len(rec.split("(")[0]) + 1:-1]] + args) + ')'
    lab = f'<1>{prefix}{n}' if prefix else f'<1>{n}'
    st += f'{lab}. {inst} \\in Writer\n  BY ' + ', '.join(['<1>0'] + mine + [f'{opn}Type']) + '\n'; a = lab; n += 1
    lab = memlabel or f'<1>{n}'
    st += f'{lab}. {rec} \\in Writer BY {a} DEF {rec.split("(")[0]}, {opn}{"".join(", " + x for x in names)}\n'; mem = n; n += 1
    return st, n, mem, d

HEADER = r'''---------------------------- MODULE LeanP1Proof ------------------------------
(* The TLAPS proof module for the lean protocol (docs/plans/
   lean-tlaps-inductive-proof-plan.md).  It EXTENDS LeanP1Anc.tla, the copy
   of LeanP1.tla that tlapm can read (every writer step's new tree an
   explicit record `<Step>W(..)`; tied to the original by exact distinct
   count: results/2026-10-06-tlaps-typeok/RESULTS.txt), and proves its
   theorems under the shipped shape: every rule constant TRUE (`Shipped`).

   M0 (this file's first theorem): `Spec => []TypeOK`.  TypeOK is not
   inductive on its own; `IndTypeOK` is, and it adds:
     - the three upload ghosts TypeOK leaves untyped (`upped`, `copies`,
       `orig`; `copies` is the generation of a copy handle),
     - `mv = Nil` (the plan's I0: with RenameAtomic the second CAS never
       runs, and ITS `CHOOSE q \in Paths : ...` is what would break
       `acked \subseteq Paths \X Handles`) -- the one conjunct of `Shipped`
       this theorem uses,
     - `nextGen > Seed`, so a minted handle is in `Handles`,
     - `Minted`: the document, a save in flight and every tree name only
       minted handles, and an upload's snapshot is a minted handle -- what
       `live \subseteq minted` needs at `Upload`.
   Every step lemma has the same shape: TypeOK, Ghosts and Minted taken
   from IndTypeOK as three hidden facts; the step's primed variables from
   its definition; the step's record in `Writer` from the types of what it
   reads (Z3, ~10 s); the frame conjuncts from `FrameTypeOK`; `Minted` from
   the record's `local`, `uploads` and `snap`.  A step cites only the facts
   it needs: an obligation that carries the whole invariant expanded puts
   Z3 and tlapm's encoder past their limits on goals they close in seconds
   otherwise (results/2026-10-06-tlaps-typeok/NOTES.txt).  What TLC reads
   from the cfg and a proof must be told is stated here as named ASSUMEs
   (tlapm does not use an unnamed one: micro/Asm.tla), including the six
   LeanP1Anc.tla states without names.                                     *)
EXTENDS LeanP1Anc, TLAPS, FiniteSetTheorems

------------------------------------------------------------------------------
(* What a proof must be told.                                               *)

\* The shipped shape: every rule constant TRUE (plan section 2).
Shipped ==
  /\ CommitSurfacesForeign /\ CommitVerifiesUploads /\ SweepUnderLease
  /\ CollectorSparesCited /\ CommitRecordsDeleteOverride /\ DeleteWinsPreserved
  /\ ContentConverges /\ RecheckSkipped /\ CommitAdvanceGuarded /\ RetireAge
  /\ GatewayIgnoresLease /\ GatewayJudgesRead /\ GatewaySweepGrace /\ RenameAtomic
  /\ ConsumeHonorsScope /\ RescopeUnciteFirst /\ RescopeKeepsDirty /\ WidenKeepsLocal
  /\ UnlinkChecksBytes /\ ConsumeKeepsLeft /\ SyncKeepsLeft /\ ReaderRechecksOwed
  /\ InstallFenced /\ DeletesUnderLease
ASSUME ShippedShape == Shipped

\* LeanP1Anc.tla's own assumptions, named so a proof can cite them.
ASSUME FreePaths     == Free \subseteq Paths
ASSUME MaxMintNat    == MaxMint \in Nat /\ MaxMint >= 1
ASSUME ScopesPaths   == Scopes \subseteq SUBSET Paths /\ Scopes # {}
ASSUME ReadersWriters == Readers \subseteq Writers
ASSUME NilHandle     == Nil \notin Handles
ASSUME NoneWriter    == "none" \notin Writers
\* ...and what the cfg's finite constants give TLC for free.
ASSUME MaxCopiesNat  == MaxCopies \in Nat
ASSUME PathsFinite   == IsFiniteSet(Paths)
ASSUME TouchNat      == MaxTouches \in Nat /\ MaxNonces \in Nat /\ MaxNonces >= 1 /\ MaxDeposals \in Nat

\* The rows 15-17 fields' types (LeanP1.tla `Writer`), named so a record goal
\* cites them.
PendT == [id : Nat, fold : Nat, nonces : SUBSET Nat]
AckT == [st : {"none", "ok", "partial", "refused-read-only"}, nonces : SUBSET Nat, seq : Nat,
         src : Clocks \cup {"none"}, dropped : SUBSET Paths, inst : BOOLEAN]
CarT == [id : Nat, paths : SUBSET Paths, deletes : SUBSET Paths,
         dropped : SUBSET Paths, uncited : SUBSET Paths, pub : [Paths -> Opt(Handles)]]
BoundT == [id : Nat, fold : Nat, doc : [Paths -> Opt(Handles)], seq : Nat,
           wd : SUBSET Paths, inst : BOOLEAN, clk : Clocks \cup {"none"}]

\* `Writer` with those four types NAMED: a record goal over WriterT checks
\* `pend \in PendT` as one atom, not seven nested fields (run 1: three of
\* the barrier ends' memberships timed out expanding them).
@@WRITERT@@
LEMMA WriterTEq == Writer = WriterT
BY DEF Writer, WriterT, PendT, AckT, CarT, BoundT

------------------------------------------------------------------------------
(* The inductive invariant for TypeOK.                                      *)

\* The upload ghosts TypeOK leaves untyped.
Ghosts == upped \subseteq Handles /\ copies \in Nat /\ orig \in [Handles -> Opt(Handles)]
\* Every handle the document, a save in flight or a tree names is minted;
\* an upload's snapshot is a minted handle (the upload re-cites it in `live`).
Minted ==
  /\ \A p \in Paths : doc[p] \in Opt(minted)
  /\ \A p \in Paths : gw[p] \in Opt(minted)
  /\ \A s \in Writers, p \in Paths : w[s].local[p] \in Opt(minted)
  /\ \A s \in Writers : \A p \in w[s].uploads : w[s].snap[p] \in minted
IndTypeOK == TypeOK /\ Ghosts /\ mv = Nil /\ nextGen > Seed /\ Minted

\* What every gateway or writer step conjoins (`Next`'s first disjunct).
Frame == TookUpdate /\ RetUpdate /\ AncUpdate /\ UNCHANGED <<aged, ages, rdoc, rlag>>
\* What the frame conjuncts type.
FrameOK ==
  /\ took' \in [Writers -> [Paths -> SUBSET Gens]]
  /\ retiring' \subseteq Handles
  /\ anc' \in [Handles -> SUBSET Handles]
  /\ aged' \subseteq Handles /\ ages' \in Nat
  /\ rdoc' \in [Paths -> Opt(Handles)] /\ rlag' \in BOOLEAN

------------------------------------------------------------------------------
(* Helpers.                                                                 *)

LEMMA WriterFields ==
  ASSUME NEW W \in Writer
  PROVE  /\ W.st \in {"off", "on"}
         /\ W.pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
         /\ W.local \in [Paths -> Opt(Handles)]
         /\ W.baseline \in [Paths -> Opt(Handles)]
         /\ W.integrated \subseteq Gens
         /\ W.uploads \subseteq Paths /\ W.deletes \subseteq Paths
         /\ W.snap \in [Paths -> Opt(Handles)]
         /\ W.upDone \subseteq Paths /\ W.gone \subseteq Paths /\ W.verified \in BOOLEAN
         /\ W.inst \in [Paths -> Opt(Handles)]
         /\ W.retire \subseteq Handles
         /\ W.collected \in BOOLEAN /\ W.adv \in BOOLEAN
         /\ W.synced \in Nat /\ W.derived \in Nat
         /\ W.skipped \subseteq Paths /\ W.scope \subseteq Paths
         /\ W.sStage \in {"none", "saved", "mid"}
         /\ W.sTgt \subseteq Paths /\ W.sDrop \subseteq Paths /\ W.sKeep \subseteq Paths
         /\ W.sHeld \in [Paths -> Opt(Handles)]
         /\ W.unlinked \subseteq Paths
         /\ W.memo \in Nat /\ W.rnow \in Nat
         /\ W.nt \in Nat /\ W.pend \in PendT /\ W.ack \in AckT /\ W.car \in CarT
         /\ W.cId \in Nat /\ W.cFold \in Nat /\ W.clk \in Clocks \cup {"none"}
         /\ W.iseq \in Nat /\ W.instd \in BOOLEAN /\ W.bound \in BoundT
         /\ W.cN \subseteq Nat /\ W.aN \subseteq Nat /\ W.capped \subseteq Nat
         /\ W.dset \subseteq Paths /\ W.decl \in [Paths -> Opt(Handles)] /\ W.dMint \in [Paths -> Nat]
         /\ W.moved \subseteq Paths /\ W.stamps \subseteq Nat \X (Clocks \cup {"none"})
         /\ W.lost \subseteq Opt(Handles) /\ W.deposed \in BOOLEAN /\ W.nd \in Nat
<1>1. /\ W.st \in {"off", "on"}
      /\ W.pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
      /\ W.local \in [Paths -> Opt(Handles)]
      /\ W.baseline \in [Paths -> Opt(Handles)]
      /\ W.integrated \subseteq Gens
      /\ W.uploads \subseteq Paths /\ W.deletes \subseteq Paths
      /\ W.snap \in [Paths -> Opt(Handles)]
  BY DEF Writer
<1>2. /\ W.upDone \subseteq Paths /\ W.gone \subseteq Paths /\ W.verified \in BOOLEAN
      /\ W.inst \in [Paths -> Opt(Handles)]
      /\ W.retire \subseteq Handles
      /\ W.collected \in BOOLEAN /\ W.adv \in BOOLEAN
      /\ W.synced \in Nat /\ W.derived \in Nat
      /\ W.skipped \subseteq Paths /\ W.scope \subseteq Paths
  BY DEF Writer
<1>3. /\ W.sStage \in {"none", "saved", "mid"}
      /\ W.sTgt \subseteq Paths /\ W.sDrop \subseteq Paths /\ W.sKeep \subseteq Paths
      /\ W.sHeld \in [Paths -> Opt(Handles)]
      /\ W.unlinked \subseteq Paths
      /\ W.memo \in Nat /\ W.rnow \in Nat
  BY DEF Writer
\* Through WriterT, the nested types as atoms (runs 1 and 2: `W.pend \in PendT
\* BY DEF Writer, PendT` alone ran past 60 s).
<1>4. /\ W.nt \in Nat /\ W.pend \in PendT /\ W.ack \in AckT /\ W.car \in CarT
      /\ W.cId \in Nat /\ W.cFold \in Nat /\ W.clk \in Clocks \cup {"none"}
      /\ W.iseq \in Nat /\ W.instd \in BOOLEAN /\ W.bound \in BoundT
  BY WriterTEq DEF WriterT
<1>5. /\ W.cN \subseteq Nat /\ W.aN \subseteq Nat /\ W.capped \subseteq Nat
      /\ W.dset \subseteq Paths /\ W.decl \in [Paths -> Opt(Handles)] /\ W.dMint \in [Paths -> Nat]
      /\ W.moved \subseteq Paths /\ W.stamps \subseteq Nat \X (Clocks \cup {"none"})
      /\ W.lost \subseteq Opt(Handles) /\ W.deposed \in BOOLEAN /\ W.nd \in Nat
  BY DEF Writer
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5

\* The carrier and the bound a barrier's end writes.
LEMMA CarUncitedType ==
  ASSUME NEW W \in Writer, NEW d \in [Paths -> Opt(Handles)]
  PROVE  (IF W.car.id # 0 THEN CarUncited(W.car, d) ELSE W.car) \in CarT
BY WriterFields DEF CarUncited, Uncited, CarT

LEMMA BoundType ==
  ASSUME NEW W \in Writer, NEW d \in [Paths -> Opt(Handles)], NEW q \in Nat,
         NEW wd \in SUBSET Paths, NEW b \in BOOLEAN
  PROVE  (IF W.cId # 0
          THEN [id |-> W.cId, fold |-> W.cFold, doc |-> d, seq |-> q, wd |-> wd, inst |-> b, clk |-> W.clk]
          ELSE W.bound) \in BoundT
BY WriterFields DEF BoundT

\* On its own: with a lemma's other facts in scope the same goal runs past Z3's limit.
LEMMA WriterInitType == WriterInit \in Writer
BY DEF WriterInit, Writer, Opt, NoPend, NoAck, NoCar

LEMMA HandleGen == \A h \in Handles : Gen(h) \in Gens /\ h[1] \in Paths
BY DEF Handles, Gen

LEMMA CardPaths == ASSUME NEW S \in SUBSET Paths PROVE Cardinality(S) \in Nat
BY PathsFinite, FS_Subset, FS_CardinalityType

\* The handle a GPut or an Edit mints.
LEMMA MintHandle ==
  ASSUME TypeOK, nextGen > Seed, nextGen <= MaxMint, NEW p \in Paths
  PROVE  nextGen \in Gens /\ <<p, nextGen>> \in Handles
BY MaxMintNat, MaxCopiesNat DEF TypeOK, Gens, Seed, Handles

\* The handle an Upload's copy mints.
LEMMA CopyHandle ==
  ASSUME Ghosts, copies < MaxCopies, NEW p \in Paths
  PROVE  MaxMint + copies + 1 \in Gens /\ <<p, MaxMint + copies + 1>> \in Handles
BY MaxMintNat, MaxCopiesNat DEF Ghosts, Gens, Seed, Handles

\* A document's cited generations.
LEMMA DocGens ==
  ASSUME TypeOK, NEW S \in SUBSET Paths
  PROVE  {Gen(doc[p]) : p \in {q \in S : doc[q] # Nil}} \subseteq Gens
BY HandleGen DEF TypeOK, Opt

LEMMA FrameTypeOK ==
  ASSUME TypeOK, Frame,
         w' \in [Writers -> Writer],
         doc' \in [Paths -> Opt(Handles)],
         base' \in [Handles -> Opt(Handles)]
  PROVE  FrameOK
<1>1. took' \in [Writers -> [Paths -> SUBSET Gens]]
  <2>1. \A s \in Writers, p \in Paths :
          /\ w'[s].baseline[p] # Nil => Gen(w'[s].baseline[p]) \in Gens
          /\ w'[s].local[p] # Nil => Gen(w'[s].local[p]) \in Gens
    BY WriterFields, HandleGen DEF Opt
  <2>. QED BY <2>1 DEF TypeOK, Frame, TookUpdate
<1>2. retiring' \subseteq Handles BY DEF TypeOK, Frame, RetUpdate, Opt
<1>3. anc' \in [Handles -> SUBSET Handles] BY DEF TypeOK, Frame, AncUpdate, AncOf, Opt
<1>. QED BY <1>1, <1>2, <1>3 DEF TypeOK, Frame, FrameOK

\* The step writes one tree: what the others and the typing of `w'` become.
LEMMA WriteOne ==
  ASSUME TypeOK, NEW s \in Writers, NEW R \in Writer, w' = [w EXCEPT ![s] = R]
  PROVE  /\ w' \in [Writers -> Writer]
         /\ \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t]
BY DEF TypeOK

\* Deposal writes two trees: the taker's and the deposed holder's.
LEMMA WriteTwo ==
  ASSUME TypeOK, NEW s \in Writers, NEW t \in Writers, s # t, NEW A \in Writer, NEW B \in Writer,
         w' = [w EXCEPT ![s] = A, ![t] = B]
  PROVE  /\ w' \in [Writers -> Writer]
         /\ \A u \in Writers : w'[u] = IF u = s THEN A ELSE IF u = t THEN B ELSE w[u]
BY DEF TypeOK

\* A tree's map after one path is written.
LEMMA ExceptFun ==
  ASSUME NEW f \in [Paths -> Opt(Handles)], NEW p \in Paths, NEW v \in Opt(Handles)
  PROVE  [f EXCEPT ![p] = v] \in [Paths -> Opt(Handles)]
OBVIOUS

\* Minted after a step that writes no tree.
LEMMA MintedKeep ==
  ASSUME Minted, minted \subseteq minted', w' = w,
         \A q \in Paths : doc'[q] \in Opt(minted'),
         \A q \in Paths : gw'[q] \in Opt(minted')
  PROVE  Minted'
BY DEF Minted, Opt

\* Minted after a step that writes one tree R.
LEMMA MintedWrite ==
  ASSUME Minted, NEW s \in Writers, NEW R,
         minted \subseteq minted',
         \A q \in Paths : doc'[q] \in Opt(minted'),
         \A q \in Paths : gw'[q] \in Opt(minted'),
         \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         \A q \in Paths : R.local[q] \in Opt(minted'),
         \A q \in R.uploads : R.snap[q] \in minted'
  PROVE  Minted'
BY DEF Minted, Opt

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_TypeOK == Init => IndTypeOK
<1>. SUFFICES ASSUME Init PROVE IndTypeOK OBVIOUS
<1>1. Seed \in Gens BY MaxMintNat, MaxCopiesNat DEF Seed, Gens
<1>2. \A p \in Paths : <<p, Seed>> \in Handles BY <1>1 DEF Handles
<1>3. live \subseteq Handles /\ minted = live /\ upped = live BY <1>2, FreePaths DEF Init
<1>4. doc \in [Paths -> Opt(Handles)] BY <1>2 DEF Init, Opt
<1>5. WriterInit \in Writer BY WriterInitType
<1>6. w \in [Writers -> Writer] BY <1>5 DEF Init
<1>7. /\ live \subseteq Handles /\ minted \subseteq Handles /\ live \subseteq minted
      /\ doc \in [Paths -> Opt(Handles)] /\ seq \in Nat
      /\ tomb \in [Paths -> Opt(Handles)] /\ base \in [Handles -> Opt(Handles)]
  BY <1>3, <1>4 DEF Init, Opt
<1>8. /\ acked \subseteq Paths \X Handles /\ conflicts \subseteq Paths \X Opt(Handles)
      /\ holder \in Writers \cup {"none"}
      /\ nextGen \in Nat /\ ui \in Nat /\ reqs \in Nat /\ barriers \in Nat
      /\ w \in [Writers -> Writer]
      /\ took \in [Writers -> [Paths -> SUBSET Gens]]
  BY <1>6 DEF Init, Seed
<1>9. /\ gw \in [Paths -> Opt(Handles)] /\ mv \in Opt(Paths \X Handles)
      /\ udel \subseteq Paths \X Handles
      /\ restarts \in Nat /\ syncs \in Nat /\ regressed \in BOOLEAN /\ rescopes \in Nat /\ fails \in Nat
      /\ retiring \subseteq Handles /\ aged \subseteq Handles /\ ages \in Nat
      /\ rdoc \in [Paths -> Opt(Handles)] /\ rlag \in BOOLEAN
      /\ anc \in [Handles -> SUBSET Handles]
  BY DEF Init, Opt
<1>10. TypeOK BY <1>7, <1>8, <1>9 DEF TypeOK
<1>11. Ghosts BY <1>3 DEF Init, Ghosts, Opt
<1>12. Minted BY <1>2, <1>3, <1>5, <1>6 DEF Init, Minted, WriterInit, Opt
<1>. QED BY <1>10, <1>11, <1>12 DEF Init, IndTypeOK, Seed

------------------------------------------------------------------------------
(* The gateway's steps (no tree changes).                                  *)

LEMMA GPut_TypeOK ==
  ASSUME IndTypeOK, NEW p \in Paths, GPut(p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>1. /\ live' = live \cup {<<p, nextGen>>} /\ minted' = minted \cup {<<p, nextGen>>}
      /\ base' = [base EXCEPT ![<<p, nextGen>>] = doc[p]]
      /\ gw' = [gw EXCEPT ![p] = <<p, nextGen>>]
      /\ nextGen' = nextGen + 1 /\ ui' = ui + 1
      /\ upped' = upped \cup {<<p, nextGen>>}
      /\ nextGen <= MaxMint
      /\ UNCHANGED <<doc, seq, tomb, acked, conflicts, holder, reqs, barriers, w, mv, udel,
                     restarts, syncs, regressed, rescopes, fails, copies, orig>>
  BY DEF GPut
<1>2. nextGen \in Gens /\ <<p, nextGen>> \in Handles BY <1>a, <1>b, <1>1, MintHandle
<1>3. base' \in [Handles -> Opt(Handles)] BY <1>a, <1>1, <1>2 DEF TypeOK, Opt
<1>4. w' \in [Writers -> Writer] /\ doc' \in [Paths -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
<1>5. FrameOK BY <1>a, <1>1, <1>3, <1>4, FrameTypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>4, <1>5 DEF TypeOK, Opt, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>a, <1>b, <1>1, <1>2 DEF Ghosts, TypeOK, Seed
<1>g. gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] BY <1>a DEF TypeOK
<1>8. /\ minted \subseteq minted' /\ w' = w
      /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>g, <1>1, <1>2 DEF Minted, Opt
<1>9. Minted' BY <1>c, <1>8, MintedKeep
<1>. QED BY <1>6, <1>7, <1>9 DEF IndTypeOK

LEMMA GCas_TypeOK ==
  ASSUME IndTypeOK, NEW p \in Paths, GCas(p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>1. /\ gw[p] # Nil
      /\ doc' \in {[doc EXCEPT ![p] = gw[p]], doc}
      /\ seq' \in {seq + 1, seq}
      /\ tomb' \in {[tomb EXCEPT ![p] = Nil], tomb}
      /\ acked' \in {acked \cup {<<p, gw[p]>>}, acked}
      /\ gw' = [gw EXCEPT ![p] = Nil]
      /\ UNCHANGED <<live, minted, base, conflicts, holder, nextGen, ui, reqs, barriers, w, mv, udel, aux>>
  BY DEF GCas
<1>2. gw[p] \in minted /\ gw[p] \in Handles BY <1>a, <1>c, <1>1 DEF TypeOK, Minted, Opt
<1>3. doc' \in [Paths -> Opt(Handles)] /\ \A q \in Paths : doc'[q] \in Opt(minted)
  BY <1>a, <1>c, <1>1, <1>2 DEF TypeOK, Minted, Opt
<1>4. w' \in [Writers -> Writer] /\ base' \in [Handles -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
<1>5. FrameOK BY <1>a, <1>1, <1>3, <1>4, FrameTypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>4, <1>5 DEF TypeOK, Opt, aux, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts, aux
<1>g. gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] BY <1>a DEF TypeOK
<1>8. /\ minted \subseteq minted' /\ w' = w
      /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>g, <1>1, <1>3 DEF Minted, Opt, aux
<1>9. Minted' BY <1>c, <1>8, MintedKeep
<1>. QED BY <1>6, <1>7, <1>9 DEF IndTypeOK

LEMMA GRename_TypeOK ==
  ASSUME IndTypeOK, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. RenameAtomic BY ShippedShape DEF Shipped
<1>1. /\ doc[p] # Nil
      /\ doc' = [doc EXCEPT ![q] = doc[p], ![p] = Nil]
      /\ tomb' = [tomb EXCEPT ![q] = Nil, ![p] = doc[p]]
      /\ acked' = acked \cup {<<q, doc[p]>>}
      /\ mv' = mv
      /\ seq' = seq + 1 /\ reqs' = reqs + 1
      /\ UNCHANGED <<live, minted, base, conflicts, holder, gw, udel, nextGen, ui, barriers, w, aux>>
  BY <1>0 DEF GRename
<1>2. doc[p] \in minted /\ doc[p] \in Handles BY <1>a, <1>c, <1>1 DEF TypeOK, Minted, Opt
<1>3. doc' \in [Paths -> Opt(Handles)] /\ \A r \in Paths : doc'[r] \in Opt(minted)
  BY <1>a, <1>c, <1>1, <1>2 DEF TypeOK, Minted, Opt
<1>4. w' \in [Writers -> Writer] /\ base' \in [Handles -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
<1>5. FrameOK BY <1>a, <1>1, <1>3, <1>4, FrameTypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>4, <1>5 DEF TypeOK, Opt, aux, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts, aux
<1>g. gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] BY <1>a DEF TypeOK
<1>8. /\ minted \subseteq minted' /\ w' = w
      /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>g, <1>1, <1>3 DEF Minted, Opt, aux
<1>9. Minted' BY <1>c, <1>8, MintedKeep
<1>. QED BY <1>6, <1>7, <1>9 DEF IndTypeOK

\* The two-CAS rename's second CAS: never enabled (mv = Nil).
LEMMA GRenameFinish_TypeOK ==
  ASSUME IndTypeOK, GRenameFinish, Frame
  PROVE  IndTypeOK'
BY DEF IndTypeOK, GRenameFinish

LEMMA GDelete_TypeOK ==
  ASSUME IndTypeOK, NEW p \in Paths, GDelete(p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>1. /\ doc[p] # Nil
      /\ doc' = [doc EXCEPT ![p] = Nil]
      /\ tomb' = [tomb EXCEPT ![p] = doc[p]]
      /\ udel' = udel \cup {<<p, doc[p]>>}
      /\ seq' = seq + 1 /\ reqs' = reqs + 1
      /\ UNCHANGED <<live, minted, base, acked, conflicts, holder, gw, mv, nextGen, ui, barriers, w, aux>>
  BY DEF GDelete
<1>2. doc[p] \in minted /\ doc[p] \in Handles BY <1>a, <1>c, <1>1 DEF TypeOK, Minted, Opt
<1>3. doc' \in [Paths -> Opt(Handles)] /\ \A r \in Paths : doc'[r] \in Opt(minted)
  BY <1>a, <1>c, <1>1, <1>2 DEF TypeOK, Minted, Opt
<1>4. w' \in [Writers -> Writer] /\ base' \in [Handles -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
<1>5. FrameOK BY <1>a, <1>1, <1>3, <1>4, FrameTypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>4, <1>5 DEF TypeOK, Opt, aux, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts, aux
<1>g. gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] BY <1>a DEF TypeOK
<1>8. /\ minted \subseteq minted' /\ w' = w
      /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>g, <1>1, <1>3 DEF Minted, Opt, aux
<1>9. Minted' BY <1>c, <1>8, MintedKeep
<1>. QED BY <1>6, <1>7, <1>9 DEF IndTypeOK

'''

UNCH_ALL_BUT_W = '''UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>'''

def step(name, sig, binders, pick, rec, rec_defs, other_primed, unch, unch_defs, facts, reads, reads_defs,
         minted_grows=False, extra_typeok='', wt=False):
    """One writer step.  pick: ('PICK x \\in S :', [conjuncts before w']) or None.
    facts: list of (text, by) steps proved before the membership (cited there and in TypeOK').
    reads: the record's local/uploads/snap facts for Minted'."""
    base = sig.split('(')[0]
    wt = wt or name in ('Skip', 'PullOnly', 'Finish', 'RetireByAck')   # through RECOP
    st = f'''LEMMA {name}_TypeOK ==
  ASSUME IndTypeOK, NEW s \\in Writers{binders}, {sig}, Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\\ mv = Nil /\\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \\in Writer BY <1>a DEF TypeOK, PendT, AckT, CarT, BoundT
<1>d. doc \\in [Paths -> Opt(Handles)] /\\ seq \\in Nat /\\ live \\subseteq Handles BY <1>a DEF TypeOK
'''
    if pick:
        head, conj = pick
        st += f'<1>1. {head}\n'
        for c in conj: st += f'        /\\ {c}\n'
        st += f'        /\\ w\' = [w EXCEPT ![s] = {rec}]\n  BY DEF {base}\n'
        st += f'<1>2. {other_primed}{unch}\n  BY DEF {base}{unch_defs}\n'
    else:
        st += f'<1>1. /\\ w\' = [w EXCEPT ![s] = {rec}]\n{other_primed}      /\\ {unch}\n  BY DEF {base}{unch_defs}\n'
        st += f'<1>2. TRUE OBVIOUS\n'
    n = 3; cites = []; cites1 = []
    for fact in facts:
        text, by = fact[0], fact[1]
        st += f'<1>{n}. {text}\n' + (by[1:] if by.startswith('\n') else f'  BY {by}\n'); cites.append(f'<1>{n}')
        if len(fact) > 2 and fact[2]: cites1.append(f'<1>{n}')
        n += 1
    c = ', '.join(cites)
    cc = (c + ', ') if c else ''
    if wt:   # the record through its RECOP (no IF field in the membership goal)
        _t, n, mem, _d = RECMEM(rec, n, facts, cites)
        st += _t
        st = _d + st
    else:
        if rec.split('(')[0] in ('RescopeBeginW', 'RescopeFirstW', 'RPullSyncW'):   # not TouchW: its new pend needs PendT expanded (box 10-15: 100%)   # EC2 10-15: timed out (100%) expanding the nested types; WriterT keeps them atoms
            st += f'<1>{n}. {rec} \\in Writer BY <1>0, <1>d, {cc}WriterFields, WriterTEq DEF {rec_defs.replace("Writer,", "WriterT,")}\n'; mem = n; n += 1
        else:
            st += f'<1>{n}. {rec} \\in Writer BY <1>0, <1>d, {cc}WriterFields DEF {rec_defs}, PendT, AckT, CarT, BoundT\n'; mem = n; n += 1
    st += f'<1>{n}. w\' \\in [Writers -> Writer] /\\ \\A t \\in Writers : w\'[t] = IF t = s THEN {rec} ELSE w[t]\n  BY <1>a, <1>1, <1>{mem}, WriteOne\n'; wt = n; n += 1
    st += f'<1>{n}. FrameOK BY <1>a, <1>1, <1>2, <1>{wt}, FrameTypeOK DEF TypeOK\n'; fr = n; n += 1
    st += f'<1>{n}. TypeOK\' BY <1>a, <1>1, <1>2, {cc}<1>{wt}, <1>{fr} DEF TypeOK, FrameOK{extra_typeok}\n'; tk = n; n += 1
    st += f'<1>{n}. Ghosts\' /\\ mv\' = Nil /\\ nextGen\' > Seed BY <1>b, <1>1, <1>2, {cc}<1>a DEF Ghosts, TypeOK, Seed\n'; gh = n; n += 1
    r1, r2 = reads
    c1 = (', '.join(cites1) + ', ') if cites1 else ''
    st += f'<1>{n}. {r1}\n  BY <1>0, <1>c, <1>1, <1>2, {c1}WriterFields DEF {rec.split("(")[0]}, Minted, Opt\n'; rd1 = n; n += 1
    st += f'<1>{n}. {r2}\n  BY <1>0, <1>c, <1>1, <1>2, {cc}WriterFields DEF {reads_defs}\n'; rd2 = n; n += 1
    rd = f'{rd1}, <1>{rd2}'
    st += f'<1>{n}. /\\ minted \\subseteq minted\' /\\ \\A q \\in Paths : doc\'[q] \\in Opt(minted\') /\\ \\A q \\in Paths : gw\'[q] \\in Opt(minted\')\n  BY <1>c, <1>1, <1>2 DEF Minted, Opt\n'; ev = n; n += 1
    # DEF, not MintedWrite: SMT's choice of R was sensitive to the module's other
    # definitions (2026-10-14: Scan and two more timed out at 200 s once AN5/NR5
    # were added to LeanP1Anc; the direct form proves in 3 s).
    st += f'<1>{n}. Minted\' BY <1>c, <1>{wt}, <1>{rd}, <1>{ev} DEF Minted, Opt\n'; mi = n; n += 1
    st += f'<1>. QED BY <1>{tk}, <1>{gh}, <1>{mi} DEF IndTypeOK\n\n'
    return st

def reads_unchanged(rec, rdefs):
    return ((f'\\A q \\in Paths : {rec}.local[q] \\in Opt(minted\')',
             f'\\A q \\in {rec}.uploads : {rec}.snap[q] \\in minted\''), f'{rdefs}, Minted, Opt')

def BARRIER_END(rec, d, q, wd, b, dfact='<1>d', extra=''):
    """The facts a barrier's end needs: the carrier recomputed and the bound written,
    each stated on the field's own expression (run 1: through `{rec}.car`, which
    expands the 48-field record, two of these timed out)."""
    return [(f'(IF w[s].car.id # 0 THEN CarUncited(w[s].car, {d}) ELSE w[s].car) \\in CarT',
             f'<1>0, {dfact}{extra}, CarUncitedType'),
            # Directly, BoundT expanded (the BoundType instance ran past 60 s: 2026-10-15).
            (f'(IF w[s].cId # 0 THEN [id |-> w[s].cId, fold |-> w[s].cFold, doc |-> {d}, seq |-> {q}, '
             f'wd |-> {wd}, inst |-> {b}, clk |-> w[s].clk] ELSE w[s].bound) \\in BoundT',
             '\n  <2>1. /\\ w[s].cId \\in Nat /\\ w[s].cFold \\in Nat /\\ w[s].clk \\in Clocks \\cup {"none"} /\\ w[s].bound \\in BoundT\n'
             '        /\\ w[s].inst \\in [Paths -> Opt(Handles)] /\\ w[s].iseq \\in Nat /\\ w[s].gone \\subseteq Paths /\\ w[s].instd \\in BOOLEAN\n'
             '    BY <1>0, WriterFields\n'
             f'  <2>. QED BY <2>1, {dfact} DEF BoundT\n')]

BODY = ''
BODY += '------------------------------------------------------------------------------\n(* The agent.                                                               *)\n\n'
BODY += r'''LEMMA Edit_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK, PendT, AckT, CarT, BoundT
<1>1. /\ nextGen <= MaxMint
      /\ minted' = minted \cup {<<p, nextGen>>}
      /\ base' = [base EXCEPT ![<<p, nextGen>>] = IF w[s].local[p] # Nil THEN w[s].local[p] ELSE IF p \in w[s].uploads THEN w[s].snap[p] ELSE w[s].baseline[p]]
      /\ w' = [w EXCEPT ![s] = EditW(s, p)]
      /\ nextGen' = nextGen + 1
      /\ UNCHANGED <<live, doc, seq, tomb, acked, conflicts, holder, ui, reqs, barriers, gw, mv, udel,
                     restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Edit, aux
<1>2. nextGen \in Gens /\ <<p, nextGen>> \in Handles BY <1>a, <1>b, <1>1, MintHandle
<1>2a. [w[s].local EXCEPT ![p] = <<p, nextGen>>] \in [Paths -> Opt(Handles)]
  BY <1>0, <1>2, WriterFields, ExceptFun DEF Opt
<1>2b. w[s].integrated \cup {nextGen} \subseteq Gens /\ w[s].unlinked \ {p} \subseteq Paths
  BY <1>0, <1>2, WriterFields
<1>3. EditW(s, p) \in Writer BY <1>0, <1>2, <1>2a, <1>2b, WriterFields DEF EditW, Writer, Opt, PendT, AckT, CarT, BoundT
<1>4. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN EditW(s, p) ELSE w[t]
  BY <1>a, <1>1, <1>3, WriteOne
<1>5. base' \in [Handles -> Opt(Handles)] BY <1>a, <1>0, <1>1, <1>2, WriterFields DEF TypeOK, Opt
<1>6. doc' \in [Paths -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
<1>7. FrameOK BY <1>a, <1>1, <1>4, <1>5, <1>6, FrameTypeOK
<1>8. TypeOK' BY <1>a, <1>1, <1>2, <1>4, <1>5, <1>7 DEF TypeOK, FrameOK
<1>9. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>a, <1>b, <1>1, <1>2 DEF Ghosts, TypeOK, Seed
<1>10. /\ \A q \in Paths : EditW(s, p).local[q] \in Opt(minted')
       /\ \A q \in EditW(s, p).uploads : EditW(s, p).snap[q] \in minted'
  <2>1. \A q \in Paths : [w[s].local EXCEPT ![p] = <<p, nextGen>>][q] \in Opt(minted')
    BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF Minted, Opt
  <2>2. EditW(s, p).local = [w[s].local EXCEPT ![p] = <<p, nextGen>>]
        /\ EditW(s, p).uploads = w[s].uploads /\ EditW(s, p).snap = w[s].snap
    BY DEF EditW
  <2>3. \A q \in w[s].uploads : w[s].snap[q] \in minted' BY <1>c, <1>1 DEF Minted
  <2>. QED BY <2>1, <2>2, <2>3
<1>11. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1 DEF Minted, Opt
<1>12. Minted' BY <1>c, <1>4, <1>10, <1>11, MintedWrite
<1>. QED BY <1>8, <1>9, <1>12 DEF IndTypeOK

'''
BODY += r"""LEMMA Delete_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK, PendT, AckT, CarT, BoundT
<1>1. /\ w' = [w EXCEPT ![s] = DeleteW(s, p)]
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Delete, bucket, aux
<1>2. [w[s].local EXCEPT ![p] = Nil] \in [Paths -> Opt(Handles)] BY <1>0, WriterFields, ExceptFun DEF Opt
<1>3. DeleteW(s, p) \in Writer BY <1>0, <1>2, WriterFields DEF DeleteW, Writer, Opt, PendT, AckT, CarT, BoundT
<1>4. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN DeleteW(s, p) ELSE w[t]
  BY <1>a, <1>1, <1>3, WriteOne
<1>5. FrameOK BY <1>a, <1>1, <1>4, FrameTypeOK DEF TypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>4, <1>5 DEF TypeOK, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>8. /\ \A q \in Paths : DeleteW(s, p).local[q] \in Opt(minted')
      /\ \A q \in DeleteW(s, p).uploads : DeleteW(s, p).snap[q] \in minted'
  <2>1. \A q \in Paths : [w[s].local EXCEPT ![p] = Nil][q] \in Opt(minted')
    BY <1>0, <1>c, <1>1, WriterFields DEF Minted, Opt
  <2>2. DeleteW(s, p).local = [w[s].local EXCEPT ![p] = Nil]
        /\ DeleteW(s, p).uploads = w[s].uploads /\ DeleteW(s, p).snap = w[s].snap
    BY DEF DeleteW
  <2>3. \A q \in w[s].uploads : w[s].snap[q] \in minted' BY <1>c, <1>1 DEF Minted
  <2>. QED BY <2>1, <2>2, <2>3
<1>9. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1 DEF Minted, Opt
<1>10. Minted' BY <1>c, <1>4, <1>8, <1>9, MintedWrite
<1>. QED BY <1>6, <1>7, <1>10 DEF IndTypeOK

"""
BODY += step('Checkout', 'Checkout(s)', '', ('PICK T \\in Scopes :', []), 'CheckoutW(s, T)', 'CheckoutW, Writer, Opt',
             '', UNCH_ALL_BUT_W, ', bucket, aux',
             [('T \\subseteq Paths', 'ScopesPaths'),
              ('{Gen(doc[p]) : p \\in {q \\in T : doc[q] # Nil}} \\subseteq Gens', '<1>a, <1>3, DocGens'),
              ('/\\ CheckoutHeld(s, T) \\in [Paths -> Opt(Handles)]\n      /\\ \\A q \\in Paths : CheckoutHeld(s, T)[q] \\in Opt(minted)', '<1>a, <1>c DEF CheckoutHeld, TypeOK, Minted, Opt', True)],
             *reads_unchanged('CheckoutW(s, T)', 'CheckoutW, CheckoutHeld'))
BODY += '------------------------------------------------------------------------------\n(* The barrier: consume, scan, upload.                                      *)\n\n'
BODY += r'''LEMMA Consume_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Consume(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK, PendT, AckT, CarT, BoundT
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                  nextGen, ui, reqs, barriers, restarts, syncs, rescopes, upped, copies, orig>>
  BY DEF Consume
<1>ck. PICK ck \in CarryClocks(s) :
          IF CheapPath(s)
          THEN /\ w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
               /\ UNCHANGED <<regressed, fails>>
          ELSE \E fail \in SUBSET ConsumeOwed(s) :
                  /\ fails + Cardinality(fail) <= MaxFetchFails
                  /\ w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
                  /\ fails' = fails + Cardinality(fail)
                  /\ regressed' = (regressed \/ \E p \in ConsumeOwed(s) \ fail : Back(s, p))
  BY DEF Consume
<1>ct. ck \in Clocks \cup {"none"} /\ w[s].pend.id \in Nat /\ w[s].pend.fold \in Nat
  BY <1>0, WriterFields DEF CarryClocks, PendT
<1>2. /\ w' \in [Writers -> Writer]
      /\ \A t \in Writers : w'[t] = IF t = s THEN w'[s] ELSE w[t]
      /\ \A q \in Paths : w'[s].local[q] \in Opt(minted')
      /\ \A q \in w'[s].uploads : w'[s].snap[q] \in minted'
      /\ regressed' \in BOOLEAN /\ fails' \in Nat
  <2>1. CASE CheapPath(s)
    <3>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] /\ UNCHANGED <<regressed, fails>> BY <2>1, <1>ck
    <3>2. ConsumeCheapW(s, ck) \in Writer BY <1>0, <1>ct, WriterFields DEF ConsumeCheapW, Writer, PendT, AckT, CarT, BoundT
    <3>3. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN ConsumeCheapW(s, ck) ELSE w[t]
      BY <1>a, <3>1, <3>2, WriteOne
    <3>4. /\ \A q \in Paths : ConsumeCheapW(s, ck).local[q] \in Opt(minted')
          /\ \A q \in ConsumeCheapW(s, ck).uploads : ConsumeCheapW(s, ck).snap[q] \in minted'
      BY <1>c, <1>1 DEF ConsumeCheapW, Minted
    <3>. QED BY <1>a, <3>1, <3>3, <3>4 DEF TypeOK
  <2>2. CASE ~CheapPath(s)
    <3>1. PICK fail \in SUBSET ConsumeOwed(s) :
            /\ fails + Cardinality(fail) <= MaxFetchFails
            /\ w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
            /\ fails' = fails + Cardinality(fail)
            /\ regressed' = (regressed \/ \E p \in ConsumeOwed(s) \ fail : Back(s, p))
      BY <2>2, <1>ck
    <3>2. fail \subseteq Paths /\ ConsumeTaken(s, fail) \subseteq Paths BY DEF ConsumeOwed, ConsumeConv, ConsumeTaken
    <3>3. Cardinality(fail) \in Nat BY <3>2, CardPaths
    <3>4. {Gen(doc[p]) : p \in {q \in ConsumeTaken(s, fail) : doc[q] # Nil}} \subseteq Gens
      BY <1>a, <3>2, DocGens
    <3>5. ConsumeW(s, fail, ck) \in Writer BY <1>0, <1>d, <1>ct, <3>2, <3>4, WriterFields DEF ConsumeW, Writer, Opt, PendT, AckT, CarT, BoundT
    <3>6. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN ConsumeW(s, fail, ck) ELSE w[t]
      BY <1>a, <3>1, <3>5, WriteOne
    <3>7. /\ \A q \in Paths : ConsumeW(s, fail, ck).local[q] \in Opt(minted')
          /\ \A q \in ConsumeW(s, fail, ck).uploads : ConsumeW(s, fail, ck).snap[q] \in minted'
      <4>1. \A q \in Paths : ConsumeW(s, fail, ck).local[q] \in Opt(minted')
        BY <1>0, <1>c, <1>1, WriterFields DEF ConsumeW, Minted, Opt
      <4>2. \A q \in ConsumeW(s, fail, ck).uploads : ConsumeW(s, fail, ck).snap[q] \in minted'
        BY <1>c, <1>1 DEF ConsumeW, Minted
      <4>. QED BY <4>1, <4>2
    <3>. QED BY <1>a, <3>1, <3>3, <3>6, <3>7 DEF TypeOK
  <2>. QED BY <2>1, <2>2
<1>3. FrameOK BY <1>a, <1>1, <1>2, FrameTypeOK DEF TypeOK
<1>4. TypeOK' BY <1>a, <1>1, <1>2, <1>3 DEF TypeOK, FrameOK
<1>5. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>6. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1 DEF Minted, Opt
<1>7. Minted' BY <1>c, <1>2, <1>6, MintedWrite
<1>. QED BY <1>4, <1>5, <1>7 DEF IndTypeOK

'''
BODY += step('Scan', 'Scan(s)', '', ('PICK dels \\in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) :', []), 'ScanW(s, dels)', 'ScanW, Writer, Opt',
             '/\\ barriers\' = barriers + 1\n      /\\ ',
             '''UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>''',
             ', bucket, aux',
             [('dels \\subseteq Paths /\\ ScanUps(s) \\subseteq Paths', 'DEF ScanAbsent, ScanUps, ScanDirty')],
             ('\\A q \\in Paths : ScanW(s, dels).local[q] \\in Opt(minted\')', '\\A q \\in ScanW(s, dels).uploads : ScanW(s, dels).snap[q] \\in minted\''),
             'ScanW, ScanUps, ScanDirty, Minted, Opt')
BODY += step('Skip', 'Skip(s)', '', None, 'SkipW(s)', 'SkipW, Writer',
             '      /\\ barriers\' = barriers + 1\n',
             '''UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>''',
             ', bucket, aux', BARRIER_END('SkipW(s)', 'doc', 'seq', '{}', 'FALSE'), *reads_unchanged('SkipW(s)', 'SkipW'))
BODY += r'''LEMMA Upload_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>. DEFINE h == w[s].snap[p]
<1>0. w[s] \in Writer BY <1>a DEF TypeOK, PendT, AckT, CarT, BoundT
<1>1. /\ p \in w[s].uploads
      /\ UNCHANGED <<doc, seq, tomb, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails>>
  BY DEF Upload
<1>2. h \in minted /\ h \in Handles BY <1>a, <1>c, <1>1 DEF TypeOK, Minted
<1>3. CASE h \notin upped
  <2>1. /\ live' = live \cup {h} /\ upped' = upped \cup {h}
        /\ w' = [w EXCEPT ![s] = UploadW(s, p)]
        /\ UNCHANGED <<minted, base, copies, orig>>
    BY <1>3 DEF Upload
  <2>2. UploadW(s, p) \in Writer BY <1>0, WriterFields DEF UploadW, Writer, PendT, AckT, CarT, BoundT
  <2>3. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN UploadW(s, p) ELSE w[t]
    BY <1>a, <2>1, <2>2, WriteOne
  <2>4. FrameOK BY <1>a, <1>1, <2>1, <2>3, FrameTypeOK DEF TypeOK
  <2>5. TypeOK' BY <1>a, <1>1, <1>2, <2>1, <2>3, <2>4 DEF TypeOK, FrameOK
  <2>6. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <2>1 DEF Ghosts
  <2>7. /\ \A q \in Paths : UploadW(s, p).local[q] \in Opt(minted')
        /\ \A q \in UploadW(s, p).uploads : UploadW(s, p).snap[q] \in minted'
    BY <1>c, <2>1 DEF UploadW, Minted
  <2>7a. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
    BY <1>c, <1>1, <2>1 DEF Minted, Opt
  <2>8. Minted' BY <1>c, <2>3, <2>7, <2>7a, MintedWrite
  <2>. QED BY <2>5, <2>6, <2>8 DEF IndTypeOK
<1>4. CASE h \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>1. /\ copies < MaxCopies
        /\ live' = live \cup {c} /\ upped' = upped \cup {c} /\ minted' = minted \cup {c}
        /\ base' = [base EXCEPT ![c] = base[h]]
        /\ orig' = [orig EXCEPT ![c] = Content(h)]
        /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)]
        /\ copies' = copies + 1
    BY <1>4 DEF Upload
  <2>2. c \in Handles BY <1>b, <2>1, CopyHandle
  <2>3. Content(h) \in Opt(Handles) BY <1>b, <1>2 DEF Ghosts, Content, Opt
  <2>4. /\ [w[s].snap EXCEPT ![p] = c] \in [Paths -> Opt(Handles)]
        /\ [w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]] \in [Paths -> Opt(Handles)]
    <3>1. c \in Opt(Handles) /\ (IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]) \in Opt(Handles)
      BY <1>0, <2>2, WriterFields DEF Opt
    <3>. QED BY <1>0, <3>1, WriterFields, ExceptFun
  <2>5. UploadCopyW(s, p, c) \in Writer BY <1>0, <2>4, WriterFields DEF UploadCopyW, Writer, Opt, PendT, AckT, CarT, BoundT
  <2>6. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN UploadCopyW(s, p, c) ELSE w[t]
    BY <1>a, <2>1, <2>5, WriteOne
  <2>7. base' \in [Handles -> Opt(Handles)] /\ orig' \in [Handles -> Opt(Handles)]
    BY <1>a, <1>b, <1>2, <2>1, <2>2, <2>3 DEF TypeOK, Ghosts
  <2>8. doc' \in [Paths -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
  <2>9. FrameOK BY <1>a, <2>6, <2>7, <2>8, FrameTypeOK
  <2>10. TypeOK' BY <1>a, <1>1, <1>2, <2>1, <2>2, <2>6, <2>7, <2>9 DEF TypeOK, FrameOK
  <2>11. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>a, <1>b, <1>1, <2>1, <2>2, <2>7 DEF Ghosts, TypeOK
  <2>12. /\ \A q \in Paths : UploadCopyW(s, p, c).local[q] \in Opt(minted')
         /\ \A q \in UploadCopyW(s, p, c).uploads : UploadCopyW(s, p, c).snap[q] \in minted'
    <3>1. \A q \in Paths : [w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]][q] \in Opt(minted')
      BY <1>0, <1>c, <2>1, <2>2, WriterFields DEF Minted, Opt
    <3>2. \A q \in w[s].uploads : [w[s].snap EXCEPT ![p] = c][q] \in minted'
      BY <1>0, <1>c, <2>1, <2>2, WriterFields DEF Minted
    <3>3. /\ UploadCopyW(s, p, c).local = [w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]]
          /\ UploadCopyW(s, p, c).uploads = w[s].uploads
          /\ UploadCopyW(s, p, c).snap = [w[s].snap EXCEPT ![p] = c]
      BY DEF UploadCopyW
    <3>. QED BY <3>1, <3>2, <3>3
  <2>12a. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
    BY <1>c, <1>1, <2>1 DEF Minted, Opt
  <2>13. Minted' BY <1>c, <2>6, <2>12, <2>12a, MintedWrite
  <2>. QED BY <2>10, <2>11, <2>13 DEF IndTypeOK
<1>. QED BY <1>3, <1>4

'''
BODY += '------------------------------------------------------------------------------\n(* The commit section.                                                      *)\n\n'
BODY += step('PullOnly', 'PullOnly(s)', '', None, 'PullOnlyW(s)', 'PullOnlyW, Writer, Opt',
             '      /\\ w[s].uploads = {}\n', UNCH_ALL_BUT_W, ', bucket, aux',
             BARRIER_END('PullOnlyW(s)', 'doc', 'seq', '{}', 'FALSE'),
             ('\\A q \\in Paths : PullOnlyW(s).local[q] \\in Opt(minted\')', '\\A q \\in PullOnlyW(s).uploads : PullOnlyW(s).snap[q] \\in minted\''), 'PullOnlyW, Minted, Opt')
BODY += step('Claim', 'Claim(s)', '', None, 'ClaimW(s)', 'ClaimW, Writer',
             '      /\\ holder\' = s\n',
             '''UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>''',
             ', aux', [], *reads_unchanged('ClaimW(s)', 'ClaimW'))
BODY += step('Verify', 'Verify(s)', '', None, 'VerifyW(s)', 'VerifyW, Writer',
             '', UNCH_ALL_BUT_W, ', bucket, aux', [], *reads_unchanged('VerifyW(s)', 'VerifyW'))
_im_t, _, _, _im_d = RECMEM('InstallW(s)', 1, [], ['<1>3', '<1>4', '<1>4a'], prefix='m', memlabel='<1>6')
BODY += _im_d + r'''LEMMA Install_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Install(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK, PendT, AckT, CarT, BoundT
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. /\ doc' = InstallInst(s)
      /\ seq' = IF InstallInst(s) = doc THEN seq ELSE seq + 1
      /\ tomb' = [p \in Paths |-> IF InstallInst(s)[p] # Nil THEN Nil
                                  ELSE IF doc[p] # Nil THEN doc[p]
                                  ELSE tomb[p]]
      /\ conflicts' = conflicts \cup {<<p, w[s].snap[p]>> : p \in w[s].gone}
                                \cup {<<p, doc[p]>> : p \in InstallContested(s)}
                                \cup {<<p, DeletedAt(s, p)>> : p \in InstallDelOverridden(s)}
                                \cup {<<p, doc[p]>> : p \in InstallDelOver(s)}
      /\ w' = [w EXCEPT ![s] = InstallW(s)]
      /\ udel' = udel \cup {<<p, doc[p]>> : p \in {q \in Paths : doc[q] # Nil /\ InstallInst(s)[q] = Nil}}
      /\ UNCHANGED <<live, minted, base, acked, holder, gw, mv, nextGen, ui, reqs, barriers,
                     restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Install, aux
<1>1u. udel' \subseteq Paths \X Handles BY <1>a, <1>1 DEF TypeOK, Opt
<1>2. \A p \in InstallMine(s) : w[s].snap[p] \in minted /\ w[s].snap[p] \in Handles
  BY <1>a, <1>0, <1>c, WriterFields DEF InstallMine, Minted, TypeOK
<1>3. InstallInst(s) \in [Paths -> Opt(Handles)] /\ \A p \in Paths : InstallInst(s)[p] \in Opt(minted)
  BY <1>a, <1>0, <1>c, <1>2, WriterFields DEF InstallInst, TypeOK, Minted, Opt
<1>4. InstallRetired(s) \subseteq Handles BY <1>a DEF InstallRetired, TypeOK, Opt
<1>5. conflicts' \subseteq Paths \X Opt(Handles)
  BY <1>a, <1>0, <1>1, WriterFields DEF TypeOK, InstallMine, InstallContested, InstallDelOverridden, InstallDelOver, DeletedAt, Opt
<1>4a. InstallCar(s) \in CarT
  \* L-137's `pub` is an IF over the paths: name it, type it alone.
  <2> DEFINE C0 == InstallCar0(s)
             cPub == [p \in Paths |-> IF p \in w[s].uploads \ w[s].gone THEN w[s].snap[p]
                                      ELSE IF p \in w[s].uploads \cup w[s].deletes THEN Nil
                                      ELSE C0.pub[p]]
  <2>1. C0 \in CarT
    <3>1. w[s].car \in CarT /\ w[s].cId \in Nat BY <1>0, WriterFields
    <3>2. [id |-> w[s].cId, paths |-> {}, deletes |-> {}, dropped |-> {}, uncited |-> {},
           pub |-> [p \in Paths |-> Nil]] \in CarT
      BY <3>1 DEF CarT, Opt
    <3> QED BY <3>1, <3>2 DEF InstallCar0
  <2>2. /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
        /\ w[s].snap \in [Paths -> Opt(Handles)]
    BY <1>0, WriterFields
  <2>3. C0.pub \in [Paths -> Opt(Handles)] BY <2>1 DEF CarT
  <2>4. cPub \in [Paths -> Opt(Handles)]
    <3> SUFFICES ASSUME NEW p \in Paths PROVE cPub[p] \in Opt(Handles) OBVIOUS
    <3> QED BY <2>2, <2>3 DEF Opt
  <2>5. InstallCar(s) = [id |-> C0.id, paths |-> C0.paths \cup w[s].uploads \cup w[s].deletes,
                         deletes |-> (C0.deletes \ w[s].uploads) \cup w[s].deletes,
                         dropped |-> C0.dropped \cup w[s].gone \cup {p \in w[s].deletes : InstallInst(s)[p] # Nil},
                         uncited |-> C0.uncited, pub |-> cPub]
    BY DEF InstallCar
  <2> QED BY <2>1, <2>2, <2>4, <2>5 DEF CarT
@@INSTALLMEM@@<1>7. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN InstallW(s) ELSE w[t]
  BY <1>a, <1>1, <1>6, WriteOne
<1>8. tomb' \in [Paths -> Opt(Handles)] BY <1>a, <1>1, <1>3 DEF TypeOK, Opt
<1>9. FrameOK BY <1>a, <1>1, <1>3, <1>7, FrameTypeOK DEF TypeOK
<1>10. TypeOK' BY <1>a, <1>1, <1>1u, <1>3, <1>5, <1>7, <1>8, <1>9 DEF TypeOK, FrameOK
<1>11. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>12. /\ \A q \in Paths : InstallW(s).local[q] \in Opt(minted')
       /\ \A q \in InstallW(s).uploads : InstallW(s).snap[q] \in minted'
  BY <1>c, <1>1 DEF InstallW, Minted
<1>12a. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>3 DEF Minted, Opt
<1>13. Minted' BY <1>c, <1>7, <1>12, <1>12a, MintedWrite
<1>. QED BY <1>10, <1>11, <1>13 DEF IndTypeOK

LEMMA Collect_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Collect(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK, PendT, AckT, CarT, BoundT
<1>1. /\ live' \subseteq live
      /\ w' = [w EXCEPT ![s] = CollectW(s)]
      /\ UNCHANGED <<minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Collect, aux
<1>2. CollectW(s) \in Writer BY <1>0, WriterFields DEF CollectW, Writer, PendT, AckT, CarT, BoundT
<1>3. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN CollectW(s) ELSE w[t]
  BY <1>a, <1>1, <1>2, WriteOne
<1>4. FrameOK BY <1>a, <1>1, <1>3, FrameTypeOK DEF TypeOK
<1>5. TypeOK' BY <1>a, <1>1, <1>3, <1>4 DEF TypeOK, FrameOK
<1>6. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>7. /\ \A q \in Paths : CollectW(s).local[q] \in Opt(minted')
      /\ \A q \in CollectW(s).uploads : CollectW(s).snap[q] \in minted'
  BY <1>c, <1>1 DEF CollectW, Minted
<1>7a. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1 DEF Minted, Opt
<1>8. Minted' BY <1>c, <1>3, <1>7, <1>7a, MintedWrite
<1>. QED BY <1>5, <1>6, <1>8 DEF IndTypeOK

LEMMA Sweep_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>1. /\ live' = live \ {h}
      /\ UNCHANGED <<minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, w, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Sweep, aux
<1>2. FrameOK BY <1>a, <1>1, FrameTypeOK DEF TypeOK
<1>3. TypeOK' BY <1>a, <1>1, <1>2 DEF TypeOK, FrameOK
<1>4. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>5. /\ minted \subseteq minted' /\ w' = w
      /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1 DEF Minted, Opt
<1>6. Minted' BY <1>c, <1>5, MintedKeep
<1>. QED BY <1>3, <1>4, <1>6 DEF IndTypeOK

'''
BODY += step('Finish', 'Finish(s)', '', None, 'FinishW(s)', 'FinishW, Writer, Opt',
             '      /\\ holder\' = IF holder = s THEN "none" ELSE holder\n',
             '''UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>''',
             ', aux', BARRIER_END('FinishW(s)', 'w[s].inst', 'w[s].iseq', 'w[s].gone', 'w[s].instd', extra=', WriterFields'),
             ('\\A q \\in Paths : FinishW(s).local[q] \\in Opt(minted\')', '\\A q \\in FinishW(s).uploads : FinishW(s).snap[q] \\in minted\''), 'FinishW, Minted, Opt')
BODY += '------------------------------------------------------------------------------\n(* The restart and the sync.                                                *)\n\n'
BODY += step('Restart', 'Restart(s)', '', None, 'RestartW(s)', 'RestartW, Writer, Opt',
             '      /\\ holder\' = IF holder = s THEN "none" ELSE holder\n      /\\ restarts\' = restarts + 1\n',
             '''UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                     nextGen, ui, reqs, barriers, regressed, syncs, rescopes, fails, upped, copies, orig>>''',
             '', [], ('\\A q \\in Paths : RestartW(s).local[q] \\in Opt(minted\')', '\\A q \\in RestartW(s).uploads : RestartW(s).snap[q] \\in minted\''), 'RestartW, Minted, Opt')

def synclemma(name, recname, other_primed, unch, unch_defs):
    return step(name, f'{name}(s)', '',
                ('PICK fail \\in SUBSET SyncAll(s) :',
                 ['fails + Cardinality(fail) <= MaxFetchFails',
                  'fails\' = fails + Cardinality(fail)',
                  'regressed\' = (regressed \\/ \\E p \\in SyncOwed(s, fail) : Back(s, p))']),
                f'{recname}(s, fail)', f'{recname}, Writer, Opt', other_primed, unch, unch_defs,
                [('fail \\subseteq Paths /\\ SyncOwed(s, fail) \\subseteq Paths', 'DEF SyncAll, SyncOwed'),
                 ('Cardinality(fail) \\in Nat', '<1>3, CardPaths'),
                 ('{Gen(doc[p]) : p \\in {q \\in SyncOwed(s, fail) : doc[q] # Nil}} \\subseteq Gens', '<1>a, <1>3, DocGens'),
                 ('SyncBl(s, fail) \\in [Paths -> Opt(Handles)]', '<1>a, <1>0, WriterFields DEF SyncBl, TypeOK, Opt')],
                *reads_unchanged(f'{recname}(s, fail)', f'{recname}, SyncBl'), extra_typeok='')
BODY += synclemma('Sync', 'SyncW', '/\\ syncs\' = syncs + 1\n      /\\ ',
                  '''UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, rescopes, upped, copies, orig>>''', ', bucket')
BODY += '------------------------------------------------------------------------------\n(* The narrow / widen verb.                                                 *)\n\n'
BODY += step('RescopeBegin', 'RescopeBegin(s)', '',
             ('PICK T \\in Scopes \\ {w[s].scope} :', ['\\A p \\in Leaving(s, T) : w[s].local[p] = w[s].baseline[p]']),
             'RescopeBeginW(s, T)', 'RescopeBeginW, Writer, Opt',
             '/\\ rescopes\' = rescopes + 1\n      /\\ ',
             '''UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, fails, upped, copies, orig>>''',
             ', bucket',
             [('T \\subseteq Paths /\\ Leaving(s, T) \\subseteq Paths', 'ScopesPaths DEF Leaving')],
             ('\\A q \\in Paths : RescopeBeginW(s, T).local[q] \\in Opt(minted\')', '\\A q \\in RescopeBeginW(s, T).uploads : RescopeBeginW(s, T).snap[q] \\in minted\''),
             'RescopeBeginW, Minted, Opt')
BODY += step('RescopeFirst', 'RescopeFirst(s)', '', None, 'RescopeFirstW(s)', 'RescopeFirstW, Writer, Opt',
             '', UNCH_ALL_BUT_W, ', bucket, aux',
             [('KeepSet(s) \\subseteq Paths /\\ RescopeFirstDd(s) \\subseteq Paths', '<1>0, WriterFields DEF KeepSet, RescopeFirstDd')],
             *reads_unchanged('RescopeFirstW(s)', 'RescopeFirstW'))
BODY += r'''LEMMA RescopeSecond_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK, PendT, AckT, CarT, BoundT
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. PICK wfail \in SUBSET {p \in RescopeFetch0(s) : RescopeLocal1(s)[p] = Nil \/ ~WidenKeepsLocal} :
        /\ fails + Cardinality(wfail) <= MaxFetchFails
        /\ fails' = fails + Cardinality(wfail)
        /\ w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
  BY DEF RescopeSecond
<1>2. UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                  nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, upped, copies, orig>>
  BY DEF RescopeSecond, bucket
<1>3. /\ w[s].sTgt \subseteq Paths /\ RescopeAdd(s) \subseteq Paths
      /\ RescopeFetch0(s) \subseteq Paths /\ RescopeFetch(s, wfail) \subseteq Paths
  BY <1>0, WriterFields DEF RescopeAdd, RescopeKept, RescopeFetch0, RescopeFetch
<1>4. Cardinality(wfail) \in Nat BY <1>3, CardPaths
<1>5. /\ RescopeLocal1(s) \in [Paths -> Opt(Handles)] /\ RescopeBase1(s) \in [Paths -> Opt(Handles)]
      /\ \A p \in Paths : RescopeLocal1(s)[p] \in Opt(minted)
  BY <1>0, <1>c, WriterFields DEF RescopeLocal1, RescopeBase1, Minted, Opt
<1>6. \A p \in RescopeFetch(s, wfail) : doc[p] \in Handles /\ Gen(doc[p]) \in Gens
  BY <1>a, <1>0, <1>3, HandleGen, WriterFields DEF RescopeFetch, RescopeFetch0, RescopeAdd, TypeOK, Opt
<1>7. RescopeSecondW(s, wfail) \in Writer
  BY <1>0, <1>d, <1>3, <1>5, <1>6, WriterFields DEF RescopeSecondW, Writer, Opt
<1>8. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN RescopeSecondW(s, wfail) ELSE w[t]
  BY <1>a, <1>1, <1>7, WriteOne
<1>9. FrameOK BY <1>a, <1>2, <1>8, FrameTypeOK DEF TypeOK
<1>10. TypeOK' BY <1>a, <1>1, <1>2, <1>4, <1>8, <1>9 DEF TypeOK, FrameOK
<1>11. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>2 DEF Ghosts
<1>12. /\ \A p \in Paths : RescopeSecondW(s, wfail).local[p] \in Opt(minted')
       /\ \A q \in RescopeSecondW(s, wfail).uploads : RescopeSecondW(s, wfail).snap[q] \in minted'
  BY <1>0, <1>c, <1>2, <1>5 DEF RescopeSecondW, Minted, Opt
<1>12a. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>2 DEF Minted, Opt
<1>13. Minted' BY <1>c, <1>8, <1>12, <1>12a DEF Minted, Opt
<1>. QED BY <1>10, <1>11, <1>13 DEF IndTypeOK

'''
BODY += '------------------------------------------------------------------------------\n(* A reader\'s tick.                                                         *)\n\n'
BODY += step('RPullRead', 'RPullRead(s)', '', None, 'RPullReadW(s)', 'RPullReadW, Writer',
             '', UNCH_ALL_BUT_W, ', bucket, aux', [], *reads_unchanged('RPullReadW(s)', 'RPullReadW'))
BODY += synclemma('RPullSync', 'RPullSyncW', '',
                  '''UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, rescopes, upped, copies, orig>>''', ', bucket')

BODY += '------------------------------------------------------------------------------\n(* The boundary verbs\' answer and deposal (rows 15-17).                    *)\n\n'
BODY += step('Touch', 'Touch(s)', '', None, 'TouchW(s)', 'TouchW, Writer, Opt',
             '', UNCH_ALL_BUT_W, ', bucket, aux',
             [('TouchN(s) \\in Nat /\\ TouchNs(s) \\subseteq Nat /\\ TouchKept(s) \\subseteq Nat',
               '<1>0, WriterFields DEF TouchN, TouchNs, TouchKept, PendT'),
              ('TouchDirty(s) \\subseteq Paths /\\ nextGen \\in Nat', '<1>a DEF TouchDirty, TypeOK')],
             *reads_unchanged('TouchW(s)', 'TouchW'))
BODY += step('Honor', 'Honor(s)', '', None, 'HonorW(s)', 'HonorW, Writer, Opt',
             '', UNCH_ALL_BUT_W, ', bucket, aux',
             [('HonorDr(s) \\subseteq Paths', '<1>0, WriterFields DEF HonorDr, HonorCarried, BoundT, CarT')],
             *reads_unchanged('HonorW(s)', 'HonorW'))
BODY += step('HonorReader', 'HonorReader(s)', '', None, 'HonorReaderW(s)', 'HonorReaderW, Writer, Opt',
             '', UNCH_ALL_BUT_W, ', bucket, aux', [], *reads_unchanged('HonorReaderW(s)', 'HonorReaderW'))
BODY += step('RetireByAck', 'RetireByAck(s)', '', None, 'RetireByAckW(s)', 'RetireByAckW, Writer, NoPend',
             '', UNCH_ALL_BUT_W, ', bucket, aux', [], *reads_unchanged('RetireByAckW(s)', 'RetireByAckW'))
BODY += r"""LEMMA Depose_TypeOK ==
  ASSUME IndTypeOK, NEW t \in Writers, Depose(t), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>1. /\ holder \in Writers /\ holder # t
      /\ holder' = t /\ seq' = seq + 1
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ UNCHANGED <<live, minted, doc, tomb, base, acked, conflicts, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Depose, aux
<1>2. w[t] \in Writer /\ w[holder] \in Writer BY <1>a, <1>1 DEF TypeOK
<1>3. DeposeW(t) \in Writer BY <1>2, WriterFields DEF DeposeW, Writer, PendT, AckT, CarT, BoundT
<1>4. DeposedW(holder) \in Writer BY <1>2, WriterFields DEF DeposedW, Writer, PendT, AckT, CarT, BoundT
<1>5. /\ w' \in [Writers -> Writer]
      /\ \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>a, <1>1, <1>3, <1>4, WriteTwo
<1>6. doc' \in [Paths -> Opt(Handles)] /\ base' \in [Handles -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
<1>7. FrameOK BY <1>a, <1>5, <1>6, FrameTypeOK
<1>8. TypeOK' BY <1>a, <1>1, <1>5, <1>7 DEF TypeOK, FrameOK
<1>9. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>10. Minted'
  <2>1. \A u \in Writers : w'[u].local = w[u].local /\ w'[u].uploads = w[u].uploads /\ w'[u].snap = w[u].snap
    BY <1>5 DEF DeposeW, DeposedW
  <2>. QED BY <1>c, <1>1, <2>1 DEF Minted
<1>. QED BY <1>8, <1>9, <1>10 DEF IndTypeOK

LEMMA Fence_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Fence(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>1. UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                  nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Fence, bucket, aux
<1>2. \/ w' = [w EXCEPT ![s] = FenceBeforeW(s)]
      \/ w' = [w EXCEPT ![s] = FenceAfterW(s)]
  BY DEF Fence
<1>3. FenceBeforeW(s) \in Writer BY <1>0, WriterFields DEF FenceBeforeW, Writer, Opt, PendT, AckT, CarT, BoundT
<1>4. FenceAfterW(s) \in Writer BY <1>0, WriterFields DEF FenceAfterW, Writer, PendT, AckT, CarT, BoundT
<1>5. /\ w' \in [Writers -> Writer]
      /\ \A t \in Writers : t # s => w'[t] = w[t]
      /\ \A q \in Paths : w'[s].local[q] \in Opt(minted')
      /\ \A q \in w'[s].uploads : w'[s].snap[q] \in minted'
  <2>1. CASE w' = [w EXCEPT ![s] = FenceBeforeW(s)]
    <3>1. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN FenceBeforeW(s) ELSE w[t]
      BY <1>a, <2>1, <1>3, WriteOne
    <3>. QED BY <1>0, <1>c, <1>1, <3>1, WriterFields DEF FenceBeforeW, Minted, Opt
  <2>2. CASE w' = [w EXCEPT ![s] = FenceAfterW(s)]
    <3>1. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN FenceAfterW(s) ELSE w[t]
      BY <1>a, <2>2, <1>4, WriteOne
    <3>. QED BY <1>0, <1>c, <1>1, <3>1, WriterFields DEF FenceAfterW, Minted, Opt
  <2>. QED BY <1>2, <2>1, <2>2
<1>6. FrameOK BY <1>a, <1>1, <1>5, FrameTypeOK DEF TypeOK
<1>7. TypeOK' BY <1>a, <1>1, <1>5, <1>6 DEF TypeOK, FrameOK
<1>8. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>9. Minted' BY <1>c, <1>1, <1>5 DEF Minted, Opt
<1>. QED BY <1>7, <1>8, <1>9 DEF IndTypeOK

"""

TAIL = r'''------------------------------------------------------------------------------
(* The retire age and the document's reader: no frame, `anc` unchanged.   *)

LEMMA Age_TypeOK ==
  ASSUME IndTypeOK, Age, UNCHANGED anc
  PROVE  IndTypeOK'
BY DEF IndTypeOK, TypeOK, Ghosts, Minted, Age, aux

LEMMA Reap_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndTypeOK'
BY DEF IndTypeOK, TypeOK, Ghosts, Minted, Reap, aux

LEMMA RLoad_TypeOK ==
  ASSUME IndTypeOK, RLoad, UNCHANGED anc
  PROVE  IndTypeOK'
BY DEF IndTypeOK, TypeOK, Ghosts, Minted, RLoad, aux

------------------------------------------------------------------------------
(* The step, and the theorem.                                               *)

LEMMA Next_TypeOK == IndTypeOK /\ Next => IndTypeOK'
<1>. SUFFICES ASSUME IndTypeOK, Next PROVE IndTypeOK' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndTypeOK' BY <3>1, GPut_TypeOK
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndTypeOK' BY <3>2, GCas_TypeOK
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndTypeOK' BY <3>3, GDelete_TypeOK
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndTypeOK' BY <3>4, GRename_TypeOK
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_TypeOK
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndTypeOK' BY <3>1, Checkout_TypeOK
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndTypeOK' BY <3>2, Consume_TypeOK
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndTypeOK' BY <3>3, Scan_TypeOK
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndTypeOK' BY <3>4, Skip_TypeOK
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndTypeOK' BY <3>5, PullOnly_TypeOK
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndTypeOK' BY <3>6, Claim_TypeOK
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndTypeOK' BY <3>7, Verify_TypeOK
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndTypeOK' BY <3>8, Install_TypeOK
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndTypeOK' BY <3>9, Collect_TypeOK
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndTypeOK' BY <3>10, Finish_TypeOK
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndTypeOK' BY <3>11, Restart_TypeOK
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndTypeOK' BY <3>12, Sync_TypeOK
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndTypeOK' BY <3>13, RescopeBegin_TypeOK
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndTypeOK' BY <3>14, RescopeFirst_TypeOK
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndTypeOK' BY <3>15, RescopeSecond_TypeOK
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndTypeOK' BY <3>16, RPullRead_TypeOK
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndTypeOK' BY <3>17, RPullSync_TypeOK
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndTypeOK' BY <3>18, Edit_TypeOK
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndTypeOK' BY <3>19, Delete_TypeOK
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndTypeOK' BY <3>20, Upload_TypeOK
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndTypeOK' BY <3>21, Sweep_TypeOK
    <3>22. ASSUME NEW s \in Writers, Touch(s) PROVE IndTypeOK' BY <3>22, Touch_TypeOK
    <3>23. ASSUME NEW s \in Writers, Honor(s) PROVE IndTypeOK' BY <3>23, Honor_TypeOK
    <3>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE IndTypeOK' BY <3>24, HonorReader_TypeOK
    <3>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE IndTypeOK' BY <3>25, RetireByAck_TypeOK
    <3>26. ASSUME NEW s \in Writers, Depose(s) PROVE IndTypeOK' BY <3>26, Depose_TypeOK
    <3>27. ASSUME NEW s \in Writers, Fence(s) PROVE IndTypeOK' BY <3>27, Fence_TypeOK
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_TypeOK
  <2>2. CASE RLoad BY <2>2, RLoad_TypeOK
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndTypeOK' BY <2>3, Reap_TypeOK
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

THEOREM TypeInvariant == Spec => []TypeOK
<1>1. Init => IndTypeOK BY Init_TypeOK
<1>2. IndTypeOK /\ [Next]_vars => IndTypeOK'
  <2>1. IndTypeOK /\ Next => IndTypeOK' BY Next_TypeOK
  <2>2. IndTypeOK /\ UNCHANGED vars => IndTypeOK'
    BY DEF IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
  <2>. QED BY <2>1, <2>2
<1>3. IndTypeOK => TypeOK BY DEF IndTypeOK
<1>. QED BY <1>1, <1>2, <1>3, PTL DEF Spec
==============================================================================
'''
import re
def flatten_unchanged(text):
    """`UNCHANGED <<a, b, ...>>` in a restated step -> `a' = a /\ b' = b ...`: Z3 does not
    recover one component of a 17-tuple equality in time when a quantified goal sits beside it."""
    GROUPS = {'bucket': 'live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel',
              'aux': 'restarts, syncs, regressed, rescopes, fails, upped, copies, orig',
              'ret': 'retiring, aged, ages, rdoc, rlag'}
    def sub(m):
        names = []
        for n in m.group(1).replace('\n', ' ').split(','):
            n = n.strip()
            if not n: continue
            names += [x.strip() for x in GROUPS.get(n, n).split(',')]
        return ' /\\ '.join(f"{n}' = {n}" for n in names)
    return re.sub(r'UNCHANGED <<([^>]*)>>', sub, text)
assert BODY.count('@@INSTALLMEM@@') == 1
BODY = BODY.replace('@@INSTALLMEM@@', _im_t)
assert HEADER.count('@@WRITERT@@') == 1
HEADER = HEADER.replace('@@WRITERT@@', WRITERT)
cut = HEADER.index('(* Init.')
open(OUT, 'w').write(HEADER + BODY + TAIL)   # flatten_unchanged kept for the record: it did not help (run 9)
print('written', OUT)
