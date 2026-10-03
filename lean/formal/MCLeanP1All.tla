---------------------------- MODULE MCLeanP1All ----------------------------
(***************************************************************************)
(* LeanP1 with scope, rescope, a failed fetch and a reader on at once      *)
(* (2026-10-03).  Each was checked in its own world; the pair never        *)
(* checked together is a READER THAT RESCOPES: RescopeBegin does not       *)
(* exclude Readers, and RPullRead may run between a rescope's begin and    *)
(* its first half (sStage = "saved").  This module adds only the probes    *)
(* that show those routes are reached: LeanP1.tla is what the gate checks. *)
(***************************************************************************)
EXTENDS LeanP1

\* Fires when a reader finishes a rescope (the second half clears the intent).
ProbeReaderRescoped ==
  [][~\E s \in Readers : w[s].sStage = "mid" /\ w'[s].sStage = "none"]_vars

\* Fires when a reader's pull completes while its rescope is saved, not begun.
ProbeReaderPulledMidRescope ==
  [][~\E s \in Readers : w[s].sStage = "saved" /\ w[s].pc = "pulling" /\ w'[s].pc = "idle"]_vars
==============================================================================
