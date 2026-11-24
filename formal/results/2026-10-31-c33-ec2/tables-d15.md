### d15: FlintCatchupS rev 15 3d409f1b (the drain window)

EC2 c33: m7i.16xlarge spot us-east-1f, i-0ac5876cab9ffd6bd (launched 21:22:38Z; reclaimed by AWS 21:53:34Z, "no Spot capacity"; d15 finished 21:44:02Z, before it). tla-mc 0.2.1 compiled, the eight arms in parallel, 3 workers each, -fpmem/-queue-mem 4096/4096 MB, each arm in its own copy of ms15 with its own TLAMC_CACHE; FlintCatchupS.tla 3d409f1b and every cfg md5 asserted on the instance. The seven drain arms: WholeMark TRUE, LastFails FALSE (rev 13 failure model), OperatorMark TRUE (the drain), MaxFails 3. Bounded rows stopped at the 1,200 s cap.

| arm | replicas, fault | rev 14/15 knobs on | invariants | verdict | secs | what it tests |
|---|---|---|---|---|---|---|
| D15NNA | 3, lost mark, MaxFails 3 | WholeMark, OperatorMark | Inv_AckStamped | VIOL Inv_AckStamped d5 (3,115 distinct) | 36 | drain: no guard, no stamp |
| D15GNA | 3, lost mark, MaxFails 3 | WholeMark, OperatorMark, InWriteGuard | Inv_AckStamped | VIOL Inv_AckStamped d5 (3,397 distinct) | 36 | drain: in-write guard, no stamp |
| D15NN | 3, lost mark, MaxFails 3 | WholeMark, OperatorMark | Inv_MembersHold, Inv_RaidHolds, Inv_AdmitNoLoss | VIOL Inv_MembersHold d12 (1,692,391 distinct) | 40 | drain: no guard, no stamp |
| D15NS | 3, lost mark, MaxFails 3 | WholeMark, OperatorMark, DrainStamps | Inv_MembersHold, Inv_RaidHolds, Inv_AdmitNoLoss | VIOL Inv_MembersHold d12 (2,783,875 distinct) | 44 | drain: stamp, no guard |
| D15Id | 3, lost mark, MaxFails 1 | none | Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds | HOLD 7,286,102 / d36 | 88 | identity: every rev 14/15 knob off (= rev 13) |
| D15GS | 3, lost mark, MaxFails 3 | WholeMark, OperatorMark, InWriteGuard, DrainStamps | Inv_MembersHold, Inv_RaidHolds, Inv_AdmitNoLoss | no violation to the cap: depth 18, 89,002,087 distinct, 245,401,753 generated (bounded) | 1202 | drain: guard + stamp |
| D15GSA | 3, lost mark, MaxFails 3 | WholeMark, OperatorMark, InWriteGuard, DrainStamps | Inv_AckStamped | no violation to the cap: depth 18, 89,002,087 distinct, 245,401,753 generated (bounded) | 1203 | drain: guard + stamp |
| D15GN | 3, lost mark, MaxFails 3 | WholeMark, OperatorMark, InWriteGuard | Inv_MembersHold, Inv_RaidHolds, Inv_AdmitNoLoss | no violation to the cap: depth 19, 93,175,140 distinct, 255,637,291 generated (bounded) | 1203 | drain: in-write guard, no stamp |

cfg md5s: D15GN 98f6a046, D15GNA 29fea7cd, D15GS bf5b1fb3, D15GSA 7d92bcd1, D15Id 2a3a5cde, D15NN 5f9c7b28, D15NNA ae7f7258, D15NS afabfc9b
