# Phase 0 hazard reproduction — live cluster, 2026-06-10

Companion to `incremental-replica-rebuild.md` §3/§9. Both §3 hazards were
reproduced end-to-end on a fresh cluster running the released code, and both
manual recovery procedures were validated. **Net result: today, any pod
reschedule or replica-node reboot permanently breaks a 2-replica volume's
redundancy, and a reschedule bricks the volume entirely (RTO = ∞ without
manual SPDK surgery; data itself remains intact on both replicas).** The §3
superblock-examine mechanism is real and behaves exactly as the code analysis
predicted, but in practice the failure chain fires *earlier*, through
driver-layer bugs this repro surfaced — fixing the sb hazard alone would not
make restage or reboot-recovery work.

## Environment

- 4 nodes (1 control plane + 3 workers `tests-aws-1/2/3`), AWS, Amazon Linux
  2023, kernel 6.1.174, local NVMe (i3en-class, ~434 GiB lvolstore per worker).
- Flint v1.0.0 images (`dilipdalton/flint-driver:1.0.0`,
  `dilipdalton/spdk-tgt:1.0.0`), SPDK v26.01 (git 2ef883ef9), k8s v1.34.8.
- StorageClass `numReplicas: "2"`, `thinProvision: "true"`, `autoRebuild: "true"`.
- Two 2 Gi volumes, each with replica_0 on aws-1 and replica_1 on aws-2;
  workload pods on aws-1; 200 MB random data + recorded md5 on each.
- Baseline confirmed §2's facts directly: raids (`superblock: true`) exist only
  on the workload node; replica nodes export bare lvols; every subsystem is
  `allow_any_host: true`.

## Hazard (b) — pod reschedule bricks the volume (CONFIRMED, worse than predicted)

Procedure: cordon aws-1, delete the pod, let it reschedule (landed on aws-3,
the node holding no replica). Result: NodeStage fails forever; pod stuck
`ContainerCreating`; `need minimum 2 replicas, only 0 available`. Three
independent blocking layers, in the order they fire:

