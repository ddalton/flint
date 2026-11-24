### c31 gate arms (items 5 + 6): FlintCatchupS rev 12 (8e54566d), before the switch to the proof

c31 (m7i.48xlarge us-west-1a, i-08f520c6866db048b): tla-mc 0.2.2 compiled, one arm at a time, 190 workers, -fpmem/-queue-mem 180000 MB (out/c31.txt: `S 8e54566d sreach 2e192540 tla-mc 0.2.2 compiled m7i.48xlarge workers 190 fpmem/queue-mem 180000 19:57:05Z`). Every input md5 asserted before any build (in/run.sh; out/RESULTS.txt: md5ok=1); every arm's generated source and locked dependency set asserted equal to the box's (in/gensrc.expect). Bounded rows stopped at their cap: no violation to the depth shown, not a full pass.

| arm | setting (cfg header) | Reps | MaxFails | checked | cfg md5 | gensrc / lockdeps | verdict | secs / cap |
|---|---|---|---|---|---|---|---|---|
| SId12 | IDENTITY check (full space, MaxFails 1) | {1, 2, 3} | 1 | Inv_NoNewLoss Inv_MembersHold Inv_RaidHolds | d3f1d910 | 5900f94c/c5f4db1d | HOLD 7,286,102 / d36 (full) | 10 / 900 |
| XAck | three replicas, the lost mark, the full rule set | {1, 2, 3} | 2 | Inv_AckStamped | 8693daec | ccac9e0e/c5f4db1d | HOLD 1,774,204,193 / d45 (full) | 527 / 3600 |
| XLSole | three replicas, the lost mark, the full rule set | {1, 2, 3} | 2 | Inv_SoleOnly | 130d08a5 | 4c8c013e/c5f4db1d | HOLD 1,774,204,193 / d45 (full) | 523 / 3600 |
| XCAck | three replicas, a consumer crash, the full rule set | {1, 2, 3} | 2 | Inv_AckStamped | d4fd6287 | b08a71cc/c5f4db1d | HOLD 2,325,892,812 / d46 (full) | 731 / 3600 |
| XZTNJRisk | three replicas, a zombie move, the full rule set WITH the expiry | {1, 2, 3} | 2 | Inv_TornJunkMarked Inv_LossMarked | 1529db32 | 596f49af/c5f4db1d | no violation to the cap: d28, 4,048,397,901 distinct (bounded) | 3647 / 3600 |
| XZSole | three replicas, a zombie move, the full rule set | {1, 2, 3} | 2 | Inv_SoleOnly | 26f40765 | ff3b9afc/c5f4db1d | no violation to the cap: d29, 4,776,730,423 distinct (bounded) | 3645 / 3600 |
| X4ZTNJ | FOUR replicas, a zombie move, the full rule set | {1, 2, 3, 4} | 2 | Inv_NoNewLoss Inv_MembersHold Inv_RaidHolds Inv_TornJunkMarked | 72ae6c87 | 042c194d/c5f4db1d | CUT, not run (the user, 22:24Z, repurposed c31's cores for the proof's rev 17 checks; X4ZTNJ is rev 12 without the zombie triple at MaxFails 2, superseded) |  |
