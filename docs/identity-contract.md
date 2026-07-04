# Flint volume-identity contract

**Status:** BINDING since v1.5.0+identity (live-validated 2026-07-04 on
cluster runk — full gate, teardown regressions, drills A/A′/B/C/D, zero
divergence). History and site-level detail:
`docs/plans/identity-unification.md` + `identity-unification-phase0-audit.md`.
Enforced in code by `src/identity.rs` (the ONLY module that mints or
parses identity-shaped names — CI lint
`identity::tests::no_naming_mints_outside_identity`).

## The three identities

| Identity | Shape | Meaning |
|---|---|---|
| Storage identity (`storage_id`) | `pvc-<uuid>` | What lvols, NQNs, raids, epoch snapshots and replica records derive from; == the user PV's name |
| User attachment handle | `pvc-<uuid>` (same string) | RWO: the block consumer. RWX/ROX: NFS clients with NO block path |
| NFS backing handle | `nfs-server-<storage_id>` | The synthetic PV behind an RWX volume's NFS server pod — the server's own block attachment |

Every RPC entry parses its handle into a `VolumeRef`
(`Block` / `NfsShared{read_only}` / `NfsBacking`), resolution order:
`nfs-server-` prefix → volume_context role hint
(`flint.csi.storage.io/role`, stamped at CreateVolume) → the cached
role resolver (PV access modes) → default `Block`.

**Failure defaults:** unreadable PV ⇒ `Block` (fencing semantics
preserved), except NodeUnstage which falls back to findmnt mount-state
sniffing first. Cache entries never go stale (volume ids are UUIDs,
access modes immutable); DeleteVolume invalidates as hygiene.

## RPC × role behavior matrix (the contract)

| RPC | Block | NfsShared (user RWX/ROX) | NfsBacking (`nfs-server-*`) |
|---|---|---|---|
| CreateVolume | mint storage id, create lvols/replicas; stamp role hint | same + `nfs.flint.io/*` context; server pod NOT created here | n/a — backing PV is driver-minted during publish |
| DeleteVolume | replica/lvol/target teardown | server pod delete + bounded flush wait → backing detach → storage teardown | REFUSED (driver-managed, never provisioner-driven) |
| ControllerPublish | export + host-fence to node | ensure server pod (a Terminating pod is NOT reusable — bounded wait, then recreate), return Service endpoint | block export + fence to the server's node |
| ControllerUnpublish | remote consumer ⇒ remove volume target (fencing) | **no-op** — departing party is an NFS client; the target is the server's live backing export | block-path bookkeeping; target lifecycle belongs to DeleteVolume |
| NodeStage | connect + assemble raid (`raid_<staging handle>`) | no-op (clients mount at publish) | block stage; raid keys on the BACKING handle; records resolve to the user PV |
| NodePublish | bind-mount staged device | NFS mount from publish context | n/a |
| NodeUnstage | disconnect + raid teardown (full handle), ublk id (storage id) | unmount-only — never SPDK teardown | block unstage |
| ControllerExpand | grow lvols/replicas, then node expansion | **REFUSED loudly** (audit L1: server-side expansion not yet supported; never half-apply) | REFUSED (driver-managed, never provisioner-resized) |
| NodeExpand | nvme resize + fs grow on the staged device | no-op success — the consumer holds an NFS mount, nothing to grow node-side (audit L3) | block path |
| NodeUnpublish | bounded unmount; an INCONCLUSIVE mountpoint probe (timeout = dead-NFS signature) means ASSUME MOUNTED and lazy-unmount | ← same rule (this is where dead client mounts drain) | ← |
| NodeGetVolumeStats | fs stats + raid health, all fs syscalls BOUNDED (5 s, spawn_blocking); timeout ⇒ condition abnormal, never a hung RPC | ← | ← |

Background subsystems (epoch scheduler, catch-up, cutover, hot rejoin,
replica sync, orphan sweep, dashboard) key on **storage identity only**
and skip backing PVs where consumer semantics would double-run.

## Operational corollaries (Phase-3 validated)

- A dead NFS server leaves client mounts hard-blocked by design
  (integrity over availability); nodes stay healthy (bounded stats) and
  teardown drains via the assume-mounted lazy unmount. A writer caught
  mid-write is unkillable until the server returns — kernel semantics.
- Server pod recreation belongs to the cutover machinery or the next
  client ControllerPublish. There is deliberately no independent pod
  reconciler (open item; see the audit doc).
- Fencing an RWO consumer on a node that hosts an NFS server removes
  exactly the RWO volume's target; the server's export is untouched.
