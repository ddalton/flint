### c16: flint-cb's two rev 7 asks; FlintCatchupT rev 15 (8ac3cf00) + FlintCatchupTTieNA (780c1009); box, tla-mc 0.2.1 compiled, 3 workers, -fpmem/-queue-mem 2048 MB; C16DONE 2026-10-30 21:49:23Z; every arm finished inside its cap (no bounded rows). Bound D for all: three replicas, two failures, one write, two epochs, one retained.

**Ask 1: does rev 6's fence-flip mark close the defer seam without option (b)?** Zombie consumer, capture, gated marks, F36c's last-writer defer ON in every arm, no (b), the mark-race fix (CutStampsAll FALSE). DF = with the fence mark, DN = without it (the control).

| arm | policy | fence mark | invariants | verdict | reading |
|---|---|---|---|---|---|
| DFTrustANL | trust | on | TypeOK, AdmitNoLoss | HOLD, full space 226,393,635 / d48 | |
| DNTrustANL | trust | off (control) | TypeOK, AdmitNoLoss | VIOL AdmitNoLoss d17 (1,848,937) | control KILLED: under trust the fence mark is what holds AdmitNoLoss |
| DFTieANL | tie | on | TypeOK, AdmitNoLoss | HOLD, full space 46,886,451 / d50 | |
| DNTieANL | tie | off (control) | TypeOK, AdmitNoLoss | HOLD, full space 28,953,576 / d45 | control NOT killed: under the tie the fence mark gets no credit for AdmitNoLoss (the tie alone holds it at bound D) |
| DFTrustNNL | trust | on | NoNewLoss | VIOL NoNewLoss d8 (8,462) | |
| DNTrustNNL | trust | off | NoNewLoss | VIOL NoNewLoss d9 (19,656) | |
| DFTieNNL | tie | on | NoNewLoss | VIOL NoNewLoss d8 (7,828) | |
| DNTieNNL | tie | off | NoNewLoss | VIOL NoNewLoss d9 (18,991) | |

Answer: **no.** Without (b), NoNewLoss fails with and without the fence mark, under trust and the tie, within 8-9 steps. The fence mark only carries the weaker AdmitNoLoss, and only under trust.

**Ask 2: does the tie's attached conjunct (per-epoch membership's cut-time read) earn its place?** TieNA = the tie admitting on stale-since-copy alone.

Pre-check, without (b): the cfgs of c15's full-tie HOLD arms, unchanged (so CutStampsAll as c15 ran them), on TieNA. Per those cfgs, a violation here would make the under-(b) arms a live test.

| arm | fault | invariants | verdict | c15, full tie, same cfg |
|---|---|---|---|---|
| NAZombieTieGuardANLAsm | zombie | TypeOK, AdmitNoLoss | HOLD, full space 17,934,639 / d43 | HOLD 18,538,320 / d44 |
| NALostTieANL | lost mark | TypeOK, AdmitNoLoss | HOLD, full space 7,256,009 / d40 | HOLD 7,363,457 / d40 |
| NACrashTieANL11 | crash | TypeOK, AdmitNoLoss | HOLD, full space 16,183,287 / d41 | HOLD 16,480,908 / d41 |

Under (b): AckAfterMark (record-before-ack), WriteQuorum 1, AssemblyQuorum 1, defer dropped, the mark-race fix. NA* on TieNA, TF* = the full tie on FlintCatchupT.

| fault | NA (no conjunct) | TF (full tie) | invariants |
|---|---|---|---|
| lost mark | NAL HOLD, full space 6,918,623 / d38 | TFL HOLD, full space 6,975,995 / d38 | NoNewLoss, MembersHold, RaidHolds |
| zombie (gated marks, no defer) | NAZ HOLD, full space 13,264,413 / d43 | TFZ HOLD, full space 13,662,813 / d42 | NoNewLoss, MembersHold, RaidHolds |
| crash | NAC HOLD, full space 15,109,620 / d39 | TFC HOLD, full space 15,348,672 / d39 | NoNewLoss, MembersHold, RaidHolds |

Answer: **no arm kills the conjunct**, with or without (b), so these rows cannot credit it. They show only that dropping it breaks nothing at bound D. Do not cite the TF rows as evidence that the conjunct is load-bearing.
