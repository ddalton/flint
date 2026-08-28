# Drill evidence

Verbatim run output, committed on purpose. These runs cost real time and
a real bucket, and until 2026-08-26 they lived only in an ephemeral
session scratchpad — the same place the radar generator spent five weeks
before it was nearly lost. A number quoted in a plan or a score should
be re-readable here without re-running anything.

| file | what it is |
|---|---|
| `bucket-drill-2026-08-26.log` | `run-verbs.sh`, all 27 legs (B1–B25, B11 split a/b/c) in ONE end-to-end pass against a real MinIO: **27 passed, 0 failed, 0 skipped** |
| `formal-gate-2026-08-26.log` | `lean/formal/check.sh` as it stood that day: **61/61** |
| `formal-gate-2026-08-27.log` | the same gate at HEAD: **65/65** — every strict run green, every mutation and probe finding its designated counterexample |
| `real-s3-qualification-2026-08-26.md` | the chart installed from production images against **real S3 over TLS** — the one path no MinIO rig can exercise |

Re-run either with `./run-verbs.sh` (rig resets itself) or
`lean/formal/check.sh`. `ONLY=B9 ./run-verbs.sh` runs a single leg.

## An evidence log pins what ran that day, not what the gate expects today

The 61/61 above was quoted as "the formal gate" for a month, and a chart
in `docs/` still said 61 while `check.sh` had grown to declare
`$PASS/65`. A stale artifact and a current script disagree silently:
the artifact is not wrong, it is just *old*, and nothing links them.

So when you quote a gate, check the script's declared total as well as
the newest log — and when the gate grows, drop a fresh log in here.

(Re-running the gate needs ~1.5G for `lean/formal/states/`. The
2026-08-27 run first died at 43/65 with `No space left on device`,
which reads exactly like a spec failure until you look at the last line.)
