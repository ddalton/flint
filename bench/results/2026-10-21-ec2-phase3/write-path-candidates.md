# Replicated write path: candidates that do not cost reads (2026-10-21)

Research only. No cluster was touched. Sources: `bench/results/2026-10-08-ec2-phase2/`
(report-r3.md, flint-r3-f74c2/*/rep*/{fio.json,reactor-*.txt,cpu-*.json},
flint-f74c2-epoch-events.txt, ceilings.log, ceilings/), the Flint tree at HEAD,
the chart `flint-csi-driver-chart/`, and SPDK at tag v26.05 plus Flint's patches
(`spdk-csi-driver/*.patch`, `docker/Dockerfile.spdk`).

Labels: **[M]** = measured (a file is named), **[I]** = inferred (my reasoning,
not yet measured), **[S]** = read in source.

## 0. What the existing data already says

### 0.1 The randwrite range is the position of the epoch cut [M + I]

Cuts land at hh:m0:20 and hh:m5:20 (`flint-f74c2-epoch-events.txt`). fio's
`timestamp` is the end of the job. Each window is 30 s ramp + 120 s measured.

| rep | fio end | last cut before the window | cut inside measured window | IOPS | consumer reactor busy |
|---|---|---|---|---|---|
| rep2 | 00:33:23 | 00:30:20 (33 s before ramp) | none | 68,996 | 84% |
| rep1 | 00:10:39 | 00:05:20 | 00:10:20 (19 s before end) | 55,374 | 74% |
| rep3 | 00:56:07 | 00:50:20 | 00:55:20 (47 s before end) | 35,675 | 63% |

The order follows how much of the measured window comes after a cut. Before
every rep the epoch then running had seen almost no writes (the tests before
it only read), so every rep starts in a copy phase. The consumer reactor is
less busy in the low reps: they are bound by copying, not by CPU. The range
is the cut, not noise. A report should label each rep with where its cuts
fell (the harness already has the cut log).

### 0.2 The QD1 write p50 of 473 us is mostly copy-on-write [M + I]

`lat_randwrite_4k/rep1/fio.json` latency buckets: 19.0% in 100-250 us,
57.5% in 250-500 us, 22.7% in 500-750 us; min 141 us, mean 450 us. All three
reps give p50 473-477 us. Each lat_randwrite window starts within about 5 min
of a cut, and nothing wrote in between. At 2.2k IOPS over the 90 GiB file
(737k clusters of 128 KiB), only ~20% of the window's writes land on a
cluster already copied in that epoch (1 - exp(-N/737k)). That matches the 19%
fast bucket. A COW write is serial: 128 KiB read, then 128 KiB write, then an
extent-page md write, then the user write
(`lib/blob/blobstore.c:2862-2961` [S]), and the raid waits for the slowest leg.

- **Inferred:** a non-COW QD1 write is ~150-250 us, against Mayastor's 163.
  The 473 us is what Flint costs while copying, not its steady latency.
- A 2.2k IOPS writer over 90 GiB needs ~1.7M writes (~13 min) to copy 90% of
  its clusters. With 300 s epochs it **never leaves the copy phase**.

### 0.3 The single reactor is saturated for reads and mixed I/O, and nearly so for steady randwrite [M]

The consumer node (bench3-aws-1) runs the raid, the local leg and the kernel
initiator. Its reactor busy % (`reactor-*.txt`, framework ticks):

| test | consumer | replicas |
|---|---|---|
| randread QD32x4 | 99% | 14-18% |
| seq read 1M / 128K | 99% / 95-97% | 3-12% |
| mixed 70/30 | 85-99% | 20-27% |
| randwrite (rep2, mostly steady state) | 84% | 25-30% |
| seq write 1M | 66-69% | 28-37% |

Flint's whole storage footprint is 3.0 cores across 3 nodes. Mayastor uses
11.5 and Longhorn 5.5.

### 0.4 Ceilings [M] (`ceilings.log`, `ceilings/`)

- Disk (raw): randwrite 110k IOPS; seq write 1,061 MiB/s; seq read 1,356 MiB/s;
  QD1 4K write ~26 us (38k IOPS).
