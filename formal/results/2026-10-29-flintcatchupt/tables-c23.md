### c23: FlintCatchupS rev 9; FlintCatchupS.tla c41c6986, FlintCatchupSReach.tla 01df6860

| arm | replicas, fault | knobs on | decree form | invariants | verdict | trace class | method | flag |
|---|---|---|---|---|---|---|---|---|
| S2ReachVouchL | 2, lost mark | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | Reach_Vouch | reachable d4 | probe: the state is reachable |  |  |
| S2ReachVouchZ | 2, zombie | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | Reach_Vouch | reachable d2 | probe: the state is reachable |  |  |
| S2ReachVouchC | 2, crash | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | Reach_Vouch | reachable d4 | probe: the state is reachable |  |  |
| S2ReachVouchSpotLOff | 2, lost mark | MajorityRead count, RelView | majority read | Reach_VouchSpot | reachable d3 | probe: the state is reachable |  |  |
| S2ReachVouchSpotZOff | 2, zombie | MajorityRead count, RelView | majority read | Reach_VouchSpot | reachable d1 | probe: the state is reachable |  |  |
| S2ReachVouchSpotCOff | 2, crash | MajorityRead count, RelView | majority read | Reach_VouchSpot | reachable d3 | probe: the state is reachable |  |  |
| S2ReachVouchLOff | 2, lost mark | MajorityRead count, RelView | majority read | Reach_Vouch | unreachable, full space 4,341,608 / d37 |  |  | BY CONSTRUCTION: with the vouch off no stamped stage can set the ghost; shows only that the knob removes the rule (the situation itself is reachable: VouchSpot); do not cite as evidence of deferral |
| S2ReachVouchZOff | 2, zombie | MajorityRead count, RelView | majority read | Reach_Vouch | unreachable, full space 38,019,676 / d39 |  |  | BY CONSTRUCTION (as S2ReachVouchLOff) |
| S2ReachVouchCOff | 2, crash | MajorityRead count, RelView | majority read | Reach_Vouch | unreachable, full space 4,907,607 / d35 |  |  | BY CONSTRUCTION (as S2ReachVouchLOff) |
| S2LV | 2, lost mark | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 4,730,836 / d37 |  |  |  |
| S2CV | 2, crash | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 5,410,162 / d35 |  |  |  |
| S2ZVA | 2, zombie | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | TypeOK, AckedHeld, MembersHold, RaidHolds | HOLD, full space 50,779,768 / d39 |  |  |  |
| S2ZV | 2, zombie | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d16 | hole class, pre-stamp junk (rev 9 has no ZombiePreStampLegal; see c24) |  |  |
| S2LVR | 2, lost mark | MajorityRead count, RelView, EvenVouch read | majority read | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 4,730,836 / d37 |  |  |  |
| S2CVR | 2, crash | MajorityRead count, RelView, EvenVouch read | majority read | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 5,410,162 / d35 |  |  |  |
| S2ZVRA | 2, zombie | MajorityRead count, RelView, EvenVouch read | majority read | TypeOK, AckedHeld, MembersHold, RaidHolds | HOLD, full space 44,074,902 / d39 |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| S2L | 2, lost mark | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 4,341,608 / d37 |  |  |  |
| S2C | 2, crash | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 4,907,607 / d35 |  |  |  |
| S2ZA | 2, zombie | MajorityRead count, RelView | majority read | TypeOK, AckedHeld, MembersHold, RaidHolds | HOLD, full space 38,019,676 / d39 |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| S2Z | 2, zombie | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d18 | hole class, pre-stamp junk (as S2ZV) | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| S2ZVSoleOnly | 2, zombie | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | SoleOnly | HOLD, full space 50,779,768 / d39 |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| S2LVSoleOnly | 2, lost mark | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | SoleOnly | HOLD, full space 4,730,836 / d37 |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| S2ZVLate2 | 2, zombie | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | NoLateJunk2 | HOLD, full space 50,779,768 / d39 |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| S2LVAck | 2, lost mark | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | AckStamped | HOLD, full space 4,730,836 / d37 |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| S2ZVAck | 2, zombie | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | ZombieNoAck | VIOL ZombieNoAck d5 | permitted: the zombie of a record-confirmed sole continuation acks (design) | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| SId9 | 3, lost mark, MaxFails 1 | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 7,286,102 / d36 |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| SId9V | 3, lost mark, MaxFails 1 | MajorityRead count, RelView, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 7,286,102 / d36 |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |

_STOPPED c23 rerun 18:22:35Z: the remaining three- and four-replica arms (SLV SCV SZVA S4LV S4ZVA) are superseded by c27 on EC2 (rev 12 full rule set, 192 cores)_

