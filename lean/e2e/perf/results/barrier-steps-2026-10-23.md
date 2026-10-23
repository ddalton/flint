# Where a barrier's time goes: per-step timings, 2026-10-23

`BarrierReport::steps` (and `steps` on the `barrier_end` trace event)
now records each step's wall time and the requests the store sent
during it. The steps tile the barrier: `tests_steps.rs` checks that
every request the store sent is charged to exactly one step. Two
controls, both run: removing the `release` lap fails the step-order
check; moving it before the release call keeps the order and fails
only the request check (10 PUTs charged, 11 sent).

**Rig.** The box (i3-12100, 4C/8T, 30 GiB, ext4 on NVMe, kernel 6.12),
otherwise idle. `996802b6` plus this change, release build, default
features plus `s3`. Two stores: the in-memory double (CPU and local disk
only), and MinIO RELEASE.2026-09-22 run natively on loopback with its
data on the second NVMe. Bench: `tests_bench::barrier_cost_against_workspace_size`
with the shipped default config, one writer B and a peer A, files of
about 10 bytes. 20k files: three runs per store. 100k and 1M: ONE run
per store, so no spread is established at those sizes. One more run
per size split three laps further (`baseline_load` out of `scan`,
`retire_set` out of `surface`, `free` out of `release`).

**Loopback MinIO is a floor.** A request costs well under a
millisecond there; on S3 each costs tens of milliseconds, and the
request column is what that latency multiplies. The serial chain that
matters is `load`'s 220 chunk GETs at 1M, sequential on purpose
(`manifest.rs`, "Sequential on purpose for now"). Not measured on S3.

## A one-file publish (B publishes, nobody else commits), MinIO

`†` = held under the lease.

| step | 20k ms | 100k ms | 1M ms | requests at 1M |
|---|---:|---:|---:|---|
| cell | 1.9–2.2 | 2.3 | 2.0 | G1 |
| consume | 17–18 | 83 | 914 | G1 |
| scan | 48–52 | 256 | 2794 |  |
| intent | 1.3–1.6 | 1.9 | 2.0 |  |
| upload | 3.3–3.8 | 5.2 | 3.3 | P1 |
| claim † | 6.4–6.5 | 6.7 | 6.5 | G1 P1 |
| verify † | 0.4–0.5 | 0.7 | 0.4 | H1 |
| load † | 23–24 | 122 | 1208 | G220 |
| fence † | 0.7–0.8 | 0.9 | 0.9 | G1 |
| merge † | 14 | 70 | 754 |  |
| cas † | 37–38 | 124 | 1323 | P3 |
| surface † | 6.9–7.1 | 36 | 373 |  |
| retire_ledger † | 2.6–2.8 | 2.9 | 3.1 | G1 L1 |
| reap † | 2.1 | 7.0 | 70 |  |
| baseline † | 12 | 58 | 548 |  |
| generation_sweep † | 1.3–1.4 | 1.4 | 1.5 | G1 L1 |
| chunk_sweep † | 1.4–1.5 | 1.9 | 5.0 | G2 L1 |
| release † | 10 | 35 | 308 | P1 |
| **total** | **194–196** | **835** | **8525** | |
| **held the lease** | **117–120** | **467** | **4602** | |

It grows close to linearly with the workspace: 195 ms, 835 ms, 8.5 s for
20k, 100k and 1M files. The change itself is one 10-byte file. The
in-memory store gives the same shape (175–176 ms, 938 ms, 9.6 s), so on
loopback the cost is CPU, not requests.

## The same publish, with three laps split further

| step | MinIO 100k ms | memory 1M ms |
|---|---:|---:|
| consume | 91 | 1593 |
| **baseline_load** | 73 | 1355 |
| scan (the walk itself) | 171 | 2016 |
| load † | 123 | 979 |
| merge † | 72 | 902 |
| cas † | 132 | 1209 |
| **retire_set** † | 36 | 429 |
| surface † | 0.0 | 0.0 |
| baseline † | 50 | 898 |
| **free** † | 9.8 | 328 |
| release † | 29 | 0.2 |
| **total** | **839** | **10060** |

`surface` was the retired-set computation, and `release` was mostly
dropping the commit section's copies of the document. Both are O(N).

