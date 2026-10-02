# step 6 on the build box — phases A–D (option 1)

Plan: `docs/plans/flint-lite-nfs-proxy-step6-rig-plan.md`. Build box (8
cores, 30 GiB, NVMe, Linux 6.12), 2026-09-30.

The setup, for every phase:
- kind, with the real hub and operator images built from HEAD;
- RustFS as the bucket (a lower bound on real S3 latency);
- the host kernel as the NFS client;
- files of 1–64 KiB, log-uniform, 100 per directory.

The box measures the hub process and the proxy's CPU; it does not
measure flint-spdk or a network. Those two are the AWS session.

## A — what one real hub costs (`step6-box-hubcost.sh`, 6/6)

30 hubs, 10 each at 1k / 5k / 10k files, all seeded through the proxy
(160,000 files in 429 s, 30 writers). Each tier's bucket holds exactly
N files plus 3 bookkeeping objects (`A-bucket-extras.txt`).

| | 1k files | 5k files | 10k files |
|---|---|---|---|
| RSS idle (MiB) | 84–103, median 92 | 65–88, median 74 | 81–98, median 92 |
| CPU idle (millicores) | 4–5 | 7–9 | 9–12 (see below) |
| `state.db` | 356 KiB | 800 KiB | 1.35 MiB |
| RSS / CPU under a light metadata load | — | — | 69–90 MiB / 51–54 m |
| pod start → Ready, disk kept | — | — | 10–12 s |

The operator used 56 MiB and the proxy 83 MiB with 30 hubs
(`A-context.txt`).

