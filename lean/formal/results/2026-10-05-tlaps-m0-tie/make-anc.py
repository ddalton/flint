#!/usr/bin/env python3
"""Generate lean/formal/LeanP1Anc.tla from LeanP1.tla (run from lean/formal).

The copy is LeanP1.tla with its two RECURSIVE operators restated over a ghost
`anc` (tlapm 1.6.0-pre refuses any module containing RECURSIVE), two ASSUMEs
TLC gets from model values, and nothing else.  Every edit asserts that its
anchor text occurs exactly once, so a changed LeanP1.tla fails loudly here
instead of producing a silently different copy.  The tie to the original is
the exact distinct count on the gate's worlds (see RESULTS in this directory).
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
    '   results/2026-10-05-tlaps-m0-tie/make-anc.py and differs ONLY in: the\n'
    '   module name; two ASSUMEs TLC gets free from model values; the variable\n'
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
rep('=============================================================================\n', '==============================================================================\n')
open('LeanP1Anc.tla', 'w').write(out)
print('LeanP1Anc.tla:', len(out.splitlines()), 'lines')
