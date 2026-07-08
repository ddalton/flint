# MDS sharding — per-volume shards for aggregate metadata scaling

Status: **IN PROGRESS** (decision 2026-07-07: sharding is the chosen
perf direction; perf-plan Tiers 2–3 shelved)
Prereqs shipped: dir-per-volume CSI (v1.12.0), per-pod Service pattern
(v1.9.0, durable-DS Phase 1), MDS restart hardening (v1.10.0)

## Why

Every metadata operation in the fleet funnels through one MDS process.
Tiers 1–3 raise that process's ceiling; sharding multiplies how many
ceilings the fleet has:

1. **Aggregate throughput.** N shards ≈ N × the per-MDS mdsbench
   numbers (post-Tier-1: 489 creates/s, 8.7k open/close per shard),
   with zero shared state — the same scaling shape ADR 0004 proved for
   the data path.
2. **Blast radius.** The v1.12.0 live gate is the case study: the
   csi-node-roll landmine killed NVMe-oF under the MDS's own PVC and
   **every** pNFS volume lost provisioning + metadata until the
   scale-cycle. Sharded, that incident scopes to 1/N of volumes.
3. **Control-plane isolation.** A CreateVolume storm, a hung export
   filesystem, or a client bombarding one shard cannot stall the
   others.
4. **Composition with dir-per-volume (the Spark angle).** PVCs are now
   cheap subtrees, so the natural Spark layout is one PVC per
   table/output. That spreads a single job's metadata load across
   shards with no application awareness — sharding gives *intra-job*
   scaling that, before v1.12.0's dir-per-volume, it could not.

What sharding does NOT do: raise the per-volume ceiling. One volume =
one shard, so a single-PVC workload sees exactly the Tier 1–3 per-MDS
numbers. Tier 2 (`return_on_close=false`) is that lever; the two
multiply.

## Endpoint model — what stays single, what fans out

There is deliberately **no single NFS endpoint** in front of the
shards, because there cannot be one: NFSv4.1 sessions are stateful and
bind to a server instance. A VIP that round-robins TCP connections
across shards would scatter one client's session ops across servers
that don't share state. Ganesha/NetApp scale-out deployments make the
same choice: clients mount the owning node directly.

What users and machines actually touch:

| consumer | endpoint | single? |
|----------|----------|---------|
| App pods / PVC authors | StorageClass `flint-pnfs` | **yes** — unchanged, the CSI layer hides everything |
| Kernel client (NodePublish) | per-shard Service IP stamped in the PV's `pnfs.flint.io/mds-ip` | per-shard, stamped automatically at provision |
| CSI controller (Create/DeleteVolume) | the owning shard's gRPC :50051 | routed by controller (see §Assignment) |
| DS registration/heartbeat | ALL shards — endpoint list rendered into the DS config by the chart (count is template-time) | one config key, N targets |
| Dashboard / operators | `/api/*` aggregates all shards | yes (aggregation) |
| Manual mounts / runbook recipes | a specific shard's Service | per-shard (recipes gain a "which shard owns this volume" lookup: it's in the PV) |

The single point of contact that matters — `kubectl apply` a PVC —
is untouched. Nobody types an MDS address today; nobody will after.

## Design

### Chart: N ranged single-replica Deployments (default N=1)

(Amended at Phase 0 implementation: ranged Deployments, NOT a
StatefulSet. volumeClaimTemplates would rename shard 0's PVC and
force a data migration on every upgrade from a pre-sharding install;
ranged Deployments let shard 0 keep the exact legacy object names —
Deployment/Service `flint-pnfs-mds`, PVC `flint-pnfs-mds-data` — so
upgrades adopt in place with zero surgery. Each Deployment keeps
`strategy: Recreate`, preserving the per-shard sqlite single-writer
fence the pre-sharding chart documents.)

- `pnfs.server.mds.count: N` (default **1** — renders semantically
  identical to the pre-sharding chart, verified by diff; sharding is
  opt-in).
