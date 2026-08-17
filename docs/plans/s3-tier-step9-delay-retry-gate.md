# Step 9 rig gate — NFS4ERR_DELAY retry behavior across multi-minute hydrations

**Verdict: GO.** The Linux kernel client retries `NFS4ERR_DELAY` indefinitely at a
flat ~0.1 s cadence, never surfaces an error to the application, resumes within
~0.2 s of hydration completing, and keeps the rest of the mount fully responsive.
Two operational findings below are inputs to steps 10–11, not blockers.

- Design of record: `docs/plans/s3-tier-l2-design-review.md`, implementation
  order item 9 ("a hard go/no-go before any destructive code ships"), closing
  gate (b) as re-scoped by A5 (hydration parks concurrent I/O on
  `NFS4ERR_DELAY`; the in-RPC-hold half of gate (b) was measured earlier —
  commit `afd0c18`, 70 s holds clean).
- Rig: `tests/lima/pnfs/delay-retry-gate.sh <secs> [read|all]` — standalone hub
  (`lite.yaml`) on the macOS host with `FLINT_TEST_HYDRATION_DELAY_SECS=N`;
  Ubuntu 24.04 kernel 7.0.0-28-generic client in lima; default mount options
  (v4.1, TCP, timeo=600). The injector answers every READ/WRITE of a `.cold.`
  file with `NFS4ERR_DELAY` until N seconds after first touch, logging each
  answer with attempt number + elapsed — the server log is the cadence record.
- Measured 2026-08-17.

## Data

| leg | park | app elapsed | overshoot | DELAY answers | retry gaps | integrity | dmesg |
|-----|------|-------------|-----------|---------------|------------|-----------|-------|
| read | 60 s | 60.21 s | **0.21 s** | 556 | flat ~0.1 s | sha256 match, rc=0 | quiet |
| read | 180 s | 180.18 s | **0.18 s** | 1 679 | flat ~0.1 s | sha256 match, rc=0 | quiet |
| read | 300 s | 300.10 s | **0.10 s** | 2 797 | n=2796 min=0.101 mean=0.107 max=0.120 s | sha256 match, rc=0 | quiet (read side) |
| write (dd 1 MiB + fsync) | 60 s | 60.05 s, rc=0 | 0.05 s | 1 074 | ~0.06 s | — | quiet |
| write | 300 s | 300.02 s, rc=0 | 0.02 s | 5 372 | ~0.06 s | — | hung-task INFO (below) |
| warm reads DURING park | 60/300 s | max 0.177 s / 0.031 s | — | — | — | — | quiet |

## What the gate establishes

1. **The client never gives up.** Five full minutes of continuous
   `NFS4ERR_DELAY` on both READ and WRITE: no application-visible error, no
   short read, no session reset, no `server not responding`, no retransmission
   storm. `sha256sum` and `dd conv=fsync` both return 0 and the content is
   byte-identical.
2. **Resume latency is excellent.** Overshoot past the simulated hydration was
   0.10–0.21 s on reads — the flat 0.1 s retry clock means the app resumes on
   the first retry after hydration completes. A multi-minute hydration costs
   the reader the hydration, plus nothing measurable.
3. **DELAY-parking does not stall the mount.** Warm reads of another file
   during a park: ≤ 177 ms (cache-cold) / ≤ 31 ms (warm). This is the A5
   posture working as designed — the slot is released, unlike an in-RPC hold
   which pins one of the session's 64 slots per parked RPC.

## Finding 1 — the retry clock is FLAT, not exponential

Expected: `nfs4_delay`-style exponential backoff (0.1 s → 15 s cap). Measured:
**flat ~0.107 s mean, max 0.120 s, for the entire five minutes** — the data-op
retry path re-drives the RPC on a fixed ~100 ms clock. Consequences:

- Sizing input for step 11: **each parked reader costs the hub ~9–10 READ
  RPCs/s; each parked writer ~17–18 WRITE RPCs/s**, for the whole hydration.
  Every answer is a cheap immediate reply (no I/O, no locks), so 100 parked
  files ≈ 1–2 k RPCs/s of pure protocol chatter — acceptable, but it belongs
  in the hydration-concurrency math, and the A12 meter's DELAY-answer counter
  will make it visible in production.
- The good half: there is no 15 s backoff cliff — resume latency stays ~0.1 s
  no matter how long the hydration ran.

## Finding 2 — write-parks longer than ~2 minutes log a client hung-task INFO

The 300 s write leg produced the standard khungtaskd warning on the client
(`INFO: task dd blocked for more than 122 seconds`, stack through
`nfs_file_fsync → folio_wait_writeback`), kernel not tainted, task completed
successfully afterward. Reads never trigger it (NFS read waits are killable;
khungtaskd only flags uninterruptible tasks — fsync's writeback wait is
uninterruptible). Notes:

- This is inherent to ANY multi-minute write-park, in-RPC hold or DELAY alike —
  the wait is client-side in fsync either way. It is cosmetic on default
  configs, but a fleet running `kernel.hung_task_panic=1` would PANIC the
  client. Step 11 must therefore **prioritize hydration of files with pending
  WRITEs** (bound the write-park, not just the read-park), and the docs should
  name the sysctl caveat.
- v1 scope note: the write gate's exclusion (eviction) parks writes only for
  the exclusion window, far under 120 s; only whole-file hydration can reach
  this threshold.

## Disposition

- Steps 10–12 are unblocked; this record is the gate artifact.
- Step 11 requirements fed by this gate: write-pending hydration priority
  (finding 2), DELAY-answer meter + parked-file count in the A12 surface
  (finding 1), and no need for any client-side mount-option guidance — default
  mounts behave correctly.
