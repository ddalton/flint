#!/usr/bin/env python3
"""Generate lean/formal/LeanP1Anc.tla from LeanP1.tla (run from lean/formal).

Version 4 (2026-10-15; rows 15-17: the 21 new Writer fields, the edited
steps, the new section as records, the span pin).  Version 3 (2026-10-06; v1 is results/2026-10-05-tlaps-m0-tie/make-anc.py, v2
is make-anc-v2-superseded.py here).  The copy is LeanP1.tla with
  1. its two RECURSIVE operators restated over a ghost `anc` (tlapm
     1.6.0-pre refuses any module containing RECURSIVE), two ASSUMEs TLC
     gets from model values -- v1;
  2. every writer step's new tree written as an EXPLICIT 27-field record,
     an operator `<Step>W(...)` (the fields the step changes new, every
     other one `w[s]`'s), the step itself `w' = [w EXCEPT ![s] = <Step>W(..)]`,
     and the LET-bound names those records need lifted to operators
     (`Install*`, `Consume*`, `Scan*`, `Sync*`, `Rescope*`, `CheckoutHeld`).
     Why: tlapm prepares every obligation against the WHOLE module context,
     expanding each hidden LET by substitution and feeding Z3 an axiom set
     for each record EXCEPT it finds, used or not; the original's LET chains
     and `[w EXCEPT ![s].f = ..]` updates cost every obligation 9 s and
     2.2 GB and made Z3 time out on any record goal, while explicit records
     cost nothing and Z3 proves their membership in ~2 s (NOTES.txt here).
     Equal in every reachable state: `[w EXCEPT ![s].f = v]` and the record
     with f |-> v and every other field w[s]'s agree whenever w[s] has
     exactly Writer's fields, which TypeOK gives.  Nothing else.
Every edit asserts its anchor text occurs exactly once, so a changed
LeanP1.tla fails loudly here instead of producing a silently different
copy.  The tie to the original is the exact distinct count on the gate's
worlds (RESULTS.txt here and in the v1 directory).
"""
import hashlib, os
HERE = os.path.dirname(os.path.abspath(__file__))
RECORD = os.environ.get('RECORD_SPANS') == '1'
SPANS = {}
if not RECORD:
    for line in open(os.path.join(HERE, 'span-md5.txt')):
        if line.strip() and not line.startswith('#'):
            h, key = line.rstrip('\n').split(' ', 1); SPANS[key] = h
src = open('LeanP1.tla').read()
out = src
def rep(old, new):
    global out
    assert out.count(old) == 1, (old[:70], out.count(old))
    out = out.replace(old, new)
def rep_span(first, nxt, new):
    """Replace from the line `first` up to (not including) the text `nxt`."""
    global out
    first = '\n' + first   # at a line start: `Sync(s) ==` is inside `RPullSync(s) ==`
    assert out.count(first) == 1 and out.count(nxt) == 1, (first, out.count(first), nxt, out.count(nxt))
    a = out.index(first) + 1; b = out.index(nxt)
    assert a < b, (first, nxt)
    # v4: the ORIGINAL span's text is pinned (span-md5.txt).  v3 checked only
    # the two anchors, so the rows 15-17 edits inside Install, Finish, ...
    # and the whole new section (inside RPullSync's span) were dropped from
    # the copy without a word.  A changed span stops here until its
    # rewrite is reviewed and `RECORD_SPANS=1` re-pins it.
    key = first.strip().splitlines()[0]
    h = hashlib.md5(out[a:b].encode()).hexdigest()
    if RECORD:
        SPANS[key] = h
    else:
        assert SPANS.get(key) == h, ('LeanP1.tla changed inside a rewritten span; review its rewrite', key)
    out = out[:a] + new + out[b:]

FIELDS = ['st', 'pc', 'local', 'baseline', 'integrated', 'uploads', 'deletes', 'snap',
          'upDone', 'gone', 'verified', 'inst', 'retire', 'collected', 'adv', 'synced',
          'derived', 'skipped', 'scope', 'sStage', 'sTgt', 'sDrop', 'sKeep', 'sHeld',
          'unlinked', 'memo', 'rnow',
          # rows 15-17 (2026-10-14)
          'nt', 'pend', 'ack', 'car', 'cId', 'cFold', 'clk', 'iseq', 'instd', 'bound',
          'cN', 'aN', 'capped', 'dset', 'decl', 'dMint', 'moved', 'stamps', 'lost',
          'deposed', 'nd']
def rec(name, params, upd):
    """The explicit tree record: the fields in `upd` new, every other one w[s]'s."""
    for f in upd: assert f in FIELDS, f
    body = ',\n   '.join((f'{f} |-> {upd[f]}' if f in upd else f'{f} |-> w[s].{f}') for f in FIELDS)
    return f'{name}({params}) ==\n  [{body}]\n'
SEP = '------------------------------------------------------------------------------\n'

# --- v1: the module name and header note. ---
rep('------------------------------- MODULE LeanP1 ---------------------------------',
    '----------------------------- MODULE LeanP1Anc -------------------------------\n'
    '(* LeanP1.tla for TLAPS: the SAME protocol, restated where tlapm needs it.\n'
    '   tlapm (1.6.0-pre, bfa9468) dies in module elaboration on the mere\n'
    '   presence of a RECURSIVE definition (results/2026-10-05-tlaps-m0/), so the\n'
    '   proof module cannot EXTEND LeanP1.tla.  This copy is GENERATED from it by\n'
    '   results/2026-10-06-tlaps-typeok/make-anc.py and differs ONLY in: the\n'
    '   module name; two ASSUMEs TLC gets free from model values; the variable\n'
    '   `anc` -- for each minted handle, every handle its base chain reaches --\n'
    '   written by the step (`AncUpdate`, like `TookUpdate`) from `base`, which\n'
    '   is write-once per handle; `Derives` / `Supersedes` restated as the\n'
    '   one-level search over `anc` that their recursion computed; and every\n'
    '   writer step\'s new tree written as an EXPLICIT record, an operator\n'
    '   `<Step>W(..)` (the fields the step changes new, every other one w[s]\'s),\n'
    '   with the LET names those records need lifted to operators -- tlapm\n'
    '   prepares each obligation against the whole module, expanding hidden\n'
    '   LETs by substitution and giving Z3 axioms for every record EXCEPT it\n'
    '   finds, and LeanP1.tla\'s shapes cost each obligation 9 s and 2.2 GB and\n'
    '   put every record goal past Z3\'s time limit\n'
    '   (results/2026-10-06-tlaps-typeok/NOTES.txt).  `anc` is a function of\n'
    '   `base`, and a record with the changed fields new and the rest w[s]\'s IS\n'
    '   the EXCEPT whenever w[s] has Writer\'s fields (TypeOK), so the copy has\n'
    '   exactly the original\'s states: the tie to LeanP1.tla is the EXACT\n'
    '   distinct count on the gate\'s worlds (Holds1p3b = 461,094,969), and\n'
    '   MCLeanP1AncCheck.tla checks the restated operators against the\n'
    '   recursive originals state by state.  LeanP1.tla stays the module the\n'
    '   gate checks; this one is what LeanP1Proof.tla EXTENDS.               *)\n'
    '(* --- LeanP1.tla\'s own header follows unchanged. ---                        *)')
