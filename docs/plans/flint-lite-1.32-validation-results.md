# Validating 1.31.0 on a real cluster — and the bug that found

**Cluster `runbu`** (all-spot, CP + 2 × `i4i.large`, us-west-1c),
bucket `flint-131-drill-20260820` (versioned, real S3), 2026-08-20.
Every leg below has an oracle that can fail and an anti-vacuity guard
independent of that oracle.

## Result

**All seven legs PASS.** The three fixes 1.31.0 shipped are now
cluster-proven rather than unit-proven, on the same infrastructure that
found the defects.

| Leg | Claim | Result | Anti-vacuity |
|---|---|---|---|
| **V1** | a stale `dev` no longer empties the manifest | **PASS** — 13 rows injected with an impossible `dev`, manifest held at **14 entries**, `beyondRpo` 0, seq advanced 5→6 | hub logged `re-homed 13 generation row(s)` — **13 = 13**, so the injection landed |
| **V2** | the clean epoch release lands | **PASS** — `released` **false→true** across a clean drain, epoch **preserved at 2** (the old path DELETED the cell, resetting numbering) | the same cell read `released: false` while the hub was live |
| **V3** | an identity-changing wake no longer pays the lease | **PASS** — released-cell wake **17s**, `epochTakeovers=0` | a foreign, not-released, freshly-renewed cell still took **80s** with a logged `TAKEOVER` — the fast path did NOT become "never wait" |
| **V4** | expired leases decay without inbound traffic | **PASS** — `activeLeases` 1 → **0 at t+110s** under a real partition, sweep logged `1 expired client lease(s) retired` | a live mounted client read **1**; 1.30.0 read 1 for **770s** |
| **V5** | downloads no longer buffer whole | **PASS** — 512 MiB GET moved `VmHWM` by **0.0 MiB** | the SAME hub with the threshold raised to 1 GiB moved it **493.7 MiB** on the same payload |
| **E2** | hibernate destroys the PVC only after a verified flush | **PASS** — new PVC uid, **13/13 byte-identical** | a planted corruption made the comparator report **exactly 1** |
| **B4** | the PVC goes only after the bucket says released | **PASS** — `released:true` at **t+62s**, PVC deleted at **t+496s**, order correct | both transitions observed on ONE clock (the drill's), not compared across the bucket's and the operator's |

The phase trace for E2/B4 shows verify-then-delete doing exactly what it
claims: `Ready → IdleSuspended → **Starting** → Hibernated → PVC gone`.
That `Starting` is the operator waking the hub to verify a clean flush
before it deletes the disk.

**A spot node was reclaimed** (`runbu-aws-2`, terminated) after every leg
above had completed. It voids nothing; it was replaced and the release
validation continued on the replacement.

## What the run found — fixed in 1.32.0

### ⚠ A cold read failed its own guard (SHIPPED BUG)

**After a hibernate/DR wake, the first `GET` of every file answered 409.**
Measured: **13 of 13**, every one **200 on the immediate retry**.

The download's terminal check refuses when `change` moved under the read
— that is how a rename-over is caught. But the tier **rewrites the local
inode when it hydrates a stub**, so the hub's own hydration moved
`change` and the read failed its own guard. On the streaming path it was
worse: past a committed `200` the mismatch cannot be a clean 409 any
more, so it poisoned the body instead.

`hubfs::render_etag` already documented this hazard — but only for
`If-Match`, where it fails closed as a spurious 412. Nothing had noticed
the download's terminal guard has the same dependency, where it fails as
a mandatory retry on every cold file.

Fixed by settling hydration with a one-byte probe BEFORE the guard's
baseline is taken. A replacement landing in that window is still caught:
`fileid` and logical size must both survive it.

**Re-proven on the same cluster, same corpus, same hibernate: 13/13
first-GET 200, all byte-identical, `stubsCreated: 13`.** Then again on
the published 1.32.0 artifacts with a fresh share: **6/6 first-GET 200**,
6/6 byte-identical, woken in 20s with `epochTakeovers=0`.

### Success was reported under an `error` key

`POST /files/folder` answered `{"error":"created"}` on a 201; `PUT`,
`DELETE` and `move` did the same. Anything scanning for `error` read
every successful mutation as a failure. Now `{"status": ...}`.
**Wire-format change** on those four routes.

### Neither documented mount command worked

- The operator chart's `NOTES.txt` said `<address>:/data/exports` — a
  path inside the container, refused `NFS4ERR_NOENT`.
- The operator guide said `<status.address>:/` — which expands to
  `host:2049:/`, refused by `mount` outright.

A new user following either could not mount the share at all. Both now
show the form verified on this cluster, which also survives an
`advertiseAddress` on a non-2049 port: host before the colon, port in
`-o port=`.

The guide also never named the keys `credentialsSecretRef` must carry.
They are loaded with `envFrom`, so they must be `AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY` verbatim. **This cost a crash loop during this
very run**: the wrong key names leave the SDK with no credentials, it
falls back to the instance role, IMDS is unreachable from pods, and the
error reads `bucket <name> unreachable: dispatch failure` — which names
the bucket, not the cause.

## Observations — not fixed, recorded

- **The PVC lingers ~300s after a share reports `Hibernated`.** The disk
  is reclaimed on a LATER reconcile, and `IdleState::Hibernated`
  requeues at `REQUEUE_SETTLED` (300s). Measured 196s → 496s. The data
  is safe throughout; it is the reclaim that is late, not the flush.
- **F29 is reachable without `--force`.** A plain `kubectl set image`
  rollout on a `Recreate` Deployment holding a `flint-spdk` PVC wedged
  in `ContainerCreating` for 300s with `staging path ... is not mounted
  — restage required (F29)`. The documented recovery worked: scale to 0,
  wait for the pod to go, scale to 1 — **Running in 20s**.
- **Operator anti-affinity is `preferred`, so both replicas can land on
  one node** — observed after the node replacement. Correct default (a
  `required` rule makes single-node installs unschedulable), but it
  means the PDB's `minAvailable: 1` does not by itself buy node-failure
  tolerance.
- **`streamThresholdBytes` has no `FlintShare` field.** It is settable
  in `flint-lite-chart` but not through the operator, so
  operator-managed shares take the 8 MiB default. Deliberately not
  added here: a CRD schema bump for a knob nobody has asked for, against
  a derived CRD that is not structural.
- **The write reserve (D8) remains open** and was not re-run — it would
  re-measure a known-open defect.

## Rig notes

- The bucket + scoped IAM key are minted by
  `tests/cloud/drill-bucket-setup.sh` and torn down explicitly; nothing
  does it for you.
- `aws sso login --profile trove-admin --use-device-code` is required —
  the `rolesanywhere` identity trove provisions with has zero `s3:`
  actions and cannot create a bucket.
- Disk-init remains the standing gate on every node, replacements
  included.
