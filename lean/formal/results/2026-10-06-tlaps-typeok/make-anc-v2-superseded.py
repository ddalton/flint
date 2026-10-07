#!/usr/bin/env python3
"""Generate lean/formal/LeanP1Anc.tla from LeanP1.tla (run from lean/formal).

Version 2 (2026-10-06; v1 is results/2026-10-05-tlaps-m0-tie/make-anc.py).
The copy is LeanP1.tla with its two RECURSIVE operators restated over a ghost
`anc` (tlapm 1.6.0-pre refuses any module containing RECURSIVE), two ASSUMEs
TLC gets from model values, and -- new in v2 -- the LET-bound names of
`Install` and `RescopeSecond` lifted to module-level operators: tlapm expands
every LET in an obligation's CONTEXT by substitution, used or not, and those
two chains cost every obligation of the proof 9 s and 2.2 GB (this
directory's NOTES.txt).  Nothing else.  Every edit asserts that its anchor
text occurs exactly once, so a changed LeanP1.tla fails loudly here instead
of producing a silently different copy.  The tie to the original is the
exact distinct count on the gate's worlds (RESULTS.txt here and in the v1
directory).
"""
src = open('LeanP1.tla').read()
out = src
def rep(old, new):
    global out
    assert out.count(old) == 1, (old[:70], out.count(old))
    out = out.replace(old, new)

