### c24: FlintCatchupS rev 10; FlintCatchupS.tla 2a9567c7, FlintCatchupSReach.tla 2e192540

| arm | replicas, fault | knobs on | decree form | invariants | verdict | trace class | method | flag |
|---|---|---|---|---|---|---|---|---|
| S2ReachJunkNoStampZ | 2, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | Reach_JunkNoStamp | reachable d3 | probe: the state is reachable |  |  |
| S2ReachTornMarkedZ | 2, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | Reach_TornMarked | reachable d4 | probe: the state is reachable |  |  |
| S2ReachTornServedZ | 2, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | Reach_TornServed | reachable d5 | probe: the state is reachable |  |  |
| S2ReachTornMarkedC | 2, crash | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | Reach_TornMarked | reachable d6 | probe: the state is reachable |  |  |
| SReachTornMarkedZ | 3, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | Reach_TornMarked | reachable d4 | probe: the state is reachable |  |  |
| S2ZT | 2, zombie | MajorityRead count, RelView, TornExcluded, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d16 | hole class, pre-stamp junk (TornExcluded alone) |  |  |
| S2ZH | 2, zombie | MajorityRead count, RelView, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d17 | torn leg served, control (TornExcluded off) |  |  |
| S2ZTH | 2, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 33,637,829 / d39 |  |  |  |
| S2ZTHLate2 | 2, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | NoLateJunk2 | HOLD, full space 33,637,829 / d39 |  |  |  |
| S2LTH | 2, lost mark | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 4,452,875 / d36 |  |  |  |
| S2CTH | 2, crash | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 6,424,612 / d35 |  |  |  |
| S2ZTHOff | 2, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal | majority read | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 22,983,727 / d38 |  |  |  |
| S2ZTLate2 | 2, zombie | MajorityRead count, RelView, TornExcluded, EvenVouch decree | majority read or the vouch | NoLateJunk2 | no violation to the cap (14,789,186 / d24): bounded |  |  |  |
| SId10 | 3, lost mark, MaxFails 1 | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 7,286,102 / d36 |  |  |  |
| SZTH | 3, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds | no violation to the cap (18,609,637 / d17): bounded |  |  |  |
| SZH | 3, zombie | MajorityRead count, RelView, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds | no violation to the cap (22,577,098 / d17): bounded |  |  | CONTROL NOT KILLED within the cap (no violation to d17 at three replicas, bounded); rules out nothing; the killed control is two-replica S2ZH (VIOL d17) |

