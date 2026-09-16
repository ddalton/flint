# MinIO does NOT enforce `If-Match` on DELETE — 2026-09-15

`minio RELEASE.2025-09-07T16-13-09Z` (quay.io/minio/minio:latest, arm64),
single node, `server /data`. The same image tag our drills pull
(`lean/e2e/minio.yaml`, `lean/cluster/minio.yaml` name `minio/minio` with
no tag — note Docker Hub now denies that pull anonymously; quay.io is the
live source).

This matters because the lean collector's DELETE carries `If-Match`
precisely because the model REFUTES the unconditional variant
(`LeanBarrierLeaseGCUnconditional`). A store that ignores the header runs
the shipped binary straight into that mutation.

## On the wire (AWS CLI control, aws-cli/2.35.1)

| surface | result |
|---|---|
| GetObject `If-Match` stale | **412 PreconditionFailed** |
| PutObject `If-None-Match: *` over an existing key | **412 PreconditionFailed** |
| DeleteObject `If-Match` stale | **200, and the object is GONE** |

So this is not a store that cannot do preconditions — it enforces them on
GET and on PUT. It ignores them on DELETE specifically.

The header really was sent: the signed canonical request carries
`if-match:"...dead"` and lists it in SignedHeaders
(`host;if-match;x-amz-content-sha256;x-amz-date`). Without that check the
control would only have tested the CLI.

## Through our own path

    flint-sync probe-conditional
    FAIL (DELETE): DELETE If-Match on a STALE etag removed the object —
    this store ignores the condition, so a garbage collector would delete
    another writer's upload

## The mitigation, end to end (first run against a store that fails)

`conformance.rs` turned the collector off, and the syncer kept working:

1. checkout and barrier print the give-way warning and publish normally;
2. `barrier seq=Some(2) up=0 del=0 parked=0 consumed=0 **leaked=1**`
   plus `left 1 object(s) in the bucket that this barrier retired`;
3. the delete REACHED the boundary — the manifest does not cite
   `src/x.txt`;
4. the object is still at `ws1/files/src/x.txt` (`hello from minio`);
5. a FRESH checkout materializes **0** files — the leaked object is
   served to nobody.

Storage growth, not a loss, which is what §2 of `lean/SAFETY.md` claims.
The probe leaves nothing behind: no `…/probe/conditional-*` keys remain.

## What this means for the drill fleet

Every MinIO-backed leg has been running the refuted collector. With two
writers, a peer's lease-free upload landing between the GC's HEAD and its
DELETE could be collected and its citation left dangling — the exact race
`hostlegs.sh` H1-H3 exist to open, which is why those legs run on real S3.
Single-writer legs are unaffected (nothing else uploads), and the host
legs' verdicts stand.

From this commit those legs get the collector OFF instead, so a delete
leaks rather than races. `run-chaos.sh`'s "the object is gone after the
second barrier" assertion is updated to expect that on a store that
fails the probe.
