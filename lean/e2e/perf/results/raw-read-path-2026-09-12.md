# The raw HTTP read path — built, and measured against S3 — 2026-09-12

`FLINT_SYNC_RAW_READS=true` (CRD `rawReads: true`) routes every GET and
HEAD through `crates/flint-store/src/rawread.rs`: pooled HTTP/1.1
keep-alive connections (hyper), SigV4 signed by hand with the SDK's own
cached credential chain, the SDK's timeouts, retry budget and error
table reproduced, and no SDK per-request machinery. Writes stay on the
SDK. Integrity does not depend on the client: every reader verifies
each fetch against the manifest's CRC-64 (`d1539fb9`, `328bbf5d`).

## Why

Section 4 of `fanout-ceiling-2026-09-12.md` measured what a raw client
would buy on the loopback rig: ~50 µs of CPU per 8 KiB object against
the SDK path's ~165 µs, the SDK's cost diffuse (interceptors, config
bag, runtime components, `Arc` traffic, allocation) with no knob for
it. On the cluster the syncer's 350-520 µs per file was the client's
whole cost once the fan-out was driven from several threads; on Ozone
the same 470-510 µs. The user's direction (2026-09-12): build it.

## What it is

- `RawReader`: `head`, `get_whole`, `get_range_segments`, versioned
  variants via `?versionId=`. Virtual-hosted addressing on real S3
  (path-style for a dotted bucket, as the SDK), path-style on an
  endpoint override, `http` or `https` as given.
- Signing per attempt (`x-amz-date` must be fresh after a backoff),
  S3's settings: single percent-encoding, no path normalisation,
  `x-amz-content-sha256` of the empty body, session token included.
- Retries: three attempts, full-jitter exponential backoff (1 s base,
  20 s cap), on 5xx, 429, `RequestTimeout`, transport errors, a
  first-byte timeout (10 s) and a body stall (30 s). S3's `SlowDown`
  under a hot prefix is exactly this path.
- Errors: the store's own `classify` table (412 → PreconditionFailed,
  404 → NotFound, 401/403 and the named codes → Auth), the S3 error
  document's `<Code>` read when there is one.
- A `200` to a ranged GET is refused (a proxy dropped the header) rather
  than written at the wrong offset.
- Same TLS provider as the SDK in this process (aws-lc-rs), native
  roots.

## Tests

Ten tests against an in-process hyper server that records every
request and answers from a script: request shape (path encoding,
`host`, `if-match`, `range`, `x-amz-checksum-mode`, the empty-body
sha256, the session token, a well-formed `Authorization` with the
signed-header set), metadata parsed back (etag, size, `x-amz-meta-*`
stripped, version, checksum header, storage class, last-modified),
the status table, retry on 5xx then success, give-up after the budget,
first-byte timeout, transport error, keep-alive reuse (one connection
for twenty GETs), addressing. The signature is cross-checked by
re-signing the recorded request with the same inputs and the
`x-amz-date` it carried.

Mutation controls, each failing exactly the test that pins it: request
headers added BEHIND the signature (the classic SignatureDoesNotMatch);
5xx treated as final; pooling off; a 200 to a ranged GET accepted;
`x-amz-meta-` not stripped; the first-byte timeout not applied. (A
seventh — leaving `host` out of the signed set — was a no-op mutation:
aws-sigv4 derives `host` from the URI, so the signature is the same.)
Lean battery 212/212 with the store change.

## Measured

### Loopback: the client with no store in the way

6-vCPU Lima VM (aarch64), `fakes3` in the VM on 127.0.0.1 (plain HTTP,
no TLS), 20,000 × 8 KiB published into it with the SDK path, checkout
into tmpfs, `fetch_drivers` auto, n=3 interleaved, both arms
byte-identical to the corpus (sha256 over 20,000 files, 0 diff lines).
CPU is the syncer process's exact user+sys per file (the shell's own
accounting — a `/proc` sampler that keeps its last reading undercounted
a 0.4 s run by 6x, and those numbers are not quoted).

| arm | fanout 128 | fanout 256 | fanout 512 | CPU/file |
|---|---:|---:|---:|---:|
| sdk | 25,348-29,282 | 24,600-30,349 | 26,666-29,717 | 130-161 µs |
| raw | 45,454-48,899 | 40,322-47,961 | 38,022-45,454 | 76-98 µs |

1.5-1.7x the files per second, 55-65 µs less CPU per file. The
per-file cost that remains on the raw arm — tmp+rename+fsync-free
write, the CRC, the manifest bookkeeping, the allocator — is the same
on both arms, so the SDK's per-request machinery is the whole
difference: roughly 60 µs of the SDK arm's 130-160.

A rig note that cost a rerun: the first pass ran while the host was
compiling two cross-builds, and reps 2-3 of BOTH arms halved and
scattered (sdk 7,117-25,094). A VM measurement is a host measurement;
the host has to be idle.

### Real S3 with TLS: one spot i4i.xlarge, us-west-1a

`lean/e2e/perf/raw-read-drill.sh` on the node over SSM, bucket
`flint-raw-drill` (fresh that hour), tree on the instance-store NVMe,
`drop_caches` before every run, n=3 with the arms interleaved per rep.
CPU is the syncer process's exact user+sys. Guards passed: one byte
total across every row, `ranged=0` on small and `8` on big, no failed
checkout, and each arm's tree sha256-identical to the seed (0 diff
lines, small and big).

**Small, 20,000 × 8 KiB:**

