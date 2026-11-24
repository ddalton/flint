### q14 / q14b: FlintCatchupS rev 14 231150e6 (the premise the stamp proof inherits)

Box (i3-12100, 8 cores, shared), tla-mc 0.2.1 compiled, 3 workers, -fpmem/-queue-mem 2048/2048 MB, nice 19; FlintCatchupS.tla 231150e6 asserted. q14 20:33:04Z (STOPPED 20:42:41Z: its LastFails arm was in the WRONG CONFIGURATION, MaxFails 2; only Q14Id is cited from it), q14b 20:43:01Z - 21:44:26Z. Bounded rows stopped at the 1,800 s cap: no violation to the depth shown, not a full pass.

| arm | replicas, fault | rev 14/15 knobs on | invariants | verdict | secs | what it tests |
|---|---|---|---|---|---|---|
| Q14Id | 3, lost mark, MaxFails 1 | none | Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds | HOLD 7,286,102 / d36 | 80 | identity: every rev 14 knob off (= rev 13) |
| Q14LF3 | 3, lost mark, MaxFails 3 | LastFails | Inv_MembersHold, Inv_RaidHolds, Inv_AdmitNoLoss | VIOL Inv_MembersHold d16 (11,761,818 distinct) | 77 | killed control: the premise dropped, no rule |
| Q14LFW3 | 3, lost mark, MaxFails 3 | LastFails, WholeMark | Inv_MembersHold, Inv_RaidHolds, Inv_AdmitNoLoss | no violation to the cap: depth 19, 55,239,809 distinct, 189,246,191 generated (bounded) | 1803 | the whole mark (R1) with the premise dropped |
| Q14N3 | 3, lost mark, MaxFails 3 | none | Inv_MembersHold, Inv_RaidHolds, Inv_AdmitNoLoss | no violation to the cap: depth 23, 142,784,028 distinct, 462,650,880 generated (bounded) | 1804 | control: rev 13 (premise kept) |

cfg md5s: Q14Id e9db9a6e, Q14LF 897eb965, Q14LF2 e7f56d49, Q14LF3 3bbebdf0, Q14LFW f5404ff7, Q14LFW3 48becf4b, Q14N3 ab33320f, Q14W 7a2a1352
