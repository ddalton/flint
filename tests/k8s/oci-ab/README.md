# Five-arm cold-start A/B — oci-image-serving-design.md §9.4, phase 1

The gate for any headline ratio in the OCI image-serving design. The rung-1
pilot (tests/lima/oci-pilot/) passed §9.3 on loopback; this campaign puts the
arms on a real network. Kill conditions K1–K5 (§9.5) are pre-registered — a
loss here retreats to the cheaper arm, per the design.

## Phase 1 scope (this kit)

N=1 attribution legs + a 3-node mini-storm. The N∈{8,32} storm legs are
phase 2 (scale-out later; kind/loopback reps are never quoted).

| arm | what | phase 1 |
|---|---|---|
| A1 | baseline containerd pull (registry-on-flint) | yes |
| A2 | SOCI parallel-pull mode, same registry | stretch (config swap + daemon restart) |
| A3 | SOCI lazy vs registry-s3 (S3 driver, redirect → presigned S3 ranges) | yes |
| A4 | rung-1: EROFS blob on the flint RWX mount, node loop-mount (pilot P3, real network) | yes |
| A5 | SOCI lazy vs registry-flint — **endpoint swap only vs A3: the clean backend attribution** | yes |

## Cluster

- trove all-spot (CP included): `aws-live-allspot.fish create <name> 4`
  with i4i.xlarge workers, single AZ us-west-1. 1 CP + 4 workers:
  **3 DS nodes + 1 dedicated client node** (keeps serving CPU off the
  measured node).
- flint chart 1.43.0, pNFS server fleet: values-oci-ab.yaml.
- **Cilium WireGuard encryption OFF for the campaign** (runbb: WG collapses
  node-pair traffic into one flow, ~918 MiB/s cap) — recorded as an arm
  property; S3 traffic never pays WG, so leaving it on would tax only the
  flint arms.
- Est. cost: 5 × ~$0.10–0.12/node-hr spot ≈ **$0.55/hr; a full campaign day
  ≈ $4–5** plus pennies of S3 requests (in-region transfer free). Single-AZ
  is mandatory (cross-AZ $0.02/GB kills it).

## Runtime inputs (same shape as tests/cloud/lite-tier-l4.sh)

- `BUCKET` — existing S3 bucket (rolesanywhere cannot CreateBucket); plus a
  bucket-scoped access key in `S3_KEY_ID`/`S3_SECRET` for the registry-s3
  Secret. us-west-1.
- Image: python:3.12 linux/amd64, pushed identically to both registries;
  SOCI index built once, pushed to both; EROFS blob (pinned 5.4 profile)
  written to the RWX PVC for A4.

## Run order

1. Provision + verify spot + disk-init workers (PCI 0000:00:1f.0, never the
   /dev name) + label the client node `oci-ab/role=client`.
2. `helm upgrade --install` flint with values-oci-ab.yaml; wait DS fleet.
3. Disable WG, restart cilium ds; record.
4. `kubectl apply -f registries.yaml` (+ the S3 Secret); push image + SOCI
   index to both registries; stage the EROFS blob.
5. `node-soci-setup.sh <client-node>` via SSM: containerd 1.7 config
   (`disable_snapshot_annotations = false`, soci proxy plugin), soci
   0.11.1, restart containerd.
6. **`drive-ab.sh preflight`** — clock, client label, instance-id, fleet
   settle, digest reference, and whether the MDS is at debug level (if it is
   not, the stripe-width gate is blind and no run can be certified). Cheap,
   and it fails before any cluster time is spent.
7. `drive-ab.sh run` — interleaved reps, guards enforced, NDJSON out.
8. `drive-ab.sh warm-leg` and `drive-ab.sh broken-lazy-leg` — the two
   falsifiability controls. Run them; a rig whose controls were never
   exercised has not been shown to measure anything.
9. `drive-ab.sh score <results.ndjson>` — paired per-rep ratios.

