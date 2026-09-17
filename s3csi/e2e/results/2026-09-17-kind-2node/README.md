# s3csi on kind, 2026-09-17 — and what a single-node cluster does to it

Run on one Ubuntu 25.10 box (kernel 6.12, 8 cores), kind + MinIO, no AWS.
Baseline for comparison: `s3csi-runcr-20260910-run2.log`, a REAL two-node
EC2 cluster, **182 ok / 1 BAD** — and that BAD is the rig refusing to
record a vacuous pass (S17's adoption race closed before it could observe
which branch ran; it says so and prescribes widening the window).

## The single-node trap — `clean-legs.log`, 160 ok / 20 BAD

Every one of those 20 was the CLUSTER, not the code. `run-s3csi.sh:62`:

    NODE=${NODE:-$($K get nodes -l '!node-role.kubernetes.io/control-plane' ...)}

selects a WORKER. A one-node kind cluster has only a control-plane, so
that selector matches nothing and `$NODE` is EMPTY. `onnode()` then runs
`docker exec "" sh -c …` behind `2>/dev/null`, so every node observation
failed SILENTLY and returned "". The BADs land exactly in the legs that
touch the node (S16 18 references, S20 18, S18 6) and the legs that never
touch it passed 100%.

Worth stating plainly: reading the drill is what found this, not the
control run. The control (same rig, same cluster, pre-Chainguard
flint-sync base) would have reproduced the BADs and concluded "not this
release's doing" — a true answer for entirely the wrong reason.

## Two-node, first attempt — `fin-legs.log`, 30 ok / 10 BAD

Also not the code: MinIO RESTARTED after the seed job completed, lost its
emptyDir, and took the bucket with it. `NoSuchBucket` crashlooped the
worker, so tenant pods never started and every downstream leg reported
`pod reader is 'absent'`.

## Two-node, seeded — `r2-legs.log`

The real run. Note the probe that nearly produced a third false
diagnosis: `mc ls local` reported no buckets, which looked like the seed
failing again. The `mc-s3` pod has no `local` alias — the rig uses
`mc ls m/$BUCKET/`, which showed 13 objects, correctly seeded. A bad
probe is indistinguishable from the failure it imitates until you check
the probe.

## Status

INCOMPLETE: stopped at 21 of 25 legs when the box was released, at
104 ok / 11 BAD (S10 2, S13 1, S14 5, S17 1, S17f 1, S17f-seed 1). The
S14 entries are PRECONDITION failures — the lean takeover legs could not
stage their scenario (empty lease/identity reads, `lean-agent2` never
reaching Running in 400s), so they prove nothing either way rather than
indicting the product. Not yet attributed; the honest verdict is UNKNOWN.

NOT RUN: the forge and pnfs e2e rigs.

`linux-lib-suite.log` is the lib suite on REAL Linux: 2535 passed, 0
failed — 21 tests more than the same tree yields on macOS, where
`#[cfg(target_os = "linux")]` test code neither runs nor typechecks.
