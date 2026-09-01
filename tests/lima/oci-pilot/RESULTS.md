# rung-1 pilot results — 2026-09-01

Rig for `docs/plans/oci-image-serving-design.md` §9.3. Raw data:
`results-20260901-090235.json`. Rig: `pilot-host.sh` / `pilot-vm.sh`.

## Verdict: PASS — the file-layout lazy tier (rung 1) is viable.

All six pre-registered criteria green: P3 fault p99 < 5 ms (worst block size
3.03 ms), P3 ready ≤ 1.5× P2 (measured 1.09×), G1 faults-went-remote, G2
falsifiability, G3 zero EIO, G4 digest identity.

## Numbers (medians over 5 interleaved reps)

| arm | ready-time | what it is |
|---|---|---|
| P1 baseline pull+start | 17.2 s | tar.gz over flint mount + gunzip + untar + first-exec (n=4; rep 2 ENOSPC, rig not product) |
| P2 lazy EROFS, local blob | 0.73 s | the format term |
| P3 lazy EROFS over flint | 0.80 s | format + remote-fault tax |

- **Format win P1/P2 = 23.6×** — deleting pull+unpack is the dominant term,
  now measured, consistent with the design's piece-1 claim.
- **Flint remote tax P3/P2 = 1.09×** — 67 ms median added by serving faults
  over the flint NFS loopback.
- **Per-fault floor (fio qd1 O_DIRECT randread on the blob over the mount):**
  p99 4k = 3.03 ms, 64k = 0.99 ms, 128k = 0.20 ms. All under the 5 ms
  criterion; against SOCI's ~4.6 ms warm per-fetch [E], directionally better
  — but see caveat 1 before quoting a ratio.
- Cold working set: 19.2 MB compressed / 529–536 READ ops per exec of a
  python-with-stdlib first-exec; **warm rerun = exactly 0 remote ops, all 5
  reps** — the falsifiability oracle at its cleanest.

## Caveats — read before quoting any number upstream

1. **Loopback, warm-server.** Client and server share one lima VM; no real
   network RTT, and the server's page cache held the blob (the fio NFS arm
   beat the local-disk arm at 64k/128k because loopback+page-cache beats
   virtio media). These are per-fault *path* numbers in the warm-hub steady
   state, not cold-cluster numbers. The five-arm A/B on real nodes (§9.4)
   remains the gate for any headline ratio.
2. **Contended VM**: loadavg 2.6–3.5 of 2 vCPUs during reps (own P1 pipeline
   + a concurrent session). Paired same-rep ratios stand; absolutes are soft.
3. **Image ~1.1 GB uncompressed** (python:3.12), not multi-GB — VM disk cap.
4. **P1 carries no registry HTTP** (cp over the mount) — a lower bound for
   the baseline, i.e. conservative *against* the lazy arms.
5. **Server = standalone flint-nfs-server v1.43.0** (binary built 4 min after
   the release commit), not the striped pNFS fleet — right for per-fault
   path, says nothing about fan-out; storm legs need a real cluster.
6. **Scoring correction, recorded**: first scoring failed G1/G2 on harness
   thresholds encoding wrong models (20 MB uncompressed working-set floor vs
   19.2 MB compressed truth; a 5× total-ready collapse that ignored the fixed
   losetup+mount cost). The oracles themselves (remote-op deltas; warm
   zero-ops) were unambiguous in the raw data; thresholds fixed in
   `pilot-host.sh` with comments, same raw data re-scored.

## What this decides / does not decide

Decides: rung 1 survives its viability gate — the §9.3 "fail ⇒ file-layout
tier not viable" branch was NOT taken; the pilot rig and its guards exist and
are rerunnable; the format win is measured, not asserted.

Does not decide: A3/A5-vs-A4 attribution, storm behaviour, K1–K5 for the
block lane — those need the five-arm A/B with real SOCI/nydus arms, real
network, and N ∈ {1, 8, 32} storm legs on a cluster (§9.4), which spends
money and waits for a deliberate decision.
