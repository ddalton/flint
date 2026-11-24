### Liveness on rev 17 (FlintCatchupS 61ae6829, the proof's Full), two replicas, MaxFails 2

c31 (m7i.48xlarge, i-08f520c6866db048b), tla-mc 0.2.2 compiled, 12-16 workers, -fpmem/-queue-mem 16384 MB. lv1 through FlintCatchupSLiveW.tla (752b1a56: SpecNoReturn = Spec less WF Return); lv2 through FlintCatchupSLiveW2.tla (6604c794: Prop_BarrierEndsM = a barrier ends or the raid has no member left; Prop_BarrierEndsH = ... or no member is healthy). The lv1 lasso: leg 2 begins to fail, the drain (or a second failure) takes leg 1, the failing leg fails the stamp: StampLand with no survivor, members = {}, attached and held forever.

| job | arm / base | cfg md5 | verdict | secs |
|---|---|---|---|---|
| lv1 | L17LNoRec | 3b0693c9 | VIOL Prop (lasso) d7 (3,185 distinct) | 49 |
| lv1 | L17ZNoRet | e98b96ba | VIOL Prop (lasso) d7 (6,023 distinct) | 49 |
| lv1 | L17ZNoRec | 825cba36 | VIOL Prop (lasso) d7 (5,673 distinct) | 50 |
| lv1 | L17CNoRec | fb1c27d1 | VIOL Prop (lasso) d7 (4,134 distinct) | 50 |
| lv1 | L17LNoRet | 48793bd0 | VIOL Prop (lasso) d7 (3,489 distinct) | 50 |
| lv1 | L17L | e729c341 | VIOL Prop (lasso) d7 (3,489 distinct) | 50 |
| lv1 | L17PathNoRet | 3fa0c513 | VIOL Prop (lasso) d7 (3,489 distinct) | 50 |
| lv1 | L17CNoRet | debb25c4 | VIOL Prop (lasso) d7 (4,466 distinct) | 51 |
| lv1 | L17C | 7bf5bd31 | VIOL Prop (lasso) d7 (4,466 distinct) | 51 |
| lv1 | L17Z | c42d5199 | VIOL Prop (lasso) d7 (6,023 distinct) | 51 |
| lv1 | L17PathSpec | 4fd28392 | HOLD 28,546,374 / d37 (full) | 260 |
| lv2 | L17LH | 4ad9a9d2 | HOLD 28,546,374 / d37 (full) | 140 |
| lv2 | L17LM | dc4ee5e4 | HOLD 28,546,374 / d37 (full) | 143 |
| lv2 | L17CM | 385bbec1 | HOLD 38,136,587 / d36 (full) | 160 |
| lv2 | L17CH | 5fabf2ce | HOLD 38,136,587 / d36 (full) | 166 |
| lv2 | L17ZH | aa269528 | HOLD 149,739,816 / d40 (full) | 443 |
| lv2 | L17ZM | 72c4d732 | HOLD 149,739,816 / d40 (full) | 456 |
