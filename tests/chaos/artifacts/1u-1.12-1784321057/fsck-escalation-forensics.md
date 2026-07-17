# U8 v2 escalation validation + durability forensics (2026-07-17)

Volume: pvc-af48c7b0-a077-4e1c-b48a-2dd1b6cbabd7 (the phase-1u harness
volume, in service since the start of the ublk campaign — never fsck'd
before U8 landed).

## Driver behavior (U8 v2) — VALIDATED

ublk.6 (preen-only): `e2fsck -p` exit 4 → mount refused → MountDevice
retry loop (fail-closed but wedged). ublk.7 (escalation): next kubelet
retry logged

    fsck -p /dev/ublkb0 before mount
    e2fsck -p could not repair /dev/ublkb0 (code 4); escalating to e2fsck -fy
    e2fsck -fy repaired /dev/ublkb0 (code 1)
    Mounting /dev/ublkb0 -> .../globalmount

pg-0 Ready 45s after the DS roll. PGDATA-on-ublk gate: /dev/ublkb0. The
driver-side A/B is complete: preen-insufficient corruption is repaired
unattended, mount proceeds.

## Data verdict — REAL LOSS of 4,298 acked writes (pre-drill ones)

verify-db ledger check first FAILED spuriously (ledger_pkey had a zero
page → `ORDER BY seq` index scan aborted → everything "missing").
Heap seqscan (enable_indexscan=off) ground truth:

- heap: 12,866 rows, seq 1..31786; acked.log: 17,165 entries
- missing: 4,298 acked seqs, a MID-RANGE hole ~18785..23082
- ack timestamps: seq 18785 acked T0-1165s, seq 23082 acked T0-15s —
  ALL lost writes were fsync-acked BEFORE the drill, up to 19 min old.

Recovery log: `redo starts at 10/A83E0650 ... redo done at 10/ABFFE460`
(0.12s) — replay stopped at the END of segment 0xAB. Segment 0xAC is one
of the e2fsck-cleared deleted-inode files. Postgres treats the first
unreadable segment as end-of-WAL and starts "cleanly", discarding every
later commit whose heap pages weren't yet background-flushed. Rows after
the hole survive only because their heap pages happened to be flushed.
Ongoing `could not access status of transaction` + `pg_subtrans invalid
entry` errors under new load = pg_xact/pg_subtrans SLRU damage too —
logical damage no fs repair can undo. VOLUME CONDEMNED (reset rule).

## Interpretation

The destroyed WAL segments were fsync-durable files; what died was the
ext4 METADATA about them (dir entries -> deleted inodes, cross-linked
blocks between two pre-drill WAL segments, bitmap drift). This volume
absorbed the ENTIRE phase-1u campaign of dirty tgt kills and severs with
journal-replay-only recovery (U8: no fsck ever ran). Each dirty event
that replays a journal against already-inconsistent metadata can
compound silently; the 1.12u sever + the pre-fix crash-loop remounts
surfaced the accumulated divergence. Earlier drills' "all acked present"
verdicts were true at their T0s.

Cannot fully exonerate the ublk/SPDK flush path post hoc; the
discriminating experiment is a FRESH volume with fsck-on-stage active
from first mount, rerunning 1.12u: single-sever damage should be
preen-repairable with zero acked loss. (That rerun is the clean 1.12u
verdict; this run stays FAIL in results.csv.)
