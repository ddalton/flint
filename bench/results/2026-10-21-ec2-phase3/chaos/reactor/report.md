# Two SPDK reactors (`-m 0xC0`) vs one — PARTIAL (bench4, 2026-10-22)

Flint 1.60.1 (spdk-tgt 1.7.1), r3, epochs on (300 s), 3 x i4i.2xlarge.
Arm `flint-reactor-0xc0-r3`; baseline `flint-a-again2-r3` (1 reactor, same
cluster, 3 reps). **Stopped by the user after rep 1 of 7 tests** — one
sample per test, so read the differences as direction, not size.

| Test | 2 reactors (1 rep) | 1 reactor (3 reps) |
|---|---|---|
| 4K randread QD32 | 237,472 IOPS (p50 518 µs) | 179,327 IOPS (p50 700 µs) |
| 4K randwrite | 69,327 IOPS | 49,000–69,000 IOPS |
| 70/30 mixed | 83,980 IOPS (1 rep; may have overlapped an epoch cut) | 107,000–131,000 IOPS |
| seq read 1M | 2,629 MiB/s | 2,771 MiB/s |
| seq write 1M | 523 MiB/s | 523 MiB/s |
| QD1 | unchanged | — |

Reading: a second reactor lifts queue-depth-32 random reads by about a
third (the single reactor is ~100% busy there) and leaves sequential and
QD1 unchanged; random write and mixed need more reps. Decision (user,
2026-10-22): 1 reactor stays the default; 2 reactors (cores other than 0)
is a documented option for large or IOPS-heavy nodes. The first attempt at
this arm measured nothing: the driver name `flint-reactor0xC0-r3` had
upper-case letters, invalid in Kubernetes names.