- Per-shard stable ClusterIP Services `flint-pnfs-mds-{i}` for ALL
  ordinals (uniform enumeration); the legacy `flint-pnfs-mds` Service
  stays as a shard-0 alias (pre-shard PVs have its IP stamped; the DS
  config points at it until Phase 2).
- Per-shard PVC (state.db + exports tree). Each shard's exports hold
  only its own volumes' directories.
- `FLINT_MDS_SHARD_ID={i}` injected per Deployment.
- All MDS/DS pods carry `flint.io/role` labels; the Tier-A
  NetworkPolicies select by role (per-shard `app` values can't be
  enumerated in a selector).

### Assignment: pin at CreateVolume, never move

- Controller discovers shard control-plane endpoints (env list from
  the chart, or headless-Service DNS).
- Pick: **least volumes** (each shard's control plane already knows
  its volume count; add a trivial `GetShardInfo` RPC). Hash-mod is the
  v0 fallback. Least-loaded also makes scale-out self-balancing: new
  volumes prefer the new empty shard.
- Stamp the shard's Service IP into `pnfs.flint.io/mds-ip` (the
  NodePublish path reads it today — **zero node-side changes**).
- Return `volume_id` with a shard suffix (`pvc-…~m2`). CSI hands
  DeleteVolume/Expand only the volume_id, not the context — the suffix
  makes routing stateless. (Fallback for pre-shard volumes without a
  suffix: shard 0, which is where they all live.)
- Volumes never migrate between shards. Scale-in is refused unless the
  shard owns zero volumes. Rebalancing is explicitly future work
  (would need state.db subtree export + placement re-key + remount).

### DS: register with every shard

- `mds.endpoint: String` becomes an endpoint list in the DS config;
  the chart renders all shard Service DNS names (scale changes are a
  helm upgrade ⇒ DS config change ⇒ rolling DS restart — explicit and
  observable). One registration client per shard endpoint — the
  existing client in a loop; per-shard re-register/NACK state kept
  independently (the Phase 3 heartbeat thread already isolates
  liveness from the data path).
- Heartbeats to N shards cost N small RPCs/interval — noise.
- Stripe-cleanup instructions arrive from all shards and merge safely:
  shards own disjoint files (see file_id below), and DELETE_STRIPE_FILE
  is path-scoped + fd-cache-evicting (v1.12.0).
- Capacity truth: every shard sees the same statvfs numbers from the
  same DSes. Placement stays per-shard-local; ENOSPC is enforced at
  the DS (pool-level), exactly as today.
- The DS identity guard (Phase 2) binds DS↔data-volume, not DS↔MDS —
  unchanged.

### file_id disjointness across shards

Stripe files are `{file_id:x}.stripeN` in a flat per-DS namespace, so
file_ids must not collide across shards. `allocate_file_id()` is
already random (UUID v4 xor-fold), so collisions are birthday-bounded
on u64 — but a collision means two volumes silently sharing stripe
files, a data-integrity class we don't accept probabilistically when
determinism costs one line: fold the shard ordinal into the top 8 bits
(`shard_id << 56 | random_56`). 2^56 ids per shard; the 0 sentinel
rule preserved. Legacy pre-shard file_ids all live on shard 0 and
cannot collide with shard>0 allocations by construction.

### Client behavior (kernel) — nothing to build

The kernel NFS client keeps one client instance per server address:
a node mounting volumes from three shards runs three independent
sessions, slot tables, and lease clocks. That's standard multi-server
NFS, exercised by every multi-filer shop. Renewals per shard are
negligible overhead.

### Failure semantics

- Shard pod dies → its volumes lose metadata service until reschedule;
  other shards unaffected. Per-shard restart hardening (grace,
  state.db reload, instance counter, client re-registration) is
  exactly the single-MDS machinery, per shard.
- The csi-node-roll landmine now hits at most the shards whose pods
  share the rolled node — and the MDS scale-cycle recipe applies
  per shard.

## Phases

