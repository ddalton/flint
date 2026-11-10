------------------------- MODULE FlintCatchupSRSymV -------------------------
(***************************************************************************)
(* FlintCatchupSRSym with FlintCatchupSSymV's VIEW (35a7ec38): every       *)
(* variable but the three write-only ghosts vouched, tornMarked and        *)
(* tornServed.  Rev 18 reads none of the three either: they are only set   *)
(* (x' = x \/ ...) or left UNCHANGED, as in rev 17, and no resize action,  *)
(* guard or invariant names them.  Rev 18's own ghosts (shortAdm,          *)
(* unfinished, wiped) stay in the view: the resize invariants read them.   *)
(***************************************************************************)
EXTENDS FlintCatchupSRSym

ViewVars == <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
              cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
              reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, used,
              tainted, floor, mustRev, restamp, cfgAt, conf, tr, htr, zx, cutAck,
              stamp, nseq, aseq, agen, asize, rgenA, rmemA, hold, due, sole, zseq, zgen, zsize, zm0,
              zrgen, zrmem, zst, badAck, zAcked, zfreed, zLeg, zjv, zexcl, postMove, legalJ, riskTail,
              tornBare, zpost,
              lv, cells, prv, mv, fan, ctl, fsz, rsz, hw, shortAdm, unfinished, wiped>>
==============================================================================
