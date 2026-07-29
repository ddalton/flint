# runan — the S2 live gate (drill 3.12) + the F55 live gate (drill 3.13)

**2026-07-28/29, trove project 58**, 4× i4i.xlarge workers + spot CP, us-west-1,
k8s v1.34.10. Image `dilipdalton/flint-driver:1.22.0-rc3`
(`sha256:617dfecb97e1…`), built at main `ee31213`. Both gates **PASSED** on the
first run; no new F-number. Second consecutive campaign to find nothing new.

## What was owed, and what closed

| gate | owed since | result |
|---|---|---|
| **3.12** in-place RWX admission (S2) | the S2 implementation (`ee31213`) | **PASS** — 228 ms quiesce, no bounce |
| **3.13** bounce-mid-checkpoint (F55) | runam (`a4902ef` DrainGate) | **PASS** — drained, zero PANICs |

## Drill 3.12 — bounce-free RWX admission

Terminate a backing-leg node that is NOT the nfs-server's node, under live
postgres writes, so the server stays alive and the replacement can only be
admitted into the LIVE serving raid. Through v1.21.0 that admission rode the
cutover bounce (~237s outage). S2 admits in place.

```
node_gone=89s  degrade=77s  kill_stall=37s (P4 budget 60)
swap=141s -> runan-aws-4    standby=162s   in_sync=237s   settled=+360s
window_ms=228               admit_stall=1s (budget 10)
nfs pod uid 1dc9d694 UNCHANGED, restarts 0->0
pg-0 restarts 0->0          estale=0 panic=0
cutover events 0/0/0        witness fresh 609s, 0 mismatches
db PASS (zero acked loss, amcheck clean)
```

**The mechanism, in the controller's own words** (`s2-admission-trace.txt`):

```
[REPLACE]    identity swapped off lost node — full build queued  aws-2 -> aws-4
[CATCHUP]    replica caught up to warm standby                   base=empty
[CLAIMS]     wanted_op="catch-up" held_by="hot-rejoin"           <- the F43 yield
[HOT_REJOIN] Window committed                                    window_ms=228
[HOT_REJOIN] Localization complete
[HOT_REJOIN] Rejoin complete                                     localized=true
```

No `[CUTOVER]` line fires after startup. The admission never opens a second
assembly, so the F48 two-head phase and the F52 fh-identity re-proof are
removed structurally rather than guarded — and with no bounce there is no F55
exposure on this path at all.

**228 ms of quiesce replaces a ~237s bounce** — three orders of magnitude on
the RWX availability number, with durability unchanged (zero acked loss, as on
every drill to date).

### What the formal model predicted, and the drill confirmed

`FlintReplication`'s `AdmissionNotStarved` says a warm standby's wait always
resolves; its F43 mutation (`ClaimArb=FALSE`) finds the
`ReleaseCatchup → AcquireCatchup` starvation lasso and so proves the fix had to
be claim PRIORITY, not fairness. The `[CLAIMS] held_by="hot-rejoin"` line is
that priority observed on the wire: catch-up's tick found the volume already
claimed by the window and skipped, rather than renewing over it.

## Drill 3.13 — F55, bounce mid-checkpoint

runam found that a bounce truncates in-flight NFS replies → client EIO → pg
checkpointer PANIC, and that every prior "clean" bounce drill was
checkpoint-phase luck. This drill removes the luck: it tightens the cadence,
waits for `checkpoint starting` in the postgres log, and kills inside that
window.

```
ckpt_at=32s  kill=32s  back=46s  io_resume=54s
drained=1    drain_deadline_expired=0
panic=0      eio=0     pg_restarts=0->0     db PASS
```

The DrainGate marker (`drained in-flight replies`) is asserted present — a
clean run on a pre-fix image would otherwise prove only that the checkpoint
missed the window, so its absence FAILS the drill as INVALID rather than
passing quietly.

## Fleet / config census

- every driver-image pod on `1.22.0-rc3`; `FLINT_RWX_INPLACE_ADMISSION` unset
  → ON (`pods-running-driver-image-post.txt`)
- exactly ONE process with orchestrators ENABLED (`orchestrator-decisions.txt`)
  — the F50/F53 invariant
- `Dead-target timeouts applied` on all 5 nodes (`p4-dead-target-markers.txt`)
  — P4 reproduces off-runam: kill stall 37s here vs 36s there, vs the 150–177s
  class before the fix
- 4 worker lvstores @ 869Gi, CP has none (`lvstore-census.txt`)

## Harness changes this campaign

- **new drill 3.12** — the 3.6e vector with the inverted expectation: a bounce
  is the FAILURE. Gates on nfs pod uid + restart count unmoved, zero
  `CutoverStarted`, admission stall ≤ budget, and the driver's own `window_ms`.
- **new drill 3.13** — the F55 gate, forced rather than lucky.
- **`max_stall_between`** (lib.sh) — the admission's stall has to be measured
  apart from the kill's detection stall, which precedes it and is an order of
  magnitude larger.
- **`deansi`** (lib.sh) — tracing's ANSI attributes sit BETWEEN a field name
  and its `=`, so an un-stripped `grep -o 'window_ms=[0-9]*'` silently matches
  nothing. Field greps need it; message greps don't.
- **3.6e** now reads the kill-switch state and flips its bounce expectation:
  with S2 ON a bounce-free landing is the design, not an anomaly.

## Traps

1. **`psql` in the harness needs `-U postgres -d bench`.** A bare
   `psql -qtAX -c 'SHOW ...'` returns EMPTY, not an error — 3.13's first run
   read an empty `checkpoint_timeout` and refused to proceed. The empty read is
   the tell.
2. **Controller-side markers are invisible to `driver_log_hits`**, which greps
   only the csi-node containers. It reported `yields=0 seizures=0` for markers
   that only ever appear in the controller log. Fixed to read `ctrl_log`, plus
   a `held_by` counter that captures the yield directly.
3. **`window_ms` is not the whole admission.** The record flips to `in_sync`
   at localization (`Rejoin complete`), ~39s after the window commits — a
   mid-flight check will show `standby` against a raid that is already 2/2.
   That is the ordering, not a gap.
4. **Docker Desktop wedged in "stopping"** before the build; the CLI reported
   `dial unix docker.raw.sock` while the backend ran. `pkill -9 -f com.docker`
   then relaunch. The build script now proves the image exists before claiming
   success — it once printed DONE over a dead daemon.

Related: `docs/s2-bounce-free-rwx-admission.md`,
`docs/f55-bounce-truncated-reply-eio.md`, `docs/p4-dead-target-detection.md`,
`formal/README.md`, `artifacts/runam-p4-f55-gate/`.
