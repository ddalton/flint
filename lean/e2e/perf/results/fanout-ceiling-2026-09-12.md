# The small-file ceiling: one thread, then one lock — 2026-09-12

**Question.** `flint-sync checkout` of 20,000 x 8 KiB plateaued at
~3,300 files/s on i4i.large from fanout 128 to 512, at 1.4 of 2 cores,
futex-dominated, thirteen mechanisms proposed and dead. Hard limit, or
does the syncer scale past 128? And why do 2 syncers give 1.7x?

**Answer.** Not a hard limit. Two ceilings stacked, each invisible until
the one above it was removed:

1. **One task.** `materialize()` drove every fetch future from a single
   `buffer_unordered` on the `#[tokio::main]` block_on thread. Every
   request's SDK work — build, sign, orchestrate, parse headers, collect
   the body, validate the checksum — ran on one thread whatever `fanout`
   said. `fanout` past the knee was dead code.
2. **One lock.** The shipped binaries are static musl. Spread the SDK
   work over N threads and musl's malloc, which holds one global lock,
   turns the parallelism into 3x the CPU per file and *negative* scaling.
   The earlier "allocator ruled out" measurement was made with one
   driver thread, where the lock could not have been contended.

With N driver tasks **and** a scalable allocator (mimalloc), a 6-vCPU
Linux VM went from 9,380-12,554 files/s (shipped) to 20,876-27,662 on
less total CPU than the shipped binary uses. One such syncer matches
four shipped syncers running side by side.

Nothing here was found by reading: every mechanism was a per-thread CPU
table first and a hypothesis second.

## Instrument

Per-thread CPU from `/proc/<pid>/task/*/stat`, sampled every 200 ms
until exit, last complete sample kept; tid == pid is the main thread.
Files/s = 20,000 / `fetch=` from flint-sync's own phase line. Every arm
must report `bytes=163840000`, and a sha256 of the materialised tree is
diffed against the corpus (that diff is what caught the data loss below).

Rigs, all local, no AWS: `lean/e2e/perf/fakes3` (in-memory S3) seeded by
publishing the corpus into it; Lima VM `flint-nfs-client` (aarch64,
6.8 kernel) at 2 vCPU then 6 vCPU; checkout root on tmpfs unless noted.
n = 3 interleaved reps per cell; ranges quoted, never means.

## 1. The single task (2-vCPU VM, fakes3 on the host)

| fanout | shipped files/s | main thread CPU | all other threads |
|---:|---:|---:|---:|
| 32 | 8,576-9,009 | 1.97 s | 1.13 s |
| 128 | 8,203-8,247 | 2.00-2.17 s | 1.05-1.16 s |
| 256 | 8,051-8,254 | 2.08 s | 1.03 s |
| 512 | 7,754-8,064 | 2.17 s | 1.21 s |

Fetch window 2.4-2.6 s: the main thread is saturated, every other thread
combined is under half a core, half a core idle. That is the i4i.large
signature ("1.4 of 2 cores") reproduced on loopback with no TLS and no
network, so the ceiling was never S3, the NIC, or TLS.

**Spawning each fetch as its own task** (`tokio::spawn` per entry) moved
the main thread to 0.16-0.17 s and the work to the workers (2.95-3.38 s):
128 -> 9,541-9,896 (+15-20%), which is all a box with 0.7 idle cores can
give. On ext4 (virtio) it was a wash: 8,295-8,456 vs 8,216-8,302.

**It lost one file in 5 of 12 runs.** `resolve_contained` stats a missing
parent, then `create_dir`s it; a sibling fetch can win the mkdir in
between, and the loser's EEXIST was reported as a containment REFUSAL:
"19999 materialized, 0 present", checkout complete, one file missing with
only a conflict record to say so. Unreachable while one task polled every
fetch (the walk is synchronous); reachable on any tree with a
subdirectory once fetches run in parallel. Fixed: EEXIST re-stats and
accepts a plain directory, refuses a symlink. Pinned by
`siblings_racing_to_create_one_parent_do_not_refuse_each_other`, which
fails on the old walk in round 0. 12/12 and 3/3 identity-clean since.