| arm | fanout 128 | fanout 256 | fanout 512 | CPU/file | sockets peak |
|---|---:|---:|---:|---:|---:|
| sdk | 5,028-5,095 | 7,521-8,009 | 7,809-8,288 | 385-429 µs | 179 / 296 / 658 |
| raw | 5,076-5,200 | 9,157-9,402 | 11,806-12,224 | 244-274 µs | 173 / 347 / 647 |

At fanout 128 both arms sit on S3's first-byte latency (128 in flight
over ~24 ms ≈ 5,300/s) and are equal. At 256 the raw arm is 15-25%
ahead; at 512 it is 42-56% ahead — 11.8-12.2k files/s against the
SDK's 7.8-8.3k on the same four vCPUs — because at 512 both arms are
CPU-bound (sdk 7.7-8.3 s of CPU over a 2.4 s fetch = 3.2 cores; raw
4.9 s over 1.65 s = 3.0 cores) and the raw path spends 35-40% less
per file. The prefix rate did not bind at 12k/s on this bucket.

**Big, 8 × 64 MiB (the ranged arm, `get_range_segments` with
`If-Match` per range), fanout 128:** sdk 784-893 MiB/s at
4,023-4,062 µs/MiB; raw 816-882 MiB/s at 3,945-3,984 µs/MiB. No
difference worth a sentence — the cost there is bytes (TLS, copy,
CRC, the write), not requests — and the raw path is exactly as fast
and as correct on it.

**Warm, raw at fanout 256 for six minutes (93 back-to-back
checkouts):** 8,795-9,596 files/s, flat from the first run to the
last. The bucket had already been through ~40 checkouts of the same
prefix that hour; whatever splitting S3 did, it did before this leg,
and 256 in flight is latency-bound at ~9.5k either way.

**The defect only the cluster could find.** The first pass ran the
raw arm at 170-690 files/s with three threads and 0.02 s of worker
CPU — the process was WAITING. `SdkConfig::credentials_provider()` is
the bare chain; the SDK's caching lives in the identity layer this
path bypasses, and on EC2 the chain is the instance metadata service,
so every GET paid an IMDS round trip and twenty thousand of them got
IMDS throttled hard enough that the SDK arm's own startup then failed
("dispatch failure", "the credential provider was not enabled"). A
single-flight cache (refresh five minutes ahead of expiry, never more
than once per ten seconds) fixed it; two tests pin it (a burst of
fifty reads asks the chain once; a near-expiry credential refreshes
but not per request) and the mutation that disables the cache fails
both. The rows from that pass are kept in
`results/raw-read/node-small-imds-per-request-DEFECT.log`.

**Sampling note.** The `/proc` per-thread sampler kept its last
reading before exit and undercounted short runs (twice it read 0.00
for every worker on a 1.5 s raw run); CPU is now the shell's exact
accounting of the process, and the sampled rows are kept beside the
exact ones for the record.

## The S3 prefix limit, for testing

There is no knob. S3 general-purpose buckets serve 5,500 GET/HEAD and
3,500 PUT/COPY/POST/DELETE per second **per partitioned prefix**, and a
fresh bucket is one partition. S3 splits partitions on its own under
sustained load (tens of minutes, with 503 `SlowDown` in the meantime —
the SDK's and the raw path's retry budget absorb those), which is why
the runct numbers "grew with load" from ~3k to ~6.5k/s and why every
cold-bucket measurement here is capped the same for both arms. What
can be done, in order of usefulness for a client measurement:

1. **Measure CPU per file, not files/s, on a cold bucket.** The wall
   caps both arms identically; the client's cost per file is the thing
   the raw path changes, and it is visible under the cap.
2. **Warm the bucket.** The `warm` mode runs back-to-back checkouts on
   one prefix and prints files/s per run; S3 re-partitions when the
   pressure holds. Costs time, not a ticket.
3. **Ask AWS Support to pre-partition** a bucket for a known key
   layout. Real, used for production launches, needs a case.
4. **S3 Express One Zone** (directory buckets): 200,000 reads/s per
   bucket by default (2M with a limit increase), no per-prefix
   throttling, single-digit-ms latency, conditional writes supported
   (`If-None-Match`, `If-Match` on PutObject, CopyObject and
   CompleteMultipartUpload), user metadata supported, **no
   versioning** (cadence mode only, like Ozone), one AZ. The SDK path
   works on it unchanged (session auth is automatic). The raw path
   needs one addition: `CreateSession` and signing with the session
   credentials and `x-amz-s3session-token`, service name `s3express`
   — ~50 lines. That is the throttle-free S3 arm if one is wanted.
5. **Spread keys across prefixes** helps S3 split faster and raises
   the ceiling once split, but a cold bucket is still one partition
   at t=0; it is a layout decision (`files/<shard>/…`), not a test
   knob.

Until one of 3-4 is in place, the loopback rig is the throttle-free
client measurement and the cluster rig is the CPU-per-file one.

## Teardown

Same day. Node `i-01b5c07d49c4edd90` terminated, bucket
`flint-raw-drill` emptied and deleted, inline policy removed from
`TroveSSMInstanceProfile`; verified under trove-admin: 0 instances,
0 buckets, no inline policies, 0 spot requests. The Lima VM is back
to 2 vCPUs / 2 GiB, stopped. Result files: `results/raw-read/`
(TSVs, node and VM logs), drill `lean/e2e/perf/raw-read-drill.sh`,
VM A/B `lean/e2e/perf/raw-read-vm-ab.sh`.
