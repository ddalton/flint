# SPDK reactor busy %, sampled by hand during the r3 run (2026-10-07)

`framework_get_reactors` on each node's spdk-tgt (one reactor, lcore 0),
two samples 20 s apart, busy = Δbusy / (Δbusy + Δidle) ticks. Process CPU
cannot show this: a polling reactor is 100% of its core either way.

| Test (r3, rep 1) | fio node bench2-aws-2 | bench2-aws-1 | bench2-cp-1 |
|---|---|---|---|
| iops_mixed70_4k (10 s window) | 36.9% | 22.1% | 18.1% |
| seq_write_1m (20 s window, after ramp) | 61.2% | 21.9% | 22.6% |

The JSON pairs for seq_write_1m are beside this file (the mixed-test pair
was not kept). No reactor was saturated in either test: whatever bounds
r3 throughput (precondition 1M write: 368 MiB/s vs 1,064 at r1), it is
not the single reactor core.
