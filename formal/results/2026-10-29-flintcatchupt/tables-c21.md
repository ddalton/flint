
### c21: FlintCatchupS rev 7; FlintCatchupS.tla 6859c997, FlintCatchupSReach.tla 7d5d053b

| arm | replicas, fault | knobs on | decree form | invariants | verdict | trace class | method | flag |
|---|---|---|---|---|---|---|---|---|
| SReachHold | 3, lost mark | MajorityRead count, RelView | majority read | Reach_Hold | reachable d2 | probe: the state is reachable |  |  |
| SReachMajRel | 3, lost mark | MajorityRead count, RelView | majority read | Reach_MajRel | reachable d4 | probe: the state is reachable |  |  |
| SReachSole | 3, lost mark | MajorityRead count, RelView | majority read | Reach_Sole | reachable d5 | probe: the state is reachable |  |  |
| SReachZStamp | 3, zombie | MajorityRead count, RelView | majority read | Reach_ZStamp | reachable d2 | probe: the state is reachable |  |  |
| SReachExcluded | 3, lost mark | MajorityRead count, RelView | majority read | Reach_Excluded | reachable d4 | probe: the state is reachable |  |  |
| SReachSeq3 | 3, lost mark | MajorityRead count, RelView | majority read | Reach_Seq3 | reachable d4 | probe: the state is reachable |  |  |
| SReachZombieAck | 3, zombie | MajorityRead count, RelView | majority read | Reach_ZombieAck | reachable d7 | probe: the state is reachable |  |  |
| SReachServeOnZ | 3, zombie | MajorityRead count, RelView | majority read | Reach_ServeOnZ | reachable d7 | probe: the state is reachable |  |  |
| SZDecreeAll | 3, zombie | MajorityRead count, RelView | ungated (any stage attempt), last leg not spared | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d8 | stamp gap, control (step 4 as first written: the decree strands the last in_sync leg) |  |  |
| SZDecreeUngated | 3, zombie | MajorityRead count, RelView | ungated (any stage attempt) | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d13 | stamp gap, control (the decree ungated: the third gap) |  |  |
| SZAck | 3, zombie | MajorityRead count, RelView | majority read | ZombieNoAck | VIOL ZombieNoAck d7 | permitted: the zombie of a record-confirmed sole continuation acks (design) |  |  |
| SZFree | 3, zombie | MajorityRead count, RelView | majority read | FreedStale | VIOL FreedStale d2 | write-hole class as classed at rev 7; rev 10 splits it: pre-stamp junk = the hole, a torn leg = excluded |  |  |
| SZClean0 | 3, zombie | MajorityRead count, RelView | no decree | MembersClean | VIOL MembersClean d4 | write-hole class as classed at rev 7 (decree off); rev 10 splits it as SZFree |  |  |
| SZClean | 3, zombie | MajorityRead count, RelView | majority read | MembersClean | VIOL MembersClean d4 | write-hole class as classed at rev 7; rev 10 splits it as SZFree |  |  |
| SZSoleOnly | 3, zombie | MajorityRead count, RelView | majority read | SoleOnly | no violation to the cap (168,363,702 / d21): bounded |  |  |  |
| SLSoleOnly | 3, lost mark | MajorityRead count, RelView | majority read | SoleOnly | no violation to the cap (68,016,172 / d22): bounded |  |  |  |
| SLNoRV | 3, lost mark | MajorityRead count, RelView OFF | majority read | NoNewLoss, MembersHold, RaidHolds | VIOL MembersHold d13 | stamp gap, control (RelView off: an unconfirmed minority stamp hides the released membership) |  |  |
| SCNoRV | 3, crash | MajorityRead count, RelView OFF | majority read | NoNewLoss, MembersHold, RaidHolds | VIOL MembersHold d13 | stamp gap, control (RelView off) |  |  |
| SZNoRV | 3, zombie | MajorityRead count, RelView OFF | majority read | NoNewLoss, MembersHold, RaidHolds | VIOL MembersHold d13 | stamp gap, control (RelView off) |  |  |
| SL0 | 3, lost mark | NO STAMP, RelView | - | NoNewLoss, MembersHold, RaidHolds | VIOL MembersHold d8 | control: no stamp (FlintCatchupT, the d9-class residual) |  |  |
| SC0 | 3, crash | NO STAMP, RelView | - | NoNewLoss, MembersHold, RaidHolds | VIOL MembersHold d8 | control: no stamp (FlintCatchupT) |  |  |
| SZ0 | 3, zombie | NO STAMP, RelView | - | NoNewLoss, MembersHold, RaidHolds | VIOL MembersHold d8 | control: no stamp (FlintCatchupT) |  |  |
| SLF3NoMR | 3, lost mark, MaxFails 3 | MajorityRead off, RelView | any conclusive read (majority read off) | NoNewLoss, MembersHold, RaidHolds | no violation to the cap (58,354,189 / d21): bounded |  |  | WRONG CONFIGURATION: the model's last healthy member never fails and nothing fails after unstage, so the lost-mark shape cannot reach the gap; leave out |
| SCF3NoMR | 3, crash, MaxFails 3 | MajorityRead off, RelView | any conclusive read (majority read off) | NoNewLoss, MembersHold, RaidHolds | no violation to the cap (35,625,323 / d19): bounded |  |  | WRONG CONFIGURATION (as SLF3NoMR); leave out |
| SZF3NoMR | 3, zombie, MaxFails 3 | MajorityRead off, RelView | any conclusive read (majority read off) | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d12 | stamp gap, control (majority read off: the stage serves a leg whose stamp predates an unreached majority stamp; via the zombie, two failures) |  |  |
| SZ | 3, zombie | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d17 | hole class, pre-stamp junk (the zombie write that finds the fence, readmitted by a later catch-up); not a stamp gap |  |  |
| SL | 3, lost mark | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | no violation to the cap (106,466,304 / d23): bounded |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.1.2 |  |
| SC | 3, crash | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | no violation to the cap (100,322,249 / d22): bounded |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| SZMem | 3, zombie | MajorityRead member, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d17 | hole class, pre-stamp junk (as c21 SZ; the member read changes nothing here) | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| SLMem | 3, lost mark | MajorityRead member, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | no violation to the cap (106,261,050 / d23): bounded |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| SCMem | 3, crash | MajorityRead member, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | no violation to the cap (99,941,057 / d22): bounded |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| S31 | 3, lost mark, record down | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds, AckStamped | no violation to the cap (120,460,751 / d24): bounded |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| SAck | 3, lost mark | MajorityRead count, RelView | majority read | AckStamped | no violation to the cap (106,466,304 / d23): bounded |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| SZF3 | 3, zombie, MaxFails 3 | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d17 | hole class, pre-stamp junk (as c21 SZ: the same 18-state trace up to the order of two snapshots; the third failure is not used) | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| SLive | 3, lost mark | MajorityRead count, RelView | majority read | TypeOK, Prop_BarrierEnds | NO VERDICT: cut at the cap (d20, 26,300,370 states) before the liveness check could conclude; claims nothing |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 2 workers (a parallel runner) |  |
| SLiveNoRec | 3, lost mark, record down | MajorityRead count, RelView | majority read | TypeOK, Prop_BarrierEnds | VIOL Prop_BarrierEnds (lasso) d9 | control, expected: with the record down a minority barrier never ends (Prop_BarrierEnds lasso) | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 2 workers (a parallel runner) |  |
| SLF3 | 3, lost mark, MaxFails 3 | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | no violation to the cap (152,014,024 / d23): bounded |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| SZLate2 | 3, zombie | MajorityRead count, RelView | majority read | NoLateJunk2 | no violation to the cap (100,707,189 / d20): bounded |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 2 workers (a parallel runner) |  |
| SZDecreeSkip | 3, zombie | MajorityRead count, RelView, MonitorSkipsHold | ungated (any stage attempt) | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d17 | hole class, pre-stamp junk by another path (z's junk on its local leg is copied to a returning leg by its catch-up and served after a second move); MonitorSkipsHold removes the ungated decree's d13 violation (none at depth <= 16), but the d17 hole masks the rest: whether the alternative fix closes the third gap is NOT shown (needs a rev 10+ arm with pre-stamp junk legal) | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 2 workers (a parallel runner) |  |
| SCF3 | 3, crash, MaxFails 3 | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | no violation to the cap (93,412,376 / d21): bounded |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |

_STOPPED c21r 14:06:28Z: SL cut at ~14 min (10.2G RSS, growing) while flint-91's cargo unit holds 6G; DEFERRED: SL SC SZMem SLMem SCMem S31 SAck SZF3 SLF3 SCF3 SZLate2 SZDecreeSkip SLive SLiveNoRec_

_STOPPED c21 rerun 16:11:05Z: SC cut at ~12 min to make room for flint-91's 6G unit flint91-mon; DEFERRED again: SC SZMem SLMem SCMem S31 SAck SZF3 SLF3 SCF3 SZLate2 SZDecreeSkip SLive SLiveNoRec (to rerun on tla-mc 0.2.1 via rerun2.sh)_

_c21 CLOSED 2026-10-31 00:10:02Z. Every arm deferred by the two STOPPED lines above has since run (its row above is the rerun). For the night pause (the user, 10-30 22:36Z) SZLate2, SZDecreeSkip, SLive and SLiveNoRec ran on two parallel runners (2 workers each) from ms7p, a byte-identical copy of ms7 (same FlintCatchupS.tla 6859c997 / SReach 7d5d053b, same cfgs, compared with cmp); the main runner's four "ERROR <arm> no cfg" lines are those arms skipped there and are superseded by the ms7p rows. Bounded rows stopped at the box's caps (1800-3600 s, 3 workers or 2, 2048 MB fpmem/queue): no violation to the depth shown, not a full pass._