rep('Handles == Paths \\X Gens\n',
    'Handles == Paths \\X Gens\n'
    '\\* TLC gets these from model values; a proof has to be told.\n'
    'ASSUME Nil \\notin Handles\n'
    'ASSUME "none" \\notin Writers\n')
rep('  retiring, aged, ages, rdoc, rlag\n\nbucket ==',
    '  retiring, aged, ages, rdoc, rlag,\n'
    '  anc     \\* GHOST: per minted handle, every handle its base chain reaches\n\nbucket ==')
rep('          nextGen, ui, reqs, barriers, w, took, gw, mv, udel, aux, ret>>',
    '          nextGen, ui, reqs, barriers, w, took, gw, mv, udel, aux, ret, anc>>')
rep('  /\\ rdoc \\in [Paths -> Opt(Handles)] /\\ rlag \\in BOOLEAN\n',
    '  /\\ rdoc \\in [Paths -> Opt(Handles)] /\\ rlag \\in BOOLEAN\n'
    '  /\\ anc \\in [Handles -> SUBSET Handles]\n')
rep('RECURSIVE Derives(_, _)\n'
    'Derives(k, h) == \\/ k = h\n'
    '                 \\/ k # Nil /\\ h # Nil /\\ Content(k) = Content(h)\n'
    '                 \\/ k # Nil /\\ base[k] # Nil /\\ Derives(base[k], h)\n',
    '\\* LeanP1.tla: recursive over base[k].  Here: k itself or any handle on its\n'
    '\\* base chain (`anc[k]`) is h or holds h\'s bytes.\n'
    'Derives(k, h) == \\/ k = h\n'
    '                 \\/ k # Nil /\\ h # Nil /\\ Content(k) = Content(h)\n'
    '                 \\/ k # Nil /\\ \\E a \\in anc[k] : a = h \\/ (h # Nil /\\ Content(a) = Content(h))\n')
rep('RECURSIVE Supersedes(_, _, _)\n'
    'Supersedes(k, h, p) ==\n'
    '  \\/ k = h\n'
    '  \\/ k # Nil /\\ h # Nil /\\ Content(k) = Content(h)\n'
    '  \\/ k # Nil /\\ base[k] # Nil /\\ Supersedes(base[k], h, p)\n'
    '  \\/ <<p, k>> \\in acked /\\ <<p, h>> \\in acked /\\ Later(k, h)\n',
    '\\* LeanP1.tla: recursive over base[k], the acked clause tried at every level.\n'
    'Supersedes(k, h, p) ==\n'
    '  \\/ Derives(k, h)\n'
    '  \\/ k # Nil /\\ \\E a \\in {k} \\cup anc[k] : <<p, a>> \\in acked /\\ <<p, h>> \\in acked /\\ Later(a, h)\n')
rep('  /\\ rdoc = [p \\in Paths |-> Nil] /\\ rlag = TRUE\n',
    '  /\\ rdoc = [p \\in Paths |-> Nil] /\\ rlag = TRUE\n'
    '  /\\ anc = [h \\in Handles |-> {}]\n')
rep('Next ==\n'
    '  \\/ (GatewayStep \\/ WriterStep) /\\ TookUpdate /\\ RetUpdate /\\ UNCHANGED <<aged, ages, rdoc, rlag>>\n'
    '  \\/ Age \\/ RLoad \\/ \\E s \\in Writers, h \\in Handles : Reap(s, h)\n',
    '\\* THE GHOST: the ancestry of what this step minted (GPut, Edit, a copy at\n'
    '\\* Upload), from the base it recorded; `base` is write-once per handle.\n'
    'AncOf(b) == IF b = Nil THEN {} ELSE {b} \\cup anc[b]\n'
    'AncUpdate == anc\' = [h \\in Handles |-> IF base\'[h] = base[h] THEN anc[h] ELSE AncOf(base\'[h])]\n'
    '\n'
    'Next ==\n'
    '  \\/ (GatewayStep \\/ WriterStep) /\\ TookUpdate /\\ RetUpdate /\\ AncUpdate /\\ UNCHANGED <<aged, ages, rdoc, rlag>>\n'
    '  \\/ (Age \\/ RLoad \\/ \\E s \\in Writers, h \\in Handles : Reap(s, h)) /\\ UNCHANGED anc\n')
rep('LSpec == Spec /\\ \\A p \\in Paths : WF_vars(GCas(p) /\\ TookUpdate /\\ RetUpdate /\\ UNCHANGED <<aged, ages, rdoc, rlag>>)',
    'LSpec == Spec /\\ \\A p \\in Paths : WF_vars(GCas(p) /\\ TookUpdate /\\ RetUpdate /\\ AncUpdate /\\ UNCHANGED <<aged, ages, rdoc, rlag>>)')

# --- v3: every writer step's tree as an explicit record. ---
NILMAP = '[p \\in Paths |-> Nil]'
# --- v4 (rows 15-17): what a barrier's end writes, and the barrier's reset. ---
def bound_of(doc, seq, wd, inst):
    return ('IF w[s].cId # 0\n'
            f'             THEN [id |-> w[s].cId, fold |-> w[s].cFold, doc |-> {doc}, seq |-> {seq},\n'
            f'                   wd |-> {wd}, inst |-> {inst}, clk |-> w[s].clk]\n'
            '             ELSE w[s].bound')
def car_uncited(d):
    return f'IF w[s].car.id # 0 THEN CarUncited(w[s].car, {d}) ELSE w[s].car'
BARRIER_RESET = {'cId': '0', 'cFold': '0', 'clk': '"none"', 'iseq': '0', 'instd': 'FALSE'}
MOVED = 'IF w[s].pend.id # 0 THEN w[s].moved \\cup {p} ELSE w[s].moved'

