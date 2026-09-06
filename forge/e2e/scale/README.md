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
| S5–S9 | **the front's party table** (the runner's acceptance, A3 of the simplification note; opt-in `LEGS="S0 S5 S6 S7 S9 S8"`): a client stalled 70 s mid-pack (S5, X3), five concurrent pushes and a sixth request timed (S6, X4), a rollout mid-push (S7, X6), the keepalive-gap probe from git's own packet trace (S9, run 3 finding 2) and its control with `receive.keepAlive=0` under a lowered door bound (S8, X5) | the door, the runner and git receive-pack in one pod behind a real door; the **control arm** is the same legs against the pre-A3 git image (`nginx` + `fcgiwrap`) and must FAIL S5, S6 and S9 |

INCONCLUSIVE is not PASS: a leg that could not measure what it exists
for is counted separately and fails the run.

## The runner's acceptance — legs S5–S9

`flint-forge-gitcgi` (A3, `641e5819`) replaced nginx + fcgiwrap in the
git container and claims to remove the class of front-layer knobs the
campaign kept finding members of. These legs judge it by that class,
each one a row of the simplification note's party table:

| leg | party and knob | how it is exercised | pass |
|---|---|---|---|
| S5 | nginx `client_body_timeout` (60 s default, X3) | the agent's `git-remote-http` is SIGSTOPped 3 s into the body for `STALL_SECS` (70) and resumed; the precondition checks the stop landed mid-transfer | acknowledged, the bucket holds it |
| S6 | `FCGIWRAP_CHILDREN=4` (X4) | `CONC_N` (5) pushes stopped mid-body; the git container's `receive-pack` count is read; one advertisement request is timed beside them; all resumed | 5 receive-packs, the request answered in ≤ 5 s, 5/5 durable |
| S7 | `terminationGracePeriodSeconds: 30` (X6) | `kubectl rollout restart` once the syncer reports `pushing`; the old pod's exit is timed against the grace period | told ok ⇒ durable, told failed ⇒ unchanged (or durable with a no-op retry), the successor serves, the retry converges, no orphan survives — and the outcome is RECORDED for the X6 decision |
| S9 | `receive.keepAlive` (X5) through the front | a `GAP_MB` push under `GIT_TRACE_PACKET` + `GIT_TRACE_CURL`; `gap_stats` takes the upload's end from curl and every `sideband<` packet from git | the wait ≥ `GAP_MIN_WAIT`, every gap ≤ `GAP_MAX` (8 s against 5 s keepalives) |
| S8 | the control for S9 | `receive.keepAlive=0` and the door's `--upstream-timeout-secs` patched to `CTRL_DOOR_SECS` (30); the same push; both restored after | the door cuts the client ≈ 30 s after the upload ends; the batch lands anyway (told failed but durable) |

Run S8 last: it restarts the door. The **control arm** deploys the same
tree with `server.gitImage=dilipdalton/flint-forge-git:1.46.0-forge.6`
(the last nginx + fcgiwrap image) and must FAIL S5 (408 at 60 s), S6
(four receive-packs, the request queued) and S9 (one burst with the
report); a control that passes means the legs cannot see what they
judge. S0 prints the git container's PID 1 so a run names its arm.

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
clamped to `[MIN_BIG_MB, MAX_MB]` (2048, 40960 — it was 10240 until the
restore fan-out made an i4i.xlarge restore at ~340 MiB/s, at which
10 GiB comes back in 30 s, inside the window). `DUR_MB` (2048) and
`DUR_ITER` (3) shape the durability leg. `LEGS` selects legs.
Results land in `results/scale-<stamp>.log` with the raw timelines in
`results/work-<stamp>/`.

**The expectation is named, because the oracles invert with the fixes.**
A drill that encodes today's behaviour as PASS confirms nothing about
the fixed tree, and one that encodes the fix would fail today for the
wrong reason. So:

