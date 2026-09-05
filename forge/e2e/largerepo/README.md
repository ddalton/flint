# The large-repository leg

Every other forge drill runs in one size regime. `f1-durability` pushes
48x256 KiB (12 MiB); `lfs.sh` pushes 4 MiB. The whole-put ceiling is
**64 MiB**. So the suite could not reach the multipart upload path, the
restore's memory behaviour, or anything that depends on a restore
taking real time.

Three shipped defects lived in that gap:

| defect | needs | suite's max |
|---|---|---|
| the multipart upload had never run in any test or leg | a pack > 64 MiB | 12 MiB |
| the restore held every pack **twice** (2.05x of object size) | a GB-scale repo | 12 MiB |
| a restore outlasting the 60 s takeover window loses the repo | a 40-80 s restore | sub-second |

None are subtle. They were invisible because every leg stands in the
one place from which they do not exist.

## Running it

```sh
./run-largerepo.sh              # 160 MiB of incompressible data
REPO_MB=512 ./run-largerepo.sh  # bigger
KEEP=1 ./run-largerepo.sh       # keep $WORK
```

Needs Docker (MinIO on port 9101, its own bucket and container name, so
it does not collide with the composition rig) and the binaries built
**with the s3 feature** — see the precondition below.

## What it decides

- **L1 — a pack above the ceiling is uploaded multipart.** Proven from
  the outside by the object's ETag: S3 and MinIO give a composed object
  a `-<partcount>` suffix and a whole PUT a bare MD5. That suffix is
  the only externally visible evidence of *which* upload path ran.
- **L2 — a restore does not need as much memory as the repository is
  big.** The invariant is deliberately crude, because the defect was
  crude: peak memory must be under the pack size. The pre-fix code read
  202 MiB for a 96 MiB pack — the 2.05x measured independently in
  `flint-forge-design.md` §5. The shipped code's peak is a FLOOR, not a
  ratio: ~24 MiB + 20 MiB per chunk in flight, measured 43 MiB at
  `FANOUT=1` and 104 MiB at the default fanout 4 on a 160 MiB pack. So
  the pack must sit above that floor with margin; the leg refuses
  (INCONCLUSIVE) a `REPO_MB` that does not.
- The restore is also checked for **correctness**, not just cost:
  `git fsck --strict` and the tip matching what was pushed. A cheap
  restore that produces the wrong bytes is worse than an expensive one.

## Two traps this leg is built around

**`cargo build --bins` silently skips the syncer.** `flint-forge-syncer`
carries `required-features = ["s3"]`, so a plain build leaves whatever
binary was there before at exactly the path `FORGE_BIN` points to, and
the drill then reports green about code that is not in the tree. This
happened for real while writing this leg. The `binary_is_fresh`
precondition refuses to run when any source under `forge/syncer` or
`crates/flint-store` is newer than the binary, and names the rebuild.

**RSS is the wrong instrument.** macOS compresses inactive pages and
its RSS does not count them: reading RSS reported a 2 GiB fetch as
1.05 GB and *plateaued*, which would have called this defect benign.
The leg reads `peak memory footprint`, falling back to maximum RSS only
where that metric does not exist.

## INCONCLUSIVE is not PASS

A leg that could not measure what it exists to measure reports
`INCONCLUSIVE` and the run exits 2 — it is not folded into "0 failed".
Borrowed from `tests/k8s/oci-ab`, which earned the rule.

## Not covered here

The takeover-during-restore window (design §5) needs a restore lasting
tens of seconds. On loopback MinIO a 96 MiB restore returns in about a
second, so this rig cannot reach it honestly; it would need injected
latency or a real backend. Deliberately absent rather than faked.

The round-trip cost of a push and a restore is the neighbouring
`../latency/` leg, which injects latency with toxiproxy: it measures the
sibling fan-out and the restore fan-out against a fanout-1 control. Its
slowest restore here is 8.7 s, still nowhere near the 60 s window.
