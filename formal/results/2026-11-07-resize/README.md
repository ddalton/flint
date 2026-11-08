# Resize in the membership-stamp model (gate item 7)

`formal/FlintCatchupSR.tla` is rev 18: FlintCatchupS rev 17 (md5 `61ae6829`, pinned by the
TLAPS proof, not touched) plus a `Resize` knob and its sub-knobs. With `Resize = FALSE` it is
rev 17 exactly; `RESULTS.md` shows this by running the same cfgs on both modules and matching
distinct states and depth.

## How the identity is kept

- The new variables (`lv cells prv mv fan ctl fsz rsz hw shortAdm unfinished wiped`) are
  frozen at `Init` (`lv = 2`, `cells = {2}`, `fsz = rsz = 1`, `hw = 2`, the rest 0 or FALSE).
- `Next == (NextOld /\ RFollow) \/ ResizeActs`. `RFollow` leaves every resize variable
  unchanged when `~Resize`; every guard added to an old action is TRUE when `~Resize`.
- The old `vars` is `oVars`; `vars == <<oVars, rVars>>`.

## Position model

- The lvol size `lv[r]` is 2 (one data cluster plus the stamp tail) or 3 (grown once).
  `MaxSz = 3`: one grow.
- The stamp tail is at position `lv[r]`. `cells[r]` is the set of positions holding stamp bytes.
- Block `b1` lives at position 2 (`BlockPos`): after a grow that is the old tail.
  Landing a data write over position 2 removes it from `cells`. A barrier or assembly stamp write
  adds the current tail to `cells`.
- `Readable(q) == ~Resize \/ lv[q] \in cells[q]`: a leg whose tail holds no stamp has no
  readable stamp.

## Action table: model action to code or doc

| Model action | Stands for |
|---|---|
| `ExpandStart` | ControllerExpandVolume begins; the belt (`replicas_not_in_sync`, `BeltOK == InSyncR = R`) passes. `expand.rs` `expand_replicated`; doc 12 "the belt refuses the fan-out while any leg lags". |
| `GrowLeg(r)` | The fan-out to one leg: `resize_leg` / `bdev_lvol_resize` on the leg's node. A leg already at the grown size (an admission aligned it) is a same-size no-op success. |
| `CopyStamp(r)` | The doc's Resize bullet: the stamp is copied from the old tail to the new tail (`prv` = old tail). Atomic, snapshot copy assumed. |
| `ZeroOld(r)` | The old tail is zeroed (the region becomes data space). |
| `ClearPrev(r)` | The `prv` marker is cleared; the move is done (`mv = 4`, `hw = 3`). |
| `CrashMove(r)` | The two crash windows of the move: window 1 (grown, before the copy) and window 2 (copied, before the zero/clear). `mv = 5`. |
| `RetryLeg(r)` | The re-drive of a leg after a crash (`ExpandReplicaFanoutIncomplete`). With `RetryRuns` it re-runs the move; without it, a same-size no-op success (the Design_ cfg). |
| `ExpandDone` | The controller sees every leg done (`mv = 4`; with `~MoveBeforeAck`, merely grown) and acks the expand. |
| `RaidResize` | `raid1_resize`: the array follows the lvol, `rsz = min over members (lv - Res)`, `Res` the reserve. Asynchronous, only after some leg is grown. |
| `FsGrow` | NodeExpandVolume: the filesystem grows to `rsz` after the controller ack (`ctl`); with `~FsAfterCtl` it needs only `fan`. |
| `StageRec` | The window-1 record fallback: no attachable leg has a readable stamp, so the assembly uses the record. |
| `RFollow` (on old actions) | A create (stage or admission) takes the reserve off (`rsz = min(lv' - 1)`); finishes a pending move when `prv # 0` (`FinishAtCreate`: "the assembly finishes the move itself rather than trusting the next retry"); aligns an admitted standby's size up to the cohort (`align_dst_head_size`); sets the ghosts `shortAdm` and `unfinished`. |
| `Fit` / `GuardOK` | `leg_size_guard.rs` `partition_legs`: the largest cohort is kept, the floor `hw` holds; shorter legs are excluded (`SizeGuard`). |

Knobs (TRUE is the doc): `Resize SizeGuard Reserve FinishAtCreate FsAfterCtl MoveBeforeAck
RetryRuns MoveCrash`.

## Invariants

Existing: `Inv_NoNewLoss Inv_MembersHold Inv_RaidHolds Inv_AckStamped`.
New: `Inv_RTypes`, `Inv_ArrayLeBases` (`rsz <= lv - 1` for members), `Inv_FsFits`,
`Inv_FsRangeClean` (an attached array's members have no stamp byte inside the fs range), `Inv_VacatedZero`
(a leg with `mv = 4` has no stamp bytes below its tail: the vacated region is zero),
`Inv_ShortNeverServes` (no short leg ever admitted), `Inv_MoveFinished` (no create leaves `prv` set),
`Inv_NoWipe` (a finish never zeroes live data), `Inv_FsLegsFit` (every in-sync leg is longer than the fs).

## Files

- `mkcfg.py`: generates all 27 cfgs in `cfg/`. `run.sh NAME [tla-mc args]`: runs one, log in `logs/`.
- `IdS_*` / `IdSR_*`: the rev-17 identity pair (S and SR with `Resize = FALSE`).
- `Res_*_r2f1a3`, `Res_*_r2f1a4`: two replicas, Resize on, all Bug_ off.
- `Res3_*_r3f1a3`: three replicas, bounded.
- `Bug_*`: planted bugs; `Design_*`: alternatives that fail by design.

## Out of scope

- The hot-rejoin refusal (the doc's guard 2): rev 17 has no live base add; members grow only at an assembly.
- Shrink, a second grow, sizes above 3, more than one block (one block at position 2).
- The driver-side RPC path for a leg under no raid ("not yet in code" in the doc); the model treats
  the fan-out as one mechanism.
- The raid superblock and the data-offset arithmetic below the cluster: sizes are in clusters.
- Liveness. Safety only.
- The short-leg guard and the floor guard are mostly postconditions here: `ExpandDone` needs every
  leg grown, so a short leg with the fs grown is unreachable in the correct model. The Bug_ cfgs show
  the guards are not vacuous.

## Timing assumptions

1. The annotation (stamp) write is atomic.
2. Each move step is atomic (grow, copy, zero, clear), and the stamp copy is a snapshot copy: the
   barrier-before-copy race is not modelled.
3. The raid resize event is asynchronous but happens-after some leg's grow; it is not forced to happen.
4. One grow only (`MaxSz = 3`); one block at position 2.
5. No hot-rejoin base add.
6. The belt is re-checked on each re-drive (`GrowLeg` requires `BeltOK`).
7. A crash can land only at the two windows; a retry may come at any later time, or never.
8. A create is atomic: sizing the array, finishing the move and aligning a standby happen together.
9. The controller ack and the fs grow are separate steps; the fs grow may happen at any time after
   its guard.
