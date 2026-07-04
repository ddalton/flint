# Identity unification — one typed volume identity at every boundary

**Status:** Phases 0–1 COMPLETE (2026-07-04). Phase 0: `src/identity.rs`
(canonical vocabulary, delegating parsers, live-shape tests) +
`identity-unification-phase0-audit.md` (eleven role signals, full RPC×role
matrix, ~25-site inventory, latent findings L1–L6). Phase 1: all queue
sites converted onto identity.rs + the cached `RoleResolver`
(ControllerUnpublish/NodeUnstage classify through it; backing parses
unified on the handle with `IDENTITY-DIVERGENCE` transitional assertions;
DeleteVolume enforces the backing-refusal matrix cell + cache hygiene).
Phase 2: CreateVolume stamps the canonical role
(`flint.csi.storage.io/role`) into every volume_context (all four create
paths); ControllerPublish/NodeStage seed the role caches from it
(hint ≡ resolver by test-pinned construction), with a transitional
publish-side divergence assertion. Phase 3: LIVE-VALIDATED
2026-07-04 on cluster `runk` (all-spot, fresh) — full gate + rwx/rox
teardown regressions + upgrade ride-through + drills A/A′/B/C/D all
PASS; divergence assertions SILENT throughout (0 lines, all pods);
three pre-existing dead-NFS-mount P1/P2s found by drill A and fixed
(e67563b, a78c79c, 7e75419 — see the audit doc's Phase-3 section).
Phase 4 COMPLETE (2026-07-04):
identity.rs owns every naming body (old owners delegate); all
production mints converted; CI lint
`identity::tests::no_naming_mints_outside_identity` enforces it (one
allowlisted L4 exception); transitional divergence assertions removed
(earned); contract published at docs/identity-contract.md. ALL PHASES
DONE. Follow-ups closed 2026-07-04 (pre-release hardening pass): the
NFS-server-pod liveness reconciler landed (rwx_nfs.rs, default-enabled,
contract corollary updated); L1/L3 fixed (loud expand refusal for shared
volumes, contract matrix rows added); L4 verified unreachable (legacy
operator chart-disabled, audit note); L5 was already unified in Phase 1;
L6 is the contract cell, not a bug. L2 was fixed via a78c79c.
**Motivation:** the RWX identity-aliasing bug class has produced P1s on
three separate occasions, each found live: the RWX cutover validation
batch (637be1c, six fixes), the v1.4.0 release gate (d7490de, NodeUnstage
classification), and the v1.5.0 post-release validation (c879bc3,
ControllerUnpublish removing the NFS server's live backing export). Every
fix so far adds a *local* heuristic at one RPC entry. This plan removes
the class.

## The problem, precisely

An RWX/ROX volume has three identities that today share string shapes and
are disambiguated ad-hoc:

| Identity | Today's shape | Where it appears |
|---|---|---|
| Storage identity | `pvc-<uuid>` | lvols (`vol_pvc-…`), NQNs (`nqn…volume:pvc-…`, `…:replica:N`), raids (`raid_pvc-…`), epoch snapshots (`epoch-pvc-…-N`), replica records |
| User attachment (RWX/ROX: NFS clients; RWO: the block consumer) | `pvc-<uuid>` — **same string** | every CSI RPC for the user PV |
| NFS-server backing attachment | `nfs-server-pvc-<uuid>` | every CSI RPC for the synthetic backing PV (minted once, rwx_nfs.rs) |

Two structural defects follow:

1. **The user handle is ambiguous.** `pvc-X` in a context-free RPC
   (ControllerUnpublish, NodeUnstage, DeleteVolume carry no
   volume_context) cannot distinguish "RWO block consumer — run
   fencing/teardown" from "RWX NFS client — there is no block path".
   Each bug fix so far answers this locally with a PV access-modes
   lookup (d7490de, c879bc3) or findmnt heuristics. Sites that haven't
   been patched yet are latent bugs of the same class.
2. **Role parsing is scattered.** The `nfs-server-` prefix is minted in
   one place but stripped/tested in ≥6 files (17 `actual_volume_id`
   sites) with hand-rolled `starts_with`/`strip_prefix`, and storage
   names are derived by scattered `format!` calls. 637be1c's six bugs
   (duplicate epoch streams, alias-NQN export squatting, zombie raids,
   consumer unstage detaching live legs, …) were all sites where one of
   these conversions was missing or double-applied.

## Design

**Unify in code, not on the wire.** No persisted identifier changes: no
new handle shapes, no PV re-minting, no migration. Existing volumes,
CSI sidecar behavior, NQN/lvol/epoch names, and the on-disk/state.db
world are untouched. (The alternative — role-qualified user handles like
`nfs-client-pvc-X` — self-identifies in every RPC but requires handle
migration for every existing RWX PV and re-teaches every sidecar-visible
surface; the only benefit over a cached resolver is avoiding one kube
lookup on cold context-free RPCs. Rejected.)

### 1. `identity.rs` — the single vocabulary

```rust
/// Parsed once at every entry point; passed by value everywhere after.
pub enum VolumeRef {
    /// RWO volume, or any bare handle whose PV says RWO — full block path.
    Block { storage_id: String },
    /// User RWX/ROX handle — attachments are NFS clients; NO block path.
    NfsShared { storage_id: String, read_only: bool },
    /// nfs-server-<id> backing handle — the server's block attachment.
    NfsBacking { storage_id: String },
}
```

- `VolumeRef::resolve(handle, ctx, resolver)` — the ONLY parser:
  `nfs-server-` prefix → `NfsBacking`; else volume_context `role` hint
  (fast path, see §3); else the role resolver (§2); else `Block`.
- `storage_id()` — the one conversion to storage identity.
- A naming module owning every derived name and its parser:
  `lvol_name`, `volume_nqn`, `replica_nqn`, `raid_name`,
  `epoch_snapshot_name`, plus `classify_subsystem_nqn` (absorbed from
  orphan_sweep) and `SnapshotInfo::volume_id_from_snapshot_name`
  (absorbed from snapshot_models). Grepping for `format!("nqn.` or
  `format!("vol_` outside identity.rs becomes a CI lint.

### 2. One role resolver, not N lookups

`resolve_role(storage_id) -> Role` with a per-volume cache:
in-memory cache → PV access-modes (the d7490de source of truth) →
documented per-RPC failure default. Volume ids are UUIDs (never
reused), so cache invalidation is deletion-only. All current
site-local classification (access-modes lookups, findmnt fallbacks,
`is_shared_nfs_consumer`) collapses into this resolver; findmnt
survives only inside it as the PV-unreadable last resort.

Failure defaults are part of the contract, stated once: unreadable PV ⇒
RWO semantics (fencing preserved — the c879bc3 choice), except
NodeUnstage which keeps its findmnt fallback (the d7490de choice).

### 3. Context hint for the fast path

CreateVolume stamps `flint.csi.storage.io/role: nfs-shared|block` into
volume_context. RPCs that carry context (ControllerPublish, NodeStage,
NodePublish) resolve with zero lookups. Context-free RPCs
(ControllerUnpublish, NodeUnstage, DeleteVolume) hit the cached
resolver. Old volumes without the hint resolve identically — the hint
is an optimization, not a compatibility surface.

### 4. The behavior matrix (the contract)

The deliverable that prevents recurrence — every RPC × role, exact
action, in one table (excerpt; full table written during Phase 0):

| RPC | Block | NfsShared | NfsBacking |
|---|---|---|---|
| ControllerPublish | export + fence to node | NFS bookkeeping only (no block) | export to server node |
| ControllerUnpublish | remove target for node (fencing) | **no-op** (c879bc3) | detach bookkeeping; target lifecycle belongs to DeleteVolume |
| NodeStage | connect + assemble raid | no-op (clients mount at publish) | block path (d7490de: `nfs-server-*` stays block) |
| NodeUnstage | disconnect + disassemble | unmount-only (d7490de) | block path |
| DeleteVolume | storage teardown | orchestrate: NFS pod delete + wait (567c582) → backing detach → storage teardown | refused (never provisioner-driven) |

Background subsystems (epoch scheduler, catch-up, hot-rejoin,
replica_sync, cutover, orphan sweep, data-path-lost detection,
dashboard backend) key on **storage identity only**; consumer-facing
decisions take a `VolumeRef`. Phase 0's audit records, per subsystem,
which identity each of its ~700 `volume_id` references means.

## Phases

- **Phase 0 — audit + contract (no behavior change).** Write
  `identity.rs` (types, parsers, naming, unit tests using live RPC/NQN
  shapes — the lvol-counter lesson: fixtures must mirror live output).
  Write the full behavior matrix. Inventory all 6-file prefix sites, 17
  `actual_volume_id` sites, both access-modes sites, and every naming
  `format!`; capture current behavior in tests first.
- **Phase 1 — mechanical convergence.** Replace every site with
  `identity.rs` calls; introduce the cached resolver; delete site-local
  heuristics. Suite green throughout; behavior identical by
  construction (divergence assertions during transition: resolver
  result vs old heuristic, log on mismatch, then remove).
- **Phase 2 — context hint.** Stamp `role` at CreateVolume; fast-path
  the resolver. Half a day.
- **Phase 3 — live validation.** Fresh cluster (runj was deleted
  2026-07-04; provisioning via trove + the `flint` SC clone note).
  Full kuttl suite + clean-shutdown; the rwx/rox teardown regression
  (no Terminating flint-nfs pods); targeted aliasing drills — the
  historical triggers: client churn during server cutover, delete
  during client churn, server bounce with staged clients, RWO volume
  on a node also hosting an NFS server (fencing must still fire).
  Upgrade check: volumes created by 1.5.0 ride a controller roll onto
  the unified build untouched (nothing persisted changed).
- **Phase 4 — hardening.** CI lint against out-of-module naming
  `format!`s; orphan sweep + dashboard adopt the shared parsers;
  contract table into the architecture docs.

Estimate: Phase 0 ≈ 1 day, Phase 1 ≈ 2–3 days (the decision sites are
~25 even though references are ~700), Phases 2+4 ≈ 1 day combined,
Phase 3 ≈ 1 day on a live cluster. Controller-side only; no node-DS or
spdk-tgt changes; ships in a normal release.

## Risks

- **Resolver staleness / failure semantics** — bounded by UUID
  non-reuse and the documented per-RPC defaults; the defaults are
  today's shipped behavior, just centralized.
- **Phase 1 regressions in untested corners** — mitigated by
  capture-current-behavior tests before each site converts, and by the
  divergence-assertion transition.
- **Latent sites the audit finds** — each is a bug of the existing
  class discovered at audit time rather than live; fix-as-found is the
  point of the exercise.
