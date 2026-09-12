# Lean against Apache Ozone 2.2.1's S3 gateway — 2026-09-12

The on-prem target ([user, 2026-09-12: lean "also needs to work on-prem
against ozone instances"]) probed against a real Ozone, the day the
manifest CRC became mandatory (`d1539fb9`, `328bbf5d`). Everything below
was measured; nothing is inferred from the docs, which are wrong about
one load-bearing thing (conditional requests).

## Rig

One spot i4i.xlarge (4 vCPU, 30 GiB) in us-west-1a, AL2023, Docker
25.0.16 + Compose v5.5.1, Ozone **2.2.1**'s own `compose/ozone`
environment from the release tarball (om, scm, one datanode, s3g, recon,
httpfs — `apache/ozone-runner:20260206-2-jdk21`), s3g on
`localhost:9878`, path-style, no security (any access key is accepted).
`flint-sync` at `328bbf5d` plus two probe verbs (this commit), x86_64
musl, `fastalloc` on. Ozone's six JVMs share the four vCPUs with the
syncer, so every throughput number here is "Ozone on four vCPUs", not
the client's wall. Drill scripts: `oz-prep.sh`, `oz-up.sh`,
`oz-drill{,2,3}.sh` in `lean/e2e/perf/ozone-probe/`, driven over SSM;
the three logs are in `lean/e2e/perf/results/ozone-probe/`.

## What Ozone 2.2.1 does on the wire

aws cli 2.33.15 and `flint-store`'s probes through the SDK, against the
`lean` bucket.

| surface | result |
|---|---|
| PutObject `If-None-Match: *` on an existing key | 412 PreconditionFailed |
| PutObject `If-Match` stale / current | 412 / 200 |
| GetObject `If-Match` stale / current | 412 / 200 |
| `flint-sync probe-conditional` (the SDK path, all four legs) | **PASS** |
| HeadObject with checksum mode | no checksum header at all; ETag is the MD5 |
| PutObject with a CRC32 or CRC64NVME checksum header | accepted, nothing echoed |
| PutObject with a **wrong** CRC32 | **accepted**, object created with the normal ETag |
| `flint-sync probe-versions` | FAIL: PUT returns no `x-amz-version-id` (no versioning) |
| `flint-sync probe-copy`, CopyObject arm | FAIL: the copy is byte-identical but REPORTS no checksum (the source HEAD carries none) |
| `flint-sync probe-copy`, MPU + UploadPartCopy arm | FAIL: refused — "the source carries no CRC-64" |
| `x-amz-meta-*` round-trip (`flint-gen`, `flint-epoch`, `flint-flush-uuid`) | works |
| custom headers in the shipped `ozone-s3gateway-2.2.1.jar` | `X-Ozone-Original-Content-Type` only; the conditional strings above; **no rename**, and **no `x-amz-checksum-*` string anywhere** |

Two things the documentation gets wrong for 2.2.1. The S3 API page says
conditional requests are "planned (HDDS-13117)"; 2.2.1 enforces exactly
the forms lean sends (the jar's own message is "Only If-None-Match: * is
supported for conditional put", and `*` is the only form lean uses), so
the manifest CAS holds. And "Ozone added handling for the CRC32 flow"
means it PARSES the aws-chunked trailer newer CLIs send — it validates
nothing: a deliberately wrong CRC32 landed as a normal object. The store
is checksum-blind. The SDK's "the server validated the publish"
property does not exist on Ozone, which is what today's two commits
were for.

## The lean flow, end to end

- **Publish** 302 files from workspace A (300 × 8 KiB, one 20 MiB, one
  70 MiB — the last through the MPU compose path, whose CRC-64
  FullObject headers Ozone ignores): `up=302`, no wire errors.
- **Fresh checkout** into B: 302 materialised, `ranged=2`, byte-identical
  to A over all 302 files (sha256 of both trees; the only diff is the
  per-workspace `.flint/capabilities.json`).
- **Foreign overwrite** of a cited object with `aws s3 cp`, then a fresh
  checkout into C: GET `If-Match` refused the moved object, the S3-wins
  arm adopted the new bytes — an ordinary workspace, so the same
  outcome as on S3, and C's baseline now carries the CRC of what it
  adopted.
- **Sync** on B after A published again: applied, verified against the
  manifest's CRC.
- **HITL through `flint-lean-gateway`** running on the node: PUT with
  `If-None-Match: *` → 200; the inbox entry carries the gateway's CRC
  (`L1aSChE8s3c=`); A's next barrier consumed it — verified against
  that CRC, since Ozone's HEAD attests nothing — and re-cited it with
  the same CRC from the baseline (`consumed=1`, manifest entry
  `crc64_b64: L1aSChE8s3c=`); a fresh reader H verified its fetch
  against it and got the bytes. Overwrite with `If-Match` → 200;
  without → 428 `precondition-required`. B's sync applied both.
- The chunked manifest (pointer + two chunks), the epoch cell and the
  inbox document all round-trip.

## Where it degrades on Ozone, and what to do

- **No versioning.** Gated mode (`pinned_reads`, version citations,
  `recover-staged`'s version listing) is unavailable; cadence mode is
  what runs on Ozone. The syncer already probes this at gated startup.
- **Copy reports no CRC.** Lean's only server-side copy is the draft
  promote; it works, but `published.crc64_b64` is `None`, so its inbox
  entry carries no CRC and the consume verifies against nothing (it
  records its own hash for the repair, so the manifest is still
  complete). Copies above the single-request ceiling (5 GiB default)
  REFUSE on Ozone because the MPU arm wants the source's CRC for the
  Complete header. Fix shape: the caller passes the CRC it already
  knows (the manifest's), and the store reports that when the backend
  echoes none; `probe-copy`'s "reports a different checksum" leg then
  compares against the caller's value.
- **Checksum headers are dead weight.** Every `x-amz-checksum-*` and
  `x-amz-sdk-checksum-algorithm` lean sends is ignored. A negotiated
  wire algorithm (CRC32 for Ozone) buys NOTHING on 2.2.1 — CRC32 is
  parsed, not validated. The manifest CRC is the integrity check, full
  stop; the wire-checksum negotiation planned for Ozone can be dropped
  from the list until an Ozone release validates something.
- **Rename.** There is no rename operation or header in 2.2.1's gateway
  jar, nor in master's `ObjectEndpoint`. Ozone's atomic rename lives in
  its native protocol (OM `RenameKey`, used by ofs/o3fs and `ozone sh
  key rename`) and is not exposed over S3. The S3-side analogue is S3
  Express One Zone's `RenameObject` (June 2025), directory buckets only.
  On Ozone as on S3, a lean rename is copy + delete.

## Throughput on this box

2,000 × 8 KiB, `drop_caches` before each run, the syncer's own CPU from
`time` (user + sys), each Ozone container's from its cgroup `cpu.stat`
over the run. Two runs per arm; the box is shared, so quote the ranges.

**Checkout** (fresh workspace, `fetch_drivers` auto = 4):

| fanout | files/s | syncer CPU/file | s3g CPU/file | om | datanode |
|---:|---:|---:|---:|---:|---:|
| 32 | 714-898 | 472-476 µs | 2.3-2.7 ms | 0.3-0.8 ms | 0.5-0.9 ms |
| 128 | 880-996 | 486-496 µs | 2.1-2.2 ms | 0.3-0.5 ms | 0.4 ms |
| 512 | 977-1,017 | 497-506 µs | 1.9 ms | 0.3 ms | 0.3 ms |

**Publish** (upload fanout 32): 245-308 files/s; syncer 436-455 µs/file;
s3g 3.0-5.4 ms/file, om 4.2-5.1 ms/file, datanode 2.6-3.7 ms/file,
scm 1.1-1.5 ms/file.

Read it as: the client costs what it costs on S3 (cluster runct
measured 350-520 µs/file at the same fanouts), while Ozone's gateway JVM
alone spends 4-5× that per GET and the whole Ozone side 25× per PUT.
At ~1,000 files/s the s3g holds two of the four cores. **On this box
Ozone is the wall, not the client** — the "on Ozone the client's CPU
becomes the limit" expectation only holds once the Ozone side has
roughly five times the client's cores behind the gateway, which is a
real deployment shape (several s3g replicas) but not this one. A raw
HTTP read path would cut the client's ~480 µs, and would show up in
throughput only against such a deployment; measure there before
building it for throughput. The unsigned `curl` GET in the log is a 403
(Ozone requires a signature even unsecured) — the checksum-header
absence is established by the signed HEAD/GET above.

## Teardown

Same day. Instance `i-01c5c8a77c0777045` terminated, spot request
`sir-k2afk92n` cancelled, staging bucket `flint-ozone-probe` emptied and
deleted, inline policy `ozone-probe-bucket` removed from
`TroveSSMInstanceProfile`. Verified under trove-admin: 0 non-terminated
instances, 0 buckets, no inline policies on the role. Rig cost: about
an hour of one spot i4i.xlarge at $0.105/h.