## 2. The lock (6-vCPU VM, fakes3 on loopback inside the VM)

Arms: `base` = shipped; `shardN` = N driver tasks, each its own
`buffer_unordered` over a round-robin slice of the admission list
(`FLINT_SYNC_FETCH_DRIVERS=N`); `-mi` = the same binary linked with
mimalloc as the global allocator. fanout 128 unless noted.

| arm | files/s | total CPU (all threads) | CPU / file |
|---|---:|---:|---:|
| base | 9,380-12,554 | 2.7-3.9 s | ~145 us |
| spawn-per-fetch | 7,613-8,639 | 8.9-10.7 s | ~480 us |
| shard1 (musl) | 11,869-13,046 | 1.7-2.6 s | ~110 us |
| shard6 (musl) | 8,413-10,351 | 8.0-9.4 s | ~430 us |
| shard128 (musl) | 10,136-10,214 | 7.9-8.0 s | ~400 us |
| shard1-mi | 11,129-14,388 | 2.3-3.3 s | ~140 us |
| **shard6-mi** | **21,528-27,662** (one rep 9,310 at 5.3 s CPU) | 1.7-2.9 s | ~90 us |

At fanout 512: base 10,443-11,682, shard6 (musl) 7,541-9,000,
**shard6-mi 20,876-25,252**.

Read down the musl column: every arm that runs SDK work on more than one
thread pays ~3x the CPU per file and is slower than the single driver.
Read across: the same sharded binary with mimalloc uses *less* CPU per
file than the shipped single driver and scales. shard1 beating base is
its own small finding: driving from a worker instead of the block_on
thread saves the park/unpark round trip.

**The raw-socket floor.** `fakes3/src/bin/smallclient` — HTTP/1.1
keep-alive GET per object, same tmp+rename write, no SDK — does
60-76k files/s with writes (55-75 us/file, ~85% of it the write) and
123-177k without (10 us/file). The SDK layer is ~80-130 us/file on this
VM: the next ceiling, and 8-10x the socket's cost.

**Multi-syncer** (disjoint subtrees via `FLINT_SYNC_CHECKOUT_SCOPE`,
aggregate files/s including process start):

| N | base | shard6-mi |
|---:|---:|---:|
| 1 | 9,433-12,121 | 22,222-23,255 |
| 2 | 18,518-18,867 | 25,316-27,777 |
| 4 | 21,276-21,978 | 25,641-28,169 |

Base scales 1.5-1.9x at N=2 and ~2x at N=4 — the user's "1.7x" — because
each process is one saturated thread and the box (6 vCPUs shared with the
fake server) runs out. One shard6-mi syncer is where four base syncers
end up.

## 3. On real S3 with TLS (cluster runct, us-west-1, all spot, n=3)

Bucket `flint-ceiling-runct` (fresh), tree `ceiling-drill/tree`, root on
the instance-store NVMe (ext4), `drop_caches` before every run. Arms as in
section 2; `shardN-mi` = N drivers + mimalloc. TCP RTT to S3 measured
1.4-2.6 ms on the node, so the ~24 ms per request the fanout-8 control
shows (330 files/s over 8 streams) is S3's own first-byte time.

**i4i.xlarge (4 vCPU) — the shipped ceiling, reproduced to the number:**

| arm | fanout 128 | fanout 256 | fanout 512 | main thread | CPU/file |
|---|---:|---:|---:|---:|---:|
| base | 3,277-3,481 | 3,339-3,368 | 3,130-3,153 | 5.7-6.4 s of ~6 s | 410-520 us |
| shard1 (one driver, musl) | 3,375-3,526 | 3,339-3,414 | 3,147-3,225 | 0.1-0.3 s | 450-480 us |
| shard2-mi | 4,786-4,909 | 5,467-5,758 | 5,260-5,633 | 0.1-0.2 s | 270-400 us |
| **shard4-mi** | 4,909-4,961 | **6,188-6,521** | 5,892-6,133 | 0.1-0.2 s | 350-430 us |
| shardF-mi (fanout drivers) | 4,739-4,926 | 5,931-6,064 | 2,611-5,281 | 0.1 s | 400-420 us |

