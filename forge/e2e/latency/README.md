# forge latency leg — round trips, measured

Every other forge drill runs against loopback MinIO, where a round trip
is about a millisecond and the only thing a leg can see is the request
COUNT. Two changes shipped on count evidence alone: the concurrent
sibling upload (`62775105` — "the concurrency win is structural rather
than measured") and the restore fan-out (the commit that adds this
leg). This leg puts a real round trip in front of the same MinIO —
toxiproxy on the host, a latency toxic each way — and measures both
against the one control that matters: the same binary with
`FLINT_FORGE_FANOUT=1`, which is exactly what the code did before.

    brew install toxiproxy                              # once
    (cd forge/syncer && cargo build --bins --features s3) # a plain build SKIPS the syncer
    ./run-latency.sh                                    # ~5 min; exit 0 only when every leg PASSES

## What it measures

| leg | operation | arms | prediction, stated before measuring |
|---|---|---|---|
| P0 | `GET /` through the proxy | RTT 0 vs 200 | +200 ms ± 50, and ≤ 30 ms at RTT 0 |
| P1 | `git push` of one empty commit, client wall clock | fanout 4 vs 1; 10 interleaved pushes per RTT ∈ {0, 50, 100, 200} | saving = (S − 1) × RTT for S = 3 siblings; push ≈ 5 RTT after, 7 before |
| P2 | pod start to hook socket: a restore of 33 files | fanout 4 vs 1; 5 interleaved restores per RTT ∈ {0, 100, 200} | saving = (33 − ⌈33/4⌉) × RTT = 24 × RTT |

Arms interleave with the position CHANGING every rep; they differ only
in the knob; the RTT-0 leg is a null (the knob alone must move
nothing); and the saving must scale with the RTT or it is not a
round-trip saving. INCONCLUSIVE is not PASS — the shell exits 2 on any.

## The numbers (2026-09-05, this Mac, MinIO in Docker)

`results/latency-2026-09-05.log`. Push, medians of 10, ms:

| RTT | fanout 4 | fanout 1 | saved | predicted |
|---|---|---|---|---|
| 0 | 181 | 192 | +11 (null) | 0 |
| 50 | 476 | 596 | 120 | 100 |
| 100 | 735 | 941 | 206 | 200 |
| 200 | 1236 | 1662 | 426 | 400 |

Fitted: fanout 4 = **5.1 round trips** + 225 ms; fanout 1 = **7.1** +
235 ms. The push protocol costs exactly its request count in round
trips — renew, three siblings, CAS, two derived files — with nothing
hidden (no `Expect: 100-continue`, no per-request handshake). The
225 ms intercept is git: receive-pack, index-pack, the hooks,
update-ref.

Restore of 33 files (11 packs × 3), medians of 5, ms:

| RTT | fanout 4 | fanout 1 | saved | predicted |
|---|---|---|---|---|
| 0 | 541 | 540 | −1 (null) | 0 |
| 100 | 2267 | 4872 | 2605 | 2400 |
| 200 | 3702 | 8735 | 5033 | 4800 |

Fitted: fanout 4 = **14.3 round trips** + 832 ms; fanout 1 = **38.6** +
1009 ms — 33 files plus ~5.6 fixed (claim, snapshot, list, HEAD). The
same bound covers chunks: at the design's 10 GB envelope the repository
is one pack of 1,280 chunks, and the pre-fix restore ran them in one
stream, one round trip each (`packio::fetch_all` says why the bound is
across files and chunks together).

## The control that fails

`results/latency-2026-09-05-control-cda4b21e.log`: the same leg against
the pre-fix binary (no `FLINT_FORGE_FANOUT`, sequential restore). Both
nulls pass; P1 FAILS (delta +20 ms of 200 predicted) and P2 FAILS
(+29 ms of 1300). A rig that could not see the code it exists to catch
would prove nothing about the code that replaced it.

## What the rig cannot see

- **Connection setup.** toxiproxy terminates TCP: the handshake is
  local and only data pays the toxic. Real S3 charges 2–3 round trips
  per new TLS connection. The SDK pools connections, so this understates
  every arm's first request equally and the saving not at all.
- **Bandwidth.** The pushes are empty commits and the restored files are
  a few KiB each. A 10 GB restore is bandwidth-bound; this rig says
  nothing about it. The EC2 scale drill (`forge/e2e/scale/`) is where
  that number comes from.
- **The takeover-during-restore window** (design §5, still open). A
  fanout-1 restore at RTT 200 took 8.7 s here against a 60 s window;
  reaching the window honestly needs a real backend. See
  `../largerepo/README.md`.

## Traps this leg found

- **`local a=$1 b="$a"`** expands `$a` BEFORE the builtin assigns it.
  Under `set -u` that is an unbound variable, and every restore of the
  first run died on it. One `local` per line.
- **`fanout` was a knob that did not exist.** Declared in `ForgeConfig`
  at 16, documented as bounding uploads and fetches, read NOWHERE: the
  batch used a hard-coded 4 and the restore ran one file at a time. A
  config field nothing reads is documentation of a lie.
- **`with_extension("part")`** gave `pack-X.pack` and `pack-X.idx` the
  same temporary. Harmless while fetches were sequential; a corrupted
  index the moment they were not. Found by reading before the concurrent
  path was written, which is the only time it could have been found
  cheaply.
- **The background build in the first smoke run.** A cargo build on the
  same machine put 164 ms of skew into a 3-sample null leg. Measure on a
  quiet machine; the null leg is what tells you when you did not.
