# forge at scale — the git-server-to-S3 path on a real cluster and a real bucket

Every other forge drill stands in the one size regime where the
transfer path's defects do not exist: the suite's largest payload is
12 MiB against a 64 MiB whole-put ceiling, and loopback MinIO restores
96 MiB in about a second. `forge/e2e/largerepo/` moved the LOCAL rig
past the ceiling (multipart executes; the restore's memory is flat).
This directory is what loopback cannot reach:

| leg | question | why it needs the cloud |
|---|---|---|
| S1/S2 | is a multi-GiB pack composed correctly on **AWS S3** — full-object CRC64NVME accepted at `CompleteMultipartUpload`, part count = ceil(size / 64 MiB), restored bytes pass `fsck --strict` | only S3 judges the checksum and the grid |
| S2 | does the restore's **anon RSS** stay flat (38–40 MiB claimed) for a pack of gigabytes | a restore long enough to sample, from `stats/summary`'s `rssBytes` (page cache excluded) |
| S2 | is the lease token **silent for longer than the takeover window** (6 × 10 s) during the push and during the restore | the silence is the restore's duration at S3's single-stream rate |
| S3 | does a **challenger seize** a repository whose pod is alive and mid-restore, and is any acknowledged push lost while two servers fight | the seizure needs a restore longer than 60 s; the control (a small repository) is the same procedure with a restore of seconds |
| S4 | **acknowledged means durable** with the kill placed INSIDE the multipart upload (observed via `list-multipart-uploads`, not a guessed sleep), and how many **orphaned uploads** the kills leave — forge never aborts one | the leak costs money on S3 and nothing on MinIO |

INCONCLUSIVE is not PASS: a leg that could not measure what it exists
for is counted separately and fails the run.

## Running it

```
# 0. images from THIS tree, pushed (amd64 for the EC2 nodes)
ARCH=amd64 PUSH=1 TAG=drill-$(git rev-parse --short HEAD) forge/e2e/build-forge-images.sh
# 1. a pure-spot trove cluster (CP + 2 workers, i4i.xlarge), kubeconfig exported
# 2. the workers' emptyDirs on the NVMe (an 8 GiB root cannot hold this)
forge/e2e/scale/prep-nodes.sh
# 3. the rig
BUCKET=... KEYFILE=... TAG=drill-... forge/e2e/scale/deploy.sh      # prints PREFIX
# 4. the drill
BUCKET=... PREFIX=... KEYFILE=... forge/e2e/scale/run-scale.sh
```

Knobs: `PROBE_MB` (1024) calibrates the restore rate; the large
repository is sized from it to restore for `TARGET_RESTORE_SECS` (150),
clamped to `[MIN_BIG_MB, MAX_MB]` (2048, 10240). `DUR_MB` (320) and
`DUR_ITER` (3) shape the durability leg. `LEGS` selects legs.
Results land in `results/scale-<stamp>.log` with the raw timelines in
`results/work-<stamp>/`.

## Results — 2026-09-05, cluster runbw (3 × i4i.xlarge all-spot, S3 us-west-1, image `drill-cda4b21e` = HEAD `cda4b21e`)

Run 1 (`results/scale-20260905-114635.log`): **45 passed, 0 failed, 1
inconclusive** — the inconclusive was S4's leak magnitude, re-measured
in run 2 (`results/scale-20260905-121220.log`, S4 alone at 2048 MiB).

| leg | what the wire said |
|---|---|
| S0 | the syncer in the pod carries the ranged-restore marker (judged by content; the image digest matched the staged binary's md5); both repositories serving; 814 GiB free under the NVMe-backed emptyDirs |
| S1 | 1024 MiB: push acknowledged in 29 s, token silent 13 s; ETag `-17` = ceil(1024.1 / 64); FULL_OBJECT CRC64NVME accepted; cold restore 13 s (78.8 MiB/s), anon RSS peak 4.7 MiB; `fsck --strict` clean |
| S2 | **10240.8 MiB pack, 161 serial 64 MiB parts, push acknowledged in 262 s; the token was silent 125 s during the push** (> the 60 s window); ETag `-161` = ceil(size / 64 MiB); FULL_OBJECT CRC64NVME accepted at CompleteMultipartUpload; **cold restore 136 s from the delete (135 s from the syncer's start, 75.9 MiB/s, one 8 MiB ranged GET in flight); the token was silent 141 s during it**; anon RSS peak 9.8 MiB (working set 307.8 MiB is page cache); `fsck --strict` clean in 84 s |
| S3 control | a challenger beside the 16 MiB repository's restoring successor never claimed in 150 s (its heartbeats kept the token moving); 16/16 pushes acknowledged, longest gap 11 s; serving 156 s after the delete (the challenger was removed at +150 s) |
| S3 seize | **the challenger claimed the 10 GiB repository 62 s after arriving, while the holder was `importing`**; the holder fenced at its next heartbeat and restarted (1 restart); then each side seized the other mid-restore — epochs 4, 5, 6 — until the challenger was removed at +392 s; the operator's pod served again **405 s** after the delete; 21 pushes acknowledged, 2 refused or timed out, longest gap between acknowledgements 207 s; **every acknowledged push was in the bucket** |
| S4 (320 MiB, run 1) | four kills: one after the pack existed (told ok, bucket holds it), three placed 2.0–3.5 s after the upload was first listed — all three landed AFTER the CAS (ack lost, the retry a clean no-op), because a 320 MiB upload completes inside that jitter on real S3; `fsck --strict` clean after four crashes; **0 orphaned uploads → INCONCLUSIVE**, the leak was not exercised |
| S4 (2048 MiB, run 2) | iteration 1's kill landed INSIDE the upload: told failed, the bucket unchanged, and **one orphaned multipart upload holding 384 MiB of parts** (the six 64 MiB parts uploaded before the kill) left behind — forge never aborts one (no `list_uploads`/`abort_upload` under `forge/`), so it is billed until a lifecycle rule or a hand abort; iteration 4 (killed around the CAS): told ok and the bucket holds it; `fsck --strict` clean after four crashes; 7 passed, 0 failed, 0 inconclusive. **Caveat, found reading the log:** iterations 2 and 3 were NOT mid-upload kills — the poll answered in 0 s with iteration 1's orphan and the kill fired during the git transfer, 30 s before any upload (both still held the invariant: told failed, bucket unchanged). Fixed in `run-scale.sh` after the run (uploads that predate the push are excluded), so the leak's magnitude rests on ONE interrupted upload: 384 MiB |

Real-S3 request latency from a worker (curl over SSM, HEAD on a private
key): TCP connect 1–2 ms, TLS complete at 9–15 ms, time-to-first-byte
22–35 ms on a fresh connection; 7–17 ms (median ~14 ms) per request on
a kept-alive one.

**Reading it.** The multipart path is correct on S3 at 10 GiB: the grid,
the part count, the full-object checksum, the restored bytes. The
restore's memory is flat in the pack. And design §5's "still open" is
open on the wire: at this size the token is silent for longer than the
window twice per lifecycle — once during the push (the batch renews
once, then uploads serial parts on the heartbeat's own task) and once
during the restore (never renewed) — so a second syncer, if one ever
exists, takes the repository from a live pod. The fence and the
durability invariant both held under that fight; what it costs is
availability (405 s from the delete to a serving operator pod, 207 s
between acknowledgements). A second syncer is not something the
`Recreate` Deployment makes by itself; it takes a roll against a wedged
pod, or a hand.

The leak is real and cheap to close: an orphaned upload is parts
already paid for and billed as storage until aborted, and forge has no
sweep. Either a bucket lifecycle rule (`AbortIncompleteMultipartUpload`
after a day) as a documented deployment requirement, or a
`list_uploads` + `abort_upload` pass under the repository's own prefix
at syncer start — the one writer knows nothing of its own is in flight
then. Neither is built here.

**Rig defects the real cluster exposed (both fixed in `run-scale.sh`):**
busybox `grep -a -c` matches a line as a C string, so it reported the
marker ABSENT from a binary that has it — the check now reads the
binary out and splits it locally; and the API server's pod proxy to
`:9848` is blocked by the repository's own NetworkPolicy under Cilium —
kind's CNI enforces no policy, which is why it worked on every local
rig. `prep-nodes.sh`'s `kubectl wait -A` sat 12 minutes past its own
timeout once the pods it watched were deleted; it polls the live set now.
