# P4 — RWX node-loss detection latency: attribution and fix

**Status: fix implemented 2026-07-28 (`DeadTargetTimeouts`), live gate owed
(3.6e stall ≤60s).** Measured stall class before the fix: 150–177s across
three clusters (runai ~150s, runak 159s, runal 177s).

## The measured problem

Drill 3.6e (terminate a leg node under RWX postgres writes) stalled the
ledger 150–177s while `fast_io_fail_timeout_sec=20` (F42) was configured on
every leg controller. RWO 2.5 under the same policy never stalled at all —
which made the RWX path look structurally slower. It isn't. The two runs
differ only in luck.

## Attribution (runak clean 3.6e, complete logs; T0 = 04:15:26Z)

| T0+ | event |
|-----|-------|
| 0s | instance terminated (leg node) |
| ~6s | last ledger ack — writes now block on the mirrored raid write |
| 56s | Node object deleted (drill) — replacement armed |
| 116–176s | **the gap lives here: SPDK raid still reports base=2/2** |
| 176s | `[MONITOR] RAID degraded` + `Replica marked stale` (60s-cadence pass) |
| ~165s | ledger acks resume (≈ the moment the raid deconfigured the base) |
| 186s | `[REPLACE]` identity swap (next controller tick) |
| 199s | catch-up to warm standby |

Everything downstream of the base-bdev failure is fast: stale-mark within
one monitor tick, swap 10s later, catch-up 13s after that. The entire stall
is the initiator not *failing I/O against the dead leg*.

## Root cause: the TCP blackhole

`fast_io_fail_timeout_sec` starts counting when the controller enters the
**reset/reconnect path** — i.e. after a transport error. A terminated
instance produces no RST: the peer vanishes, the kernel retransmits
silently, and with SPDK's defaults nothing bounds this —
`transport_ack_timeout=0` (no TCP_USER_TIMEOUT), `timeout_us=0` (no command
watchdog, `action_on_timeout=none`). The qpair sits "connected", I/O queued,
until the kernel's own retransmission limit gives up (tcp_retries2 ≈ 15+
minutes) or some intermediate event (ARP expiry, conntrack, the instance's
IP being reassigned) surfaces an error — observed at 116–176s, which is
noise, not design.

RWO 2.5 passed because that shutdown's ordering produced an RST while the
OS was still up → immediate transport error → reset path → fast_io_fail
armed at 20s. Same code, different shutdown race. Three RWX runs in a row
hit the blackhole; the drill terminates the instance the same way each
time, so whatever ordering the leg node's shutdown takes, it consistently
kills networking before spdk-tgt dies visibly.

## The fix — `DeadTargetTimeouts` (nvme_recovery.rs)

Global `bdev_nvme_set_options`, applied by the node agent:

| field | default | effect |
|-------|---------|--------|
| `transport_ack_timeout` | 13 (2¹³ ms ≈ 8.2s) | TCP_USER_TIMEOUT on every qpair socket: the **kernel** errors a blackholed connection once retransmitted data goes unacked that long |
| `timeout_us` + `action_on_timeout=reset` | 30s | command watchdog for the complementary failure: peer kernel ACKs but the target is wedged. A spurious trip costs one reset/reconnect cycle, never data |
| `tcp_connect_timeout_ms` | 10s | bounds each reconnect attempt against a blackholed address so the retry loop stays live |

Env knobs: `FLINT_SPDK_TRANSPORT_ACK_TIMEOUT_EXP`,
`FLINT_SPDK_IO_TIMEOUT_SECS`, `FLINT_SPDK_TCP_CONNECT_TIMEOUT_MS` (0
disables each; garbage falls back to defaults). No chart change — defaults
are compiled in.

**Ordering constraint:** SPDK returns -EPERM once any NVMe controller
exists, so the options are applied (a) at agent startup *before*
`discover_local_disks` attaches the local PCIe controller, and (b) in the
baseline-collapse recovery path the moment a tgt restart is detected,
before recovery re-attaches. An agent-only restart gets a tolerated -EPERM
(the running tgt already has them).

**Expected post-fix math:** blackhole detected ≈8s → reset path →
fast_io_fail fails queued I/O at +20s → raid1 fails the base on the first
failed child write (raid1.c: `raid_bdev_fail_base_bdev`) → writes resume
≈30s. The NFS client's retransmission backoff can add seconds on top. The
control-plane chain (stale-mark ≤60s later, swap ≤60s after that,
catch-up, cutover bounce) is unchanged — it restores *redundancy*, and it
was never the availability bottleneck.

## Gate

3.6e now records `degrade=<s>` (a 5s watcher timestamps base=2/2 → 1/2 on
the server node) and evaluates the ledger stall against
`P4_STALL_BUDGET` (default **60s**): ≤30s "never stalled", ≤60s "P4 budget
MET", above → "P4 BUDGET EXCEEDED". The acceptance gate for the next
campaign is a 3.6e PASS with the budget met and the decomposition in
results.csv.

## Residuals

1. **Set-options race after a tgt restart under live consumers:** a
   NodeStage/hot-rejoin attach can land on the fresh tgt before the
   agent's 30s collapse detector runs set_options → that boot keeps
   pre-P4 detection. Bounded: converges on the next clean boot; the
   csi-node roll landmine already marks tgt-restart-under-load as a
   reduced-guarantee zone.
2. **The cutover bounce's own gap (~50s: pod restart + session
   re-establishment) becomes the largest remaining RWX stall** once
   detection is fixed. That is S2's reframe — RWX admission without the
   bounce — and is deliberately out of this tranche's scope.
3. The kernel-initiator side (consumer `nvme connect`) has its own bounds
   via `ReconnectPolicy` and kernel defaults (kato + nvme_io_timeout);
   this fix is the SPDK-initiator mirror of that, not a replacement.

## Evidence trail

runak `tests/chaos/artifacts/3-3.6e-1785212126/driver-logs.txt` (the
complete chain), runal `runal-p1p3p4-gate/3-3.6e-1785255529/` (stall 177s,
swap tick 213s; server-node agent logs lost to the same-day spot reclaim),
runai note in phase3.sh (last ack T0+6s, controller `resetting` at
T0+148s). SPDK v26.05.1-pre source verified at ~/github/spdk:
`lib/sock/sock.c` (ack_timeout → TCP_USER_TIMEOUT),
`module/bdev/nvme/bdev_nvme.c` (`spdk_bdev_nvme_set_opts` -EPERM guard),
`module/bdev/raid/raid1.c` (fail-base-on-write-error).
