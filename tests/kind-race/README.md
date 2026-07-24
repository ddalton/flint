# Kind race-test tier

The cheap, deterministic tier for flint's **control-plane concurrency** — the
failure classes the contract's R1/R2/R5 rules address, which live AWS/real-
SPDK drills validate only expensively (and which is why F43's drill got
skipped: "needs SSM+EC2"). This tier makes those races reproducible on a
laptop with no cloud, no hugepages, no amd64.

## Why it fits a small arm64 machine

The race tier tests the **controller** and the node-agent's *decision* logic,
not the data path. The controller has **no spdk-tgt** (it calls node-agents
over HTTP); the node-agent's spdk calls are **mocked**. So none of the heavy
dependencies apply:

| Dependency | Data-plane (integration) tier | Race tier |
|---|---|---|
| spdk-tgt (amd64-only → qemu on arm64) | required | **mocked** (`mock-spdk-tgt.py`, arch-neutral) |
| hugepages | required | none |
| real NVMe / instance store | required | none |
| worker count | 4–5 | 1 CP + 2 workers |
| footprint (measured) | large | **~1.1 GB RAM, cluster up in 48 s** |

Empirically confirmed on an 8 GB arm64 Mac (Docker VM 4 GB): the 3-node kind
cluster used ~1.1 GB with headroom to spare. The **data-plane/integration
tier still needs a bigger amd64 host** — that boundary is deliberate.

## Component built: `mock-spdk-tgt.py` (the linchpin) — DONE

A ~arch-neutral stand-in for spdk-tgt. Speaks flint's socket protocol
(newline-delimited JSON-RPC, fresh connection per call,
`spdk_native.rs:269-275`), keeps just enough state (lvstores/bdevs/raids/
subsystems) that get_*/create/delete stay coherent, and — the point of the
tier — **injects faults per method** via a JSON control file
(`SPDK_MOCK_FAULTS`, re-read every request so tests inject at runtime):

```json
{"bdev_get_bdevs":       {"action":"hang"}}                       // never respond → R5 deadline
{"bdev_raid_create":     {"action":"delay","secs":3}}            // slow RPC
{"nvmf_subsystem_add_ns":{"action":"error","code":-32001,"msg":"EBUSY"}}
{"bdev_raid_get_bdevs":  {"action":"state","operational":1,"total":2}}  // degraded raid → F43 / degraded-serve
```

Smoke-tested (all pass): canned version, raid create→query coherence,
`state`→degraded 1/2 with correct `is_configured` flags, `error`→EBUSY,
`hang`→client timeout. Covers ~46 driver-called methods (most canned-ok; the
listed handlers are stateful).

## Remaining build (not yet done)

1. **Containerize the mock** — `Dockerfile` on `python:3-slim` (arm64), tiny;
   `kind load docker-image`. No registry needed.
2. **Race deploy** — flint controller + node pods on kind with the mock
   replacing the spdk-tgt sidecar. Either a `kindMode.race` chart path (swap
   the `spdk-tgt` container image + drop hugepages/privileged) or a purpose-
   built kustomize overlay under `tests/kind-race/deploy/`. Controller image =
   `dilipdalton/flint-driver:1.19.0` (arm64 manifest exists).
3. **Race scenarios** (`tests/kind-race/scenarios/`), each a script that
   crafts PV/record state, drives the fault file, and asserts:
   - **R1 CAS contention** — N concurrent writers to a PV sync-record
     annotation; assert no lost update, bounded ret[ries. (No mock needed —
     pure API.)
   - **R2 / F43 claim starvation** — craft a PV record with an in_sync + a
     standby leg + epoch history; `state`-fault the raid to degraded; let the
     epoch scheduler tick; assert cutover is NOT perpetually `held_by=catch-up`
     (the F43 acceptance — fails today, passes once R2 arbitration lands).
   - **R5 hung-socket deadline** — `hang`-fault a method mid-operation; assert
     the driver's RPC deadline fires and compensation runs (no wedge).
   - **R2 node lock** — concurrent NodeStage + monitor on one volume; assert
     serialization (handler-vs-detector interleaving).
4. **Runner** — `make kind-race` (create cluster → load images → deploy →
   run scenarios → teardown), plus a CI-friendly `--keep` for iteration.

## Scope boundary (do not confuse tiers)

This tier proves **logic under concurrency**, not I/O. It will NOT catch a
real-SPDK behavioral surprise (the 4 MUST-VERIFY-ON-REAL-SPDK items stay on
the amd64/real-spdk tier). It is the fast inner loop; the cloud drills remain
the outer acceptance. See `docs/attach-detach-robustness-contract.md`
(test-tier mapping) and `docs/f43-rwx-replacement-admission.md` (the race this
tier will regression-guard once R2 lands).