rep_span('Edit(s, p) ==\n', 'Delete(s, p) ==\n',
    '\\* The tree after each writer step: an explicit record (see the header).\n'
    + rec('EditW', 's, p', {
        'local': '[w[s].local EXCEPT ![p] = <<p, nextGen>>]',
        'integrated': 'w[s].integrated \\cup {nextGen}',
        'unlinked': 'w[s].unlinked \\ {p}', 'moved': MOVED}) +
    'Edit(s, p) ==\n'
    '  /\\ On(s) /\\ nextGen <= MaxMint\n'
    '  /\\ LET h == <<p, nextGen>> IN\n'
    '     /\\ minted\' = minted \\cup {h}\n'
    '     /\\ base\' = [base EXCEPT ![h] = IF w[s].local[p] # Nil THEN w[s].local[p] ELSE IF p \\in w[s].uploads THEN w[s].snap[p] ELSE w[s].baseline[p]]\n'
    '     /\\ w\' = [w EXCEPT ![s] = EditW(s, p)]\n'
    '     /\\ nextGen\' = nextGen + 1\n'
    '  /\\ UNCHANGED <<live, doc, seq, tomb, acked, conflicts, holder, ui, reqs, barriers, gw, mv, udel, aux>>\n'
    '\n')

rep_span('Delete(s, p) ==\n', '\\* A checkout materializes what its scope admits (`checkout_scoped`).\n',
    rec('DeleteW', 's, p', {
        'local': '[w[s].local EXCEPT ![p] = Nil]',
        'unlinked': 'w[s].unlinked \\ {p}', 'moved': MOVED}) +
    'Delete(s, p) ==\n'
    '  /\\ On(s) /\\ w[s].local[p] # Nil\n'
    '  /\\ w\' = [w EXCEPT ![s] = DeleteW(s, p)]\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>\n'
    '\n')

rep_span('Checkout(s) ==\n', '(* Step 1 (P1-lite): what the tree is OWED',
    '\\* What a checkout with scope T materializes.\n'
    'CheckoutHeld(s, T) == [p \\in Paths |-> IF p \\in T THEN doc[p] ELSE Nil]\n'
    + rec('CheckoutW', 's, T', {
        'st': '"on"',
        'local': 'CheckoutHeld(s, T)', 'baseline': 'CheckoutHeld(s, T)', 'inst': 'doc',
        'integrated': '{Gen(doc[p]) : p \\in {q \\in T : doc[q] # Nil}}',
        'synced': 'seq', 'derived': 'seq', 'skipped': '{}', 'scope': 'T'}) +
    'Checkout(s) ==\n'
    '  /\\ w[s].st = "off"\n'
    '  /\\ \\E T \\in Scopes : w\' = [w EXCEPT ![s] = CheckoutW(s, T)]\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>\n'
    '\n' + SEP)

rep_span('Consume(s) ==\n', '\\* Step 2: what differs from the baseline is an upload or a deletion; a\n',
    '\\* The consume\'s parts (LeanP1.tla: `Consume`\'s LET).\n'
    'ConsumeOwed(s) == {p \\in Paths : Owed(s, p)}\n'
    'ConsumeConv(s) == {p \\in Paths : Converges(s, p)}\n'
    'ConsumeTaken(s, fail) == (ConsumeOwed(s) \\ fail) \\cup ConsumeConv(s)\n'
    + rec('ConsumeCheapW', 's, ck', {'pc': '"consumed"', 'cId': 'w[s].pend.id', 'cFold': 'w[s].pend.fold', 'clk': 'ck'}) +
    '\\* Something left owed (fail # {}): the tree is not integrated with this\n'
    '\\* document, and nothing is recorded as derived.  A dirty path left untaken\n'
    '\\* is the agent\'s work, owed again the moment the agent backs out (a revert,\n'
    '\\* or a new file deleted unpublished): `skipped`.\n'
    + rec('ConsumeW', 's, fail, ck', {
        'pc': '"consumed"', 'cId': 'w[s].pend.id', 'cFold': 'w[s].pend.fold', 'clk': 'ck',
        'local': '[p \\in Paths |-> IF p \\in ConsumeTaken(s, fail) THEN doc[p] ELSE w[s].local[p]]',
        'baseline': '[p \\in Paths |-> IF p \\in ConsumeTaken(s, fail) THEN doc[p] ELSE w[s].baseline[p]]',
        'integrated': 'w[s].integrated \\cup {Gen(doc[p]) : p \\in {q \\in ConsumeTaken(s, fail) : doc[q] # Nil}}',
        'synced': 'IF fail # {} THEN w[s].synced ELSE seq',
        'derived': 'IF fail # {} /\\ ConsumeKeepsLeft THEN 0 ELSE seq',
        'skipped': '{p \\in Paths \\ ConsumeTaken(s, fail) : doc[p] # w[s].baseline[p] /\\ Held(s, p)\n'
                   '                                            /\\ w[s].local[p] # w[s].baseline[p]}'}) +
    'Consume(s) ==\n'
    '  /\\ On(s) /\\ s \\notin Readers /\\ w[s].pc = "idle" /\\ barriers < MaxBarriers\n'
    '  \\* Step 0 replays a rescope in flight first (`run_barrier`).\n'
    '  /\\ w[s].sStage = "none"\n'
    '  /\\ \\E ck \\in CarryClocks(s) :\n'
    '     IF CheapPath(s)\n'
    '     THEN /\\ w\' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]\n'
    '          /\\ UNCHANGED <<regressed, fails>>\n'
    '     ELSE \\E fail \\in SUBSET ConsumeOwed(s) :\n'
    '             /\\ fails + Cardinality(fail) <= MaxFetchFails\n'
    '             /\\ w\' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]\n'
    '             /\\ fails\' = fails + Cardinality(fail)\n'
    '             /\\ regressed\' = (regressed \\/ \\E p \\in ConsumeOwed(s) \\ fail : Back(s, p))\n'
    '  /\\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,\n'
    '                 nextGen, ui, reqs, barriers, restarts, syncs, rescopes, upped, copies, orig>>\n'
    '\n')

rep_span('Scan(s) ==\n', '\\* Skip-on-no-diff: no scan, no CAS.\n',
    '\\* The scan\'s parts (LeanP1.tla: `Scan`\'s LET).\n'
    'ScanDirty(s) == {p \\in Paths : w[s].local[p] # w[s].baseline[p]}\n'
    'ScanUps(s) == {p \\in ScanDirty(s) : w[s].local[p] # Nil}\n'
    'ScanAbsent(s) == {p \\in ScanDirty(s) : w[s].local[p] = Nil}\n'
    + rec('ScanW', 's, dels', {
        'pc': '"scanned"', 'uploads': 'ScanUps(s)', 'deletes': 'dels', 'snap': 'w[s].local',
        'upDone': '{}', 'gone': '{}', 'verified': 'FALSE', 'retire': '{}', 'collected': 'FALSE'}) +
    'Scan(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "consumed"\n'
    '  \\* A carrying barrier confirms first absences (the declared form).\n'
    '  /\\ \\E dels \\in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) :\n'
    '       w\' = [w EXCEPT ![s] = ScanW(s, dels)]\n'
    '  /\\ barriers\' = barriers + 1\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, aux>>\n'
    '\n')

