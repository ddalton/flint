-------------------------- MODULE FlintCatchupSSymV --------------------------
(***************************************************************************)
(* FlintCatchupSSym with a VIEW that leaves out the three write-only       *)
(* ghosts: vouched, tornMarked and tornServed.  Each is only ever set      *)
(* (x' = x \/ ...) or left UNCHANGED; no action, guard or invariant of     *)
(* these arms reads them (the reachability probes do, and they are not     *)
(* run under this module).  Two states that differ only in them have the   *)
(* same successors up to those three, so checking the view is exact for   *)
(* every invariant here.  Kept apart from FlintCatchupSSym so the runs     *)
(* queued on that module keep their input.                                 *)
(***************************************************************************)
EXTENDS FlintCatchupSSym

ViewVars == <<ver, head, hAl, hPar, sPres, sCon, sAl, sPar, anc, recorded, nextSeq,
              cutK, cutTgt, cutDone, cutGone, members, attached, path, dState, mark,
              reverted, sb, pin, riskRec, pend, acked, fails, badAdmit, badLoss, ackedEver, used,
              tainted, floor, mustRev, restamp, cfgAt, conf, tr, htr, zx, cutAck,
              stamp, nseq, aseq, agen, asize, rgenA, rmemA, hold, due, sole, zseq, zgen, zsize, zm0,
              zrgen, zrmem, zst, badAck, zAcked, zfreed, zLeg, zjv, zexcl, postMove, legalJ, riskTail,
              tornBare, zpost>>
==============================================================================