1. **Zombie raid at the old node.** NodeUnstage unmounts and returns success
   but never deletes the raid bdev: `raid_pvc-…` stayed ONLINE on aws-1 with
   both bases after pod deletion, volume detach, and unstage. Its exclusive
   claim (`claimed: true, claim_type: exclusive_write`) on the local replica
   lvol makes aws-1's export fail: `nvmf_subsystem_add_ns` → `-32602 Invalid
   parameters`. The old node also keeps its NVMe-oF initiator connection to
   the other replica's export, and the volume's loopback subsystem and kernel
   nvme controller linger as well.
2. **Non-idempotent export path.** On aws-2 (replica already exported since
   creation) the new node's export request fails the same way: re-adding an
   existing namespace is `-32602`. `nvmf_create_subsystem` is wrapped with
   "already exists (idempotent)" handling, but `nvmf_subsystem_add_ns` and
   `nvmf_subsystem_add_listener` are not. **This alone bricks
   return-to-origin**: re-pinning the pod back to aws-1 still fails (`only 1
   available` — local replica fine, remote add_ns duplicate).
3. **The §3 examine/-EEXIST layer, exactly as documented.** Demonstrated
   manually on aws-3 (it is masked behind layers 1–2 in the driver's own
   flow): `bdev_nvme_attach_controller` to aws-2's replica export →
   examine read the sb at block 0 and **instantly auto-assembled a phantom**
   `raid_pvc-…` (state `configuring`, 1/2 bases, missing-base uuid listed,
   attached bdev claimed `exclusive_write`) → a Flint-equivalent
   `bdev_raid_create` with the volume's raid name returned
   `-17: File exists`.

Additional finding: **NodeStage is not idempotent across its own retries.**
During recovery validation, one staging attempt got through export + attach +
raid create and then failed waiting for the kernel loopback device
("NVMe device did not appear after 3 seconds" — a stale kernel controller
needed an async ns rescan; a fresh connect enumerates in time). Every
subsequent kubelet retry then failed at a *different* step poisoned by the
partial success (add_ns duplicate, then — after manual ns removal —
add_listener duplicate). Each partially-successful attempt plants a landmine
for the next; the loop never converges.

## Hazard (a) — replica node reboot permanently orphans the replica (CONFIRMED)

Procedure: reboot aws-2 (remote-replica node for vol1) while vol1's pod runs
on aws-1. Result, on aws-2 after reboot:

- lvolstore auto-loaded, both replica lvols re-registered **carrying raid
  superblocks** → examine auto-assembled **two phantom raids** (`configuring`,
  1/2 bases), claiming both replica lvols `exclusive_write`. Matches §3
  consequence (a) precisely.
- NVMe-oF subsystems are not persisted; after reboot only the discovery
  subsystem existed. When an export was later attempted (vol2's NodeStage
  retries), the subsystem was recreated but `add_ns` failed `-32602` against
  the phantom's claim, leaving an **empty subsystem**.
- **The reconcile that should re-export replicas is dead code in practice:**
  `reconcile_replica_targets` (node_agent.rs:1660) lists PVs by label
  `flint.csi.storage.io/replica-{node_uid}=true`, but CreateVolume never
  applies any label to PVs (verified: PVs have `LABELS: <none>`). Reconcile
  logged `success_count=0 skip_count=0 error_count=0` — it reconciles an empty
  set. vol1's replica re-export was therefore never even *attempted*.

On the consumer side (aws-1):

- The raid kept reporting `online`, 2/2 configured for **minutes** while the
  underlying nvme controller sat in state `failed`. **Failure detection is
  purely I/O-driven**: only when a direct write hit the dead leg did the raid
  drop it (online, 1/2). Buffered writes + sync succeeded throughout (single
  surviving leg acks the write — live confirmation of the §5 raid1
  single-leg-ack semantics).
- **No detection, no repair, no signal anywhere else:** PV
  `replicas[].health` still said `online` for both replicas; the controller
  logged zero rebuild/health activity (`autoRebuild: "true"` is a no-op for
  this); no k8s event. vol1 ran un-redundant indefinitely with the control
  plane reporting it healthy.

## Recovery procedures (both validated on stock SPDK v26.01)

**vol1 — degraded but mounted (replica orphaned by reboot):**
1. On the replica node: `bdev_raid_delete <phantom>` (frees the claim).
2. Recreate the export (`nvmf_create_subsystem` + `add_ns` + `add_listener`)
   — add_ns succeeds the moment the phantom is gone.
3. On the consumer node: `bdev_nvme_detach_controller` (old failed controller)
   then `bdev_nvme_attach_controller` with the same name/subnqn.
4. The re-attached base's sb uuid matches the live raid → examine re-admits it
   automatically and starts a **full rebuild** (~2 GiB rebuilt in well under
   2 min on idle i3en). Raid back to `online 2/2`; data md5 verified.

This is the stock heal path: it works, but at full-copy cost — on real volumes
exactly the cost the incremental design eliminates.

**vol2 — fully bricked (zombie raid + phantoms + poisoned exports):**
1. Stop the retry source (delete the pod).
2. Old node: `bdev_raid_delete` the zombie raid; `bdev_nvme_detach_controller`
   the stale remote-replica controller; disconnect the stale kernel loopback
   controller (`echo 1 > /sys/class/nvme/nvmeX/delete_controller`).
3. Replica node: `bdev_raid_delete` the phantom; remove any half-created
   export state (`nvmf_subsystem_remove_listener`, `nvmf_subsystem_remove_ns`)
   so the driver's non-idempotent export sequence can run start-to-finish.
4. **Wipe the on-disk raid superblocks on every replica** (v26.01 has no
   `bdev_raid_delete clear_sb`): export each lvol through a temporary
   subsystem, `nvme connect` from the host kernel, `dd if=/dev/zero bs=512
   count=8 oflag=direct` over block 0 (`SPDKRAID` magic verified before/after;
   raid data starts at the 1 MiB `data_offset`, so this touches no user data),
   disconnect, delete the temporary subsystem.
5. Recreate the pod. NodeStage runs clean: export → attach (no sb → **no
   phantom**) → `bdev_raid_create` succeeds and admits both identical replicas
   **as in-sync with no rebuild** (`process: None`). Pod Running, md5 intact.

Step 5 is also the live demonstration of the Tier-1 cornerstone:
`bdev_raid_create` over equalized bases starts the array in-sync with zero
copy. Caveat from the failed intermediate attempt: any raid create with
`superblock: true` immediately re-writes sbs onto the bases — a partial stage
that dies after raid create re-arms the §3 hazard (we had to wipe twice).

## Consolidated new-bug list (beyond the §3 mechanism itself)

| # | Bug | Where | Effect |
|---|-----|-------|--------|
| 1 | NodeUnstage never deletes the raid / detaches per-replica controllers / disconnects the kernel loopback controller | node unstage path | zombie raid + claims brick later restage; stale writer risk (§3 fencing) |
| 2 | `nvmf_subsystem_add_ns` / `add_listener` not idempotent (only `create_subsystem` is) | export path used by NodeStage + reconcile | restage fails even on the original node; retry loops can never converge |
| 3 | NodeStage not convergent across retries | whole stage sequence | any partial failure (e.g. device-wait timeout) permanently poisons subsequent retries |
| 4 | `reconcile_replica_targets` queries PV label `flint.csi.storage.io/replica-{node_uid}` that CreateVolume never sets | node_agent.rs:1666 / CreateVolume | post-reboot replica re-export is dead code; replicas orphaned silently |
| 5 | 3-second kernel-device wait too tight when a stale kernel controller must rescan | block-device creation | flaky NodeStage failure that then triggers bug 3 |
| 6 | Leg failure detected only by I/O; PV `replicas[].health` never updated; no events; `autoRebuild` no-op | health/monitoring | silent redundancy loss, control plane reports healthy |
| 7 | Node SA lacks RBAC to update PVs ("Failed to store block device info in PV … Forbidden", non-fatal log) | chart RBAC / node code | node-side PV state writes silently fail |

Bugs 1–5 must be fixed for restage/reboot recovery to work at all; they are
prerequisites to (and partially overlap) the §3 sb-hygiene work, and all sit
in phase 0 of §9. Bug 6 becomes the trigger surface for phases 1–4 (sync-state
tracking and catch-up); bug 7 is a one-line chart fix.

## Evidence pointers

Raw command transcripts (baseline dumps, raid/bdev/nvmf state at each step,
exact error strings, rebuild progress) are in the session log for
2026-06-10; key artifacts inline above. Volumes/pods were left healthy on the
test cluster (`writer-vol1`, `writer-vol2` Running on tests-aws-1, both md5
verified, both raids online 2/2).
