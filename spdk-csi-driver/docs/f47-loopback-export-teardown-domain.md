# F47 — the loopback export outlives its server (two broken deletes, one shell per cutover)

**Status:** AUDITED 2026-07-27, root cause attributed from the runag drill
3.6e run-4 incident logs. **Severity: P3 residue** — no data-path impact
observed; one empty subsystem shell plus a fenced-out host accumulates on
every RWX server node that unstages after a cutover. **Deliberately NOT
fixed in this pass:** both fixes make destructive operations fire where
they currently no-op, so they are drill-gated for the next campaign (§6).

This is the loopback edition of the [F44](f44-cutover-leg-detach-leak.md)/
[F46](f46-cutover-serving-node-residue.md) class — a name derived in one
id domain matched against an object living in the other — with a second,
independent blocker layered on top (the F9 guard, §4b).

## 1. The observation that triggered the audit

Post-drill export capture on runag
(`tests/chaos/artifacts/3-3.6e-1785166909/export-domains-post-drill.txt`):

```
aws-3 (serving):  volume:pvc-e8e7ed8b…      ns = raid_nfs-server-pvc-e8e7ed8b…
aws-1 (previous): volume:pvc-e8e7ed8b…      ns = (EMPTY)
```

The serving loopback subsystem is **inner**-named while the raid it exports
is **wrapper**-named — and the previous server retains an empty bare-inner
shell. Neither fact was explained by any code path inspected at capture
time.

## 2. Attribution (from the incident's own driver logs)

`driver-logs.txt`, aws-3, 15:47:27 — the post-bounce stage, caught in one
sequence:

```
bdev_raid_create        name=raid_nfs-server-pvc-e8e7ed8b…     ← WRAPPER
[NODE] Creating block device for bdev: raid_nfs-server-pvc-…
[NVMEOF_BLOCK] Creating NVMe-oF block device …
nvmf_create_subsystem   nqn=…:volume:pvc-e8e7ed8b…             ← INNER
nvmf_subsystem_add_host host=…:node:runag-aws-3
nvmf_subsystem_add_listener 127.0.0.1:4420
```

And aws-1, 15:46:42 — the outgoing server's unstage:

```
[NVMEOF_BLOCK] Deleting NVMe-oF block device with NQN: …:volume:pvc-e8e7ed8b…
F9 guard: subsystem is serving another consumer — skipping delete, fencing
          this node out   reason=VolumeAttachment owned by runag-aws-3
nvmf_subsystem_remove_host host=…:node:runag-aws-1
```

## 3. Root cause — three parts

**a. The mint is inner-domain, on purpose.** NodeStage resolves
`actual_volume_id = parse_backing_handle(volume_id) → storage_id`
(main.rs:2331) and passes it to `create_block_device` (main.rs:2545, "for
consistent device ID generation" — the ublk-id hash keys on the inner id,
and the nvmeof branch inherits the same id). `create_nvmeof_block_device`
then mints `volume_nqn(<inner>)` (driver.rs:1179). So the loopback family
is **uniformly inner-minted at stage** — the raid name is the only
wrapper-domain object in the chain.

**b. Delete path A targets the right NQN and is blocked by the F9 guard.**
NodeUnstage deletes through the stored `CleanupData::Nvmeof{nqn}` — the
correct inner NQN, persisted at create time. But the guarded
`/api/blockdev/delete_nvmeof` route reasons about **cross-node VA
ownership**, and after a cutover the VolumeAttachment already belongs to
the NEW server — so the OLD server is refused deletion of its own
loopback and merely fenced out of it. The guard's logic is right for
remote-consumable exports and wrong for this one: the subsystem's only
listener is `127.0.0.1`, so no other node can ever be its consumer. The
shell then persists; its namespace evaporates when teardown step 2 deletes
the raid bdev.

**c. Delete path B is a wrong-domain silent no-op.** Teardown step 1
derives `volume_nqn(<staged>)` — the **wrapper** — which never existed as
a subsystem, so the belt intended to catch lost cleanup data deletes
nothing, silently, on every RWX server node (driver.rs:3440). RWO is
immune: staged == inner, so the derivation is correct there. F44's exact
signature, one object over.

Net effect: **every RWX server node that unstages after a cutover keeps an
empty bare-inner subsystem with itself fenced out.** A later re-stage on
the same node converges fine (`ensure_export` completes the partial state
and re-admits the host), which is why nothing user-visible has broken.

## 4. Why it still matters

- F46 demonstrated what empty shells do to name-keyed probes: they read as
  "exported to somebody else" and wedge assemblies. This family
  manufactures one shell per cutover, on the exact nodes future cutovers
  revisit.
- The teardown belt (path B) protects nothing on RWX today — if cleanup
  data is ever lost on an RWX server, the loopback leaks its namespace too.

## 5. What must NOT be done

**Do not "unify" the loopback mint to the wrapper domain.**
`ensure_export_for` derives the kernel-facing namespace identity from the
NQN tail (`stable_ns_identity(<nqn tail>)`, node_agent.rs:1736): changing
the mint domain changes the namespace UUID/NGUID the kernel initiator
verifies on reconnect, stranding every mounted consumer at the next
spdk-tgt restart — the exact phase-6 layer-2 hazard the identity pinning
exists to prevent, self-inflicted by a cleanup. The mint is consistent
today; the deletes must follow the mint, not the other way round.

## 6. Fix direction (drill-gated, next campaign)

1. **Teardown step 1 sweeps the bare export in BOTH domains** — mirror of
   `per_replica_controller_prefixes` (F44) — or better, prefers the stored
   `CleanupData` NQN as the source of truth with the derived pair as the
   lost-data belt.
2. **F9 guard: local-only subsystems are the staging node's to delete.**
   A subsystem whose every listener is loopback (`127.0.0.1`) has exactly
   one possible consumer — this node — so its own unstage may delete it
   regardless of VA ownership. Cross-node ownership reasoning stays for
   subsystems with remote listeners.

Both changes make deletes fire where they currently no-op, which is why
they wait for a cluster: the acceptance is drill 3.6e's latent-pin sweep
plus a new assertion that the outgoing server node retains **zero**
subsystems for the volume after unstage.

## 7. Newtype tie-in

The loopback family was the last un-belted identity-domain family. Fix 1
gives it its belt, which makes it eligible for the final
`StagedHandle`/`StorageId` newtype tranche (F45 recommendation 2) — the
tranche order is "newtype only what a runtime belt already backstops."

## 8. Provenance

Found by the post-drill export capture added for the F46 validation
(2026-07-27); attributed the same day from
`tests/chaos/artifacts/3-3.6e-1785166909/driver-logs.txt` — the mint and
the guarded refusal are both in the incident record, timestamped, with no
reproduction needed. This closes the "loopback-export domain audit" item
from the v1.20.0 backlog.