- `base` is the ~3,300 plateau exactly, flat 128 -> 512, with its main
  thread saturated and 2+ cores idle. One driver on a worker (`shard1`)
  is the same number: the thread, not where it lives, is the ceiling.
- `shard4-mi` is **1.9x** and, for the first time, **fanout 256 beats
  128** (4,909-4,961 -> 6,188-6,521): the knob was dead code behind the
  serial task. 512 does not beat 256.
- At 6,200-6,500 the process holds ~2.5 of 4 cores and has ~150
  requests effectively in flight of 365 sockets; the next ceiling is not
  the client. It is where S3 serves this one fresh prefix: the
  multi-syncer leg (below) tops out in the same place, and 4 syncers do
  not beat 2. S3's documented per-prefix budget is 5,500 GET/s and
  grows with sustained load; yesterday's bucket, hammered for a day,
  gave 7,959 aggregate.
- `shardF-mi` at 512 (512 driver tasks, 998 sockets) had one rep at
  2,611: the spawn-per-fetch shape is the worst of the parallel arms
  here too.

**i4i.large (2 vCPU) — the shipped box:**

| arm | fanout 128 | fanout 256 | fanout 512 | CPU/file |
|---|---:|---:|---:|---:|
| base | 1,916-2,041 | 1,900-2,004 | 1,758-1,844 | 460-520 us |
| shard1 | 1,971-2,398 | 1,886-2,105 | 1,986-2,180 | 420-440 us |
| shard2 (musl) | 2,476-2,570 | 2,371-2,498 | 2,393-2,558 | 370-380 us |
| **shard2-mi** | **2,737-3,027** | 2,719-2,826 | 2,728-2,919 | 320-330 us |
| shardF-mi | 2,342-2,490 | 2,160-2,359 | 2,283-2,370 | 360-400 us |

1.5x on the shipped box, on 35% less CPU per file. This node's `base`
came in at ~2,000 rather than the ~3,300 of earlier sessions with its
main thread at ~60%, not saturated — a slower or noisier vCPU pair on
this spot instance; the xlarge reproduces the historical number. NIC
allowance counters: `bw_in_allowance_exceeded` 23 (microbursts), all
others 0; the network was not the line.

**Multi-syncer (disjoint subtrees, aggregate files/s incl. process
start, n=2):**

| node | arm | N=1 | N=2 | N=4 |
|---|---|---:|---:|---:|
| i4i.xlarge | base | 2,941-2,962 | 3,610-3,663 | 3,294-3,327 |
| i4i.xlarge | shard4-mi | 3,696-3,710 | 4,415-4,424 | 3,952-3,984 |
| i4i.large | base | 1,739-1,769 | 1,429-1,438 | 1,238-1,255 |
| i4i.large | shard2-mi | 2,283-2,447 | 1,928-2,132 | 1,577-1,679 |

On 2 vCPUs a second syncer makes the aggregate WORSE — each shipped
process wants a whole core. On 4 vCPUs two base syncers give 1.2x and
four give less than two; that is the user's "2 syncers = 1.7x, then
nothing" shape, and it is the box's cores plus the prefix's rate, not
the lease (checkout takes none since 3b9201ea).

**So: not a hard limit.** The 128 plateau was one thread; fanout past
128 pays once the fan-out is driven from several threads with an
allocator that scales; the ceiling after that is S3's per-prefix rate
and the node's cores, in that order on a fresh bucket. Torn down: 0
instances, 0 spot requests, 0 buckets, inline policy removed.

## 4. What a raw HTTP GET path would buy (measured, not built)

Same 6-vCPU VM, same 20,000 objects on loopback fakes3, same
tmp+rename write per file, both binaries with symbols, shipped defaults
(6 drivers, mimalloc, fanout 128). `smallclient` is HTTP/1.1 keep-alive
GETs over 32 raw sockets with no SDK in it; a real raw path would add
SigV4 signing (~1.2% of the syncer's samples, `sha2` + canonical
request) and the TLS both already share on the node.

| | files/s (3 runs) | CPU / file | of which kernel |
|---|---:|---:|---:|
| flint-sync, shipped defaults | 22,700-28,500 | ~165 us | ~70 us |
| raw client | 73,500-79,700 | ~50 us | ~45 us |