rep_span('Skip(s) ==\n', '\\* Step 4: each upload lands at a fresh key.',
    '\\* The carrier\'s `uncited` recomputed from a document (`note_carrier_uncited`).\n'
    'CarUncited(c, d) == [id |-> c.id, paths |-> c.paths, deletes |-> c.deletes,\n'
    '                     dropped |-> c.dropped, uncited |-> Uncited(c, d), pub |-> c.pub]\n'
    + rec('SkipW', 's', dict(BARRIER_RESET, pc='"idle"', car=car_uncited('doc'),
                             bound=bound_of('doc', 'seq', '{}', 'FALSE'))) +
    'Skip(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "consumed" /\\ barriers < MaxBarriers\n'
    '  /\\ \\A p \\in Paths : w[s].local[p] = w[s].baseline[p]\n'
    '  /\\ seq = w[s].synced\n'
    '  /\\ w\' = [w EXCEPT ![s] = SkipW(s)]\n'
    '  /\\ barriers\' = barriers + 1\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, aux>>\n'
    '\n')

rep_span('Upload(s, p) ==\n', '(* The merge\'s vocabulary, read at the CAS',
    rec('UploadW', 's, p', {'upDone': 'w[s].upDone \\cup {p}'}) +
    '\\* ...and with the bytes landed at the copy c, which names them from here on.\n'
    + rec('UploadCopyW', 's, p, c', {
        'upDone': 'w[s].upDone \\cup {p}',
        'snap': '[w[s].snap EXCEPT ![p] = c]',
        'local': '[w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]]'}) +
    'Upload(s, p) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "scanned"\n'
    '  /\\ p \\in w[s].uploads \\ w[s].upDone\n'
    '  /\\ LET h == w[s].snap[p] IN\n'
    '     IF h \\notin upped\n'
    '     THEN /\\ live\' = live \\cup {h} /\\ upped\' = upped \\cup {h}\n'
    '          /\\ w\' = [w EXCEPT ![s] = UploadW(s, p)]\n'
    '          /\\ UNCHANGED <<minted, base, copies, orig>>\n'
    '     ELSE /\\ copies < MaxCopies\n'
    '          /\\ LET c == <<p, MaxMint + copies + 1>> IN\n'
    '             /\\ live\' = live \\cup {c} /\\ upped\' = upped \\cup {c} /\\ minted\' = minted \\cup {c}\n'
    '             /\\ base\' = [base EXCEPT ![c] = base[h]]\n'
    '             /\\ orig\' = [orig EXCEPT ![c] = Content(h)]\n'
    '             /\\ w\' = [w EXCEPT ![s] = UploadCopyW(s, p, c)]\n'
    '          /\\ copies\' = copies + 1\n'
    '  /\\ UNCHANGED <<doc, seq, tomb, acked, conflicts, holder, gw, mv, udel,\n'
    '                 nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails>>\n'
    '\n' + SEP)

rep_span('PullOnly(s) ==\n', '(* The writers\' commit section: one writer at a time.',
    rec('PullOnlyW', 's', {
        'pc': '"idle"', 'inst': 'doc', 'synced': 'seq', 'snap': NILMAP,
        'upDone': '{}', 'gone': '{}', 'verified': 'FALSE',
        **BARRIER_RESET, 'car': car_uncited('doc'), 'bound': bound_of('doc', 'seq', '{}', 'FALSE')}) +
    'PullOnly(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "scanned"\n'
    '  /\\ w[s].uploads = {} /\\ w[s].deletes = {}\n'
    '  /\\ w\' = [w EXCEPT ![s] = PullOnlyW(s)]\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>\n'
    '\n' + SEP)

rep_span('Claim(s) ==\n', '\\* R4a: the commit re-reads every upload it is about to cite and withholds\n',
    rec('ClaimW', 's', {'pc': '"claimed"'}) +
    'Claim(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "scanned"\n'
    '  /\\ w[s].uploads \\subseteq w[s].upDone\n'
    '  /\\ w[s].uploads \\cup w[s].deletes # {}\n'
    '  /\\ holder = "none"\n'
    '  /\\ holder\' = s\n'
    '  /\\ w\' = [w EXCEPT ![s] = ClaimW(s)]\n'
    '  /\\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,\n'
    '                 nextGen, ui, reqs, barriers, aux>>\n'
    '\n')

rep_span('Verify(s) ==\n', '\\* Step 5: the merge onto the CURRENT document, and the CAS.',
    rec('VerifyW', 's', {
        'verified': 'TRUE',
        'gone': 'IF CommitVerifiesUploads\n'
                '           THEN {p \\in w[s].uploads \\cap w[s].upDone : w[s].snap[p] \\notin live}\n'
                '           ELSE {}'}) +
    'Verify(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "claimed" /\\ (holder = s \\/ w[s].deposed) /\\ ~w[s].verified\n'
    '  /\\ w\' = [w EXCEPT ![s] = VerifyW(s)]\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>\n'
    '\n')