Env: `KC`, **`CLUSTER`** (the trove cluster name — the EC2 Name tag is
`trove/<cluster>/<node>`; there is no providerID without a
cloud-controller-manager), `BUCKET`, `S3_KEY_ID`, `S3_SECRET`, `REPS`,
`AWS_PROFILE=rolesanywhere`.

## Discipline (house rules, §9.4) — ENFORCED, not aspirational

Every rule below is a guard in `drive-ab.sh` that can VOID a rep, and every
guard has a negative leg in `rig-selftest.sh` that violates exactly one
precondition and asserts the specific void reason. **A guard that has never
been seen to fail is not a guard** — v1 recorded the attribution counters and
the loadavg in its output and then used neither.

| guard | voids when | why |
|---|---|---|
| G-CLOCK | `date +%s%N` is not integer nanoseconds | a BSD date without %N poisons every arithmetic silently |
| G-SETTLE | DS fleet not `N/N`, or recent registration rejections | the confound that voided the first runbx GREEN: a push 30 s after an image swap is two variables |
| G-COLD | the prune left images behind, or soci is not active | v1 printed `cold-ok` unconditionally, so a failed prune was measured as a cold pull |
| G-IDLE | loadavg/vCPU > `MAX_LOADAVG` (1.5), checked per ARM | saturation compresses every ratio toward 1.0 — the failure that makes a null look real |
| G-SSM | the measured command did not reach Status=Success | v1 returned stdout regardless of status, so a FAILED pull scored as a very FAST arm |
| G-PULL / G-RUN | non-zero rc from the node-side pull or run | same shape, one layer down |
| G-INTEG | the pulled manifest digest != the pushed one | this campaign's own finding is that the substrate can serve corrupt bytes with NFS4_OK; a perf number over corrupt bytes is not a slow result, it is not a result |
| G-ATTR | the other backend served >0 requests, or this arm's own served 0 | the README's original VOID rule; own-zero means the arm never went remote |
| G-WIDTH | `stripe-width-gate.py` says FAIL — **or INCONCLUSIVE** | see below |

Also enforced, without needing a guard:

- **Arm order rotates per rep.** A fixed order aliases order effects
  (registry page cache, connection reuse) onto arm identity; interleaving is
  only interleaving if the position changes.
- **Timing is taken ON the node.** v1's host-side stopwatch wrapped SSM
  submit plus a 2 s-granularity poll loop — larger than the lazy arm's entire
  signal, so quantization alone could have manufactured the result.
- **Output is NDJSON, append-only, PID-stamped.** A rep that dies leaves
  every earlier rep readable, and two runs in the same second cannot merge
  into one file (they did; `rig-selftest.sh` caught it).
- **`score` refuses**, rather than quoting a mean over one survivor, below
  `MIN_VALID_REPS` paired reps — and it prints every void reason.

### INCONCLUSIVE is not PASS

`stripe-width-gate.py` exits 0 PASS / 1 FAIL / **2 INCONCLUSIVE**, and the
third state is the point. The 1.43.0 stripe-width defect only manifests on
**bounded** LAYOUTGETs, so a log containing only whole-file grants cannot
exonerate anything, and at INFO the lines do not exist at all. Both are
blindness, not health.

`drive-ab.sh` records the verdict as a row in the results file and **`score`
withholds the headline ratio unless that verdict is PASS** — the per-rep
numbers still print, stamped `[uncertified]`, because they remain useful as
diagnostics and useless as a result. Treating exit != 1 as a pass would
reproduce this campaign's signature failure ("the check passed because the
question was never asked") on the one gate built to prevent it.

This is the same rule as G-SETTLE and as the four vacuity traps in
`FINDINGS.md`: **before trusting a clean result, ask whether the check could
have come back dirty.**

## Self-test

    ./rig-selftest.sh          # ~16 s, no cluster, no AWS, no network

Fakes `kubectl` and `aws` on PATH and drives the real `drive-ab.sh`. 31 legs:
one anchor (a clean run must come back clean — without it, a rig that voided
unconditionally would pass every negative leg and prove nothing), then one
negative leg per guard, the three-state gate mapping, and the `score`
refusal rules. Run it after any change to the rig.
