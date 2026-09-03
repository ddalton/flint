# F45 — identity-domain flow audit (wrapper `nfs-server-<pv>` vs inner `pvc-<uuid>`)

**Status:** AUDIT COMPLETE 2026-07-27; **2 BROKEN sites found — both FIXED
same day** (shipped in `1.20.0-rc4` together with the F44-cousin chase-leak
fix). **Post-rc4 (same day, after S3 realized live as
[F46](f46-cutover-serving-node-residue.md)): S3 and S5 FIXED,
recommendations 1 and 4 DONE** — `replica_export_nqn` now normalizes to the
inner domain at the choke point, `resolve_replica_export_nqn` is the
read-both transition belt, hot-rejoin pads got their own `export_pad`, and
DeleteVolume deletes leg exports in both domains. S1, S2, S4 + the fragile
list remain open, as do recommendations 2 (newtypes) and 3 (lint
extension). Triggered by [F44](f44-cutover-leg-detach-leak.md), whose root
cause was this exact bug class; the audit swept every naming-helper call
site (~300; ~50 match-class) for more of it.

**The class:** flint volumes live in two id domains — user PVs use the inner
storage id (`pvc-<uuid>`), RWX backing volumes stage under the wrapper
handle (`nfs-server-<pv>`). Every identity.rs helper is "correct" in either
domain; the bug is deriving a name in domain A and **matching/deleting it
against objects named in domain B**. Such bugs are *silent no-ops or
misdirected matches, never errors*, and they only bite on RWX server-node
paths (RWO's domains coincide) — which is why unit suites and RWO drills
never see them.

**Convention map** (traced, load-bearing for every verdict below):
- **Wrapper domain:** loopback/r1-remote export NQN, raid bdev name, kubelet
  staging record, `chert.us/ublk-id` annotation home (the backing PV).
- **Inner domain:** lvol/epoch names, ublk id mint (`hash(inner)`), sync
  record home (user PV), catch-up's replica-leg export mints.
- **Mixed by construction (S3) — UNIFIED 2026-07-27:** replica-leg export
  NQNs — initial stage minted `volume:<wrapper>_<i>`, catch-up minted
  `volume:<inner>_<i>`, replica-node reconcile re-minted wrapper. All
  three now mint inner via the normalizing `replica_export_nqn`; the
  wrapper shape survives only as `legacy_replica_export_nqn` for the
  transition belt and teardown hygiene.

## BROKEN (both fixed 2026-07-27, in 1.20.0-rc4)

### B1 — F43 #7 pre-assembly ublk converger attributes "own disk" in the wrong domain (driver.rs)
`expected_ids` derived `generate_ublk_id(staged)` (wrapper hash) and read
the annotation from the **user** PV — but ublk ids are minted from
`hash(inner)` and the annotation lives on the **backing** PV. On an RWX
server node `stale_own_ublk_disks()` therefore always returned empty: the
volume's own stale direct-serve disk over a leg is never stopped, and the
construction boundary then refuses `bdev_raid_create` with **no healer** —
the exact wedge the converger was shipped (4adf986) to prevent.
**Fix:** expected set now covers both domains (`hash(inner)`, `hash(staged)`)
plus the annotation from both PV names — same dual-domain philosophy as
`per_replica_controller_prefixes` (F44).

### B2 — NodeStage remote-branch self-heal re-exports under the INNER id (main.rs) — **CONFIRMED LIVE**
ControllerPublish mints the r1-remote export from the RAW handle (wrapper
for backing volumes); the stage-retry self-heal re-ensured it with the
inner id. **Observed on runae immediately after drill 3.6e run 2:** the
self-heal created a spurious `volume:<pv>` subsystem and **migrated the
leg's namespace into it**, leaving the wrapper `_0` subsystem the next stage
attaches as an empty shell → `F36c AssemblyDeferred` loop on every node the
pod tried (aws-4, then aws-3), pod Pending, volume down. Manual repair
(remove_ns from the spurious subsystem → add_ns back to `_0` → delete the
spurious subsystem) recovered the volume to 2/2 within 50 s.
**Fix:** pass the raw `volume_id` — one token, plus the incident comment.

## SUSPECT (ranked; S3 + S5 closed 2026-07-27)

- **S3 (the systemic one) — REALIZED LIVE as F46, then FIXED:** replica-leg
  export NQNs were minted in *different domains* by stage vs catch-up vs
  replica-node reconcile — runae 3.6e run 3 hit the duplicate-claim shape
  (wrapper shell EMPTY, inner holds the ns, gate reads "claim-blocked"
  forever; see the F46 doc). **Fix:** `identity::replica_export_nqn`
  normalizes through `storage_id_of_handle` (every mint site inherits the
  inner domain), `nvmeof_export::resolve_replica_export_nqn` adopts
  still-serving wrapper exports and retires empty shells, and teardown
  sweeps empty leg-export shells in both domains.
- **S1:** catch-up's phantom-raid scrub (`clear_head_sb`) matches the inner
  raid name; RWX raid superblocks carry the wrapper name — a head that
  inherits a wrapper sb through its clone parent assembles a phantom the
  scrub can't delete (wedges chase with a generic error).
- **S2:** orchestrator-side standby admission can never fire for RWX (inner
  raid name + consumer resolved to an NFS *client* node) — F43-adjacent;
  any future direct-admission work must fix both lines.
- **S4:** DeleteVolume's defensive net is inner-domain only — a leaked
  backing stage leaves a wrapper-named raid whose claims make `delete_lvol`
  busy forever; also `bdev_nvme_detach_controller` is passed a raw NQN where
  a mangled controller name is required (universal silent no-op).
- **S5 — FIXED 2026-07-27:** DeleteVolume replica cleanup deleted
  `replica_alias_nqn` (`…:replica:<i>`), a shape nothing mints — dead
  code; live replica exports leaked until the orphan sweep condemned
  them. Now deletes `replica_export_nqn` (canonical inner) plus the
  legacy wrapper shape.

## OK-but-fragile (watch on refactor)
**2026-07-27 addendum:** the loopback/bare-volume export family got its own
audit after the runag capture showed an inner-named subsystem exporting the
wrapper-named raid — attributed and documented as
[F47](f47-loopback-export-teardown-domain.md) (inner mint is deliberate and
must NOT change; both delete paths are broken — one wrong-domain no-op, one
F9-guard refusal on a node-local subsystem).

raid-membership uuid-belt is single-domain (replica_sync); hot-rejoin is
inner-domain-throughout and safe only behind its RWX refusal gate (the
marker-reconcile dispatch checks only `nfs_backing`, not `rwx`); rehydrate's
stale-ublk reap condemns anything outside the convention domain (B2 showed
one escape path); NodePublish block-mode hashes the raw id and bypasses the
annotation authority; `record_assembly_sync_state` relies on a hidden strip
inside replica_sync.

## Hardening recommendations
1. ~~**Unify the replica-leg export mint** (S3) — one domain (inner is the
   natural one), with a read-both-domains transition belt.~~ **DONE
   2026-07-27** (see S3 above / F46 §6).
2. **Newtype the ids** (`StagedHandle` vs `StorageId`) so the compiler
   enforces domain at helper boundaries — the CI lint catches literal
   drift, not wrong-id-into-right-helper (F44, B1, B2 all sailed past it).
   *Still the right next step; staged separately from the behavioral fix
   so live validation isolates behavior from type plumbing.*
3. Extend the CI lint: flag `volume_nqn(<var>)` where `<var>` flows from a
   CSI `volume_id` parameter without passing `storage_id_of_handle`.
4. ~~Kill S5's dead delete; point it at `replica_export_nqn` in both
   domains.~~ **DONE 2026-07-27.**

## Provenance
Audit run 2026-07-27 (read-only, full helper-call-site enumeration with
two-hop argument provenance), triggered by the F44 root cause. B2 was
independently confirmed by a live incident on cluster `runae` between the
audit's start and its delivery — the finding predicted the exact subsystem
shapes found on aws-1.
