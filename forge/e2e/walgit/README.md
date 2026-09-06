# forge versus walgit — the control arm

The comparison pre-registered in
`docs/plans/flint-forge-simplification-2026-09-05.md` §9, and the tools
that run it. The question it answers is the one the architecture
document's last page left open: is forge's core earning its place on
the wire against the nearest open-source instance of the same shape, or
would "walgit behind the door, with the export as a reader of its log"
be the honest alternative?

## What is here

| file | what |
|---|---|
| `deploy-walgit.sh` | builds walgit's image **on the cluster** (a docker-in-docker pod on one worker, `docker build` from walgit's git URL at a pinned full commit hash, the image saved into the builder's emptyDir and imported into that node's containerd by a hostPID pod) and deploys one walgit instance pinned to that node — token auth, the S3 backend on the campaign's bucket under `walgit/<stamp>`, `cache.mode = "disk"` on an emptyDir the NVMe backs, `wal.batch_window = 400ms` (forge's), bundles on the standalone shape. No registry, no credential leaves the machine. |
| `walgit.yaml.tpl` | the Secret, ConfigMap, Deployment and Service the deploy renders |
| `run-compare.sh` | the P-legs (below), against both arms from one agent pod with one stock git; every leg pushes the same bytes to both arms, order alternating per repetition |
| `cw-summary.sh` | S3 request metrics per arm (`FilterId` `forge` / `walgit`, one per prefix) for the windows the legs record — bytes uploaded per push, requests per push; CloudWatch lags ~20 min |
| `phase0.sh` | the no-cloud smoke (walgit + MinIO as containers on a laptop). Written first; **not run** — the laptop's Docker VM OOM-killed walgit's link step, which is why the image is built on the cluster |

## The arms

Forge is the scale rig's deployment (`forge/e2e/scale/deploy.sh`: the
chart with the door, two FlintRepos, the agent) at `forge/<stamp>`; the
comparison uses the `small` repository, patched to allow non-fast-forward
pushes on `agent/*` so the undo leg can force-push. walgit is one
instance at `walgit/<stamp>` on the same bucket. Both are on the same
worker; the agent is alone on the other worker (tainted for it), so each
arm is one hop away and neither shares its node with the client's git.
Forge's requests are the chart's; walgit's are the same 50m / 64Mi.