rep_span('Install(s) ==\n', '\\* Step 6: the retired set, in one batch, sparing what the installed\n',
    '\\* The merge\'s parts (LeanP1.tla: `Install`\'s LET).\n'
    'InstallMine(s) == w[s].uploads \\cap w[s].upDone\n'
    'InstallContested(s) == IF CommitSurfacesForeign\n'
    '                       THEN {p \\in InstallMine(s) \\ w[s].gone : Foreign(s, p) /\\ doc[p] # Nil}\n'
    '                       ELSE {}\n'
    'InstallDelOverridden(s) == IF CommitRecordsDeleteOverride\n'
    '                           THEN {p \\in InstallMine(s) \\ w[s].gone : Foreign(s, p) /\\ doc[p] = Nil /\\ w[s].baseline[p] # Nil}\n'
    '                           ELSE {}\n'
    'InstallDelOver(s) == IF DeleteWinsPreserved\n'
    '                     THEN {p \\in w[s].deletes : Foreign(s, p) /\\ doc[p] # Nil}\n'
    '                     ELSE {}\n'
    'InstallInst(s) == [p \\in Paths |->\n'
    '                     IF p \\in w[s].gone THEN doc[p]\n'
    '                     ELSE IF p \\in InstallMine(s) THEN w[s].snap[p]\n'
    '                     ELSE IF p \\in w[s].deletes /\\ DeleteWinsPreserved THEN Nil\n'
    '                     ELSE IF Foreign(s, p) THEN doc[p]\n'
    '                     ELSE IF p \\in w[s].deletes THEN Nil\n'
    '                     ELSE doc[p]]\n'
    'InstallRetired(s) == {doc[p] : p \\in {q \\in Paths : doc[q] # Nil /\\ InstallInst(s)[q] # doc[q]}}\n'
    'InstallNothing(s) == InstallInst(s) = doc\n'
    '\\* H10: what a carrying CAS published of the declaration, journalled with it.\n'
    'InstallCar0(s) == IF w[s].car.id = w[s].cId THEN w[s].car\n'
    '                  ELSE [id |-> w[s].cId, paths |-> {}, deletes |-> {}, dropped |-> {}, uncited |-> {},\n'
    '                        pub |-> [p \\in Paths |-> Nil]]\n'
    'InstallCar(s) ==\n'
    '  [id |-> InstallCar0(s).id,\n'
    '   paths |-> InstallCar0(s).paths \\cup w[s].uploads \\cup w[s].deletes,\n'
    '   deletes |-> (InstallCar0(s).deletes \\ w[s].uploads) \\cup w[s].deletes,\n'
    '   dropped |-> InstallCar0(s).dropped \\cup w[s].gone \\cup {p \\in w[s].deletes : InstallInst(s)[p] # Nil},\n'
    '   uncited |-> InstallCar0(s).uncited,\n'
    '   pub |-> [p \\in Paths |-> IF p \\in w[s].uploads \\ w[s].gone THEN w[s].snap[p]\n'
    '                           ELSE IF p \\in w[s].uploads \\cup w[s].deletes THEN Nil\n'
    '                           ELSE InstallCar0(s).pub[p]]]\n'
    '\\* `adv`: the CAS replaced exactly the derived document: the installed one\n'
    '\\* is it plus this tree\'s own changes.\n'
    + rec('InstallW', 's', {
        'pc': '"cased"', 'upDone': 'w[s].upDone \\ w[s].gone',
        'inst': 'InstallInst(s)', 'retire': 'InstallRetired(s)',
        'adv': 'IF CommitAdvanceGuarded THEN seq = w[s].derived ELSE TRUE',
        'collected': 'InstallRetired(s) = {}',
        'car': 'IF Carries(s) /\\ ~InstallNothing(s) THEN InstallCar(s) ELSE w[s].car',
        'stamps': 'IF Carries(s) /\\ ~InstallNothing(s) THEN w[s].stamps \\cup {<<seq + 1, w[s].clk>>} ELSE w[s].stamps',
        'lost': 'IF MaxTouches > 0 THEN w[s].lost \\cup {w[s].snap[p] : p \\in w[s].gone} ELSE w[s].lost',
        'iseq': 'IF Carries(s) THEN (IF InstallNothing(s) THEN seq ELSE seq + 1) ELSE w[s].iseq',
        'instd': 'IF Carries(s) THEN ~InstallNothing(s) ELSE w[s].instd'}) +
    'Install(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "claimed" /\\ (InstallFenced => holder = s) /\\ w[s].verified\n'
    '  /\\ LET W == w[s]\n'
    '         inst == InstallInst(s)\n'
    '         nothing == inst = doc\n'
    '     IN\n'
    '       /\\ doc\' = inst\n'
    '       /\\ seq\' = IF nothing THEN seq ELSE seq + 1\n'
    '       /\\ tomb\' = [p \\in Paths |-> IF inst[p] # Nil THEN Nil\n'
    '                                   ELSE IF doc[p] # Nil THEN doc[p]\n'
    '                                   ELSE tomb[p]]\n'
    '       /\\ conflicts\' = conflicts \\cup {<<p, W.snap[p]>> : p \\in W.gone}\n'
    '                                 \\cup {<<p, doc[p]>> : p \\in InstallContested(s)}\n'
    '                                 \\cup {<<p, DeletedAt(s, p)>> : p \\in InstallDelOverridden(s)}\n'
    '                                 \\cup {<<p, doc[p]>> : p \\in InstallDelOver(s)}\n'
    '       /\\ w\' = [w EXCEPT ![s] = InstallW(s)]\n'
    '       /\\ udel\' = udel \\cup {<<p, doc[p]>> : p \\in {q \\in Paths : doc[q] # Nil /\\ inst[q] = Nil}}\n'
    '  /\\ UNCHANGED <<live, minted, base, acked, holder, gw, mv, nextGen, ui, reqs, barriers, aux>>\n'
    '\n')

rep_span('Collect(s) ==\n', '\\* The orphan sweep (R4b): nothing cites it, no record preserves it',
    rec('CollectW', 's', {'retire': '{}', 'collected': 'TRUE'}) +
    'Collect(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "cased" /\\ ~w[s].collected\n'
    '  /\\ DeletesUnderLease => holder = s\n'
    '  /\\ LET W == w[s]\n'
    '         taken == {h \\in W.retire : ~(CollectorSparesCited /\\ \\E q \\in Paths : W.inst[q] = h)}\n'
    '     IN /\\ live\' = IF RetireAge THEN live ELSE live \\ taken\n'
    '        /\\ w\' = [w EXCEPT ![s] = CollectW(s)]\n'
    '  /\\ UNCHANGED <<minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,\n'
    '                 nextGen, ui, reqs, barriers, aux>>\n'
    '\n')

rep_span('Finish(s) ==\n', '(* The restart and the sync.',
    rec('FinishW', 's', {
        'pc': '"idle"',
        'baseline': '[p \\in Paths |->\n'
                    '                   IF p \\in w[s].uploads \\cap w[s].upDone THEN w[s].snap[p]\n'
                    '                   ELSE IF p \\in w[s].deletes /\\ w[s].inst[p] = Nil THEN Nil\n'
                    '                   ELSE w[s].baseline[p]]',
        'synced': 'IF w[s].inst = doc THEN seq ELSE w[s].synced',
        'derived': 'IF w[s].adv /\\ w[s].inst = doc THEN seq ELSE w[s].derived',
        'skipped': 'IF w[s].adv /\\ w[s].inst = doc\n'
                   '                 THEN w[s].skipped \\ ((w[s].uploads \\cap w[s].upDone) \\cup {p \\in w[s].deletes : w[s].inst[p] = Nil})\n'
                   '                 ELSE w[s].skipped',
        'adv': 'FALSE', 'uploads': '{}', 'deletes': '{}', 'snap': NILMAP,
        'upDone': '{}', 'gone': '{}', 'verified': 'FALSE', 'collected': 'FALSE',
        **BARRIER_RESET, 'deposed': 'FALSE',
        'car': car_uncited('w[s].inst'),
        'bound': bound_of('w[s].inst', 'w[s].iseq', 'w[s].gone', 'w[s].instd')}) +
    'Finish(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "cased" /\\ w[s].collected\n'
    '  /\\ w\' = [w EXCEPT ![s] = FinishW(s)]\n'
    '  \\* The release is a CAS on this writer\'s token (`lease::release`).\n'
    '  /\\ holder\' = IF holder = s THEN "none" ELSE holder\n'
    '  /\\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,\n'
    '                 nextGen, ui, reqs, barriers, aux>>\n'
    '\n' + SEP)

