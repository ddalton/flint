
### c28: FlintCatchupS rev 12; FlintCatchupS.tla 8e54566d, FlintCatchupSReach.tla 2e192540

| arm | replicas, fault | knobs on | decree form | invariants | verdict | trace class | method | flag |
|---|---|---|---|---|---|---|---|---|
| S2LiveLNoRec | 2, lost mark, record down | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | TypeOK, Prop_BarrierEnds | VIOL Prop_BarrierEnds (lasso) d9 | control, expected: with the record down a minority barrier never ends (Prop_BarrierEnds lasso) | first run, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 4 workers |  |
| S2LiveL | 2, lost mark | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | TypeOK, Prop_BarrierEnds | HOLD, full space 3,627,109 / d35 |  | first run, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 4 workers |  |
| S2LiveCNoRec | 2, crash, record down | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | TypeOK, Prop_BarrierEnds | VIOL Prop_BarrierEnds (lasso) d8 | control, expected (as S2LiveLNoRec) | first run, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 4 workers |  |
| S2LiveC | 2, crash | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | TypeOK, Prop_BarrierEnds | HOLD, full space 5,326,386 / d35 |  | first run, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 4 workers |  |
| S2LiveZNoRec | 2, zombie, record down | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | TypeOK, Prop_BarrierEnds | VIOL Prop_BarrierEnds (lasso) d8 | control, expected (as S2LiveLNoRec) | first run, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 4 workers |  |
| S2LiveZ | 2, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | TypeOK, Prop_BarrierEnds | HOLD, full space 29,140,655 / d39 |  | first run, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 4 workers |  |

_c28 (the gate's item 4, flint-cb 2026-10-31): batch run 19:09:51-19:17:49Z on the box via rerun2.sh (its "RERUN" header; a first run), model ms12 = rev 12 as c26/c27.  Each HOLD log ends "liveness: all properties checked" before "No error has been found"; each lasso ends stuttering with the barrier held.  The claim rests on Spec's weak fairness (MarkStale, Return, SelectBase, RevertD, BulkCopy, SessionFail, CutOne per leg; CutRecord, CutAbort, Observe, StampLand, Release, RecordConfirm); at three replicas liveness stays bounded only (c21 SLive)._

### c30: FlintCatchupS rev 12; FlintCatchupS.tla 8e54566d, FlintCatchupSLiveW.tla b719dbbf

| arm | replicas, fault | knobs on | decree form | invariants | verdict | trace class | method | flag |
|---|---|---|---|---|---|---|---|---|
| S2LiveLNoRet | 2, lost mark | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree, SPECIFICATION SpecNoReturn | majority read or the vouch | TypeOK, Prop_BarrierEnds | HOLD, full space 3,627,109 / d35 |  | first run, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 4 workers |  |
| S2LiveCNoRet | 2, crash | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree, SPECIFICATION SpecNoReturn | majority read or the vouch | TypeOK, Prop_BarrierEnds | HOLD, full space 5,326,386 / d35 |  | first run, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 4 workers |  |
| S2LiveZNoRet | 2, zombie | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree, SPECIFICATION SpecNoReturn | majority read or the vouch | TypeOK, Prop_BarrierEnds | HOLD, full space 29,140,655 / d39 |  | first run, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1, 4 workers |  |

### c30b: FlintCatchupS rev 12; FlintCatchupS.tla 8e54566d, FlintCatchupSLiveW.tla 752b1a56

| arm | replicas, fault | knobs on | decree form | invariants | verdict | trace class | method | flag |
|---|---|---|---|---|---|---|---|---|
| S2PathSpec | 2, lost mark | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree | majority read or the vouch | TypeOK, Prop_PathReturns | HOLD, full space 3,627,109 / d35 |  | first run, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |
| S2PathNoRet | 2, lost mark | MajorityRead count, RelView, TornExcluded, ZombiePreStampLegal, TornNeverSole, EvenVouch decree, SPECIFICATION SpecNoReturn | majority read or the vouch | TypeOK, Prop_PathReturns | VIOL Prop_PathReturns (lasso) d9 | positive control, expected: without WF(Return) a failed path may stay out (Prop_PathReturns lasso), so SpecNoReturn is what ran | first run, -fpmem/-queue-mem 2048/2048 MB, tla-mc 0.2.1 |  |

_c30 / c30b (flint-cb, 2026-10-31: does a barrier end without a failed path coming back?): run 19:22:59-19:31:11Z and 19:33:52-19:35:23Z on the box via c30.sh (every arm on FlintCatchupSLiveW.tla, both md5s asserted), model ms12 = rev 12.  SpecNoReturn = Spec with only WF_vars(Return(r)) dropped (diffed against Fairness).  Each c30 HOLD log ends "liveness: all properties checked"; each counts exactly as its c28 twin (fairness does not change the reachable states).  c30b is the positive control that the SPECIFICATION line switched the fairness: Prop_PathReturns holds under Spec and fails with a lasso (a path stays out) under SpecNoReturn.  FlintCatchupSLiveW.tla 752b1a56 = b719dbbf + the definition of Prop_PathReturns (SpecNoReturn unchanged).  So at two replicas, every fault, a barrier ends without any fairness on a failed path coming back._