So the SDK layer costs ~110 us per 8 KiB object here — 2.6-3.5x in
files/s and 3.3x in CPU per file — and the raw client's own floor is
the FILESYSTEM: 90% of its CPU is the create + rename, and its top
symbol is the directory lock (`osq_lock` under `open_last_lookups` and
`do_renameat2`, 13.5%).

Where the syncer's 165 us go (perf, self time, `flint-sync` DSO = 57%
of samples, kernel the rest): `memcpy` 5.3% (half of it
`de_get_object_http_response`), mimalloc alloc/free ~4%, Arc refcount
atomics (`ldadd8`) ~3.2%, `sha2` 1.2%, `CanonicalRequest::from`,
`RuntimeComponentsBuilder::merge_from`, `Interceptors::modify_before_
serialization`, `config_bag::ItemIter`, `SharedRuntimePlugin`,
`parse_hdr`, `fmt::write`, `String::from_iter`, `hex` — each under 1%,
thirty of them. That is the smithy orchestrator building a runtime
component set, a config bag and a signed canonical request per call,
and there is no single line to fix. Kernel side, two things the write
path pays that it need not: **20,006 `unlinkat` calls, every one
ENOENT** (`write_via_tmp_opts` removes the tmp name before `create_new`;
`O_EXCL` already refuses a leftover or a planted symlink, so the unlink
could run only on EEXIST — one syscall and one directory write-lock per
file saved), and **six `stat`s per file** (the containment walk, the
resume check, `check_parent`, the post-write metadata; ~3 would do).