rep_span('Restart(s) ==\n', '\\* A sync, between barriers: it takes what is owed, and records what it\n',
    '\\* The intent is on disk; its replay starts the apply over (`sStage`).\n'
    + rec('RestartW', 's', {
        'pc': '"idle"', 'uploads': '{}', 'deletes': '{}', 'snap': NILMAP,
        'upDone': '{}', 'gone': '{}', 'verified': 'FALSE', 'inst': NILMAP, 'retire': '{}',
        'collected': 'FALSE', 'adv': 'FALSE',
        'sStage': 'IF w[s].sStage = "none" THEN "none" ELSE "saved"', 'sKeep': '{}',
        **BARRIER_RESET, 'deposed': 'FALSE'}) +
    'Restart(s) ==\n'
    '  /\\ On(s) /\\ restarts < MaxRestarts\n'
    '  /\\ w\' = [w EXCEPT ![s] = RestartW(s)]\n'
    '  /\\ holder\' = IF holder = s THEN "none" ELSE holder\n'
    '  /\\ restarts\' = restarts + 1\n'
    '  /\\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,\n'
    '                 nextGen, ui, reqs, barriers, regressed, syncs, rescopes, fails, upped, copies, orig>>\n'
    '\n')

SYNC_FIELDS = {
    'local': '[p \\in Paths |-> IF p \\in SyncOwed(s, fail) THEN doc[p] ELSE w[s].local[p]]',
    'baseline': 'SyncBl(s, fail)',
    'integrated': 'w[s].integrated \\cup {Gen(doc[p]) : p \\in {q \\in SyncOwed(s, fail) : doc[q] # Nil}}',
    'derived': 'IF fail # {} /\\ SyncKeepsLeft THEN 0 ELSE seq',
    'skipped': 'IF fail # {} /\\ SyncKeepsLeft THEN {}\n'
               '              ELSE {p \\in Paths : doc[p] # SyncBl(s, fail)[p] /\\ w[s].local[p] # SyncBl(s, fail)[p] /\\ Held(s, p)}'}
rep_span('Sync(s) ==\n', '(* The narrow / widen verb (`checkout.rs::rescope`, scoped-read design',
    '\\* The sync\'s parts (LeanP1.tla: `Sync`\'s and `RPullSync`\'s LET): what is\n'
    '\\* owed, what of it this sync takes (the rest failed to fetch), the baseline\n'
    '\\* after it.\n'
    'SyncAll(s) == {p \\in Paths : Owed(s, p)}\n'
    'SyncOwed(s, fail) == SyncAll(s) \\ fail\n'
    'SyncBl(s, fail) == [p \\in Paths |-> IF p \\in SyncOwed(s, fail) THEN doc[p] ELSE w[s].baseline[p]]\n'
    + rec('SyncW', 's, fail', SYNC_FIELDS) +
    'Sync(s) ==\n'
    '  /\\ On(s) /\\ s \\notin Readers /\\ w[s].pc = "idle" /\\ syncs < MaxSyncs /\\ w[s].sStage \\in {"none", "saved"}\n'
    '  /\\ \\E p \\in Paths : Owed(s, p)\n'
    '  /\\ \\E fail \\in SUBSET SyncAll(s) :\n'
    '       /\\ fails + Cardinality(fail) <= MaxFetchFails\n'
    '       /\\ w\' = [w EXCEPT ![s] = SyncW(s, fail)]\n'
    '       /\\ fails\' = fails + Cardinality(fail)\n'
    '       /\\ regressed\' = (regressed \\/ \\E p \\in SyncOwed(s, fail) : Back(s, p))\n'
    '  /\\ syncs\' = syncs + 1\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, restarts, rescopes, upped, copies, orig>>\n'
    '\n' + SEP)

rep_span('RescopeBegin(s) ==\n', '\\* What the apply keeps: still cited and dirty.',
    rec('RescopeBeginW', 's, T', {
        'sStage': '"saved"', 'sTgt': 'T', 'sDrop': 'Leaving(s, T)', 'sKeep': '{}', 'sHeld': NILMAP}) +
    'RescopeBegin(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage = "none" /\\ rescopes < MaxRescopes\n'
    '  /\\ \\E T \\in Scopes \\ {w[s].scope} :\n'
    '       /\\ \\A p \\in Leaving(s, T) : w[s].local[p] = w[s].baseline[p]\n'
    '       /\\ w\' = [w EXCEPT ![s] = RescopeBeginW(s, T)]\n'
    '  /\\ rescopes\' = rescopes + 1\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, restarts, syncs, regressed, fails, upped, copies, orig>>\n'
    '\n')

rep_span('RescopeFirst(s) ==\n', '\\* The second half, the widen, and the new scope; then the intent clears.',
    '\\* What the first half drops: the drop set less what the apply keeps.\n'
    'RescopeFirstDd(s) == w[s].sDrop \\ KeepSet(s)\n'
    '\\* `sHeld`: what it uncites, recorded with the intent; a replay keeps what an\n'
    '\\* earlier run recorded.\n'
    + rec('RescopeFirstW', 's', {
        'sStage': '"mid"', 'sKeep': 'KeepSet(s)',
        'sHeld': '[p \\in Paths |-> IF p \\in RescopeFirstDd(s) /\\ w[s].baseline[p] # Nil\n'
                 '                             THEN w[s].baseline[p] ELSE w[s].sHeld[p]]',
        'baseline': 'IF RescopeUnciteFirst\n'
                    '                THEN [p \\in Paths |-> IF p \\in RescopeFirstDd(s) THEN Nil ELSE w[s].baseline[p]]\n'
                    '                ELSE w[s].baseline',
        'local': 'IF RescopeUnciteFirst\n'
                 '             THEN w[s].local\n'
                 '             ELSE [p \\in Paths |-> IF p \\in RescopeFirstDd(s) THEN Nil ELSE w[s].local[p]]',
        'unlinked': 'IF RescopeUnciteFirst\n'
                    '                THEN w[s].unlinked\n'
                    '                ELSE w[s].unlinked \\cup {p \\in RescopeFirstDd(s) : w[s].local[p] # Nil}'}) +
    'RescopeFirst(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage = "saved"\n'
    '  /\\ w\' = [w EXCEPT ![s] = RescopeFirstW(s)]\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>\n'
    '\n')