rep('------------------------------- MODULE LeanP1 ---------------------------------',
    '----------------------------- MODULE LeanP1Anc -------------------------------\n'
    '(* LeanP1.tla for TLAPS: the SAME protocol, with its two RECURSIVE operators\n'
    '   restated over a ghost.  tlapm (1.6.0-pre, bfa9468) dies in module\n'
    '   elaboration on the mere presence of a RECURSIVE definition\n'
    '   (results/2026-10-05-tlaps-m0/), so the proof module cannot EXTEND\n'
    '   LeanP1.tla.  This copy is GENERATED from it by\n'
    '   results/2026-10-06-tlaps-typeok/make-anc.py and differs ONLY in: the\n'
    '   module name; two ASSUMEs TLC gets free from model values; the LET-bound\n'
    '   names of `Install` and `RescopeSecond` lifted to module-level operators\n'
    '   (`Install*`, `Rescope*`) and the tree `Finish` and `RescopeSecond` write\n'
    '   made operators too (`FinishW`, `RescopeSecondW`, old fields explicit\n'
    '   instead of `@`) -- tlapm expands every LET in an obligation\'s context by\n'
    '   substitution and pays for each `@` of an EXCEPT there, used or not, and\n'
    '   those four cost every obligation of the proof 9 s and 2.2 GB\n'
    '   (results/2026-10-06-tlaps-typeok/NOTES.txt); the variable\n'
    '   `anc` -- for each minted handle, every handle its base chain reaches --\n'
    '   written by the step (`AncUpdate`, like `TookUpdate`) from `base`, which\n'
    '   is write-once per handle; and `Derives` / `Supersedes` restated as the\n'
    '   one-level search over `anc` that their recursion computed.  `anc` is a\n'
    '   function of `base`, so it adds no distinct states: the tie to LeanP1.tla\n'
    '   is the EXACT distinct count on the gate\'s worlds (Holds1p3b =\n'
    '   461,094,969), and MCLeanP1AncCheck.tla checks the restated operators\n'
    '   against the recursive originals state by state.  LeanP1.tla stays the\n'
    '   module the gate checks; this one is what LeanP1Proof.tla EXTENDS.      *)\n'
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

# --- v2: Install's LET lifted to operators. ---
rep('Install(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "claimed" /\\ holder = s /\\ w[s].verified\n'
    '  /\\ LET W == w[s]\n'
    '         mine == W.uploads \\cap W.upDone\n'
    '         contested == IF CommitSurfacesForeign\n'
    '                      THEN {p \\in mine \\ W.gone : Foreign(s, p) /\\ doc[p] # Nil}\n'
    '                      ELSE {}\n'
    '         delOverridden == IF CommitRecordsDeleteOverride\n'
    '                          THEN {p \\in mine \\ W.gone : Foreign(s, p) /\\ doc[p] = Nil /\\ W.baseline[p] # Nil}\n'
    '                          ELSE {}\n'
    '         delOver == IF DeleteWinsPreserved\n'
    '                    THEN {p \\in W.deletes : Foreign(s, p) /\\ doc[p] # Nil}\n'
    '                    ELSE {}\n'
    '         inst == [p \\in Paths |->\n'
    '                    IF p \\in W.gone THEN doc[p]\n'
    '                    ELSE IF p \\in mine THEN W.snap[p]\n'
    '                    ELSE IF p \\in W.deletes /\\ DeleteWinsPreserved THEN Nil\n'
    '                    ELSE IF Foreign(s, p) THEN doc[p]\n'
    '                    ELSE IF p \\in W.deletes THEN Nil\n'
    '                    ELSE doc[p]]\n'
    '         nothing == inst = doc\n'
    '         retired == {doc[p] : p \\in {q \\in Paths : doc[q] # Nil /\\ inst[q] # doc[q]}}\n'
    '     IN\n',
    '\\* The merge\'s parts, as operators (LeanP1.tla: `Install`\'s LET; tlapm\n'
    '\\* expands every LET in an obligation\'s context by substitution).\n'
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
    'Install(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "claimed" /\\ holder = s /\\ w[s].verified\n'
    '  /\\ LET W == w[s]\n'
    '         inst == InstallInst(s)\n'
    '         nothing == inst = doc\n'
    '         retired == InstallRetired(s)\n'
    '     IN\n')
rep('                                 \\cup {<<p, doc[p]>> : p \\in contested}\n',
    '                                 \\cup {<<p, doc[p]>> : p \\in InstallContested(s)}\n')
rep('                                 \\cup {<<p, DeletedAt(s, p)>> : p \\in delOverridden}\n',
    '                                 \\cup {<<p, DeletedAt(s, p)>> : p \\in InstallDelOverridden(s)}\n')
rep('                                 \\cup {<<p, doc[p]>> : p \\in delOver}\n',
    '                                 \\cup {<<p, doc[p]>> : p \\in InstallDelOver(s)}\n')
# --- v2: a definition replaced whole, located by its first and last line. ---
def rep_def(first, last, new):
    global out
    assert out.count(first) == 1 and out.count(last) == 1, (first, out.count(first), out.count(last))
    a = out.index(first); b = out.index(last) + len(last)
    assert a < b
    out = out[:a] + new + out[b:]
# --- v2: RescopeSecond: the LET lifted to operators, the tree it writes an operator. ---
rep_def('RescopeSecond(s) ==\n',
        '                 restarts, syncs, regressed, rescopes, upped, copies, orig>>\n',
    '\\* The widen\'s parts, as operators (LeanP1.tla: `RescopeSecond`\'s LET).\n'
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
    '\\* The tree after the widen, as an operator (LeanP1.tla: the EXCEPT with `@`).\n'
    '\\* Fetched where the tree has nothing (or, unguarded, over whatever it has);\n'
    '\\* adopted where its bytes ARE the document\'s.\n'
    'RescopeSecondW(s, wfail) ==\n'
    '  [w[s] EXCEPT !.sStage = "none",\n'
    '     !.local = [p \\in Paths |->\n'
    '                  IF p \\in RescopeFetch(s, wfail) /\\ (RescopeLocal1(s)[p] = Nil \\/ ~WidenKeepsLocal)\n'
    '                  THEN doc[p] ELSE RescopeLocal1(s)[p]],\n'
    '     !.baseline = [p \\in Paths |-> IF p \\in RescopeFetch(s, wfail) THEN doc[p] ELSE RescopeBase1(s)[p]],\n'
    '     !.integrated = w[s].integrated \\cup {Gen(doc[p]) : p \\in RescopeFetch(s, wfail)},\n'
    '     !.scope = w[s].sTgt,\n'
    '     !.synced = seq, !.derived = 0, !.skipped = {},\n'
    '     !.sTgt = {}, !.sDrop = {}, !.sKeep = {},\n'
    '     !.sHeld = [p \\in Paths |-> Nil],\n'
    '     !.unlinked = (w[s].unlinked \\cup {p \\in Paths : RescopeUnlink(s, p) /\\ RescopeUnciteFirst})\n'
    '                  \\ RescopeFetch(s, wfail)]\n'
    'RescopeSecond(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "idle" /\\ w[s].sStage = "mid"\n'
    '  /\\ \\E wfail \\in SUBSET {p \\in RescopeFetch0(s) : RescopeLocal1(s)[p] = Nil \\/ ~WidenKeepsLocal} :\n'
    '        /\\ fails + Cardinality(wfail) <= MaxFetchFails\n'
    '        /\\ fails\' = fails + Cardinality(wfail)\n'
    '        /\\ w\' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]\n'
    '  /\\ UNCHANGED <<bucket, nextGen, ui, reqs, barriers,\n'
    '                 restarts, syncs, regressed, rescopes, upped, copies, orig>>\n')
# --- v2: Finish: the tree it writes an operator. ---
rep_def('Finish(s) ==\n', '  /\\ holder\' = "none"\n',
    '\\* The tree after its barrier, as an operator (LeanP1.tla: `Finish`\'s EXCEPT\n'
    '\\* with `@`; tlapm\'s cost for an EXCEPT in its context grows with each `@`).\n'
    'FinishW(s) ==\n'
    '  [w[s] EXCEPT !.pc = "idle",\n'
    '     !.baseline = [p \\in Paths |->\n'
    '                     IF p \\in w[s].uploads \\cap w[s].upDone THEN w[s].snap[p]\n'
    '                     ELSE IF p \\in w[s].deletes /\\ w[s].inst[p] = Nil THEN Nil\n'
    '                     ELSE w[s].baseline[p]],\n'
    '     !.synced = IF w[s].inst = doc THEN seq ELSE w[s].synced,\n'
    '     !.derived = IF w[s].adv /\\ w[s].inst = doc THEN seq ELSE w[s].derived,\n'
    '     !.skipped = IF w[s].adv /\\ w[s].inst = doc\n'
    '                   THEN w[s].skipped \\ ((w[s].uploads \\cap w[s].upDone) \\cup {p \\in w[s].deletes : w[s].inst[p] = Nil})\n'
    '                   ELSE w[s].skipped,\n'
    '     !.adv = FALSE,\n'
    '     !.uploads = {}, !.deletes = {}, !.snap = [p \\in Paths |-> Nil],\n'
    '     !.upDone = {}, !.gone = {}, !.verified = FALSE,\n'
    '     !.collected = FALSE]\n'
    'Finish(s) ==\n'
    '  /\\ On(s) /\\ w[s].pc = "cased" /\\ w[s].collected\n'
    '  /\\ w\' = [w EXCEPT ![s] = FinishW(s)]\n'
    '  /\\ holder\' = "none"\n')
rep('=============================================================================\n', '==============================================================================\n')
open('LeanP1Anc.tla', 'w').write(out)
print('LeanP1Anc.tla:', len(out.splitlines()), 'lines')
