### c22: FlintCatchupS rev 8; FlintCatchupS.tla ab167ffe, FlintCatchupSReach.tla 3e9e01e2

| arm | replicas, fault | knobs on | decree form | invariants | verdict | trace class | method | flag |
|---|---|---|---|---|---|---|---|---|
| SReachRisk | 3, zombie | MajorityRead count, RelView, DeferExpires | majority read | Reach_Risk | reachable d4 | probe: the state is reachable |  |  |
| SZDecreeAll | 3, zombie | MajorityRead count, RelView | ungated (any stage attempt), last leg not spared | NoNewLoss, MembersHold, RaidHolds | VIOL NoNewLoss d8 | stamp gap, control (as c21) |  |  |
| SZRisk | 3, zombie | MajorityRead count, RelView, DeferExpires | majority read | NoNewLoss | VIOL NoNewLoss d12 | the expiry's cost (serve-with-risk loses a write held only on an unreachable leg); marked: see RiskMarked |  |  |
| SLRisk | 3, lost mark | MajorityRead count, RelView, DeferExpires | majority read | NoNewLoss | no violation to the cap (16,512,065 / d19): bounded |  |  |  |
| SCRisk | 3, crash | MajorityRead count, RelView, DeferExpires | majority read | NoNewLoss | no violation to the cap (25,857,285 / d19): bounded |  |  |  |
| SZRiskMarked | 3, zombie | MajorityRead count, RelView, DeferExpires | majority read | LossMarked | no violation to the cap (112,224,732 / d20): bounded |  |  |  |
| SLRiskMarked | 3, lost mark | MajorityRead count, RelView, DeferExpires | majority read | LossMarked | no violation to the cap (70,484,362 / d22): bounded |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.1.2 |  |
| SCRiskMarked | 3, crash | MajorityRead count, RelView, DeferExpires | majority read | LossMarked | no violation to the cap (106,316,435 / d22): bounded |  | rerun, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.1.2 |  |

_STOPPED c22r 14:02:07Z: SLRiskMarked cut at ~7 min and SCRiskMarked not run, to make room for flint-91's cargo unit (6G); both deferred (SLRisk/SCRisk found no loss to d19, so LossMarked holds there to d19)_
### c26: FlintCatchupS rev 12; FlintCatchupS.tla 8e54566d, FlintCatchupSReach.tla 2e192540

| arm | replicas, fault | knobs on | decree form | invariants | verdict | trace class | method | flag |
|---|---|---|---|---|---|---|---|---|
| S2ZTHJ | 2, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | TornJunkMarked | VIOL TornJunkMarked d7 | torn junk served unmarked, control (TornNeverSole off: the torn z serves alone on the vouch) |  |  |
| S2ZHJ | 2, zombie | MajorityRead count, RelView, ZombiePreStampLegal, EvenVouch decree | majority read or the vouch | TornJunkMarked | VIOL TornJunkMarked d5 | torn junk served unmarked, control (TornExcluded off: a torn z assembled beside a clean leg) |  |  |
| S2ZTNJ | 2, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds, NoLateJunk2, TornJunkMarked | HOLD, full space 29,140,655 / d39 |  |  |  |
| S2ZTNJRisk | 2, zombie | MajorityRead count, RelView, DeferExpires, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | TornJunkMarked | HOLD, full space 31,651,787 / d39 |  |  |  |
| SId12 | 3, lost mark, MaxFails 1 | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 7,286,102 / d36 |  |  |  |
| SZTNJ | 3, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds, TornJunkMarked | no violation to the cap (31,810,913 / d18): bounded |  |  |  |