rep_span('RescopeSecond(s) ==\n', '(* A reader\'s tick (`reader.rs::reader_pull`).',
    '\\* The widen\'s parts (LeanP1.tla: `RescopeSecond`\'s LET).\n'
    'RescopeDrop(s) == w[s].sDrop \\ w[s].sKeep\n'
    '\\* The unlink: what the uncite dropped, unless the tree\'s bytes there are\n'
    '\\* no longer those (the agent wrote since).\n'
    'RescopeUnlink(s, p) == /\\ p \\in RescopeDrop(s) /\\ w[s].local[p] # Nil\n'
    '                       /\\ \\/ ~UnlinkChecksBytes\n'
    '                          \\/ w[s].sHeld[p] # Nil /\\ Content(w[s].local[p]) = Content(w[s].sHeld[p])\n'
    'RescopeLocal1(s) == IF RescopeUnciteFirst\n'
    '                    THEN [p \\in Paths |-> IF RescopeUnlink(s, p) THEN Nil ELSE w[s].local[p]]\n'
    '                    ELSE w[s].local\n'
    'RescopeBase1(s) == IF RescopeUnciteFirst\n'
    '                   THEN w[s].baseline\n'
    '                   ELSE [p \\in Paths |-> IF p \\in RescopeDrop(s) THEN Nil ELSE w[s].baseline[p]]\n'
    'RescopeAdd(s) == {p \\in w[s].sTgt : RescopeBase1(s)[p] = Nil /\\ doc[p] # Nil}\n'
    'RescopeKept(s) == IF WidenKeepsLocal\n'
    '                  THEN {p \\in RescopeAdd(s) : RescopeLocal1(s)[p] # Nil /\\ Content(RescopeLocal1(s)[p]) # Content(doc[p])}\n'
    '                  ELSE {}\n'
    'RescopeFetch0(s) == RescopeAdd(s) \\ RescopeKept(s)\n'
    '\\* What the widen fetches once the failed fetches are taken out.\n'
    'RescopeFetch(s, wfail) == RescopeFetch0(s) \\ wfail\n'
    '\\* Fetched where the tree has nothing (or, unguarded, over whatever it has);\n'
    '\\* adopted where its bytes ARE the document\'s.\n'
    + rec('RescopeSecondW', 's, wfail', {
        'sStage': '"none"',
        'local': '[p \\in Paths |->\n'
                 '                IF p \\in RescopeFetch(s, wfail) /\\ (RescopeLocal1(s)[p] = Nil \\/ ~WidenKeepsLocal)\n'
                 '                THEN doc[p] ELSE RescopeLocal1(s)[p]]',
        'baseline': '[p \\in Paths |-> IF p \\in RescopeFetch(s, wfail) THEN doc[p] ELSE RescopeBase1(s)[p]]',
        'integrated': 'w[s].integrated \\cup {Gen(doc[p]) : p \\in RescopeFetch(s, wfail)}',
        'scope': 'w[s].sTgt', 'synced': 'seq', 'derived': '0', 'skipped': '{}',
        'sTgt': '{}', 'sDrop': '{}', 'sKeep': '{}', 'sHeld': NILMAP,
        'unlinked': '(w[s].unlinked \\cup {p \\in Paths : RescopeUnlink(s, p) /\\ RescopeUnciteFirst})\n'
                    '                \\ RescopeFetch(s, wfail)'}) +
    'RescopeSecond(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage = "mid"\n'
    '  /\\ \\E wfail \\in SUBSET {p \\in RescopeFetch0(s) : RescopeLocal1(s)[p] = Nil \\/ ~WidenKeepsLocal} :\n'
    '        /\\ fails + Cardinality(wfail) <= MaxFetchFails\n'
    '        /\\ fails\' = fails + Cardinality(wfail)\n'
    '        /\\ w\' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers,\n'
    '                 restarts, syncs, regressed, rescopes, upped, copies, orig>>\n'
    '\n' + SEP)

rep_span('RPullRead(s) ==\n', '\\* The whole-tree sync, against the document as it is NOW (it reads both\n',
    rec('RPullReadW', 's', {'pc': '"pulling"', 'rnow': 'seq'}) +
    'RPullRead(s) ==\n'
    '  /\\ On(s) /\\ s \\in Readers /\\ w[s].pc = "idle" /\\ w[s].sStage \\in {"none", "saved"}\n'
    '  /\\ ~ReaderSkips(s)\n'
    '  /\\ w\' = [w EXCEPT ![s] = RPullReadW(s)]\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>\n'
    '\n')

rep_span('RPullSync(s) ==\n', '(* The boundary verbs\' answer (row 15; `sentinel.rs`).',
    rec('RPullSyncW', 's, fail', dict(SYNC_FIELDS, pc='"idle"', memo='w[s].rnow')) +
    'RPullSync(s) ==\n'
    '  /\\ On(s) /\\ s \\in Readers /\\ w[s].pc = "pulling"\n'
    '  /\\ \\E fail \\in SUBSET SyncAll(s) :\n'
    '       /\\ fails + Cardinality(fail) <= MaxFetchFails\n'
    '       /\\ w\' = [w EXCEPT ![s] = RPullSyncW(s, fail)]\n'
    '       /\\ fails\' = fails + Cardinality(fail)\n'
    '       /\\ regressed\' = (regressed \\/ \\E p \\in SyncOwed(s, fail) : Back(s, p))\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, restarts, syncs, rescopes, upped, copies, orig>>\n'
    '\n' + SEP)

