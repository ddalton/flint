### c27: FlintCatchupS rev 12 on EC2 (m7i.48xlarge, tla-mc 0.2.1 compiled, -fpmem/-queue-mem 180000 MB, 160 workers; XCTNJ 190); FlintCatchupS.tla 8e54566d, FlintCatchupSReach.tla 2e192540

| arm | replicas, fault | knobs on | decree form | invariants | verdict | trace class | method | flag |
|---|---|---|---|---|---|---|---|---|
| SId12 | 3, lost mark, MaxFails 1 | MajorityRead count, RelView | majority read | NoNewLoss, MembersHold, RaidHolds | HOLD, full space 7,286,102 / d36 |  | EC2, 160 workers | identity: the box's count on this machine |
| XLTNJ | 3, lost mark, MaxFails 2 | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds, NoLateJunk2, TornJunkMarked | HOLD, full space 1,774,204,193 / d45 |  | EC2, 160 workers, 629 s |  |
| XCTNJ | 3, crash, MaxFails 2 | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds, NoLateJunk2, TornJunkMarked | HOLD, full space 2,325,892,812 / d46 |  | EC2, 190 workers, 824 s |  |
| XZTNJ | 3, zombie, MaxFails 2 | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | NoNewLoss, MembersHold, RaidHolds, NoLateJunk2, TornJunkMarked | no violation to depth 27 (2,429,420,385 distinct): bounded |  | EC2, 160 workers, cut at the user's word at 1186 s | BOUNDED, not a full pass: the space still grew x1.5 per depth at d27 and the queue had spilled to disk; cite as "no violation to depth 27" (the box's SZTN, rev 11, was bounded at d19) |

_Not run (the user cut the batch short): XZTNJRisk, XZSole, XLSole, XAck, X4ZTNJ. c16 (17 arms, FlintCatchupT rev 15 + TieNA) and the TLAPS record move to the Linux box._
