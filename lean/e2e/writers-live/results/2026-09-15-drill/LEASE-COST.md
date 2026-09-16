# Where a barrier's time actually goes — R3-S2-churn-ui, 2026-09-15

Six writers on three nodes, real S3, churn + UI on shared files. Measured
from the storm traces already on disk (`traces/*.jsonl`), not modelled.

**A correction first.** An earlier pass reported the cell costing ~1% of
barrier time. That was wrong. It summed the `waited_ms` FIELD, and a
waiting writer emits exactly ONE `claim verdict=waiting` event per grant
(max 1, measured) and then blocks silently — so that field is the first
poll's interval, not the wait. Measuring first-wait to grant:

| | share of barrier time |
|---|---|
| **waiting for the cell** | **79%** (1,402 s of 1,778 s) |
| holding the cell | 9% (165 s) |
| the consume loop | 4.5% (80 s) |

    per barrier: p50 wait 3,118 ms of a p50 4,064 ms barrier
    414 of 427 barriers waited at all

The silent waiter is itself an observability gap, the same shape as the
GC's untraced outranked branch (L-101): the trace cannot show the poll
cadence, which had to be read out of `lease.rs`.

## The grant cycle

    cell HELD           p50  383 ms
    cell IDLE between a release and the next grant
                        p50  284 ms   (p90 697 ms, 174 s total)

So a grant cycle is ~667 ms, of which **43% is nobody holding the cell** —
the next holder has not noticed yet. Over the leg the cell is idle
between holders (174 s) slightly LONGER than it is held (165 s). A writer
queues about 4.7 cycles deep, which is the 3.1 s wait.

Cadence, from `lease.rs`: the queue HEAD polls every 200 ms
(`CLAIM_POLL_HEAD_MS`); everyone else every 1 s (`CLAIM_POLL_SECS`). The
constant's own comment makes the argument for the head — "every
millisecond it takes to notice is a millisecond the cell stands idle with
the rest of the queue behind it" — and that argument does not stop at the
head: a waiter becoming head discovers it on the 1 s tick.

## Three levers, measured

| lever | worth | costs |
|---|---|---|
| **don't claim for a no-change barrier** — 91 of 427 grants (21%) were `no_change: true` and still took the cell (312 s of wait, 28 s of hold) | ~21% of queue depth | nothing; no protocol change |
| **shorten the handoff gap** | 43% of every cycle | request volume (polling) |
| `OptimisticCAS` (lease only for the GC) | the pre-CAS half of the hold: 200 ms of 667 ms, ~30% of the cycle | `Inv_CommitExclusive` retires; three shipped fixes lose their safety argument; every exhaustive world shrinks |

The two that need no protocol change are together worth more than the one
that does, and they cost nothing in verifiability.

## The consume loop

One round trip per entry, serially: p50 34 ms between consecutive
`consume` events (2,227 gaps), p50 6 entries per barrier, max 22. 80 s
over the leg — real, but an order below the lease.
