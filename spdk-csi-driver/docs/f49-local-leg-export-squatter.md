# F49 — reconcilers re-mint a leg export on the consumer's own node, wedging RWX raid assembly (EPERM)

**Status:** FIXED in-tree 2026-07-27 (all three fixes in §Fix below;
`/api/exports/drop_local` is the new deregister-then-delete endpoint),
drill-gated on the forced-placement 3.6e variant. Found live on runah during
the v1.21.0 expansion campaign's 3.6e regression pass. Pre-existing; **not**
introduced by the v1.21.0 expansion wave — the wave's entire `driver.rs` diff
is a visibility keyword, and neither reconciler touched here changed in it.
Exposure is placement luck: it fires only when the RWX server pod lands on a
node that hosts one of the volume's legs, which is why runag's 3.6e run 4
passed on the same code paths.

**Severity:** availability outage. The RWX volume is **unmountable** — the NFS
server pod cannot assemble its raid, every client loses the filesystem, and
NodeStage loops forever (kubelet `FailedMount` ×8 observed). No self-heal. The
only recovery observed is a csi-node DS roll (spdk-tgt restart), which is
itself disruptive (the EIO landmine for other mounted volumes on the node).
Data is safe throughout; nothing serves it.

**Vector:** RWX numReplicas≥2, leg-node loss → F40/F43 replacement admits a
new leg → NFS-server cutover (or any later server bounce) schedules the server
pod onto a node hosting one of the volume's legs.

## What happens

NodeStage on the co-located node takes the LOCAL branch of
`attach_replica_base` (`driver.rs:2673`): the leg lvol is to be claimed
directly as a raid base. A leftover NVMe-oF export of that lvol (minted
earlier, correctly, for the then-remote server node) holds a write-mode open,
so `bdev_raid_create`'s exclusive claim fails EPERM. The driver anticipates
exactly this and calls `drop_stale_local_exports` (`driver.rs:2643`) first —
the log shows it matching and deleting the RIGHT NQN. **The export comes
back anyway**, because two node-agent reconcilers believe it must exist:

1. **`reconcile_replica_targets` (60s tick + agent startup,
   `node_agent.rs:3548`)** — for every in_sync local replica it
   unconditionally calls `setup_nvmeof_target_for_replica`
   (`node_agent.rs:5368`), which has **no "consumer is this node" skip**. The
   VolumeAttachment is consulted only to compute the fence host list — so
   post-cutover it happily re-mints the export *fenced to the local node
   itself*. That is an export nobody can ever consume (a local consumer uses
   the raw lvol, never nvme-tcp to self), whose namespace claim exists only
   to block the raid.

2. **`reconcile_exports_if_lost` (10s fast loss-detector,
   `node_agent.rs:3446`)** — resurrects any NQN in the `exported_targets`
   registry that is missing from SPDK. Replica-leg exports are deliberately
   NOT supposed to be registry-tracked (`node_agent.rs:5434`: "the 60s
   reconcile_replica_targets owns re-export for replica volumes"), **but the
   startup seed `seed_exported_nqns_from_spdk` (`node_agent.rs:2434`) filters
   only on the `nqn.2024-11.com.flint:volume:` prefix, which leg NQNs
   (`…:volume:<pv>_<idx>`) also match**. After any node-agent restart the leg
   export is adopted into the registry, and the 10s detector then re-creates
   it "directly from the recorded params" within seconds of every drop.

`drop_stale_local_exports` deletes via the raw `/api/spdk/rpc` passthrough,
which never touches the registry — so the drop and the resurrection loop
forever:

```
🔧 [DRIVER] Creating RAID 1 on node: runah-aws-4
   Replica 2: LOCAL access (lvol: 8c9c7714-…)
   Dropping stale local export nqn.2024-11.com.flint:volume:pvc-d3f0a906-…_1 of 8c9c7714-…
🔧 [DRIVER] Creating RAID 1 bdev: raid_nfs-server-pvc-d3f0a906-… with 2 base bdevs
❌ [SPDK_RPC] bdev_raid_create failed: Code=-1 Msg=… Operation not permitted
(repeats every ~60s)
```

SPDK state during the wedge: the subsystem present, the leg lvol
`claimed=True`, `hosts=1` (fenced to the node's own host NQN — the
tell-tale of re-mint authority #1).

Evidence: `tests/chaos/artifacts/3-3.6e-1785182376/f49-assembly-eperm.txt`
(NodeStage loop, SPDK state, recovery via DS roll) and `driver-logs.txt`
therein (the `[NVME-RECOVERY #1] seeded export registry … count=1` adoption
after the agent restart).

## Why the observed recovery worked

A csi-node roll restarts spdk-tgt, wiping all subsystems; the fresh agent
seeds an empty (or claim-free) registry, and the next NodeStage claims the
lvol before either reconciler's first tick. That is a race won, not a fix —
and after recovery the 60s reconcile keeps trying to export the now
raid-claimed lvol, failing benignly-but-noisily every tick.

## Fix (three sites, one family)

1. **`reconcile_replica_targets`: consumer-local legs must have NO export.**
   When the volume's VolumeAttachment says the consumer is this node, skip the
   mint AND actively tear down + deregister any existing leg export (it can
   only be a stale squatter from a pre-cutover topology). Fail closed: on VA
   lookup error, neither mint nor tear down that tick.
2. **`seed_exported_nqns_from_spdk`: never adopt replica-leg NQNs.** Restore
   the stated invariant — the seed must classify NQNs and exclude
   `…:volume:<pv>_<idx>` leg exports, which the 60s reconcile owns.
3. **`drop_stale_local_exports`: deregister, don't just delete.** Route the
   drop through a node-agent operation that removes the NQN from
   `exported_targets` *first*, then deletes the subsystem — belt against any
   future registry adopter. The raw `/api/spdk/rpc` passthrough cannot do
   this.

(1) removes the standing authority, (2) removes the fast resurrection path,
(3) makes the driver's cleanup authoritative. Any one alone leaves a loop
alive.

## Drill gate

3.6e only exercises this when cutover happens to co-locate the server with a
leg. Add a forced variant: after replacement admission, pin the NFS server
deployment (nodeSelector) onto a leg-hosting node and bounce it — assembly
must succeed without EPERM, and the leg export must be verifiably absent
(subsystem gone, lvol claimed by the raid) while the OTHER leg's export
remains fenced to the server node.

Related: F47 (loopback-export teardown no-ops — same export-lifecycle family,
different NQN domain: F47 is the server's own volume/wrapper export, F49 is
the replica-leg export), F46 (leg-export mint unification — the adopt-or-mint
belt this reconcile correctly uses; the bug is not the NQN shape but minting
at all), F9 (the guard that makes `/api/blockdev/delete_nvmeof` initiator-
scoped; the new drop operation in fix 3 is consumer-scoped and must not
reuse it blindly), F8 (the seed pass fixed in 2).
