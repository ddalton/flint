# 2026-10-02 — `identity.mode: stsSecret`, `workers.maxPerNode`, repeated `MounterDead`: kind on the box, then a real bucket with real STS sessions

The node half of awc-docs PR #136 built into flint-s3-csi
(`docs/plans/passthrough-sts-secret-mode.md`), verified on the Linux box:
first on the kind rig (RustFS, the rig's static key with a synthetic
expiration), then — because RustFS enforces neither an expiration nor a
token — on the SAME kind cluster against a real S3 bucket with real
15-minute STS sessions (`aws-sts-secret.sh` R1).

- Box: `ddalton@10.0.0.249`, tree `~/flint-sts-2026-10-02` (a bundle of
  `520ef8d7` plus the change under test), kind `flint-sts` (two
  v1.34.0 nodes, private kubeconfig; another session's `flint-step6d`
  ran beside it, untouched), images `dilipdalton/flint-s3-{csi,worker}:sts1002`
  built locally from the tree, never pushed.
- Unit tests (box, native): `cargo test -p spdk-csi-driver --lib -- s3csi:: passthrough:: lean_operator::boundary`
  → **116 passed, 0 failed**; by name
  (`sts_ identity_mode sharing_decision cooperative`) → **23 passed**.
- AWS (user-approved, torn down after): bucket `flint-s3csi-sts-20261002`
  (us-west-1), IAM user `flint-s3csi-sts-20261002` with one inline
  bucket policy and one access key; sessions from
  `sts get-session-token --duration-seconds 900`.

## Kind, run 1 — `run-legs.sh S5c S31 S32 S33 S24` (14:22–14:45Z): 52 ok, 2 bad

| Leg | ok | bad | What it showed |
|---|---|---|---|
| S5c | 0 | 1 | Ran 5 s after setup; `reader-static` still Pending. Timing, not the arm: the pod was Running a minute later, and the re-run below is 2/2. |
| S31 | 20 | 1 | The stsSecret mount reads; the door serves `creds.json`; mount-s3's env points at the door and carries no secret; generation 2 reached the door in 80 s with a `CredentialReplaced` event; a lower generation refused and said; a 60 s candidate refused; the SAME generation re-offered with life installed (a refusal does not poison it); a foreign `serviceAccount` envelope refused, a matching envelope installed; an unknown key refused BY NAME; the refusal said once, not again a republish later; the same mount-s3 pid (15) through four generations — no remount. **The 1 bad:** "mount-s3 never fetched generation 2 within 360 s; the door served generation 1 five times" — all five at startup. The mount was IDLE: mount-s3 (the CRT) fetches credentials lazily, when a request needs signing after its refresh point, and the leg had read the file once. A test-design flaw, fixed in run 2 by a reader every 5 s. |
| S32 | 3 | 0 | Worker killed at the runtime; the read fails; **2 `MounterDead` events within 130 s**; the text says it repeats. |
| S33 | 6 | 0 | Six live workers on the node; `workers.maxPerNode=6` rolled; the seventh publish refused with `WorkerCapacity` ("already runs 6 … maxPerNode is 6 … kubelet retries"); the pod stays unmounted; kubelet's `FailedMount` carries the message; the ceiling lifted → the same pod mounts. |
| S24 | 23 | 0 | Sharing regression control: two read-only members share one worker, a third uid gets its own, first out leaves it up, last brings it down. |

Log: `kind-legs-run1.log`.

## Kind, run 2 — `run-legs.sh S31` then `S5c` (14:46–14:57Z): 21 ok + 2 ok, 0 bad

S31 with a reader every 5 s for the leg's life: everything above, and
**"mount-s3 fetched generation 2 from the door 213 s BEFORE generation 1
expired (waited 0 s)"** — the fetch was already in the door log when the
replacement was observed to land (80 s after the Secret was written, of
generation 1's 300 s). The consumer's side of the reload channel, seen
for the first time (the rotation soak P8 had sampled the file the plugin
wrote). Log: `kind-legs-run2-and-real.log` (first section).

## Real bucket, real sessions — `SUITE=aws-sts-secret.sh run-legs.sh R1` (14:58–15:30Z): 13 ok, 0 bad

The same kind cluster, the RustFS rig torn down and `run-s3csi.sh setup`
run with `STORE=s3` against `flint-s3csi-sts-20261002` (us-west-1): the
seed job, broker Secret and fixtures on the real bucket; the CR
`datasets-sts` rewritten to it. Two pods under that one CR, one
dimension apart: both start on the same real 900 s session (an `ASIA…`
key with a token, minted by `sts get-session-token`); `reader-sts`'s
Secret is rotated — a new session and a higher generation at t0+420,
+900 and +1380 s — and `reader-sts-bad`'s is left alone. A reader every
5 s in each for 1900 s.

| | reads | errors | first failure |
|---|---|---|---|
| rotated (`reader-sts`, 4 sessions) | 355 | **0** | none — across the expiries of sessions 1, 2 and 3 (15:13:46, 15:20:52, 15:28:54Z) |
| unrotated (`reader-sts-bad`, session 1 only) | 361 | 192 | **1 s after its session expired** (15:13:47Z), and never recovered |

The known-bad is what makes the zero meaningful: the store enforces
expiry to the second, and a mounter whose door holds a dead credential
has nothing else to fall back on.

The consumer's side, from the rotated worker's door log (one line per
fetch, kubelet-stamped):

| generation | written at | reached the door | FETCHED by mount-s3 | before its predecessor expired |
|---|---|---|---|---|
| 2 (expires 15:20:52) | t0+420 s | +10 s | 164 s after it landed | **299 s** |
| 3 (expires 15:28:54) | t0+900 s | +10 s | 109 s after it landed | **298 s** |
| 4 (expires 15:36:56) | t0+1380 s | +30 s | 89 s after it landed | **297 s** |

Each fetched exactly once. **The CRT asks the door again 300 s before
the credential it holds expires, once, and takes whatever is there; once
a credential has expired it asks on every request** — the unrotated
worker's door served its one dead expiration 361 times, one per read.
Four distinct AccessKeyIds landed in the rotated pod's `creds.json`;
three `CredentialReplaced` events on the pod.

What this fixes in the design note: "offer the next generation at least
four minutes before expiry" becomes **seven**: the replacement must be
INSTALLED by T−300 s, and the republish that installs it can be up to
~90 s away, over the 120 s floor.

Teardown: pods and Secrets deleted by the leg; `run-s3csi.sh teardown`;
the kind cluster deleted; bucket emptied and deleted, access key, inline
policy and user deleted; verified by the UNFILTERED listings afterwards
(0 buckets in the account; `get-user` → NoSuchEntity; 0 users with the
drill prefix; 0 non-terminated instances in us-west-1). Both copies of
the key file removed.

## Files

- `kind-legs-run1.log` — run 1, from the rig's setup to the roster line
- `kind-legs-run2-and-real.log` — the S31 re-run, S5c, the teardown, setup on the real bucket, R1, the teardown
