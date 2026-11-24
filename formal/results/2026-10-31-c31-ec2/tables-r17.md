### rev 17 (FlintCatchupS 61ae6829): the identities, the four-replica zombie arm, the goal checks, the proof's candidate peels

c33b (c6i.16xlarge, i-06c7808ed514f0dc1): tla-mc 0.2.1 compiled; c31 (m7i.48xlarge): tla-mc 0.2.2 compiled. Bounded rows stopped at their cap: no violation to the depth shown, not a full pass. Peel rows: the box's peel.sh (every candidate an INVARIANT; a violated one is dropped and the base rerun); the final set (66 conjuncts) is q5's.

| job | arm / base | cfg md5 | verdict | secs |
|---|---|---|---|---|
| pk3 (c33b, rev 16) | Id16 | dc448b9b | HOLD 7,286,102 / d36 (full) | 107 |
| pk3 (c33b, rev 16) | P16L | 0f2f6dfa | all 39 candidates HOLD: 19,725,464 / d37 (full) | 173 |
| pk3 (c33b, rev 16) | P16C | 007ba768 | all 39 candidates HOLD: 26,049,299 / d36 (full) | 216 |
| pk3 (c33b, rev 16) | P16Z3 | 273816e2 | all 39 candidates: no violation to the cap: d14, 6,211,754 distinct (bounded) |  |
| pk3 (c33b, rev 16) | P16C3 | 0c36a186 | all 39 candidates: no violation to the cap: d15, 7,557,011 distinct (bounded) |  |
| pk3 (c33b, rev 16) | P16L3 | 9d6ea37a | all 39 candidates: no violation to the cap: d16, 8,721,369 distinct (bounded) |  |
| pk3 (c33b, rev 16) | P16Z | eadb0346 | all 39 candidates: no violation to the cap: d24, 42,073,499 distinct (bounded) |  |
| pk3 (c33b, rev 16) | P16CZ | d93f854b | all 39 candidates: no violation to the cap: d23, 44,937,847 distinct (bounded) |  |
| pk3 (c33b, rev 16) | G16C | 97909e0f | no violation to the cap: d18, 42,951,411 distinct (bounded) | 1802 |
| pk3 (c33b, rev 16) | G16L | 3a590337 | no violation to the cap: d19, 45,615,170 distinct (bounded) | 1803 |
| pk3 (c33b, rev 16) | G16Z | 5c868a8e | no violation to the cap: d17, 46,098,102 distinct (bounded) | 1803 |
| pk8 (c33b) | Id17 | a7b0080f | HOLD 7,286,102 / d36 (full) | 121 |
| pk8 (c33b) | P17L | 5fda85ff | all 54 candidates HOLD: 19,725,464 / d37 (full) | 229 |
| pk8 (c33b) | P17C | a79e6772 | all 54 candidates HOLD: 26,049,299 / d36 (full) | 293 |
| pk8 (c33b) | Z4Ack17 | 5ee58d31 | ERROR rc /  | 689 |
| pk8 (c33b) | P17Z | fb5e9231 | all 54 candidates: no violation to the cap: d23, 31,278,711 distinct (bounded) |  |
| pk8 (c33b) | P17Z4 | 27670071 | all 54 candidates: no violation to the cap: d13, 16,627,079 distinct (bounded) |  |
| pk8 (c33b) | P17CZ | 44c088fb | all 54 candidates: no violation to the cap: d22, 32,477,207 distinct (bounded) |  |
| pg1 (c33b) | G17Z | 093a9a35 | no violation to the cap: d17, 46,098,102 distinct (bounded) | 1801 |
| pg1 (c33b) | G17L | ca617734 | no violation to the cap: d19, 45,615,170 distinct (bounded) | 1802 |
| pg1 (c33b) | G17C | 5333f4ee | no violation to the cap: d19, 72,297,092 distinct (bounded) | 1802 |
| q1 | P17DL | 8407f8e5 | all 58 candidates HOLD: 28,546,374 / d37 (full) | 106 |
| q1 | P17DC | 870c6f25 | all 58 candidates HOLD: 38,136,587 / d36 (full) | 133 |
| q1 | P17DZ | c76c0a01 | all 58 candidates HOLD: 149,739,816 / d40 (full) | 313 |
| q1 | P17DCZ | cb1e2b44 | all 58 candidates HOLD: 191,910,011 / d39 (full) | 332 |
| q1 | P17DZ3 | ed116dbd | all 58 candidates: no violation to the cap: d18, 312,096,379 distinct (bounded) |  |
| q1 | P17DL3 | d0bbc224 | all 58 candidates: no violation to the cap: d20, 299,956,471 distinct (bounded) |  |
| q1 | P17DC3 | 27560fd6 | all 58 candidates: no violation to the cap: d19, 321,241,871 distinct (bounded) |  |
| q1 | P17DZ4 | 3480426b | all 58 candidates: no violation to the cap: d13, 150,564,745 distinct (bounded) |  |
| q2 | G17DC | 99c1d563 | no violation to the cap: d18, 191,925,993 distinct (bounded) | 1802 |
| q2 | G17DL | 9513f4ac | no violation to the cap: d19, 178,807,970 distinct (bounded) | 1803 |
| q2 | G17DZ | 93162dde | no violation to the cap: d17, 171,150,282 distinct (bounded) | 1803 |
| q2 | Z4Ack17 | 5ee58d31 | no violation to the cap: d16, 184,876,790 distinct (bounded) | 1804 |
| q2 | X4D17 | 0bb5829f | no violation to the cap: d13, 178,880,279 distinct (bounded) | 1805 |
| q3 | P17DL | 8407f8e5 | all 62 candidates HOLD: 28,546,374 / d37 (full) | 108 |
| q3 | P17DC | 870c6f25 | all 62 candidates HOLD: 38,136,587 / d36 (full) | 141 |
| q3 | P17DZ | c76c0a01 | all 62 candidates HOLD: 149,739,816 / d40 (full) | 339 |
| q3 | P17DCZ | cb1e2b44 | all 62 candidates HOLD: 191,910,011 / d39 (full) | 341 |
| q4 | P17DL | 8407f8e5 | all 65 candidates HOLD: 28,546,374 / d37 (full) | 115 |
| q4 | P17DC | 870c6f25 | all 65 candidates HOLD: 38,136,587 / d36 (full) | 142 |
| q4 | P17DZ | c76c0a01 | all 65 candidates HOLD: 149,739,816 / d40 (full) | 337 |
| q4 | P17DCZ | cb1e2b44 | all 65 candidates HOLD: 191,910,011 / d39 (full) | 353 |
| q5 | P17DL | 8407f8e5 | all 66 candidates HOLD: 28,546,374 / d37 (full) | 110 |
| q5 | P17DC | 870c6f25 | all 66 candidates HOLD: 38,136,587 / d36 (full) | 138 |
| q5 | P17DZ | c76c0a01 | all 66 candidates HOLD: 149,739,816 / d40 (full) | 340 |
| q5 | P17DCZ | cb1e2b44 | all 66 candidates HOLD: 191,910,011 / d39 (full) | 341 |