| knob | value | S2 asserts | S3 seize arm asserts | S4 asserts |
|---|---|---|---|---|
| `EXPECT` | `window-open` (default; every tree up to and including `cda4b21e`) | token silent > 60 s during the push AND the restore | the challenger claims a live, `importing` pod; the holder fences; ping-pong is noted | — |
| `EXPECT` | `window-closed` (a renewer task holds the lease from before the restore) | silence ≤ 30 s (half the window: above a healthy heartbeat's 13 s plus a pod start, below anything a challenger could count) | the challenger NEVER claims across a restore longer than the window; the holder is never fenced, claims once, and serves with the challenger present | — |
| `SWEEP` | `none` (default) | — | — | orphans present after a mid-upload kill and still present once the successor serves; their size is the leak |
| `SWEEP` | `claim` (the successor aborts orphans under its prefix after its claim) | — | — | orphans present at the kill (sample one, taken before the successor can have claimed) and NONE once it serves (sample two) — the sweep is observed, not inferred from a zero |
| `DIGEST` | unset (default) or the `sha256:…` the build pushed | — | — | — |

S0 refuses a knob its image cannot satisfy: `EXPECT=window-closed` needs
the renewer's gate string in the syncer binary and `SWEEP=claim` the
sweep's abort string (both `2a213b01`), and each default needs its
string ABSENT — so an old image fails S0 as "wrong image", never S2–S4
as "the fix does not work", and a fixed image cannot pass the old
oracles. `DIGEST`, when given, must be the syncer pod's `imageID`. Both
exist because the chart pins `1.46.0-forge.1` and `deploy.sh` defaults
to `forge.2`, both older than the fixes: a deploy that falls back to a
default measures the wrong tree, and a tag cannot say which tree it
carries (a `1.41.1` syncer once shipped inside a `1.45.0` worker).

Under `window-closed`, S2 and S3 first check that the transfer they
judge OUTLASTED the window (the push's wall time, the restore's time
from the syncer's start, and the seize arm's own time-to-serve): inside
the window a token silent throughout is never counted and an unfixed
tree is never claimed either, so "silence under the bound" and "never
claimed" would pass on any tree. A transfer inside the window is
INCONCLUSIVE with the size to raise, not a pass — which is what the
first run on the fixed tree produced at 10 GiB before the guard existed.

Both samples are printed for every iteration in either mode. A kill
that leaves no orphan did not land inside the upload and is said so;
it counts for the durability invariant but not for the leak.

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
The second run (runbx) added two: `deploy.sh`'s "idempotent re-run"
could not move a repository to a fresh prefix, because `spec.keyPrefix`
is immutable on the CRD — it now deletes and recreates a repository
whose prefix changed; and the `window-closed` oracles had no
precondition, so the fixed tree PASSED S2's restore verdict at 10 GiB
with a 30 s restore that no challenger could have counted (see the
guard above). Run 4 added a fourth, in the observer itself: the token
watcher is `aws s3api head-object` every 2 s, and the CLI's own
connect and read timeouts default to 60 s — one HEAD hung, the sampler
was blind for 64 s, and the call that finally returned reported the
token it had seen at its START, which `longest_silence` then read as
66 s of a quiet renewer and the leg marked FAIL. The sampler now fails
a call at 5 s and records the sample as `blind`; silence is counted
only across contiguous observations; the longest blind spot is
reported beside it, and a blind spot longer than the bound makes the
verdict inconclusive rather than a pass or a fail. An observation that
can lie is a state variable (`feedback_model_the_observation`), in the
rig as much as in the product.

## Results — 2026-09-05, cluster runbx (3 × i4i.xlarge all-spot, S3 us-west-1), the confirmation campaign

The fixed tree — `2a213b01`'s renewer and orphan sweep, `7557d3c1`'s
restore fan-out — as `drill-22807f9b` (syncer digest `sha256:75f171b0…`),
run with `EXPECT=window-closed SWEEP=claim DIGEST=…`; the oracles are the
inverses of runbw's. Three runs, because the first two each found a
defect in something other than the syncer.

**Run 1** (`scale-20260905-142037`, stopped by hand in S2). S0 10/10:
the pod runs the pushed digest and the binary carries all three markers.
S1: a 1 GiB push acknowledged in 27 s with the token silent 10 s (runbw:
silent for the whole upload); the cold restore came back in 3 s from
the syncer's start — **341 MiB/s, 4.5× runbw's 76 MiB/s: the fan-out on
the wire**. Which sized the large repository at the 10 GiB clamp, and
10 GiB now restores in 30 s, inside the 60 s window: under the inverted
oracles a restore the window cannot even see would have PASSED S2 and
S3 on any tree. Stopped; the precondition guard above was added; the
clamp raised to 40 GiB.

**Run 2** (`scale-20260905-142728`, 40 GiB, 24 passed / 1 failed / 1
inconclusive). S0 10/10 again at a fresh prefix. **S2 FAILED at the
push: "the remote end hung up unexpectedly" 488 s in, the pack never
reaching the repository** — not the syncer: the door answered `502`
exactly 300.003 s after the POST began while the client was still
sending at 64 MiB/s (`results/door-timeout-trace-runbx-20260905.log`,
a traced push through the same door after the run). The door's
`upstreamTimeoutSecs` (300) was applied by reqwest to the WHOLE
exchange, body included, although its comment promised a headers-only
bound; runbw's 10 GiB push had passed at 262 s by luck. Fixed in
`ecb7c974` (an inactivity bound over both directions; see the door's
tests). S3's seize arm was inconclusive for want of the large
repository; **its control arm passed: a challenger beside a healthy
holder never claimed in 150 s, 16/16 pushes acknowledged under it are
in the bucket, longest gap 11 s**. **S4 4/4 and SWEPT 3/3**: three kills
inside a 2 GiB upload (seen 35–36 s after the push began) were refused
with the bucket unchanged; each left one orphaned upload at the kill
and NONE once the successor served; the kill around the snapshot CAS
was acknowledged and is in the bucket; fsck --strict clean after four
crashes; 0 incomplete uploads remain. The leak runbw measured is
closed, observed rather than inferred.

**Run 3** (`scale-20260905-151107`, `drill-ecb7c974` = the door fix,
40 GiB, 14 passed / 1 failed / 1 inconclusive). The push went THROUGH
the fixed door — 872 s, 40 GiB in the repository cache, a 640-part
multipart under way — and failed anyway, later, and the pieces of that
failure are three findings on the fixed tree:

1. **The syncer's own renewer went quiet for six heartbeats mid-push.**
   Between the sibling uploads and the first part, `packio::crc_of`
   streams the full-object CRC64 over the pack — ~70 s at 40 GiB — and
   ticked no progress, so the progress-gated renewer logged "moved
   nothing since the last renewal" and let the token sit for a whole
   takeover window inside a live push. A challenger present then would
   have deposed a healthy holder. Fixed in `4d66c48a` (the pass ticks
   the bytes it hashes). This is exactly the class the `window-closed`
   S2 verdict names: silence past the bound WITH gate lines is a stall
   of the sensor, not of the transfer.
2. **The client was cut 311 s after the hook phase began** — the door's
   new inactivity bound, firing because nothing crossed it during the
   hook wait. `receive-pack` does send an empty sideband keepalive every
   5 s while its hooks run (measured over the local transport on the
   same pod: +5 s, +10 s, report); through nginx + fcgiwrap the same two
   keepalives arrived in ONE burst with the report, 14 s after the pack
   ended. fcgiwrap holds the CGI's output until the request ends unless
   `NO_BUFFERING` is among its request parameters (Alpine's patch).
   nginx.conf now passes it, verified live on the pod before the image
   was rebuilt; the Dockerfile refuses an fcgiwrap without the switch.
   Fixed in `0734a2f9`.
3. **Told failed, but durable.** The syncer never learned the client had
   left: it completed the multipart, CAS'd the snapshot and moved the
   local ref, and the bucket names `agent/big` at the pushed tip while
   the client saw "the remote end hung up unexpectedly"
   (`results/told-failed-but-durable-runbx-20260905.log`). Not a loss
   and not a corruption — a retry finds the ref already there — but it
   is a transition the "acknowledged means durable" argument has to
   carry in the other direction, and the formal model of this path
   must include it.

S3's control arm passed a third time (challenger never claimed in
150 s beside a healthy holder; 16/16 pushes durable under it, longest
gap 11 s); its seize arm was inconclusive for want of the large
repository.

**Run 4** (`scale-20260905-155851`, `drill-0734a2f9` = all three fixes,
syncer `sha256:4d1ac995…311144`, 40 GiB, 30 passed / 1 failed / 0
inconclusive). The confirmation the campaign was for:

- **The push went through end to end: 40 GiB acknowledged in 1113 s.**
  641 parts by the grid, S3's FULL_OBJECT CRC64 accepted at Complete,
  the snapshot at the tip. The hook wait was over 8 minutes, so the
  keepalives crossed fcgiwrap AND the door's inactivity bound held
  across them — runs 2 and 3's two front-layer defects, closed on the
  wire in one push.
- **The restore: 139 s from the delete for 40963 MiB (297 MiB/s), refs
  exactly the snapshot's, `fsck --strict` clean in 414 s, anon RSS
  25.3 MiB.** Token silent 11 s across it.
- **S3 seize arm, CLOSED on the wire.** A challenger beside a live
  40 GiB restore for 398 s never claimed; the successor served 135 s
  after the delete WITH the challenger present; the holder was never
  fenced; 24/24 pushes acknowledged under the contention are in the
  bucket. This is the strongest observation of the campaign, because
  here the oracle is a real challenger counting quiet polls with the
  syncer's own client, not a sampler. The control arm passed a fourth
  time (150 s, 16/16 durable).
- **The one FAIL is the sampler's, not the renewer's** (rig defect
  four, above). Re-read with the corrected `longest_silence`, the push's
  token was silent 11 s where the watcher could see and the watcher was
  blind for 64 s once; the syncer's log for the window has neither a
  "moved nothing" line nor a heartbeat error, so the progress gate
  never held the token and no renew failed. What the 64 s hid is
  unobserved: under the corrected rig this verdict is INCONCLUSIVE, and
  run 5 below is its rerun with an unblinking sampler.

**Run 5** (`scale-20260905-164749`, same digest, `LEGS="S0 S2"` at
10 GiB with the corrected sampler, 21 passed / 0 failed / 1
inconclusive). The push: 10240 MiB acknowledged in 259 s (> 60 s
window), 161 parts, CRC accepted, token silent at most 11 s, **watcher
blind 0 s** — the push-side renewal verdict PASSES on an observer that
could not have hidden a silence. The restore (35 s, 301 MiB/s, fsck
clean, RSS 22 MiB) is inconclusive by the precondition, as 10 GiB must
be: run 4's 40 GiB restore and its seize arm carry that claim.

**Run 6** (`scale-20260905-171840`, `drill-1288e6ee` = D1 of the
simplification note, syncer `sha256:8945bd19…16e6af`, `LEGS="S0 S1 S4"`,
31 passed / 0 failed / 0 inconclusive). The git image's two hooks are
now the syncer binary under the hook names (one build for the hook and
the syncer it talks to; one `server.tag` in the chart): a 1 GiB push
went through them end to end (17 parts, CRC accepted, snapshot at the
tip, restore 9 s fsck-clean), and S4's four kills held told-failed ⇒
unchanged and told-ok ⇒ durable, every orphaned upload swept.

Campaign total on `drill-0734a2f9`: every product oracle green on the
wire — push and restore renewal, keepalives through the front, the
takeover window closed against a real challenger, orphan sweep, and
told-ok ⇒ durable under contention (40/40 pushes across both arms).

