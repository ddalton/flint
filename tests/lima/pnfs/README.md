# pNFS Test Suite

Two end-to-end test runners for the flint pNFS implementation:

| Target | What it validates |
|---|---|
| `make test-pnfs-smoke` | The data path is reachable end-to-end. Mounts NFSv4.1 from the Lima VM, writes 24 MiB, reads it back, checks the SHA-256, and counts bytes that landed on each DS. |
| `make test-pnfs-pynfs` | The wire protocol matches RFC 8881. Runs the pynfs `pnfs` flag set (8 tests) against the MDS endpoint. |

Both spin up `flint-pnfs-mds` plus two `flint-pnfs-ds` instances on the macOS host, then drive them from inside the Lima VM.

## Quick start

```bash
make build-pnfs          # one-time: build the pNFS binaries
make test-pnfs-smoke     # ~15 s: mount + write + read + per-DS byte count
make test-pnfs-pynfs     # ~30 s: 8-test conformance run
make test-pnfs-all       # both, sequentially
```

## What "PASS" / "DEGRADED" / "FAIL" mean

`smoke.sh` reports one of three outcomes:

- **`✓ PASS`** — bytes hit *both* DSes (real pNFS striping working).
- **`△ PARTIAL`** — bytes hit one DS only (round-robin / unbalanced policy).
- **`△ DEGRADED`** — mount + checksum round-trip succeeded, but **zero bytes reached either DS**. All 24 MiB landed on the MDS export. This is the audit's "pNFS data path is not real" gap: the MDS is acting as a single NFS server while pretending to advertise pNFS layouts.

`DEGRADED` is the **current expected baseline**. The smoke test exits 0 in this state — it's a regression guard, not a goal post. Once the data path is fixed (Tasks #4 / #5 / #6 from the original audit, plus DS-side I/O wiring), the assertion logic in `smoke.sh` should be tightened to require `PASS`.

## Topology

```
Lima VM (Ubuntu 24.04, NFSv4.1 client)
  │
  │ mount -t nfs4 -o vers=4.1,port=20490 host.lima.internal:/ /mnt/flint-pnfs
  ▼
host.lima.internal (macOS)
  ├─ MDS  : NFS port 20490, gRPC port 50051
  │     │  exports: /tmp/flint-pnfs-mds-exports
  │     ▼
  │   gRPC heartbeat & registration
  │     ▲
  ├─ DS₁ : NFS port 20491, exports /tmp/flint-pnfs-ds1
  └─ DS₂ : NFS port 20492, exports /tmp/flint-pnfs-ds2
```

The mds.yaml lists the DS endpoints by their host-internal address (`host.lima.internal:2049{1,2}`) so layouts handed back to the client point at routes the client can actually reach.

## Files

```
tests/lima/pnfs/
├── README.md                       this file
├── mds.yaml                        MDS config (8 MiB stripe, policy=stripe, 2 DSes)
├── ds1.yaml / ds2.yaml             DS configs (different deviceId / port / export dir)
├── smoke.sh                        data-path smoke test
├── pynfs.sh                        pynfs `pnfs` flag-set runner
└── baseline-pynfs-*.json           per-commit pynfs result snapshots
```

## Debugging a failed run

Each binary writes to a separate log file. After a failure:

```bash
tail -100 /tmp/flint-pnfs-mds.log
tail -100 /tmp/flint-pnfs-ds1.log
tail -100 /tmp/flint-pnfs-ds2.log
```

Common failure modes seen so far:

- **`Read-only file system (os error 30)` during OPEN** — `mds.yaml` `exports[].path` was set to `/`. Use a writable scratch dir.
- **`Registration gRPC call failed: transport error`** — DS pointed at the MDS NFS port (20490) instead of the gRPC port (50051). Check `mds.endpoint` in `ds1.yaml` / `ds2.yaml`.
- **mount hangs with `Connection refused`** — MDS died on startup; check `flint-pnfs-mds.log` for an early error (often a YAML schema mismatch).

## Adding a new test

Both scripts trap-cleanup on exit, so a new test can be a third `*.sh` script that follows the same shape: source-pre-flight → start MDS+DSes → run an assertion against `host.lima.internal:20490` from inside the VM → report. Wire it into the Makefile alongside `test-pnfs-smoke` / `test-pnfs-pynfs`.