**Idle CPU does not grow with file count; phase A read time since
seeding.** The idle-CPU probe (`idlecpu-samples.tsv`,
`step6-box-idlecpu.sh`) ran two fresh 10k-file hubs, one with
`flushFloorSecs: 3` (every rig's setting) and one with the default 60.
It sampled them more than 20 min after seeding: 5, 5, 5 m and 5, 5, 7 m.
So the flush floor does not matter, and steady idle CPU is about 5 m at
10k files. In phase A the 10k hubs finished seeding last, so their
window was nearest their writes; background work after a write burst
decays, and that is what the 4 → 8 → 11 ordering reflects.

**For §7a:** a real hub is ~90 MiB RSS whatever its size (1,000 hubs ≈ 90
GiB, under today's 128 Mi request per hub), and ~5 m CPU idle (1,000 ≈ 5
cores).

## B — wake times (`step6-box-wake.sh`, 6/6)

One hub per tier; 3 wakes each from suspend and from hibernate. Every
wake uses a fresh mount; all 18 wakes served every file.

| Wake from | Client: first byte | Client: full `find` | Server: pod created → Ready |
|---|---|---|---|
| suspend (disk kept) | 13–14 s | 13–14 s | 10–12 s |
| hibernate (CR alone; new disk, bucket import) | 26.5–26.8 s | 27–28 s | 13–15 s |

- **File count does not matter** for either kind of wake. The hub
  restores its listing from the manifest and fetches contents on read,
  so a 10k-file workspace is back as fast as a 1k one.
- **About 12 s of a hibernate wake is outside the hub pod.** For
  suspend the client pays 2–3 s over the pod (the DELAY retry step); for
  hibernate, about 12 s. Candidates, not yet split by timestamps:
  - the operator re-creating the Deployment before a pod exists;
  - publishing the new serverId;
  - the proxy's table refresh timer;
  - the client's DELAY backoff.

  This is the part of a wake worth cutting.

## C — what the proxy costs (`step6-box-proxy.sh`, 5/5)

One hub with 5k files, reached three ways from the host: direct (the pod
IP), through the proxy, and through the proxy with mTLS. Three reps
each. **Guards:** the direct arm's total proxy CPU was 87 ms, so it really
bypassed the proxy; a plain mount was refused once TLS was on; every
sequential read-back was intact.

| Workload | direct | proxy | proxy + mTLS |
|---|---|---|---|
| stat (every file, twice) | 4,982–6,304 ops/s | 2,633–2,961 ops/s | 2,511–2,656 ops/s |
| … proxy CPU per 1k ops | — | 173–189 ms | 196–207 ms |
| create + unlink (4k ops) | 57–104 ops/s | 50–93 ops/s | 44–60 ops/s |
| sequential read, O_DIRECT | 513–523 MiB/s | 372–410 MiB/s | 329–350 MiB/s |
| … proxy CPU per GiB | — | 644–770 ms | 940–1,012 ms |
| sequential write, O_DIRECT | 80–134 MiB/s | 115–188 MiB/s | 86–171 MiB/s |

- **The proxy's CPU per metadata operation (~0.18 ms) is about the hub's
  own (~0.12–0.16 ms).** One proxy core serves only about 5,500 metadata
  ops/s. That is the input for step 7 (multi-replica), and the first
  thing to profile: the proxy does far less work per op than the hub.
- Through the proxy, metadata runs at about half the direct rate on this
  path, and sequential reads at about 75%.
- mTLS adds about 10% proxy CPU on metadata and about 40% on bulk reads,
  with about 10% less throughput.
- Creates are hub-bound: 4–6 ms of hub CPU per operation in every arm,
  and noisy. Sequential writes are too noisy (3 reps) to rank the arms.

## D — the control plane at scale (`step6-box-fleet.sh`)

**Run 1, 500 live + 10,000 parked (6 workers): the box could not host
it** (`D-run1-500live-box-hung.txt`). All 10,000 parked shares were
Hibernated within ~5 min, but only 75 of 500 live stubs got Ready. kind's
local-path provisioner binds one PVC at a time, and at ~13 min the kind
apiserver pinned a core and stopped answering, with a load average ~8 on
8 cores. The operator had averaged ~0.85 core.

**The box did not hang; it lost its network.** Its journal (read
2026-10-01) shows `neighbour: arp_cache: neighbor table overflow!`
from 23:02:17 onward, 14 min into the run, with ~350 pods running. The
log is rate-limited, so it shows ~115 lines a minute until shutdown.
The table's hard limit is `gc_thresh3` = 1024 (the default), and it
counts every network namespace on the host. kindnet gives each pod
neighbor entries in both its node's namespace and its own. Once
existing entries aged out, the host could not resolve its own LAN
neighbors:
- ssh was last accepted at 23:09:00, then connections timed out;
- ping got no replies;
- the kernel kept logging until the clean shutdown at 23:22:01.

There were no hung tasks and no OOM kills. Run 2 (150 live) stayed
well under the limit. A host that runs this many kind pods needs
`gc_thresh3` raised. The box's limit is now 16384
(`/etc/sysctl.d/90-neigh-gc-thresh.conf`), and the rig aborts at 75%
of it (`arp_guard` in `rig-safety.sh`, checked every 10 s; the
load-average abort stays as a second guard).

A separate defect, which did not cause this: the step 5 rig's lazy
unmount of `/mnt/px5` at 18:14 left a hard mount's RPC client
retrying 172.18.0.4 for five hours (2,823 "not responding" lines). Every rig in this directory now unmounts with
`unmount_hard` (`rig-safety.sh`), which kills the holders first and
leaves no client behind. `rig-safety-check.sh` checks both helpers, 11/11,
with the old unmount sequence as its known-bad arm.

**Run 2, 150 live + 10,000 parked (2 workers): 5/6**
(`D-run2-150live.txt`, `D-mem.txt`, `D-api-rates.txt`). The box's load
stayed under 5.
- Converged in 322 s: 150 Ready (every one polled by the operator) and
  10,000 Hibernated.
- Operator settled: 235 MiB RSS (294 MiB peak), **181 m CPU**. Proxy: 235
  MiB, 22 m CPU. For comparison, runbv (AWS, 3,000 shares / 300 live):
  operator 53 MiB, 98 m.
- apiserver: **183 req/s** over 300 s — Deployments 37.8, PVCs 33.0,
  Services 31.0, and Secrets, Pods, ConfigMaps and FlintShares ~15.5
  each.

  **Correction, 2026-10-01:** the rate does not scale with parked shares.
  A smoke run of this rig at 20 live + 200 parked (one worker,
  `run-smoke-20live.txt`) saw **178 req/s**, with almost the same
  per-resource rates (Deployments 37.5, PVCs 31.1, Services 30.8, the
  rest ~15.4). So the earlier reading ("re-reading objects a CR-only
  share does not have") was wrong. A fixed rate like this looks like a
  periodic loop or a client-side rate limit, not per-share work. Cause
  not yet found.
- **Forced relist: the proxy relisted, the operator did not; the guard
  FAILED, correctly.** After etcd was compacted and the apiserver
  restarted, the proxy's peak rose from 235 to **334 MiB**: about +100 MiB
  on a ~200 MiB store, 1.5×, not the 2× the chart comment assumes. The
  operator's peak stayed at 294 MiB, and its log names no 410. The
  likely reason is watch bookmarks: they keep a watcher's
  resourceVersion current, so it resumed past the compaction. (That
  guess was wrong; see below.)
  **Answered 2026-10-02 (`D-relist-10k.txt`): the operator does relist
  at 10,000, about 4 minutes late, and its peak is ~2× (192 → 393 MiB).**
  The rig could not see it, for three reasons:
  - it waited 180 s, and the re-list began at +240 s;
  - it read a lifetime memory peak that the initial sync had already
    set;
  - its log check could not match. The operator logged every watch
    error as just "event queue error" (now fixed: it logs the error
    chain), and a bare "410" matched a log timestamp. That false match
    is also what the 220-share smoke "relist" above was.

  **The delay is a product issue.** kube's Controller puts ONE backoff
  over all its watches. After the apiserver restart each 410 froze every
  watch for ~35–45 s, and six 410s drained one at a time (3.5 min). No
  re-list could start until the last one had drained, and for those
  minutes the operator saw no watch events at all. Worse, a watch that had resumed
  cleanly (deployments) aged out while it sat unread behind the backoff,
  and took its own 410.

  **Fixed 2026-10-02:** the operator now gives the Controller its own
  backoff, `lite_operator::trigger_backoff`: 100 ms doubling to a 5 s
  cap, jittered, reset after a quiet minute. In the same 10,000-share
  run:
  - seven 410s cleared in 24 s instead of 3.5 min;
  - every re-list began within 29 s of the restart, instead of 4 min;
  - both FlintShare caches were rebuilt by about 33 s;
  - no watch aged out.

  The peak was 364 MiB from 236. Its tests failed against kube's default
  backoff, which the control computes at 280 s for six 410s.

## Profile — where the proxy's CPU per metadata op goes (`step6-proxy-profile.sh`)

The proxy and one hub run as host processes (no kind), from a
frame-pointer build; the host kernel mounts with `actimeo=0`; the
workload is 4 stat passes over 5,000 files, ×3
(`profile-cpu.txt`):

| | direct | through the proxy |
|---|---|---|
| ops/s | 8,474–9,394 | 4,332–4,410 |
| proxy CPU per 1k ops | — | 114.5–119 ms |
| hub CPU per 1k ops | 74.5–81 ms | 73.5–76 ms |

Without kind the proxy costs ~1.5× the hub's CPU per op (on kind,
173–189 ms included the container network).

`perf record -g` on the proxy (`profile-dso.txt`,
`profile-proxy-functions.txt`):
- **Kernel 48.5% + libc 7.2%.** Mostly TCP send and receive: a proxied
  operation costs the proxy two receives and two sends, where a direct
  mount has none. On loopback each send also pays the receiver's stack.
  This is the inherent cost of the extra hop. Each message is already a
  single write (marker and body together).
- **nf_conntrack + nf_tables 3.4%:** the box's Docker iptables, an
  environment cost.
- **The proxy's own logic is small:**
  - SEQUENCE through the embedded dispatcher, 9.9%;
  - the pseudo-root, 2.2%;
  - identity binding, 1.7%;
  - decode, 1.3%;
  - splice, ~1.5%;
  - encode, ~1.4%;
  - a task spawned per request, ~1.8%.
- **A scaling hazard inside that 9.9%: `courtesy_release_expired`, 5.3%
  with ONE client.** The dispatcher scans the whole lease map on every
  compound to reap expired clients. That suits a hub, which has few
  clients and wants a conflicting lock to self-heal at once. The proxy's
  clients are every node of every client cluster, so the scan grows
  with the fleet, per operation. That growth is inferred from the code;
  only the single-client cost is measured. The proxy needs no
  every-compound reap, since it holds no locks of its own.

  **Fixed 2026-10-01** (`leasegate-ab.txt`). The proxy had no periodic
  sweep: the per-compound pass was its only reaper. It now turns that
  pass off (`with_reap_on_compound(false)`) and reaps on a 1 s timer.
  Four alternating runs and a later fifth, one client:
  - the scan's share of proxy samples: before 5.5–5.7%, after 0.01%;
  - proxy CPU per 1k stat ops: before 113.5–118.5 ms, after
    107.5–113.5 ms. The ranges touch, so with one client the CPU drop
    is near the run-to-run noise; the perf share is the clean signal.

  The test `an_expired_downstream_client_is_reaped_on_a_timer_not_per_compound`
  pins the timer: with the laundromat removed, it fails (the control).

**For step 7:** size the proxy at ~1.5× the hub's CPU per metadata op
(~8,500 stat ops/s per core here). Most of that is the network hop
itself, so replicas, not micro-optimisation, are the lever.
