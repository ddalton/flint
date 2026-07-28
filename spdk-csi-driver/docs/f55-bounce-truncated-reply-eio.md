# F55 — cutover-bounce truncated reply → client EIO → postgres PANIC

**Found on runam (2026-07-28), drill 3.6e on `1.22.0-rc2` — the first
finding after the runal no-new-F campaign. Fixed same day (drain-then-exit);
live gate owed.**

## Symptom

Four seconds after the replacement leg reached in_sync, postgres's
checkpointer PANICked: `could not fsync file ... Input/output error`
(fsyncgate). The abort then wedged ~5 minutes in D-state on the dying
mount, and postmaster reinit + WAL redo produced a 227s ledger stall —
self-recovered, **zero acked-write loss** (1663/1663), zero container
restarts. The drill's db verdict flagged it via the pglog corruption scan.

## Attribution (deterministic, three experiments on the live cluster)

1. **Ruled out the new instance**: its full log shows no WRITE/COMMIT
   error replies; persistent state restored completely (2 clients, 38
   stateids); sessions re-created ~1s after startup. Ruled out the
   client's mount manufacturing EIO: it is `hard,timeo=600` (no
   soft-timeout path).
2. **Manual repro**: bounce the NFS pod mid-checkpoint (30s
   checkpoint_timeout makes the straddle near-certain) → PANIC 0.6s
   after `kubectl delete pod`, before the new instance existed, on a
   healthy 2/2 raid. The 3.6e context (node loss, degraded raid) is
   irrelevant — **the bounce alone is the bug**, and every prior clean
   3.6e simply never straddled a checkpoint (phase luck).
3. **Quiesced-bounce discriminator**: load scaled to 0, clean CHECKPOINT,
   idle bounce → zero PANICs, and a forced post-bounce synced write
   succeeded. So state/session recovery is sound and the poison strictly
   requires **a sync operation in flight at the kill instant**.

## Mechanism

`nfs_main`'s SIGTERM arm believed "open TCP connections dropped; clients
recover via persisted state" — which is true *between* frames. But
connection handlers are `tokio::spawn`ed tasks: when main returns, the
runtime drop cancels them at whatever await point they're at, including
**mid-reply-write**. A truncated RPC record followed by FIN is not a
retransmittable condition like an unanswered request — the client turns
it into an immediate error on the in-flight COMMIT/fsync. (The old
instance's log capture — added to 3.6e for this finding — shows the
SIGTERM line 6ms before the client-side PANIC.)

## Fix — frame-atomic shutdown (`DrainGate`, nfs/pipeline.rs)

On SIGTERM: `begin()` stops admitting work — `submit` refuses new frames
(unreplied requests are retransmitted by the client against the next
instance; that is the safe half of the protocol) and every connection
read-loop closes at its next frame boundary — then `drain()` waits for
all replies already past dispatch to finish flushing, bounded by
`FLINT_NFS_DRAIN_MS` (default 3000ms). The bound is load-bearing: the
F33b prompt-exit obligation (lazy-umount data loss at kubelet's grace
deadline) caps shutdown, so an expired deadline exits anyway and logs
the count it abandoned. The in-flight token is RAII so panics and
cancellations keep the count honest.

Tests: `f55_drain_waits_for_inflight_reply_write`,
`f55_submit_refused_once_draining`, `f55_drain_deadline_expires_dirty`.

## Gate

The runam manual repro, inverted: bounce the NFS pod mid-checkpoint on
the fixed image → pg must ride through with zero PANICs (the 3.6e pglog
check enforces this in every future run; the drill now also streams the
outgoing server's log so a dying instance can never again be the
unobserved actor).

## Relation to S2

S2 (RWX admission without the bounce) removes this whole class along
with the bounce's ~50s reconnection gap. F55's fix makes the bounce
*correct*; S2 would make it *unnecessary*. Both stand.

## Evidence

`tests/chaos/artifacts/runam-p4-f55-gate/`: the 3.6e bundle
(`3-3.6e-1785268106/`, first observation), `f55-outgoing-server.log`
(the dying instance's final line vs the PANIC timestamp),
`f55-quiesced-outgoing.log` (the clean discriminator), drill stdout logs.
