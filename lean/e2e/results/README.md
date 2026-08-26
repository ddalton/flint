# Drill evidence

Verbatim run output, committed on purpose. These runs cost real time and
a real bucket, and until 2026-08-26 they lived only in an ephemeral
session scratchpad — the same place the radar generator spent five weeks
before it was nearly lost. A number quoted in a plan or a score should
be re-readable here without re-running anything.

| file | what it is |
|---|---|
| `bucket-drill-2026-08-26.log` | `run-verbs.sh`, all 27 legs (B1–B25, B11 split a/b/c) in ONE end-to-end pass against a real MinIO: **27 passed, 0 failed, 0 skipped** |
| `formal-gate-2026-08-26.log` | `lean/formal/check.sh`: **61/61** — every strict run green, every mutation and probe finding its designated counterexample |
| `real-s3-qualification-2026-08-26.md` | the chart installed from production images against **real S3 over TLS** — the one path no MinIO rig can exercise |

Re-run either with `./run-verbs.sh` (rig resets itself) or
`lean/formal/check.sh`. `ONLY=B9 ./run-verbs.sh` runs a single leg.
