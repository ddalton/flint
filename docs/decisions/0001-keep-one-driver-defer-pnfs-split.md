# ADR 0001: Keep one CSI driver, defer pNFS split

**Date**: 2026-05-02
**Status**: Accepted
**Branch this was decided on**: `kind-no-spdk` (commits c8eb204..84d7e7a)

## Context

Throughout this session we explored splitting the Flint CSI driver into
two: the existing `spdk-csi-driver` for SPDK / block storage, and a new
`flint-pnfs-csi` for NFS / pNFS. The motivation was modularity — keep
the SPDK code path completely untouched while pNFS evolves.

Several plan revisions later, the proposed split had grown to:

- Cargo workspace with three crates (`spdk-csi-driver`, `flint-pnfs-csi`,
  `flint-shared`)
- `nfs/v4/` lifted into `flint-shared` so both drivers can share the
  protocol implementation (Option C)
- A 7-scenario regression test suite gating every refactor PR
- A multi-week phased plan (workspace split → fio bench → CSI
  integration → DS discovery → caching → durability)

At the same time, the actual product question — "does pNFS perform well
enough to be worth shipping?" — hadn't been answered yet. The pNFS data
path works (smoke green, 24 MiB stripes across two DSes, hash matches),
but no realistic workload has been benchmarked against single-NFS-server.

## Decision

**Keep one CSI driver. Integrate pNFS as a `layout: pnfs` StorageClass
parameter inside `spdk-csi-driver`. Defer the split until a real
consumer makes its cost obvious.**

The split's payoff is hypothetical until pNFS has a real workload pulling
on it. The cost — a multi-week refactor that's all "no user-visible
change" — is real and concrete. Investing in modularity for code that
hasn't been written yet inverts the right priority order.

The integration approach inside the existing driver:

- `req.parameters.get("layout")` in `main.rs::create_volume`. Default
  remains `single-server` (today's behaviour). New option `pnfs`
  branches to a new `pnfs_volume::create_volume()` helper *before* the
  existing `rwx_nfs` and SPDK-block branches.
- pNFS-specific volume_context keys live under `pnfs.flint.io/*` so they
  don't collide with the existing `nfs.flint.io/*` namespace used by
  the rwx_nfs path.
- The MDS gRPC client is its own module under `src/pnfs_csi/`. SPDK
  modules don't import from it.
- Discipline keeps the boundary; the architecture doesn't enforce it.
  This is the trade-off we're accepting.

## Rejected alternatives

### Option A: separate `flint-pnfs-csi` driver in this repo

Two CSIDriver registrations, two helm charts, two operational
footprints. Strongest isolation. Rejected because the ~10 days of
mechanical refactor + scaffolding is hard to justify before pNFS has a
validated product-market-fit signal.

### Option B: workspace split with a `StorageBackend` trait

Single binary, but two crates with a trait-based dispatch layer. Lower
risk than full split, but still ~5 days of refactor with no shipping
value. Rejected on the same "premature optimization" grounds.

### Option C (the lift-`nfs/v4/`-into-shared variant)

Same trade-off as A and B, plus the additional cost of designing a
public API for the NFS protocol crate. Rejected for the same reason.

## Consequences

### Positive

- Engineering effort focuses on shippable user value, not on internal
  reorganization.
- The pynfs conformance work (153/18/91, st_secinfo and st_verify both
  100%, and the LAYOUTRETURN/LAYOUTCOMMIT plumbing) stays in one place;
  no duplication, no drift.
- Audit Tasks #4 (CB_LAYOUTRECALL) and #5 (state persistence) — both
  real production gaps — can land directly without waiting on a
  refactor to land first.
- Existing SPDK users see zero change.

### Negative

- The boundary between SPDK code and pNFS code is enforced by
  discipline, not by the type system or build flags. PRs that
  reach across (e.g. an SPDK fix that incidentally touches `pnfs/`)
  are possible.
- A future split, if needed, will be more painful the longer we wait —
  cross-references will have accumulated. We accept this debt.
- Customers who want only one tier installed get more binary surface
  than they technically need. This is unlikely to matter in practice.

## When we'll revisit

Revisit splitting if **any** of these become true:

1. A real customer is running pNFS in production AND a bug in pNFS
   state machinery is causing concrete operational problems for SPDK
   users (or vice versa).
2. We decide to ship pNFS on an independent release cadence from SPDK
   for marketing or release-management reasons.
3. The combined `spdk-csi-driver` crate's compile time or test surface
   becomes painful enough to justify the split's cost on engineering-
   ergonomics grounds alone.

If none of these become true within ~2 quarters, the decision stays.

## What this changes about prior session work

- The drafted regression suite (`tests/regression/`) and inventory
  (`docs/spdk-driver-inventory.md`) are kept but parked. They become
  valuable when (1) a refactor actually starts, or (2) we add CI. Until
  then they're reference material.
- The "two-driver / Option C" plans in chat history become design
  archive, not roadmap.
- The pynfs work landed this session (commits `7f72ee9`, `5fb9186`,
  `84d7e7a`) is unaffected — it's pure conformance work that pays off
  regardless of how the codebase is organized.
