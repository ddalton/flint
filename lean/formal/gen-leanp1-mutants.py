#!/usr/bin/env python3
"""The LeanP1 gate's MODEL mutations (2026-10-14): copies of LeanP1.tla with
one rule broken that no rule constant switches off.  Written to
OUTDIR/<name>/LeanP1.tla, never over the module; the gate runs a world's
module in that directory (WORLDS-LeanP1.tsv, fourth column).  Each edit
asserts its anchor occurs exactly once, so a changed LeanP1.tla fails here
loudly rather than producing a mutant that mutates nothing.

  MN2       the gateway's delete is not acknowledged in `udel` (the tombstone
            stays): NR5 must break (results/2026-10-07-tlaps-m5 E14, E35).
  MA1       the gateway's delete records nothing, no tombstone and no `udel`:
            AN5 must break (E14, E41).
  EditBase  Edit's ghost base is the baseline, as before 2026-10-13: the
            scripted traces E27, E31, E33 must break NR5 (the fix they pin).

  Rows 15-17 (2026-10-14; docs/plans/lean-row15-boundary-verbs-model-plan-2026-10-14.md):
  AckIgnoresWithheld   inbox-1: a withheld upload is not named, by the honour
                       or the carrier: AK1 must break.
  AckReadsOwnReport    before H10 (L-110): the ack reads only its own
                       barrier, never the carrier: AK1.
  AckIgnoresUncited    H10: a peer's delete after the carrying CAS is not
                       named: AK1.
  CarrierPerFold       L-133: every fold mints a fresh record id, so a second
                       touch orphans the first one's carrier: AK1.
  CarrierKeepsDelete   L-136: a later carry's upload of a path an earlier one
                       deleted stays a delete, so a peer's delete of it is
                       not named: AK1.
  CarrierCitesAny      L-137: a carried upload is named only when the document
                       stops citing its path, not when it cites other bytes there
                       (a UI delete, then a rename onto the path): AK1.
  TouchDeclaresLeftover  (Table 3, 2026-10-16) a touch declares a narrow's
                       leftover (a mid-rescope uncited file the second half
                       unlinks), after a UI delete there: AK1.
  TouchDeclaresRestartLeftover  (Table 3, 2026-10-16) the same after a restart
                       mid-narrow (the stage back at "saved"): AK1.
  TouchDeclaresRenamed (Table 3, 2026-10-16) a touch after a restart between
                       a CAS and step 7 declares a published file the UI
                       renamed away; renamed back, converged, then deleted
                       by the UI, the honour says ok: AK1.
  FoldClobbers         settle-before-consume: a touch replaces the pending
                       record instead of folding into it: AK3.
  StampIndependent     the ack's clock chosen apart from the bucket's stamp: AK4.
  Rematerialise        a restart materialises the baseline where the tree has
                       nothing (a re-checkout over a live tree): NR1.
  ConsumeTakesDeleted  a consume takes the document's version over a path the
                       agent deleted: NR1.
  FinishReleasesForeign  a straggler's release frees its successor's lease:
                       Inv_OneHolder.
  RequestAfterCarry    L-135: the gateway's request folds in after the
                       barrier decided what it carries (between Consume and
                       Scan): AK1.

Run from lean/formal:  python3 gen-leanp1-mutants.py OUTDIR [NAME...]"""
import os, sys
out = sys.argv[1]
want = sys.argv[2:]
src = open('LeanP1.tla').read()
GD_T = "  /\\ tomb' = [tomb EXCEPT ![p] = doc[p]]\n  /\\ udel' = udel \\cup {<<p, doc[p]>>}\n"
EDIT = ("     /\\ base' = [base EXCEPT ![h] = IF w[s].local[p] # Nil THEN w[s].local[p]\n"
        "                                    ELSE IF p \\in w[s].uploads THEN w[s].snap[p]\n"
        "                                    ELSE w[s].baseline[p]]\n")