Both runs' forge image is `drill-be76cc9c`, which predates X13
(`40b4a079`, the holder's own term); P10 therefore records forge's
pre-X13 outage behaviour, as §9 pre-registered.

## The legs

| leg | what | metric | rule (§9) |
|---|---|---|---|
| P0 | preconditions: both arms answer, the agent's clock has sub-second resolution (busybox `date` has no `%N`; `/proc/uptime` does), versions and nodes recorded, a seed push to each arm | — | must pass |
| P1 | push latency at 0 (1 KiB), 64, 1024 MiB; 5 reps, order alternating | wall to `ok`, median/min/max | within the rep-to-rep spread |
| P4 | two clones of one base, both push with `--force-with-lease` at once | exactly one winner; the ref is the winner's tip | both must hold |
| P9 | 48 pushes of 8 MiB to one branch | bytes uploaded per push (CloudWatch) | X18's number |
| P2 | 32 pushers, tiny commits to distinct branches, 60 s | acknowledged pushes/s; per-push latency; requests per push (CloudWatch) | recorded |
| P7 | 8 concurrent single-branch clones of a 1 GiB branch, one alone first | wall, ratio to solo, aggregate MiB/s, peak serving processes | within spread |
| P5 | the arm's pod deleted (a fresh emptyDir) | time to the first correct `ls-remote`, to the first correct clone | X14's number |
| P11 | a branch force-pushed back one commit | can the previous tip be recovered from the bucket? | forge loses by construction (X15) |
| P10 | a NetworkPolicy cuts the arm's pod off from S3 | a read at +5 s, readiness over 90 s, a push, recovery | recorded, not scored |

## Running it

```
# a runby-class cluster (CP + 2 workers, i4i.xlarge, all spot), prep-nodes.sh done
forge/e2e/scale/deploy.sh                                   # BUCKET KEYFILE TAG PREFIX=forge/<stamp>
kubectl taint nodes <worker-1> arm=agent:NoSchedule; recreate agent1 there
kubectl -n agents patch flintrepo small --type merge -p '{"spec":{"branches":{"allowNonFastForward":["agent/*"]}}}'
forge/e2e/walgit/deploy-walgit.sh                           # BUCKET WPREFIX=walgit/<stamp>
forge/e2e/walgit/run-compare.sh                             # BUCKET PREFIX WPREFIX KEYFILE [LEGS ARMS]
forge/e2e/walgit/cw-summary.sh results/work-<stamp>         # twenty minutes later
```

Results land in `results/compare-<stamp>.log` with the raw timings in
`results/work-<stamp>/`. INCONCLUSIVE is not PASS; the script declares
no winner — §9's rule is applied by a human against the log.

## Results — runs 1 and 2, 2026-09-05, cluster runbz

Two runs on one cluster (CP + 2 × i4i.xlarge, all spot, us-west-1;
forge `drill-be76cc9c` and walgit `e5295e6` both on runbz-aws-2, the
agent alone on runbz-aws-1). Run 1 was all nine legs
(`results/compare-20260905-220006.log`, 22:00–22:45 PDT). Run 2
(`results/compare-20260905-224804.log`) re-ran P7, P5, P11 and P10
after the rig defects run 1 exposed were fixed; its P0 seeded a fresh
branch pair on a repository that by then held everything run 1 had
pushed. The raw timings are in the two `results/work-*` directories;
the CloudWatch sums for run 1 are in
`results/work-20260905-220006/cw-summary.txt`.

### The legs

| leg | forge | walgit | reading |
|---|---|---|---|
| P0 seed push, 1 KiB (run 1 · run 2) | 620 · 660 ms | 170 · 120 ms | the fixed cost of a lone push: forge's 400 ms batch window (X20) |
| P1 push 1 KiB, median (min–max) of 5 | 0.58 s (0.57–0.59) | 0.10 s (0.10–0.11) | walgit, beyond spread |
| P1 push 64 MiB | 2.55 s (2.23–19.10) | 1.88 s (1.82–1.90) | walgit, beyond spread; forge's max is one push caught behind a repack |
| P1 push 1 GiB | 27.4 s (24.3–31.0) | 30.8 s (30.3–42.5) | **forge**, beyond spread: stock `receive-pack` plus one multipart upload against walgit's own receive-pack |
| P4 two `--force-with-lease` pushes at once | one winner; the loser told `stale info: fetch first` | one winner; the loser told the expected/got pair | both hold |
| P9 48 pushes of 8 MiB to one branch, wall | 1021 s; per push median 0.98 s, max **816 s** | 34 s; median 0.46 s, max 0.99 s | walgit, 30× |
| P9 bytes uploaded to S3 (CloudWatch) | 12.84 GB = **33×** the 384 MiB pushed, in 737 PUTs | 0.40 GB = 1.05×, in 197 PUTs | walgit, 32×: X18's number |
| P2 32 pushers, 60 s | 64 acknowledged = 1.1/s; latency median 78.6 s (1.5–155.7) | 673 acknowledged = 11.2/s; median 2.9 s (0.7–6.5) | walgit, 10× |
| P2 S3 requests per acknowledged push | 7.9 (283 PUT, 218 HEAD, no GET); 5.44 GB uploaded — two full repacks inside the minute | 5.6 (1654 PUT, 2028 GET, 114 HEAD); 4.3 MB uploaded | walgit; its reads are conditional GETs on every request |
| P7 one 1 GiB clone, then 8 at once (run 1 · run 2) | solo 18.6 · 17.1 s; eight in 65 · 64 s; 126 · 128 MiB/s aggregate; 8 serving processes | solo 14.8 · 16.4 s; eight in 64 · 64 s; 128 · 128 MiB/s; 8 | draw: the agent's NIC is the ceiling on both |
| P5 cold start (pod deleted): first correct `ls-remote` / first complete clone (run 1 · run 2) | 119 s / 138 s · 30 s / 48 s; the clone itself 18.0 · 17.6 s | run 1: the first clone **refused** (503 + Retry-After, which stock git does not retry) · run 2: 0 s / 20 s, no refusal; the clone itself 19.7 s | walgit on refs (X14); on the clone itself a draw; forge's restore is the live pack set, 119 s with 18.6 GB in the bucket and 30 s after the sweep |
| P11 undo after a force-push | nothing: the snapshot is one CAS'd object (X15) | recovered, by hand: `wal materialize --at-seq 1444` (the sequence before the force-push) gives a bare repository whose `agent/p11` ref is the pre-force tip e8445115, its two commits and tree complete; 17 of that copy's 50 refs dangle, because the copy holds the log window's packs, not the whole repository. The rig read the wrong path in both runs (below) | walgit by construction; the rig's own read of its output failed twice, see below |
| P10 the bucket cut off from the pod for 90 s (recorded, not scored) | run 2: reads served at +5 s from the local clone; **still ready at 90 s** (this image predates X13); a push failed after 19.6 s; recovered with no restart when the bucket returned | run 2: the read at +5 s hung to the rig's 60 s timeout (every read is a conditional GET on the manifest); **still ready at 90 s**; the push hung to its 120 s timeout; recovered with no restart when the bucket returned | as pre-registered: forge's holder had no term (X13, built the same day); walgit's read path shares the bucket's fate |

### The bucket at the end of both runs

| | forge | walgit |
|---|---|---|
| objects under the prefix | 462 | 3,064 |
| bytes | 40.4 GB | 15.5 GB |
| the largest packs | 6977, 5953, 5953, 5864, 5672 MiB: five full repacks of one repository | 4416 MiB, one geometric fold; then 1344, 1168, 1024, 1024 |
| uploaded during run 1 (CloudWatch, one 2694 s window) | 39.3 GB in 4,442 requests (1814 PUT, 983 GET, 1581 HEAD) | 15.8 GB in 14,045 requests (5386 PUT, 5098 GET, 3557 HEAD) |
| run 2 (908 s: one 1 GiB push, eight clones, a cold start, an undo, an outage) | uploaded 1.07 GB; **downloaded 11.84 GB** in 1,435 GETs | uploaded 1.07 GB; downloaded 2.30 GB in 378 GETs |

Run 1 pushed about 7.2 GB of content (five of 1 GiB, five of 64 MiB,
48 of 8 MiB, 64 tiny, one 1 GiB branch for P7). Forge uploaded 5.5×
that and walgit 2.2×; walgit's excess is its own folds, forge's is
the full repack every 24 packs. Run 2's download column is X14 in
bytes: forge's cold start in P5 fetched the whole live pack set, about
11 GB for a 1 GiB clone, where walgit fetched the packs the clone and
the undo needed (`results/work-20260905-224804/cw-summary.txt`).

### Under §9's rule

walgit passed every leg forge passed, of the six legs run (P3, P6 and
P8 were not run), and beat forge beyond the rep-to-rep spread on P2,
P5 and P9, and on P1 at 1 KiB and 64 MiB. It did not beat forge on P1
at 1 GiB, where the ranges are disjoint in forge's favour, and its P5
pass needed a client that retries a 503, which stock git is not. So
the rule's letter is met by neither side. What the log settles is
where forge loses and why: P9 and P2 by an order of magnitude from one
cause, X18, the full repack every 24 packs (33× the bytes, an 816 s
push, five full packs in the bucket); P1 at 1 KiB by 0.48 s from X20,
the 400 ms window a lone push pays; P5 on refs from X14. Each is a
joint with a number here waiting to move, none is the shape, and the
decision on "walgit behind the door" is taken from the re-match after
X18 and X20 are built (§9 of the simplification note).

### What the runs found in the rig

- **P5, walgit, run 1:** the first clone after the cold start was
  refused with 503 + Retry-After while walgit materialised its packs.
  Stock git does not retry. The leg now retries every 5 s, counts the
  refusals and reports the time to the first clone that COMPLETES.
- **P10, run 1:** busybox's `timeout` on a script with no shebang
  answers rc=126, so "a read during the outage → rc=126" measured the
  rig; and forge was cut while mid-repack, with readiness already
  withdrawn for that, so "not ready at 16 s" said nothing. The leg now
  runs the probes through `sh` and requires a serving, Ready arm
  before the cut.
- **P11, walgit, run 1:** the sequence to recover was read after the
  force-push (one too late), and `wal materialize` wrote into the
  container's `/tmp`, which is its writable layer on the node's 8 GiB
  root: the node went under DiskPressure and **both arms' pods were
  evicted** (exit 137). The sequence is now read before the push and
  the output goes to the cache emptyDir.
- **P11, walgit, both runs:** the rig looked for the materialised
  repository at `<out>`, where walgit writes `<out>/acme/<repo>.git`,
  and read nothing both times. The recovery was verified by hand after
  run 2 (above); the leg now reads the right path.
- **`windows.txt`:** the rig's `window()` dropped the end time of each
  window; run 1's windows were reconstructed from the log (the
  original is kept as `windows.txt.orig`). Fixed after run 2.
- **The image import:** walgit's 949 MB image, imported into
  containerd on the node's 8 GiB root, put the node under DiskPressure
  for the kubelet's five-minute transition. Import once; apply the
  rendered Deployment by hand if the deploy's import pod is evicted.

Not run, still: P3 (the kill inside a multipart upload), P6 (a roll
mid-push), P8 (the stall and concurrency legs), the 1,000-clone storm
with bundle-uri on both arms.
