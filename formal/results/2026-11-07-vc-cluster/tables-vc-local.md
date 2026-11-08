# The 2026-11-07 two-replica validation runs (local Mac, tla-mc 0.3.0 compiled, 16 workers)

From `logs/*.log` (gitignored). Rev 17 under `ASSUME Full`, 2 replicas, 1 failure, MaxVer 1, MaxEpoch 2, MaxAsm 3.
Ids = model-value ids without symmetry; Sym = `SYMMETRY Perms` (FlintCatchupSSym `9f274f80`);
V = the VIEW (FlintCatchupSSymV `35a7ec38`); `M8_*` on FlintCatchupS `61ae6829` with natural ids.

| Run | cfg | Verdict | Distinct states | Depth |
|---|---|---|---:|---:|
| S3_IdsV_crash_r2f1a3 | `8d09fc08` | HOLD | 5,679,938 | 32 |
| S3_IdsV_full_r2f1a3 | `201e6eec` | HOLD | 28,534,394 | 34 |
| S3_IdsV_lost_r2f1a3 | `eb3ba077` | HOLD | 4,120,772 | 31 |
| S3_IdsV_zombie_r2f1a3 | `4491eb83` | HOLD | 21,091,206 | 34 |
| S3_Ids_crash_r2f1a3 | `4a0fda01` | HOLD | 5,916,705 | 32 |
| S3_Ids_full_r2f1a3 | `d611cf6b` | HOLD | 35,853,855 | 35 |
| S3_Ids_lost_r2f1a3 | `33e52752` | HOLD | 4,218,568 | 31 |
| S3_Ids_zombie_r2f1a3 | `caab47d6` | HOLD | 26,146,988 | 35 |
| S3_SymV_crash_r2f1a3 | `809d5c01` | HOLD | 2,841,339 | 32 |
| S3_SymV_full_r2f1a3 | `584183ce` | HOLD | 14,268,567 | 34 |
| S3_SymV_lost_r2f1a3 | `60b23df3` | HOLD | 2,061,203 | 31 |
| S3_SymV_zombie_r2f1a3 | `d2241e8a` | HOLD | 10,546,420 | 34 |
| S3_Sym_crash_r2f1a3 | `cc66c94e` | HOLD | 2,959,746 | 32 |
| S3_Sym_full_r2f1a3 | `79013240` | HOLD | 17,928,321 | 35 |
| S3_Sym_lost_r2f1a3 | `5b122167` | HOLD | 2,110,114 | 31 |
| S3_Sym_zombie_r2f1a3 | `9e503dd1` | HOLD | 13,074,324 | 35 |
| S3_ViewUnique_zombie_r2f1a3 | `aaffaf33` | HOLD | 26,146,988 | 35 |
| M8_Both_zombie_r2f1a3 | `9eebf1da` | HOLD | 25,373,886 | 35 |
| M8_Ctl_zombie_r2f1a3 | `774892a7` | HOLD | 22,899,418 | 34 |
| M8_Final_zombie_r2f1a3 | `4fda8893` | HOLD | 26,146,988 | 35 |
| M8_Skip_zombie_r2f1a3 | `a1ae8048` | HOLD | 22,126,316 | 34 |