- Network, iperf3 with 4 streams aws-1 -> aws-2: 11.91 Gbit/s for 7 min, then
  **4.65 Gbit/s sustained** (burst credits drained). Public spec sites list the
  i4i.2xlarge baseline as ~4.69 Gbit/s
  ([sparecores](https://sparecores.com/server/aws/i4i.2xlarge),
  [holori](https://calculator.holori.com/aws/ec2/i4i.2xlarge)).
- Ceph seq_write_1m (bench4, coordinator's NIC counters): the busiest NIC
  carries ~1.37x the write rate, against 2x for a consumer-side mirror. A
  mirror's burst ceiling is therefore ~11.9/2 Gbit/s, about 710 MiB/s
  (Mayastor 620, Longhorn 582). Flint's 518 MiB/s is 8.7 Gbit/s of egress:
  **not NIC-capped** [M].

### 0.5 Why Flint loses ~25% on large sequential writes: the replica DISK, through COW [M timing + S code + I arithmetic]

- **Every seq-write rep ran in a fresh epoch.** For example, seq_write_1m
  rep1 ran 00:20:47-00:23:17, after the cut at 00:20:20; the seq read test
  before it wrote nothing. 120 s x 518 MiB/s = 62 GiB written sequentially,
  so every 128 KiB piece lands on a cluster not yet copied in that epoch.
- **A COW writes the cluster twice.** It reads the old cluster, writes it
  whole to the new cluster (`blobstore.c:2833-2837` "Write whole cluster"),
  inserts the md, then re-executes the user write onto the same cluster.
- **So each replica disk carries** ~518 MiB/s read, plus 2 x 518 =
  **~1,036 MiB/s written**, plus md. The measured raw seq-write cap is
  **1,061 MiB/s** [M]. The disk write bandwidth is ~98% used, which matches
  the plateau: seq_write_128k gives the same 506-558 MiB/s.
- **Other suspects are not binding:**
  - The 128 KiB MDTS split (8 commands per leg per 1 MiB) and the nvmf TCP
    send memcpy cost only CPU, and the consumer reactor is at 66-69%
    (replicas 28-37%).
  - raid1 does not split writes: `raid1_submit_write_request` sends the
    whole I/O to each base (`module/bdev/raid/raid1.c:276-327` [S]).
- **Prediction [I]:** outside a copy phase, the disk writes 1x and Flint
  should reach the mirror's NIC ceiling (~650-710 MiB/s in burst).
  Experiment E0 tests this directly.

## 1. Ranked candidates

### 1. Raise the epoch interval (300 s -> 1800-3600 s). KNOB, already decided (F74 §5.2 #4), not applied

- **Mechanism:** each epoch costs working set x (cluster read + cluster write)
  per replica. For the bench's dense writer that is ~90 GiB x 2 per replica
  per cut, whatever the cluster size [I: 10M writes per 150 s touch every
  128 KiB and every 64 KiB cluster]. The interval is the only lever on how
  often that cost is paid.
- **Evidence:** §0.1, §0.2; `flint-csi-driver-chart/values.yaml:178`
  `epochIntervalSecs: 300`; F74 doc §5.2 row 4.
- **Expected write gain [I]:**
  - randwrite average toward the rep2 value (~69k) and its range collapses;
  - QD1 write p50 473 -> ~150-250 us;
  - mixed and seq write leave the copy phase for 6-12x more of the time.
- **Read impact:** none on the read path itself (a read never copies).
  Concurrent reads gain, because copy traffic stops competing for disk
  bandwidth.
- **Risk:** the rebuild delta grows to T_snap of writes; retention K and the
  space each epoch pins grow too. **F75 must land with it** (F74 §5.4 item 6).
  Write-ack semantics are unchanged.
- **Effort:** chart value, testable today (`FLINT_EPOCH_INTERVAL_SECS`).
- **A/B:** same cluster, `epochIntervalSecs` 300 vs 3600, full r3 matrix,
  3 reps, cut times logged and each rep labeled per §0.1. Primary metrics:
  `lat_randwrite_4k` p50, `iops_randwrite_4k` mean and spread.

### 2. More reactors, off CPU 0 (`spdkTarget.extraArgs: ["-m","0xC0"]` or `0x3`). KNOB

- **Mechanism:** the nvmf TCP poll groups, raid1 channels, remote
  `bdev_nvme` qpairs and blobstore channels all go per thread. Each blobstore
  channel has its own 32 COW slots (`blob-parallel-cluster-alloc.patch`,
  `SPDK_BS_MAX_INFLIGHT_CLUSTER_ALLOCS 32`). A second reactor:
  - splits the kernel initiator's 8 qpairs across 2 poll groups;
  - doubles COW parallelism;
  - opens a second TCP flow to each remote leg.

  The reactor is now pinned to CPU 0 (default mask 0x1, chart
  `values.yaml:1046,1093`), the same core the kernel, IRQs and fio use. The
  spdk-tgt container has no CPU request or limit (`templates/node.yaml:549`).
- **Evidence:** §0.3: 99% busy on reads, 84% on steady randwrite. Flint r1
  reaches the 110k disk cap on the same single core; r3 adds two
  NVMe-oF/TCP initiator legs per I/O on the consumer.
- **Expected gain [I]:**
  - steady randwrite 69k -> 85-110k (disk cap);
  - mixed up, because it is reactor-bound at 98%;
  - **reads gain too**: randread is at 99% busy, and the per-disk cap of
    200k local plus remote legs leaves room.
- **Read impact:** positive.
- **Risk:** a permanent extra polled core on every node (12.5% of 8 vCPU).
  The multi-thread COW race (two threads allocating the same cluster, the
  `-EEXIST` re-execute) is upstream code, but the F74 §5.5 correctness gate
  has never run with more than one reactor. Run the gate on the mask you
  ship.
- **Effort:** chart value.
- **A/B:** masks 0x1, 0x3 and 0xC0 (two cores, not CPU 0), full matrix. Record
  `framework_get_reactors` and `thread_get_stats` per reactor, and
  `nvmf_get_stats` (qpairs per poll group).

### 3. Skip the copy on full-cluster writes (F74 §5.2 #3). SPDK PATCH

- **Mechanism:** a write that covers a whole 128 KiB cluster writes straight
  into the new cluster, then inserts it. On `-EEXIST` it re-executes against
  the winner's cluster. Today the cluster is copied even when the write
  covers all of it (`blobstore.c:2949-2955` [S]). MDTS is 128 KiB
  (`lib/nvmf/tcp.c:45` [S]), equal to the cluster, so aligned pieces qualify.
- **Expected gain [I]:** removes both the 128 KiB read and the duplicate
  128 KiB copy write per full-cluster write (§0.5). Disk writes drop from 2x
  to 1x, so seq write in a copy phase moves from the disk cap (~518) to the
  NIC ceiling (~650-710 MiB/s burst). Nothing for 4K random writes.
- **Alignment:** with MDTS 128 KiB, only pieces aligned to clusters qualify;
  check ext4 alignment with `filefrag -v`. If they are not aligned, pair this
  with raising `max_io_size` to 1 MiB. The lvol splits a 1 MiB write at
  cluster boundaries (`vbdev_lvol.c:1196`), so 7-8 of its children are full
  clusters whatever the alignment. Raise the iobuf large pool with it.
- **Read impact:** none.
- **Risk:** a new blob ordering path; needs unit tests including a forced
  `-EEXIST`, plus the full §5.5 gate. Only worth doing if `filefrag -v` shows
  ext4 extents aligned to 128 KiB (unmeasured).
- **Effort:** medium.
- **A/B:** experiment E0 first (§2). If COW-bound, run seq_write_1m and
  seq_write_128k with the patch vs without, both straight after a cut.

### 4. Raise the TCP in-capsule data size (4096 -> 16-32 KiB). IMAGE CONFIG

- **Mechanism:** writes larger than the in-capsule size need an R2T round
  trip. Two hops are affected:
  - kernel -> local target, which reads `ioccsz`;
  - the SPDK initiator -> replicas (`lib/nvme/nvme_tcp.c:1053-1063` [S]).

  The transport is created only from the baked `config.json` with `trtype`
  alone (`Dockerfile.spdk:184`), so this needs an image change.
- **Expected gain [I]:** about one RTT (~30-60 us) per 8-32 KiB write, e.g.
  database pages. The bench's 4K writes are already in-capsule, and 128K/1M
  at QD16 is not RTT-bound, so this matrix will barely move.
- **Read impact:** none; in-capsule data applies only to host-to-controller
  transfers.
- **Risk:** memory = queue depth (128) x ICD per qpair, from hugepages. Cap
  ICD below `max_io_size` (`tcp.c:810`).
- **Effort:** low.
- **A/B:** add `lat_randwrite_16k` (QD1) to the matrix, ICD 4096 vs 32768.

### 5. Client zero-copy send for remote legs. RPC AT STARTUP

- **Mechanism:** `sock_impl_set_options` with
  `enable_zerocopy_send_client=true` and `zerocopy_threshold` around 16K.
  v26.05 posix defaults to server true, client false
  (`module/sock/posix/posix.c:96-98` [S]). The consumer acts as a client to
  its replicas, so each 128K write is memcpy'd twice (2 legs).
- **Expected gain [I]:** consumer CPU on large writes; only matters once item 2
  is done or seq write becomes reactor-bound (66-69% today).
- **Read impact:** none; the read-serving side is already the zero-copy server.
- **Effort:** low.

## 2. Measure before deciding (cheap, decisive)

- **E0, seq write: COW or NIC?** Run seq_write_1m twice back to back inside
  one epoch, with `epochIntervalSecs=3600` or right after a full rewrite, and
  sample the ENA `bw_out_allowance_exceeded` counter.
  - Second pass much faster: the cap is COW, so item 3 is the lever.
  - Equal, and the counter rises: the cap is egress.
- **E1:** read-only RPC `bdev_get_bdevs -b nvme_0000_00_1f_0n1 | jq
  .[0].supported_io_types.copy`. If true, blob COW already uses device copy
  (`blobstore.c:2841-2847`, `module/blob/bdev/blob_bdev.c:509` [S]).
- **E2:** a latency log around reaps. Deleting the oldest epoch freezes I/O on
  its clone, which is the next epoch snapshot (`blobstore.c:8466`), while
  extent pages are rewritten one at a time (`:8333-8361`) [S]. Head reads and
  COW reads that fall through to that snapshot wait for the whole reap.
  Inferred effect: a tail-latency blip on writes **and reads**. If it shows,
  run reaps away from cuts.

## 3. Not recommended / reachable only by other means

- **ublk local hop:** measured 2x randwrite QD32 on the local hop, but
  loopback NVMe-oF has +20% seq read and a far better tail
  (`attach-detach-campaign-2026-07.md:2191`). It risks the read strength and
  has no online expansion. Only after ublk queue tuning closes the seq read
  gap.
- **64 KiB clusters:** these help only sparse writers. Dense writers copy the
  whole working set anyway (§1 item 1). They double cluster splits on a read
  path that is already 99% reactor-bound, and grow the md region. **Read
  risk.**
- **Raising MDTS to 1 MiB on its own:** no COW benefit, and it multiplies
  iobuf use per request (read-stall risk unless the pool is raised). It is
  useful only as the alignment-proof companion of item 3.
- **Sustained egress:** any 2-copy fan-out from the consumer, Mayastor
  included, is held to ~4.65/2 Gbit/s (~277 MiB/s) of replicated writes after
  the 7-min burst [M baseline, I consequence]. Only primary-copy or chain
  replication (replica forwards) halves consumer egress. Ceph's busiest NIC
  carries 1.37x the write rate (measured, bench4); a mirror carries 2x. That is an
  architecture change: it adds a serial hop to QD1 latency, and the raid1 on
  the consumer is the design. It does not break the user's constraints, but
  it is not a tuning.
- **In-memory dirty map / write-intent bitmap:** out by design (F74 §5.3,
  incremental-replica-rebuild §7-8). It would remove COW entirely; not
  proposed.
- **Thin vs thick:** no effect after a cut. `bdev_lvol_snapshot` makes the
  head a thin clone either way.
