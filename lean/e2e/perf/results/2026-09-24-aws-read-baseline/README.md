# Step 0: AWS read baseline, 2026-09-24 (the same rig as 2026-09-12)

**Rig:** i4i.large spot in us-west-1, AL2023 (kernel 6.18.48), run over SSM. Bucket `flint-lean-door-20260924`,
versioned. Workload `small` (20,000 files of 8 KiB, 163,840,000 bytes), 3 reps, a cold read each time.
`results/doors-run-20260924-154206.tsv` holds the raw rows; `logs/` and `run.log` hold the drill output;
`launch.sh`, `prep.sh`, `ssmrun.sh` and `doors.sh` are the rig as it ran.

**Guards:** every arm read 20,000 files and 163,840,000 bytes, with ranged=0. The `ctl` guard failed, and that
failure was **a rig defect, not warm reads**. `cold_control` read only `s3arm-big`, and this drill ran only
`small`, so the control never ran and returned "0 0". I ran it by hand, before teardown, against the 20,000-file
`s3arm-small` tree (`find -exec cat`, 3 times, `drop_caches` before each):

    ctl-manual 1 cold 2789 warm 97
    ctl-manual 2 cold 2745 warm 97
    ctl-manual 3 cold 2741 warm 97

Cold is about 28x warm, so `drop_caches` drops. `doors.sh` is fixed: the control now falls back to `small`, and
a control with no tree prints "- -" and fails as UNCHECKED.

**Read, wall ms, range over 3 reps** (L-slow and P-1 ran once, as designed):

| arm | min | max | |
|---|---|---|---|
| L-ship (HEAD) | 5,069 | 5,349 | |
| L-0912 (f7d44444 rebuilt, not byte-identical to 09-12's binary) | 4,955 | 6,116 | |
| L-raw | 4,718 | 4,889 | |
| L-0910 | 16,038 | 16,130 | before the fan-out fix |
| L-slow | 243,146 | | a single rep |
| P-1 (mount-s3, serial) | 1,079,214 | | a single rep |
| P-32 (mount-s3, 32 parallel) | 64,696 | 69,492 | |
| Pw-32 | 66,362 | 67,773 | |
| PC-32 | 31,938 | 33,872 | |
| PCw-32 | 11,611 | 12,095 | |
| S-32 | 68,528 | 70,236 | |
| P-meta | 2,070 | 2,355 | |

**Against 2026-09-12** (lean 5.29–5.93 s, FUSE 63.25–69.24 s): the rig reproduces it. Lean (L-ship) against
uncached FUSE (P-32) is **12.1x at the worst pairing (64,696 / 5,349) and 13.7x at the best (69,492 / 5,069)**.
This is the baseline that P1 to P5 must not move.

**Teardown (verified):**
- The instance is terminated.
- 40,983 object versions and delete markers were purged, and the bucket was deleted (`head-bucket` returns 404;
  the account has 0 buckets).
- The inline policy `door-drill-20260924-bucket` was deleted. Only `AmazonSSMManagedInstanceCore` remains on the
  role, the same as before the drill.
- 0 instances, 0 volumes, 0 open spot requests.