## Where the 1M publish goes (memory, split run, 10.06 s)

| bucket | ms | share |
|---|---:|---:|
| the baseline FILE: parsed in `consume` (most of its 1593), parsed again before the scan (1355), rewritten after the CAS (898) | ≈3846 | 38% |
| the whole DOCUMENT under the lease: load, merge, cas, retire set, free | 3847 | 38% |
| walking the tree | 2016 | 20% |
| per-publish GC: retire ledger, reap, generation and chunk sweeps | 84 | 0.8% |
| the lease's own requests: claim, fence, release | 1.5 (17–37 on MinIO) | ≈0 |
| the one upload and its verify HEAD | ≈0 | ≈0 |

## Other barriers, MinIO

| barrier | 20k | 100k | 1M | dominated by |
|---|---:|---:|---:|---|
| idle tick, nothing to do | 68–72 ms | 368 ms | 3.9 s | the baseline parsed twice, and the walk |
| a peer's one-file change arriving | 113–116 ms | 553 ms | 6.6 s | `consume`: a full derive, 221 GETs at 1M |
| the first barrier after a checkout | 2.1–2.2 s | 11.0 s | 110.8 s | `scan`: the racy re-hash of every file the checkout wrote |
| a one-file publish with the orphan sweep due | 659–703 ms | 2.6 s | 36.7 s | `orphan_sweep` † 350–392 ms / 1.56 s / 28.1 s: a LIST of `files/` |

The bench runs its barriers back to back. Under the shipped 60 s
floor, the re-hash after a checkout is paid by the first barrier only;
here it shows in B's first barrier and in the peer's first publish
after seeding. The orphan sweep runs once per `untracked_sweep_secs`
(an hour by default).

## What it says about the proposals made earlier the same day

- **Per-publish GC is not where the time goes**: 0.8% at 1M, 7.5 ms of
  195 at 20k. Moving it out of the commit section buys little. The
  exception is the ORPHAN SWEEP, which holds the lease for 28 s at 1M
  when it runs.
- **Verify's HEADs** cost one HEAD per uploaded file, 0.4–0.7 ms here.
  Publishes of many files were not measured.
- **The lease's own requests** cost 17–37 ms on MinIO: about 9% of a
  publish at 20k, 0.4% at 1M. That is what a lease-free protocol would
  save on this rig.
- **A manifest cache keyed by the pointer etag** removes `load`: 10–15%
  of a publish, 1.0–1.2 s at 1M, and more on S3, where it is 220
  sequential GETs.
- **The lever is not in the protocol.** About 96% of a one-file publish
  at 1M is work in proportion to the workspace, not to the change: the
  baseline file (38%), the whole document under the lease (38%) and the
  tree walk (20%). An in-memory baseline, a cached document that is merged
  and serialised incrementally, and change tracking in place of the walk
  would make a publish proportional to what changed. None of them
  changes a protocol rule.
- **The idle tick** costs 3.9–5.3 s at 1M every floor: 6–9% of a core,
  spent finding nothing changed, on the same parse and walk.

## Not established

- Variance at 100k and 1M (one run each). At 1M the two stores differ by
  1.1–1.5 s in CPU-bound steps; whether that is the store or run-to-run
  variation is not known.
- Anything on S3. The request counts say what latency would multiply;
  the effect is not measured.
- Large files. The files are 10 bytes, so `upload` is near 0. Uploads run
  outside the lease, so large files add to the publish but not to the
  lease.

## Reproduce

```sh
# in-memory store
FLINT_BENCH_N=20000 cargo test -p flint-lean --release --lib tests_bench -- --ignored --nocapture
# an S3-compatible store
FLINT_BENCH_S3_ENDPOINT=http://127.0.0.1:9100 FLINT_BENCH_S3_BUCKET=<bucket> \
FLINT_BENCH_S3_KEY=<key> FLINT_BENCH_S3_SECRET=<secret> FLINT_BENCH_N=20000 \
  cargo test -p flint-lean --release --lib --features s3 tests_bench -- --ignored --nocapture
```

Each timed barrier prints a `STEPS {json}` line. The raw logs from this
run stay on the box (`/mnt/nvme/steps-results/`,
`/mnt/nvme/steps-results-split/`), because they are generated output.