# --- v4: the boundary verbs' answer and deposal (rows 15-17), as records. ---
rep_span("(* The boundary verbs' answer (row 15; `sentinel.rs`).", '(* The retire age (M1) and a reader of the document.',
    "(* The boundary verbs' answer (row 15; `sentinel.rs`).                     *)\n"
    '\n'
    "\\* A narrow's leftover (LeanP1.tla, verbatim; FINDINGS Table 3, 2026-10-16).\n"
    'RescopeLeftover(s, p) ==\n'
    '  /\\ w[s].sStage = "mid" /\\ RescopeUnciteFirst\n'
    '  /\\ p \\in w[s].sDrop \\ w[s].sKeep /\\ w[s].local[p] # Nil\n'
    '  /\\ \\/ ~UnlinkChecksBytes\n'
    '     \\/ w[s].sHeld[p] # Nil /\\ Content(w[s].local[p]) = Content(w[s].sHeld[p])\n'
    "\\* A request's parts (LeanP1.tla: `Touch`'s LET).\n"
    'TouchN(s) == w[s].nt + 1\n'
    'TouchFresh(s) == w[s].pend.id = 0\n'
    'TouchNs(s) == (IF TouchFresh(s) THEN {} ELSE w[s].pend.nonces) \\cup {TouchN(s)}\n'
    'TouchKept(s) == IF Cardinality(TouchNs(s)) > MaxNonces THEN TouchNs(s) \\ {Oldest(TouchNs(s))} ELSE TouchNs(s)\n'
    'TouchDirty(s) == {p \\in Paths : /\\ w[s].local[p] # w[s].baseline[p] /\\ Content(w[s].local[p]) # Content(doc[p])\n'
    '                                 /\\ ~RescopeLeftover(s, p)}\n'
    + rec('TouchW', 's', {
        'nt': 'TouchN(s)',
        'pend': '[id |-> IF TouchFresh(s) THEN TouchN(s) ELSE w[s].pend.id, fold |-> TouchN(s), nonces |-> TouchKept(s)]',
        'cN': 'w[s].cN \\cup {TouchN(s)}', 'capped': 'w[s].capped \\cup (TouchNs(s) \\ TouchKept(s))',
        'dset': '(IF TouchFresh(s) THEN {} ELSE w[s].dset) \\cup TouchDirty(s)',
        'decl': '[p \\in Paths |-> IF p \\in TouchDirty(s) THEN w[s].local[p]\n'
                '                            ELSE IF TouchFresh(s) THEN Nil ELSE w[s].decl[p]]',
        'dMint': '[p \\in Paths |-> IF p \\in TouchDirty(s) THEN nextGen\n'
                 '                             ELSE IF TouchFresh(s) THEN 0 ELSE w[s].dMint[p]]',
        'moved': '(IF TouchFresh(s) THEN {} ELSE w[s].moved) \\ TouchDirty(s)'}) +
    'Touch(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "idle" /\\ w[s].nt < MaxTouches\n'
    "  /\\ w' = [w EXCEPT ![s] = TouchW(s)]\n"
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>\n'
    '\n'
    "\\* The honour's parts (LeanP1.tla: `Honor`'s LET).\n"
    'HonorCarried(s) == IF w[s].car.id = w[s].pend.id THEN w[s].car.dropped \\cup w[s].car.uncited ELSE {}\n'
    'HonorDr(s) == w[s].bound.wd \\cup HonorCarried(s)\n'
    + rec('HonorW', 's', {
        'ack': '[st |-> IF HonorDr(s) = {} THEN "ok" ELSE "partial",\n'
               '            nonces |-> w[s].pend.nonces, seq |-> w[s].bound.seq,\n'
               '            src |-> w[s].bound.clk, dropped |-> HonorDr(s), inst |-> w[s].bound.inst]',
        'aN': 'w[s].aN \\cup w[s].pend.nonces'}) +
    'Honor(s) ==\n'
    '  /\\ On(s) /\\ s \\notin Readers /\\ w[s].pc = "idle"\n'
    '  /\\ w[s].pend.id # 0 /\\ ~Covered(s)\n'
    '  /\\ w[s].bound.id = w[s].pend.id /\\ w[s].bound.fold = w[s].pend.fold\n'
    "  /\\ w' = [w EXCEPT ![s] = HonorW(s)]\n"
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>\n'
    '\n'
    + rec('HonorReaderW', 's', {
        'ack': '[st |-> "refused-read-only", nonces |-> w[s].pend.nonces, seq |-> 0,\n'
               '            src |-> "none", dropped |-> {}, inst |-> FALSE]',
        'aN': 'w[s].aN \\cup w[s].pend.nonces'}) +
    'HonorReader(s) ==\n'
    '  /\\ On(s) /\\ s \\in Readers /\\ w[s].pc = "idle"\n'
    '  /\\ w[s].pend.id # 0 /\\ ~Covered(s)\n'
    "  /\\ w' = [w EXCEPT ![s] = HonorReaderW(s)]\n"
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>\n'
    '\n'
    + rec('RetireByAckW', 's', {'pend': 'NoPend'}) +
    'RetireByAck(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "idle" /\\ w[s].pend.id # 0 /\\ Covered(s)\n'
    "  /\\ w' = [w EXCEPT ![s] = RetireByAckW(s)]\n"
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>\n'
    '\n' + SEP +
    "(* Deposal (row 17; `lease.rs::claim_step`, `barrier.rs`).                 *)\n"
    '\n'
    "\\* The taker's tree, and the deposed holder's.\n"
    + rec('DeposeW', 's', {'pc': '"claimed"', 'nd': 'w[s].nd + 1'})
    + rec('DeposedW', 's', {'deposed': 'TRUE'}) +
    'Depose(t) ==\n'
    '  /\\ On(t) /\\ w[t].pc = "scanned" /\\ w[t].nd < MaxDeposals\n'
    '  /\\ w[t].uploads \\subseteq w[t].upDone /\\ w[t].uploads \\cup w[t].deletes # {}\n'
    '  /\\ holder \\in Writers /\\ holder # t\n'
    "  /\\ holder' = t\n"
    "  /\\ seq' = seq + 1\n"
    "  /\\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]\n"
    '  /\\ UNCHANGED <<live, minted, doc, tomb, base, acked, conflicts, gw, mv, udel,\n'
    '                 nextGen, ui, reqs, barriers, aux>>\n'
    '\n'
    "\\* The straggler learns it: before its CAS the barrier is abandoned; after\n"
    "\\* it, the deletes stop.\n"
    + rec('FenceBeforeW', 's', dict(BARRIER_RESET,
        pc='"idle"', deposed='FALSE', uploads='{}', deletes='{}', snap=NILMAP,
        upDone='{}', gone='{}', verified='FALSE', inst=NILMAP, retire='{}',
        collected='FALSE', adv='FALSE'))
    + rec('FenceAfterW', 's', {'retire': '{}', 'collected': 'TRUE'}) +
    'Fence(s) ==\n'
    '  /\\ On(s) /\\ w[s].deposed\n'
    "  /\\ \\/ w[s].pc = \"claimed\" /\\ w' = [w EXCEPT ![s] = FenceBeforeW(s)]\n"
    "     \\/ w[s].pc = \"cased\" /\\ ~w[s].collected /\\ w' = [w EXCEPT ![s] = FenceAfterW(s)]\n"
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>>\n'
    '\n' + SEP)

rep('=============================================================================\n', '==============================================================================\n')
assert '![s].' not in out, 'a record EXCEPT on the tree survived'   # (GRenameFinish keeps two function-EXCEPT @s)
if RECORD:
    with open(os.path.join(HERE, 'span-md5.txt'), 'w') as fh:
        fh.write('# md5 of each LeanP1.tla span make-anc.py rewrites; re-pin (RECORD_SPANS=1)\n'
                 '# only after reviewing the rewrite against the changed span.\n')
        for k in sorted(SPANS): fh.write(f'{SPANS[k]} {k}\n')
open('LeanP1Anc.tla', 'w').write(out)
print('LeanP1Anc.tla:', len(out.splitlines()), 'lines')