**Verdict.** A raw path for small whole-object GETs is worth ~2.5-3x in
CPU per file, and 2.6-3.5x in files/s ONLY where the client is the wall.
On the node it is not: at 6,200-6,500 files/s the shipped-defaults
syncer holds 2.5 of 4 cores and S3's per-prefix rate on a fresh bucket
is what stops it, so a raw path there would buy CPU (smaller pod, more
syncers per node), not throughput, until the key space spans more S3
partitions or the prefix has been warmed. It costs owning SigV4,
credential refresh, retry/backoff with the standard-mode token bucket,
412/404/5xx classification, checksum validation, path-style and
endpoint overrides, and `x-amz-meta-*` parsing behind the existing
`ObjectStore` trait — 400-600 lines plus a live drill. Recommendation:
not yet. First the two syscall trims above (they are a third of the
raw client's whole cost, and every path pays them), then measure S3's
delivered rate on a partitioned prefix; build the raw path when a real
workload is CPU-bound after this commit.

## 5. The two syscall trims (done, `lean/syncer/src/safefs.rs`)

Same 6-vCPU VM, loopback fakes3, tmpfs, shipped defaults, n=3
interleaved, `strace -c -f` for the counts:

| per 20,000 files | before | after |
|---|---:|---:|
| `unlinkat` (pre-create, all ENOENT) | 20,006 | 1 |
| `mkdirat` (`create_dir_all` on an existing parent) | 20,024 | 25 |
| `newfstatat` | 120,268 (40,033 ENOENT) | 60,269 (20,034) |
| `fstat` (the write returns its own) | 9 | 20,014 |
| all syscalls per file | 22.2 | 18.6 |
| files/s | 29,761-31,201 | 32,206-33,003 |

The temp name is now created `O_EXCL` first and unlinked only on
`EEXIST`; the parent is recreated only on `ENOENT` and only for the
materialising writer (a state or control directory that vanished
mid-run stays an error); the containment walk hands back the final
component's `lstat`, so checkout's resume check no longer pays
`exists()` + `metadata()` for an answer the walk already held; and the
write returns the `fstat` of the handle it just wrote instead of the
caller `stat`ing the target. Six stats per file became three. The safety
argument is unchanged and the test
`the_temp_retry_paths_replace_a_leftover_and_recreate_a_vanished_parent`
fails on the leftover leg with the `EEXIST` arm deleted and on the
vanished-parent leg with the `ENOENT` arm deleted. The three remaining
stats are the two-component containment walk (a symlink check that must
stay) and `check_parent`.

**On-prem Ozone.** The syncer also runs against Apache Ozone's S3
gateway on-prem. Nothing in sections 1-2 or 5 depends on the backend;
section 3's ceiling and the section 4 verdict do: an Ozone gateway on a
LAN has no per-prefix throttle and sub-millisecond first-byte time, so
the client's CPU becomes the wall far sooner there, and the raw-path
question should be re-asked with Ozone's delivered GET rate measured
the same way (the fanout-8 control gives first-byte time; the
multi-syncer leg gives the backend's rate). Ozone may also omit
`x-amz-checksum-crc64nvme` on GET, in which case the SDK validates
nothing and lean's own manifest CRC is the only integrity check on a
fresh fetch — an argument for end-to-end CRC verification in checkout
whichever client is underneath.

## 6. Fresh fetches verified against the manifest CRC (done, `checkout.rs`)

The backend-independent integrity check the raw-path discussion kept
coming back to, built first because it does not depend on the client.
The whole-object arm hashes the body (CRC-64/NVME, slice-by-8) on the
blocking pool before the write and compares it to the manifest entry's
`crc64_b64`; the ranged arm takes each range's CRC on the pass that
writes it and folds the ranges in offset order with `crc64_combine`,
compared before the rename. Only CITED bytes are checked — an If-Match
hit or a pinned version. The S3-wins adoption arm adopts bytes that
moved past the manifest, whose CRC describes the old ones, and is
exempt by construction; legacy entries carry no CRC and stay unchecked.
A mismatch refuses the checkout with nothing written or renamed.

Mutation controls: the whole-arm check deleted fails only the
whole-arm test; the ranged fold deleted fails only the ranged test; the
check applied to adopted bytes fails
`an_ordinary_workspace_still_adopts_bytes_that_moved_past_the_manifest`
with the CRC refusal. The memory double gained `inject_corrupt_body`,
which changes a key's bytes while its etag and stored checksum claim
stay intact — the shape of bit-rot, a broken gateway, or a cache
serving the wrong object, none of which If-Match can see.

Cost, 6-vCPU rig, loopback, tmpfs, fanout 128, n=3 interleaved:
previous build 32,102-33,388 files/s, with the check 31,496-32,573.
An 8 KiB CRC is ~8 us on the blocking pool; the ranges overlap. Identity
20,000/0 on both runs.

## Rig defects found on the way (each would have been quoted)

- macOS `paste -sd,` with no `-` prints usage: every "scoped" syncer got
  an EMPTY scope and fetched the whole tree, and the multi-syncer leg
  read as negative scaling. Caught by `total_bytes = N x corpus`; a
  scope guard now asserts each syncer's declined count.
- `pkill -f "fakes3 --seed-dir"` matched the shell running it.
- bsdtar from macOS shipped 20,021 `._*` AppleDouble files and the Mac's
  `.flint-sync` state dir into the VM; the barrier there uploaded only
  the 21 extras against a baseline citing objects the VM store never
  held.
- Lima's user-mode network collapsed the raw client at 128 connections
  (1,028-1,780 files/s vs 9,600 at 32) while the SDK's pool was fine —
  fakes3 moved inside the VM before any 6-vCPU number was written down.
- The raw client at conns=32 first read as "no faster than lean": it was
  the same network path capping both.

## What ships, and what does not yet

- `resolve_contained` EEXIST fix + test: ship regardless.
- Sharded drivers (`fetch_drivers`, default = cores clamped to 8): the
  env knob `FLINT_SYNC_FETCH_DRIVERS` is NOT stamped by the webhook, so
  either it becomes a CR field or the default stays computed and the env
  read is removed before release (see `inject.rs`
  `every_knob_the_sidecar_reads_is_stamped_by_the_webhook`).
- mimalloc as global allocator (`--features mimalloc`): behind a feature
  until the cluster leg lands; the shipped images are static musl, so
  this applies to every image, and the SAME lock sits under
  `barrier`'s `upload_fanout` and forge's push.
- Not touched: the SDK-per-request cost (~100 us/file), which is now the
  floor. A raw hyper GET path for small objects is the next lever.