M = {
  'MN2': [(GD_T, "  /\\ tomb' = [tomb EXCEPT ![p] = doc[p]]\n  /\\ UNCHANGED udel\n")],
  'MA1': [(GD_T, "  /\\ UNCHANGED <<tomb, udel>>\n")],
  'EditBase': [(EDIT, "     /\\ base' = [base EXCEPT ![h] = w[s].baseline[p]]\n")],
  'AckIgnoresWithheld': [
    ("         dr == W.bound.wd \\cup carried\n", "         dr == carried\n"),
    ("!.dropped = @ \\cup W.gone \\cup {p \\in W.deletes : inst[p] # Nil},",
     "!.dropped = @ \\cup {p \\in W.deletes : inst[p] # Nil},")],
  'AckReadsOwnReport': [
    ("carried == IF W.car.id = W.pend.id THEN W.car.dropped \\cup W.car.uncited ELSE {}",
     "carried == {}")],
  'AckIgnoresUncited': [
    ("carried == IF W.car.id = W.pend.id THEN W.car.dropped \\cup W.car.uncited ELSE {}",
     "carried == IF W.car.id = W.pend.id THEN W.car.dropped ELSE {}")],
  'CarrierPerFold': [
    ("![s].pend = [id |-> IF fresh THEN n ELSE W.pend.id, fold |-> n, nonces |-> kept],",
     "![s].pend = [id |-> n, fold |-> n, nonces |-> kept],")],
  'CarrierKeepsDelete': [
    ("!.deletes = (@ \\ W.uploads) \\cup W.deletes,", "!.deletes = @ \\cup W.deletes,")],
  'CarrierCitesAny': [
    ("                                  /\\ \\/ d[p] = Nil\n                                     \\/ c.pub[p] # Nil /\\ Content(d[p]) # Content(c.pub[p])}",
     "                                  /\\ d[p] = Nil}")],
  'TouchDeclaresLeftover': [
    ("Content(W.local[p])\n                                 /\\ ~RescopeLeftover(s, p)}",
     "Content(W.local[p])}")],
  'TouchDeclaresRestartLeftover': [
    ('     \\/ w[s].sStage = "saved" /\\ w[s].baseline[p] = Nil /\\ w[s].sHeld[p] # Nil\n', '')],
  'TouchDeclaresRenamed': [
    ("\n                                 /\\ W.local[p] # Nil => \\A q \\in Paths : Content(doc[q]) # Content(W.local[p])", '')],
  'FoldClobbers': [
    ("         ns == (IF fresh THEN {} ELSE W.pend.nonces) \\cup {n}\n", "         ns == {n}\n")],
  'StampIndependent': [
    ("  /\\ \\E src \\in {w[s].bound.clk} :\n", "  /\\ \\E src \\in Clocks :\n")],
  'Rematerialise': [
    ("                    ![s].sKeep = {},\n                    ![s].cId = 0,",
     "                    ![s].sKeep = {},\n                    ![s].local = [p \\in Paths |-> IF @[p] = Nil THEN w[s].baseline[p] ELSE @[p]],\n                    ![s].cId = 0,")],
  'ConsumeTakesDeleted': [
    ("Owed(s, p) == /\\ doc[p] # w[s].baseline[p] /\\ w[s].local[p] = w[s].baseline[p]\n",
     "Owed(s, p) == /\\ doc[p] # w[s].baseline[p] /\\ (w[s].local[p] = w[s].baseline[p] \\/ w[s].local[p] = Nil)\n")],
  'RequestAfterCarry': [
    ("  /\\ On(s) /\\ w[s].pc = \"idle\" /\\ w[s].nt < MaxTouches\n",
     "  /\\ On(s) /\\ w[s].pc \\in {\"idle\", \"consumed\"} /\\ w[s].nt < MaxTouches\n")],
  'FinishReleasesForeign': [
    ("  \\* successor holds (`lease::release`).\n  /\\ holder' = IF holder = s THEN \"none\" ELSE holder\n",
     "  \\* successor holds (`lease::release`).\n  /\\ holder' = \"none\"\n")],
}
for name, edits in M.items():
    if want and name not in want:
        continue
    s = src
    for old, new in edits:
        assert s.count(old) == 1, (name, old[:60], s.count(old))
        s = s.replace(old, new)
    d = os.path.join(out, name)
    os.makedirs(d, exist_ok=True)
    open(os.path.join(d, 'LeanP1.tla'), 'w').write(s)
    print(f'{name}: {d}/LeanP1.tla')