- **Phase 0 — chart topology. DONE 2026-07-07.** Ranged single-replica
  Deployments + per-shard Services + role-labeled NetworkPolicies,
  `count: 1` default. Acceptance met: N=1 render diff vs the v1.12.0
  chart is strictly additive (role/shard labels, FLINT_MDS_SHARD_ID
  env, the flint-pnfs-mds-0 Service) — no renames, no removals, so a
  pre-sharding install upgrades in place with its PVC untouched; N=3
  renders 3 Deployments/PVCs/Services with shard 0 on legacy names.
- **Phase 1 — CSI routing. CODE DONE 2026-07-07** (unit-covered; the
  live N=2 rig legs run with the Phase 3 drills, since a multi-shard
  fleet is only functional once Phase 2's DS fan-out exists).
  `PnfsShards` in pnfs_csi.rs: pick = FNV-1a(name) % N — retry-stable
  by design (least-loaded could double-provision on provisioner
  retries; recorded as future work needing an ownership pre-check);
  pin travels as `~m<shard>` volume_id suffix; route() strips it and
  sends the BARE name to the MDS; no suffix ⇒ shard 0; out-of-range
  pin ⇒ ShardRouting error naming the scale-down mistake. The suffix
  doubles as the pNFS marker in DeleteVolume (closes the PV-already-
  GC'd leak) and gates an honest expand refusal. Chart renders
  FLINT_PNFS_MDS_SHARD_ENDPOINTS (ordered, index = shard) alongside
  the legacy single-endpoint var for older driver images.
- **Phase 2 — DS fan-out + file_id shard bits.** Acceptance: both
  shards grant layouts against the same DS fleet; stripe files from
  two shards coexist; cleanup from each shard removes only its own;
  identity drill unchanged.
- **Phase 3 — drills + runbook.** New drills: shard-down blast radius
  (volumes on the healthy shard keep full service through a shard-0
  kill), per-shard landmine scale-cycle, dual-shard fsx concurrently.
  Runbook: "which shard owns volume X" (read the PV), per-shard
  scale-cycle, scale-out/scale-in rules.
- **Phase 4 — bench.** mdsbench against N=2/N=4 with per-shard volume
  pools; acceptance: aggregate w1-create ≥ 1.8× at N=2, ≥ 3.5× at N=4
  of single-shard baseline; single-volume numbers unchanged (proves
  isolation, no regression).

Estimated 4–6 sessions total. The MDS server core is untouched except
file_id shard bits — sharding is deployment topology + routing, which
is what makes it clean.

## Non-goals

- **Active-active MDS for one volume** — distributed stateids/session
  state; ruled out in the durable-DS plan, still ruled out.
- **Intra-volume (subtree) sharding / NFSv4 referrals** — cross-shard
  RENAME is either broken or a copy, and rename-commit is load-bearing
  for the Spark committer path we just enabled. CephFS-style dynamic
  subtree partitioning is a different product.
- **Volume migration/rebalance between shards** — future work; v1
  pins forever and balances only via placement of new volumes.
- **Metadata HA / standby replicas** — orthogonal (would be per-shard
  anyway); the restart-hardening path is the current answer.

## Risks / watchpoints

- **Shard-suffix in volume_id** touches every CSI RPC's id parsing —
  keep it strictly additive (`~m<i>` suffix, absent ⇒ shard 0) and
  gate with the full kuttl + lima csi-e2e matrix.
- **DS fan-out registration** multiplies the re-register/NACK state
  machine by N; the mds-restart-load drill must run with N=2 to catch
  cross-shard interference in the heartbeat thread.
- **Operator confusion**: N state.dbs, N export trees. The dashboard
  and runbook work in Phase 3 is not optional polish — without it,
  debugging "which shard owns this volume" costs every incident
  minutes.
- **Tier 2 interaction**: moot — Tiers 2–3 shelved by decision
  2026-07-07; sharding proceeds alone, so gate-baseline changes are
  attributable to it by construction.
