# Incremental replica rebuild (snapshot-epoch delta resync)

**Status:** design / proposal — **revision 2**
**Author:** rev 1 drafted with Claude (Opus 4.8); rev 2 revised with Claude (Fable 5), 2026-06-10
**Scope:** Flint CSI multi-replica volumes (SPDK `bdev_raid` RAID1 over lvols)

**What changed in rev 2.** Rev 1 concluded that incremental rebuild requires adopting
a thin SPDK fork carrying Longhorn's raid patches. Rev 2 replaces that with a
**two-tier design**: Tier 1 needs **zero SPDK changes** and eliminates blind full
rebuilds in every case except hot rejoin into a live array; Tier 2 (optional,
data-driven) covers hot rejoin with a **single ~200-line local patch** in the patch
pipeline Flint already runs — not a fork, no Longhorn branch tracking, no delta
bitmap. Rev 2 also corrects several current-state facts (shipped SPDK version,
controller-operator status, NFS data path) and documents a newly found
**superblock-examine hazard** (§3) that any design — and possibly today's code —
must address. All SPDK citations below were re-verified on 2026-06-10 against
stock v26.05 (`/Users/ddalton/github/spdk`, `v26.05-1-gbb2b757ac`); line numbers
may differ slightly in the shipped v26.01, but every cited behavior predates
both. Flint citations are against `main`.

---

## 1. Problem

A multi-replica Flint volume is an SPDK RAID1 (`bdev_raid`, level 1) assembled
over N lvols, one per node, the remote ones reached via NVMe-oF. The raid is
created during **NodeStageVolume** on the workload node (`main.rs:1885` →
`create_raid_from_replicas`, `driver.rs:1584`), with `superblock: true`
(`driver.rs:1743-1751`). When a replica's node goes offline, the array runs
degraded and writes continue to the surviving replicas.

What happens today when the node returns (verified):

- **Nothing re-adds the replica.** The only re-add/rebuild logic lives in
  `controller_operator.rs`, which is **not shipped**: its `[[bin]]` is commented
  out (`Cargo.toml:7-9`), `Dockerfile.csi` builds only `csi-driver` and
  `flint-nfs-server`, and the chart's `spdk-controller-operator` Deployment runs
  the `flint-driver` image with its default `csi-driver` entrypoint (no `command:`
  override). The volume stays degraded until the pod is re-staged. The node agent
  does re-expose the returned replica's lvol over NVMe-oF at startup
  (`reconcile_replica_targets`, `node_agent.rs:1657-1751`), but no live code ever
  calls `bdev_raid_add_base_bdev`.
- **If re-add were wired up, stock SPDK does a blind full rebuild.**
  `bdev_raid_add_base_bdev` on an online array always starts the rebuild process
  (§7 has the code evidence). raid1 rebuild reads every window from a healthy
  base and writes it to the target (`raid1.c:564-584`, window walk
  `bdev_raid.c:2866,3082`) — no compare, no dirty tracking, no thin-awareness.
  Worse, every write to a thin lvol allocates a cluster regardless of content
  (`blobstore.c:3221-3256`; allocation is unconditional in
  `bs_allocate_and_copy_cluster`, `:2932-2934`, and even `write_zeroes`
  allocates), so a full rebuild also **destroys the destination's thin
  provisioning**.

For a multi-hundred-GB volume that means a full network copy — and a fully
allocated replica — on every transient outage (spot reclaim, reboot, network
blip). The goal: **copy only the delta the stale replica missed, preserve
thinness, and avoid an SPDK fork if at all possible.**

## 2. Architectural constraints and current-state facts

Two facts about Flint's topology drive everything:

1. **The raid1 bdev is ephemeral and roams.** It is assembled at NodeStage and
   re-created on whatever node next consumes the volume. (Note: it is *not*
   torn down on the old node — see fact 3 below.)
2. **The lvols are the persistent source of truth.** All data — and the
   blobstore's per-cluster copy-on-write allocation map — lives on the lvols,
   which survive reboot, pod death, and node moves.

**Governing principle:** *resync/dirty state must live with the persistent data
(the lvols) and the control plane (PV state), never in the ephemeral raid layer.*
Any scheme that stores rebuild progress only inside the raid bdev is **incorrect**
here: when the raid is re-created on another node it would have no idea a base is
half-stale, and raid1 serves reads from any in-sync base → silent corruption.

Verified current-state facts the design must account for:

3. **There is no raid teardown path.** `bdev_raid_delete` exists only in dead
   code (`raid_service.rs:44-66`, zero callers — all of `raid_service.rs` is
   dead; the live raid code is private methods in `driver.rs`). NodeUnstage
   unmounts, deletes the block device, and detaches only the *volume-level* NQN
   (`main.rs:2464-2468`) — but raid base replicas use per-replica NQNs
   `{volume_id}_{index}` (`driver.rs:1695`), so the old node's SPDK keeps the
   raid bdev **and** its NVMe-oF controllers to the replicas until that SPDK
   restarts. An orphaned-but-ONLINE raid can still write raid superblocks to the
   replica lvols (e.g., marking a base FAILED on a transient path error) and
   fight a newer assembly. **Orphan hygiene is a design responsibility** (§3, §9
   phase 0).
4. **`superblock: true` puts a raid superblock at block 0 of every replica
   lvol**, with base data starting at a 1 MiB `data_offset`
   (`RAID_BDEV_MIN_DATA_OFFSET_SIZE`, `bdev_raid.h:13`; default applied at
   `bdev_raid.c:3542-3544`). The sb is identical on every *configured* base and
   its `seq_number` increments on each write (`bdev_raid_sb.c:362-392, 432`).
5. **Shipped SPDK is v26.01 plus four local patches**, built by
   `docker/Dockerfile.spdk` (clones `v26.01`; applies `lvol-flush.patch`,
   `ublk-debug.patch`, `blob-recovery-optimized.patch`,
   `blob-shutdown-debug.patch`, plus two inline `sed` edits to
   `lib/nvmf/ctrlr.c`). Rev 1's "our v26.05.x" describes only the local dev
   checkout. **The patch pipeline matters:** Flint already carries SPDK patches
   as part of its image build, so one more small patch (Tier 2) has the same
   maintenance shape as what exists today — it is not a new "fork" posture.
6. **The deployed RWO data path is NVMe-oF loopback** (`values.yaml`
   `blockDevice.backend: "nvmeof"`; the code default is ublk,
   `driver.rs:1133-1146`). Either way the workload consumes a kernel block
   device (`/dev/nvmeXnY` or `/dev/ublkbX`) + filesystem — there is **no
   transparent way to tear down and re-create the raid under a live mount**.
7. **RWX volumes are served by a `flint-nfs-server` pod** that exports a plain
   directory backed by a *synthetic RWO PVC* staged through the normal block
   path on whichever node the NFS pod lands (`rwx_nfs.rs:198-257, 388-444`).
   Clients mount via a stable per-volume Service (`rwx_nfs.rs:467-498`). The NFS
   server does not touch SPDK directly — but because it is itself an ordinary
   RWO consumer, **bouncing the NFS pod re-stages the volume (re-assembling the
   raid) while NFS clients ride through with retries**. This is Tier 1's
   transparent-cutover lever for RWX volumes.

## 3. The superblock-examine hazard (pre-existing; likely affects today's code)

Discovered while verifying §7 and serious enough to stand alone. The mechanism
is fully verified in stock SPDK; the end-to-end failures need a live repro
(§9 phase 0).

**Mechanism.** The raid module registers an `examine_disk` hook
(`bdev_raid.c:1497` → `raid_bdev_examine`, `:4124`) that runs on **every bdev
registration** with no opt-out. It reads block 0; on a valid raid superblock with
no matching raid bdev it **auto-creates a CONFIGURING raid named from the sb**
(`raid_bdev_create_from_sb`, called at `:3932`) and **claims the base with the
exclusive module claim** (`spdk_bdev_module_claim_bdev`, `:3519`).

**Consequence (a) — replica re-export after node reboot may fail.** After the
first assembly writes superblocks onto the replica lvols, a replica node's SPDK
restart re-registers the lvol carrying that sb → phantom raid claims it → the
write-mode open inside `nvmf_subsystem_add_ns` fails → `reconcile_replica_targets`
cannot re-export the replica. The replica can't rejoin *at the transport level*,
independent of any rebuild question.

**Consequence (b) — re-staging on a new node may fail.** At NodeStage the driver
attaches the remote replicas (`bdev_nvme_attach_controller`); each attached nvme
bdev carries the sb → examine auto-assembles a phantom `raid_{volume_id}` → the
driver's own `bdev_raid_create` with the same name fails, and the error
propagates to a NodeStage failure (`node_agent.rs:856-868` returns HTTP 500 on
RPC error; `driver.rs:1753` propagates with `?`; no EEXIST tolerance).

Note: first-time staging is unaffected (fresh lvols carry no sb; examine runs at
registration, *before* the first sb write), which is why this can hide from
basic testing.

**Mitigation (zero-patch, control plane).** Own assembly explicitly: before
exporting a replica or assembling a raid, **delete any phantom raid**
(`bdev_raid_delete` works on a CONFIGURING raid and releases the claims), then
proceed. Deletion+fresh-create is data-safe: a fresh `bdev_raid_create` with
`superblock: true` recomputes the same 1 MiB default `data_offset`
(`bdev_raid.c:3542-3544`), so base data alignment is preserved. Because v26.01's
delete does not clear the on-disk sb (`clear_sb` arrived in v26.05), phantom
cleanup must be a standing reconcile step, not a one-shot fix. The same hygiene
pass should reap **orphaned raids and per-replica nvme controllers on
previously-used nodes** (fact 3 in §2) so no stale writer can touch the lvols
during catch-up or after reassembly.

**Fencing (the zombie-consumer case).** Hygiene by RPC assumes the old node is
reachable. The dangerous variant is a node that goes *NotReady* (kubelet dead)
while its SPDK and mounted filesystem stay alive: the pod is rescheduled and the
volume re-staged elsewhere, but the old node still holds an ONLINE raid over the
replicas **and a mounted kernel filesystem that can keep issuing writes**
(journal commits, writeback) — two active writers, classic split-brain. This
exposure exists in Flint today independent of this design, but the design must
not *assume* "no writers" without enforcing it. Flint has a clean fencing lever
available: the per-replica NVMe-oF subsystems can restrict allowed host NQNs —
on re-stage, the orchestrator flips each replica subsystem's allowed-host list
to the new consumer node before assembly, severing any zombie's connections at
the target side (NVMe reservations are the heavier alternative). See §10-9.

(Longer term, `superblock: false` for *new* volumes would remove this hazard
class entirely — PV state is already the authoritative membership record per §2
— but existing volumes cannot switch in place: their data sits at the 1 MiB
sb-mode offset. See §10-7.)

## 4. Approach: snapshot-epoch delta resync, in two tiers

Reuse the dirty tracking the blobstore **already** maintains and persists on each
lvol: the COW cluster-allocation map of a clone relative to its parent snapshot
*is* a record of "what changed since that snapshot." Snapshot all replicas at
common epochs; on rejoin, transfer only the clusters changed since the last
common epoch — replica-to-replica, **outside** the raid.

- **Tier 1 (zero SPDK changes):** out-of-band catch-up brings the returned
  replica to a *warm standby* that trails the array by ≤ `T_snap` + one delta
  copy; it rejoins as a full member **at the next raid assembly**, where
  membership is decided by the control plane and no rebuild ever starts (§6).
- **Tier 2 (optional, one ~200-line carried patch):** *hot* rejoin into a live
  array via a `skip_rebuild` flag on `bdev_raid_add_base_bdev` plus a quiesce
  mechanism (§7 — we choose a quiesce/unquiesce RPC pair) plus an upstream
  esnap clone — functionally what Longhorn's fork `grow` primitive does.
  Built only if Tier 1's measured residual (time spent degraded with a ready
  standby and no reassembly event) justifies it.

Actors (rev 1's component map, updated):

- **Snapshot scheduler** (control plane, Rust): cuts common-epoch snapshots on
  all in-sync replicas, manages retention.
- **Catch-up orchestrator** (control plane, Rust): detects returned replicas,
  runs hygiene (§3) and the catch-up sequence (§5), maintains standbys, and
  gates reassembly admission (§6). Hosting for both is decided in §9 phase 2
  (revive the controller-operator binary vs. fold into existing loops).
- **Node agent** (existing): exposes lvols over NVMe-oF and proxies SPDK RPCs.
- **Persistent state — PV annotations** (chosen over a separate DB): per-replica
  `{node, lvol_uuid, sync_state, last_epoch}` plus the current common-epoch
  name. This — not the raid superblock — is the authoritative membership and
  sync record.

Why this fits the governing principle: the delta lives in the lvol's blobstore
metadata (persisted, roams with the data, no new on-disk format), and the
catch-up runs between two persistent lvols over NVMe-oF, not inside the raid. If
the raid is torn down or re-created mid-catch-up, the stale replica simply was
never a member yet — there is no half-rebuilt base to mistake for valid, and the
orchestrator restarts the idempotent copy from the latest common epoch.

Relation to Longhorn V2 (corrected from rev 1): Longhorn has **no epoch
scheme** — its engine walks the healthy replica's snapshot tree
(`getRebuildingSnapshotList`, `engine.go:1428-1456`) and rebuilds the whole
chain on the destination; its `fastSync` additionally relies on **fork-only lvol
RPCs** (fragmap, range shallow copy, snapshot checksums). Our epoch scheme is
deliberately shaped so the data movement needs **only upstream primitives**.

## 5. Common machinery (all upstream; contracts verified)

Used by both tiers. The delta/clone RPCs below (`shallow_copy` + `check`,
`set_parent`, `set_parent_bdev`, `clone_bdev`) are upstream since v24.05
(CHANGELOG lines 877-882; authored by Damiano Cipriani, SUSE, 2023-07–2024-02)
and present in shipped v26.01; the snapshot RPCs are far older.

- **Epoch snapshots** — `bdev_lvol_snapshot` per in-sync replica at a common
  epoch `epoch-<vol>-<seq>`, every `T_snap`; retain last K; delete older
  (`bdev_lvol_delete`). Flint already drives this RPC
  (`snapshot_service.rs:57-67`).
- **Delta transfer** — `bdev_lvol_start_shallow_copy` /
  `bdev_lvol_check_shallow_copy`. Verified contract:
  - Source blob must be **read-only** (a snapshot; enforced in the blobstore,
    `blobstore.c:7479-7481`, surfacing asynchronously as `state=error`/EPERM).
  - Copies **only clusters allocated in the source blob itself** — not clusters
    inherited through its parent chain (`bs_shallow_copy_cluster_find_next`,
    `blobstore.c:7445-7451`) — i.e., shallow-copying snapshot `E_{n+1}` whose
    parent is `E_n` transfers exactly the `E_n → E_{n+1}` delta, skipping holes.
  - Writes each cluster at the **identical offset** on the destination
    (`blobstore.c:7433-7436`): a sparse, offset-correct image.
  - Destination is any bdev ≥ the lvol's full virtual size
    (`blobstore.c:7487-7495`); the lvolstore block size must be an integer
    multiple of the destination's (`:7497-7503`); the destination is exclusively
    claimed for the copy's duration (`vbdev_lvol.c:2047-2055`).
  - The source blob takes a locked-op for the copy (`blobstore.c:7507-7514`):
    concurrent snapshot/resize/set_parent on *that blob* fail EBUSY — the
    scheduler must serialize per-blob operations (§10-3).
- **Chain alignment** — `bdev_lvol_set_parent` (re-parent a thin clone onto a
  same-lvolstore snapshot; exact size match required, `blobstore.c:7702-7706`)
  and `bdev_lvol_set_parent_bdev` (re-parent onto an external bdev/esnap).
- **Esnap clones** (Tier 2) — `bdev_lvol_clone_bdev`: a thin clone of an
  arbitrary external bdev; reads of unallocated clusters forward to it
  (`blobstore.c:3213-3215`; opened read-only with a shared claim,
  `vbdev_lvol.c:1956-1962`). The external bdev needs a static UUID; if absent at
  lvs load the clone comes up degraded and hotplug-recovers.
- **Catch-up sequence** (R_src = an in-sync replica; R_dst = the returning stale
  replica; E = most recent common epoch):
  1. Hygiene pass on R_dst's node and any previous consumer node (§3).
  2. Re-expose R_dst over NVMe-oF (existing `reconcile_replica_targets` path).
  3. Attach R_dst as a bdev on R_src's node (`bdev_nvme_attach_controller`).
  4. For each epoch `E → E+1 → … → E_latest`: `bdev_lvol_start_shallow_copy`
     of the epoch snapshot to the attached R_dst bdev; poll
     `bdev_lvol_check_shallow_copy`. Online; the volume keeps serving degraded.
  5. Align R_dst's snapshot chain (`bdev_lvol_snapshot` on R_dst +
     `bdev_lvol_set_parent`) so both replicas carry the same epoch lineage.
  Note the sb region caveat: with `superblock: true` the raid sb occupies the
  first 1 MiB of each replica lvol and *changes on membership events*
  (`seq_number` bump marking R_dst FAILED), so the copied delta will faithfully
  bring R_src's sb — which marks R_dst's own slot FAILED — onto R_dst. That is
  harmless under this design (reassembly rewrites the sb; Tier 2's add rewrites
  it too) but must never be "fixed up" by hand-editing — §7 shows superblock
  surgery cannot work anyway.
- **Progress/sizing** — `num_allocated_clusters` per lvol via
  `bdev_lvol_get_lvols` (no fragmap RPC exists upstream; that is Longhorn
  fork-only).

### Correctness note: epoch skew across replicas

Epoch snapshots are cut per-replica while writes flow through the raid, so
replica A's `epoch-N` and replica B's `epoch-N` are **not byte-identical** — a
write racing the cut can land inside one replica's snapshot and after the
other's. This does not break the catch-up. The argument needs only two facts:
(1) raid1 acknowledges a write after **all** live bases complete it, so R_dst
holds every write acknowledged before it went offline; (2) epoch E is recorded
as common only after it **completed on every in-sync replica**, so R_src's E
was cut while R_dst was still receiving writes. Therefore every byte where
R_dst differs from R_src's final state was written on R_src *after* R_src's E
cut — and the copied chain (every cluster R_src changed since its own E) is a
**superset** of the true divergence. Clusters copied redundantly (writes R_dst
already has) are overwritten with identical content. The epoch invariant is
temporal ("completed everywhere before recorded"), not byte-equality — no
quiesce is needed at epoch-cut time.

### Tunables and space overhead

- **`T_snap` (epoch cadence):** smaller → smaller per-epoch deltas (faster
  chasing, smaller final delta at rejoin) but more snapshot churn and COW
  metadata. Start at a few minutes; consider making it adaptive (cut on write
  volume, not just time).
- **`K` (epochs retained):** `K · T_snap` must cover the longest replica outage
  you intend to heal incrementally; a replica offline longer than the oldest
  retained epoch falls back to the thin-aware full build (§6 has the explicit
  state-machine transition).
- **Snapshot space overhead — measured against the code, because it bites.**
  Cutting a snapshot itself allocates almost nothing: it is a cluster-map swap
  (the snapshot blob takes ownership of the head's allocated clusters,
  `bs_snapshot_swap_cluster_maps`, `blobstore.c:6830-6831`) — no data copy.
  But two consequences follow:
  1. **The head silently becomes thin-provisioned**
     (`blob_set_thin_provision(origblob)`, `blobstore.c:6770-6771`) — even if
     the lvol was created thick. **Flint creates lvols thick by default**
     (`thinProvision` StorageClass parameter defaults to `false`,
     `main.rs:868-870`), so the first epoch snapshot quietly revokes the
     volume's full-allocation guarantee: subsequent writes COW into *newly
     allocated* clusters and can hit lvolstore ENOSPC mid-write if capacity was
     budgeted 1×.
  2. **The first snapshot of a thick (or fully written) volume pins the entire
     current image.** As the workload rewrites, the head re-accumulates
     allocated clusters toward a full second copy — i.e., a full-overwrite
     workload approaches **2× the volume size** while that snapshot is
     retained, and in the worst case (full churn every epoch) usage tends
     toward `size + K × per-epoch unique writes`, capped at `(K+1) × size`.
  This is inherent to COW dirty-tracking — it *is* the mechanism the design
  exploits — but it is bounded by provisioning mode and retention:
  - **Prefer `thinProvision: "true"` for epoch-managed volumes.** Then the
    baseline pinned by the first epoch is the actual working set, not the
    volume size, and steady-state overhead is `allocated + Σ retained epoch
    deltas`.
  - **Keep `K` tight and delete epochs promptly.** Deleting a snapshot with a
    single descendant merges cluster ownership into the descendant and frees
    every cluster the descendant had already overwritten — space returns as
    retention rolls.
  - **Propagate discards** (`fstrim`/`-o discard`) so deleted-file clusters are
    released on thin heads (cluster-aligned unmap frees clusters,
    `blobstore.c:3259-3296`); snapshots retain their copies until deleted.
  - Capacity rule of thumb for thick volumes that must remain snapshot-safe:
    budget `2×` (or convert to thin at the next opportunity). §10-4 keeps the
    follow-up to quantify metadata overhead at target sizes.

## 6. Tier 1 (zero SPDK changes): warm standby + rejoin at reassembly

### State machine

Per-replica `sync_state` enum in PV annotations: `in_sync` → `stale` (offline
or behind, no usable catch-up yet) → `standby` (caught up and chasing; prose:
"warm standby") → `in_sync`.

- **Steady state.** All replicas `in_sync`. Scheduler takes common-epoch
  snapshots every `T_snap`; retains K epochs.
- **Replica goes offline.** Array runs degraded (current behavior). Mark the
  replica `stale` with its last-good epoch; stop including it in new epochs.
- **Replica returns.** Run the catch-up sequence (§5) to the latest epoch, then
  **keep chasing**: each new epoch's delta is shallow-copied as it is taken.
  The replica is now `standby`: persistent, thin, trailing the array by
  ≤ `T_snap` + one delta-copy time. It is *not* in the raid and is never a
  read source.
- **Retention expiry.** If a `stale` replica's last common epoch ages out of
  the K retained epochs before catch-up completes, it transitions to the
  thin-aware full build below (E = "empty") — same machinery, larger copy.
- **Rejoin at the next assembly.** NodeStage already re-creates the raid
  (`create_raid_from_replicas`). Insert, in order:
  1. Hygiene + fencing pass (§3) on this node **and any previous consumer
     node**: delete phantom/orphaned raids, detach stale per-replica
     controllers, flip replica subsystems' allowed hosts to this node.
  2. If a standby exists: run the **final delta now** — after step 1 there are
     no writers, so no quiesce is needed; a final snapshot cut here equals the
     head.
  3. Mark `in_sync` in PV state; include the replica in the
     `bdev_raid_create` base list. **Creation admits all listed bases as
     in-sync** — no rebuild process exists at create time (rebuild starts only
     on add-to-ONLINE, §7) — and writes a fresh sb over all bases.
- **Crash / roam during catch-up.** The copy is outside the raid; a teardown or
  re-assembly at any point just abandons it. The replica stays `stale` (or
  `standby`, if it had already caught up) and is not included in assembly until
  its catch-up completes. The orchestrator restarts idempotently from the
  latest common epoch.
- **New / replaced replica (no shared history).** Thin-aware full build: same
  machinery with E = "empty" — shallow-copy *all allocated* clusters of a fresh
  R_src snapshot. Still skips holes, still preserves thinness — unlike stock
  rebuild, which allocates every cluster including zeros (§1).

### Cutover opportunities (when does "next assembly" happen?)

- **Naturally**: pod reschedule/restart, node drain, spot churn — largely the
  same events that cause replica outages in the first place. No action needed.
- **RWX volumes — transparently, on demand**: bounce the `flint-nfs-server`
  pod. Its synthetic RWO PVC is re-staged on restart (raid re-assembled with the
  standby included) while workload pods ride through NFS retries via the stable
  per-volume Service. Clients see a stall, not an error.
- **RWO volumes — by policy**: an opt-in knob (per StorageClass or PV
  annotation) to bounce the workload pod during a maintenance window, for
  workloads that tolerate restarts. Otherwise wait for a natural event.

### Trade-off, stated honestly

Until cutover the array remains degraded: the standby bounds *data-loss
exposure* (if all in-sync replicas were subsequently lost, the standby is behind
by at most `T_snap` + the last delta) but it is **not synchronous redundancy**.
The deciding metric for Tier 2 is therefore: **time spent degraded with a ready
standby and no reassembly opportunity** (§9 phase 6). If pods reschedule often
(spot fleets — the motivating environment), Tier 1 alone may close most of the
gap.

## 7. Tier 2 (optional): hot rejoin with one small carried patch

### Verified: stock SPDK cannot hot-rejoin, full stop

- `bdev_raid_add_base_bdev` takes no options (`bdev_raid_rpc.c:258-261`: only
  `base_bdev`, `raid_bdev`). Adding to an ONLINE array sets
  `is_process_target = true` (`bdev_raid.c:3371`) and unconditionally calls
  `raid_bdev_start_rebuild` (`:3397-3403`). Both the no-sb and matching-sb add
  paths funnel there (`:3698` → `:3424-3438`).
- **Superblock surgery is closed off** (the tempting zero-patch hack: sync data
  out-of-band, craft a sb marking the base CONFIGURED, let examine admit it):
  the ONLINE examine path *asserts* the slot state is MISSING or FAILED
  (`bdev_raid.c:3953-3954`); in a release (NDEBUG) build the forged CONFIGURED
  state is simply never read again and the path still ends in
  `raid_bdev_configure_base_bdev(…, existing=true)` (`:3987` → `:3622-3623`) —
  i.e., the same rebuild. A *higher* `seq_number` than the live array's is
  rejected `-EBUSY` (`:3891`); a *lower* one is ignored in favor of the
  in-memory sb (`:3903`). Every avenue ends in rebuild or rejection.
- **Nothing upstream is coming** (checked 2026-06-10): upstream `master`
  registers only six raid RPCs; no `grow`/`assume-clean`/delta-bitmap change is
  merged or pending on GitHub or `review.spdk.io`; the feature request
  (spdk/spdk#3349, opened 2024-04) is still open/Todo. CHANGELOG v24.01–v26.05
  contains no alternative primitive.

### The minimal primitive — and proof it's the right one

The only missing piece is **"add this base without starting the rebuild
process."** Longhorn's fork implements exactly that: its `grow` path sets
`base_info->skip_rebuild = true` immediately before configuring the base
(`longhorn/spdk` branch `longhorn-v25.09`, `bdev_raid.c` ~4363), wrapped in
quiesce + superblock write. The fork's four raid RPCs
(`bdev_raid_rpc.c:780,845,916,989` on that branch) bundle this with a delta
bitmap we deliberately **do not want** (in-memory; only helps faults within one
raid lifetime; it cannot survive a roaming raid — §2's governing principle).

So Tier 2 is: an optional **`skip_rebuild` flag on `bdev_raid_add_base_bdev`**,
carried as one more `.patch` in `Dockerfile.spdk`.

### Verified patch shape (traced on v26.05; port to shipped v26.01 analogous)

- Plumbing: `schema/schema.json` param (+`genrpc.py` regeneration — RPC decoder
  structs are build-generated in this SPDK era), decoder row + call site in
  `bdev_raid_rpc.c`, prototype in `bdev_raid.h`, flag stored **on
  `raid_base_bdev_info`** — it must survive the silent divert into
  `raid_bdev_examine_sb` when the added bdev carries a matching old sb
  (`bdev_raid.c:3429`), which it will after a shallow-copy catch-up (§5).
- Skip branch in `raid_bdev_configure_base_bdev_cont`: don't set
  `is_process_target`; replicate the three state mutations
  (`is_configured`/`discovered` `:3377-3379`, `operational` `:3398`).
- New completion sequence modeled on the existing process-finish code
  (`:2772-2826`): `spdk_bdev_quiesce` → install `base_channel[slot]` on every
  channel → write sb → unquiesce. The sb flip helper
  (`raid_bdev_process_finish_write_sb`, `:2639-2663`) is target-agnostic and
  reusable verbatim. The plain add path does **not** quiesce (its channel sync,
  `raid_bdev_ch_sync` `:3333-3337`, is a visibility barrier only), so the patch
  must add this — the primitives and pattern already exist in-file.
- **The patch must also expose a quiesce window to the control plane.**
  `spdk_bdev_quiesce` is a C API with no upstream RPC, and the skip-rebuild
  add's internal quiesce covers only the add itself. The hot-rejoin sequence
  below requires snapshot→clone→add to be atomic w.r.t. writes: any write
  landing on survivors after the final snapshot but before the add would exist
  nowhere on R_dst (not under its esnap parent, not fanned out) and could not
  be backfilled safely onto a live member without racing live writes — silent
  divergence. So the patch additionally adds either (a) a
  `bdev_raid_quiesce`/`bdev_raid_unquiesce` RPC pair wrapping
  `spdk_bdev_quiesce` (~30–50 lines), with the control plane performing
  snapshot/clone/add inside the window, or (b) an atomic variant of the add
  RPC that takes the snapshot itself. **We choose (a)**: it is simpler, and a
  raid-quiesce RPC is independently useful — e.g. it would let an epoch cut be
  made atomic across replicas if the §5 epoch-skew argument ever needs
  tightening, and it gives `lvol-flush` a clean pre-snapshot sync point
  (§10-6).
- Estimated ~150–200 lines of C **including the quiesce RPC pair**, ~180–250
  total with schema/CLI. Crash safety is fail-*safe*: a crash between channel
  install and sb write leaves the slot FAILED on disk → next assembly treats
  the replica as stale (a redundant catch-up, never corruption).

### Correct hot-rejoin sequence (one short quiesce window, metadata ops only)

1. Bulk catch-up R_dst to the latest epoch (§5) — online, hours if need be.
2. Quiesce the raid → take final snapshot `E_f` on survivors → expose R_src's
   `E_f` over NVMe-oF → create R_dst's new head as an **esnap clone** of it
   (`bdev_lvol_clone_bdev`) → `bdev_raid_add_base_bdev … skip_rebuild=true` →
   unquiesce. All steps inside the window are metadata operations.
3. From unquiesce: new writes fan out to R_dst's head; reads of not-yet-local
   clusters forward through the esnap to `E_f` — **correct from the first I/O**.
4. Backfill the remaining epoch deltas via `shallow_copy` at leisure; then
   `bdev_lvol_set_parent` to localize the chain and drop the esnap dependency.

> Safety gate (unchanged from rev 1): a base must never be a read source unless
> its reads are genuinely consistent. Here that is structural: the esnap parent
> is the snapshot taken **inside the same quiesce window** as the add. Whether
> Longhorn holds IO across snapshot→grow or relies solely on grow's internal
> quiesce is open question §10-2 — our sequence above is the conservative
> ordering that is correct regardless.

### What we deliberately do not port

The delta-bitmap RPCs (wrong tool for a roaming raid, per §2) and Longhorn's
fastSync lvol RPCs (`bdev_lvol_get_fragmap`, range shallow copy, snapshot
checksums — all fork-only). Upstream full shallow copy of epoch deltas is
sufficient; epoch granularity replaces fastSync. Longhorn's
`bdev_raid_clear_base_bdev_faulty_state` is also unnecessary in our flow: it
services the fork's delta-bitmap faulty-state machinery, whereas our stale base
is fully removed from the array (slot FAILED/MISSING in the sb) and re-admitted
through the patched add, whose completion path flips the slot to CONFIGURED
(the reused `raid_bdev_process_finish_write_sb` keys purely off
`is_configured`).

## 8. What is upstream vs. what needs the patch

- **lvol/blobstore delta primitives — UPSTREAM** (since v24.05, in shipped
  v26.01): `shallow_copy` + `check`, `set_parent`, `set_parent_bdev`,
  `clone_bdev` (esnap). Verified present and contracts as in §5.
- **raid "add as in-sync" (+ a quiesce RPC) — NOT upstream anywhere** (verified
  2026-06-10): fork-only in `longhorn/spdk`; Tier 2 carries the minimal
  ~200-line equivalent as a local patch in the existing `Dockerfile.spdk`
  pipeline.

Rejected alternatives:

- **In-raid in-memory write-intent bitmap:** incorrect for a roaming raid (§2).
- **Persisted md-style on-base WIB:** correct but a large crash-consistent
  format change; duplicates dirty info the blobstore already persists.
- **Superblock surgery / examine tricks:** *proven impossible* — see §7's
  evidence; every path ends in rebuild or rejection.
- **Porting Longhorn's fork branch wholesale (rev 1's recommendation):**
  superseded — it imports the delta bitmap and fastSync surface we don't want,
  plus a fork-tracking obligation, for a primitive that reduces to ~200 lines.
- **Custom replication vbdev (drop bdev_raid):** most work; reinvents raid1's
  write fan-out. Reserve for if we outgrow raid1.

## 9. Phasing

0. **Repro + fix the §3 examine/orphan hazards.** Reboot-replica repro
   (export fails?) and restage repro (EEXIST?). Add the hygiene pass to the node
   agent reconcile and the pre-assembly path; add raid teardown + per-replica
   NQN detach to NodeUnstage; add allowed-host fencing on re-stage (§3).
   *Independent bug fix; prerequisite for everything below; ships on its own.*
1. **Persistent replica sync-state** in PV annotations (`sync_state` ∈
   `in_sync`/`stale`/`standby`, `last_epoch`, current epoch name). *Control
   plane.*
2. **Snapshot scheduler** (common epochs + retention). *Control plane.* Decide
   hosting: revive the controller-operator binary (currently dead per §1; its
   raid-status/replace RPCs also route to `localhost:5260` instead of the
   per-node agent and need fixing) or fold into existing loops (node agent's 30s
   interval; controller's capacity-cache refresh loop).
3. **Catch-up orchestrator**: detect returned replica → hygiene → bulk
   shallow-copy → epoch chasing (warm standby). *Control plane.*
4. **Tier 1 reassembly admission**: final delta at NodeStage + standby inclusion
   in `bdev_raid_create`; RWX NFS-pod bounce; RWO pod-bounce policy knob.
   *Control plane.*
5. **Thin-aware full build** for new/replaced replicas. *Control plane.*
6. **Measure** the Tier 1 residual: time degraded with a ready standby and no
   reassembly event. *Decides Tier 2 with data.*
7. *(Conditional)* **Tier 2**: `skip_rebuild` patch + esnap-clone hot rejoin
   (§7).
8. **Tests:** offline→rejoin delta resync; roam-during-catch-up (no
   corruption); outage past epoch retention → thin-aware full build; reboot →
   phantom-raid repro; restage → EEXIST repro; power-cut during final delta;
   Tier 2: quiesce-window bound; crash between channel install and sb write
   (must trigger rebuild-or-recatchup, never serve stale reads).

Phases 0–5 are pure Rust control plane against upstream RPCs. The SPDK patch
decision moves from "gating dependency, sequence first" (rev 1) to "phase 7,
decided by phase 6's data."

## 10. Open questions to validate

1. **Repro the §3 hazards** on a live cluster. If consequence (a) reproduces,
   multi-replica volumes currently cannot heal at the transport level after a
   replica-node reboot — raising phase 0's priority above this design.
2. **Longhorn's snapshot→grow atomicity** — read `engine.go` `ReplicaAdd`
   (~:824) and `replica.go` `RebuildingDstStart` (~:3020): is engine IO
   suspended across snapshot→grow, or does grow's internal quiesce suffice?
   Informs whether our §7 single-window sequence can be relaxed.
3. **Shallow-copy locked-op interplay**: a chasing copy holds the source epoch
   snapshot's blob lock (EBUSY for concurrent ops on that blob) — confirm the
   scheduler's snapshot/delete cadence never needs to touch a blob mid-copy.
4. **Snapshot/COW cost** (metadata + held space) at target volume sizes; pick
   `T_snap`/`K` from measurement; consider write-volume-adaptive `T_snap`.
5. **Two replicas simultaneously stale:** catch-up ordering; which is
   authoritative; do we chase both as standbys concurrently?
6. **`lvol-flush` patch interaction with epoch snapshots:** epoch and
   final-delta snapshots are crash-consistent only — does the data path need a
   flush (and does the `lvol-flush` patch provide the right hook) immediately
   before a snapshot cut, so the snapshot captures all completed-and-acked
   writes from the guest's perspective?
7. **`superblock: false` for new volumes**: removes the §3 hazard class and
   makes the control plane the sole membership authority (already the §2
   principle). Needs a split-brain analysis (what stops a stale node assembling
   raid over lvols without sb protection? — note the sb does not actually
   prevent this today either, per §3) and a migration story for existing
   volumes.
8. **Orphan reaping completeness**: enumerate everything a dead consumer node
   can leave behind (raid bdev, nvme controllers, ublk/nvmf frontends, mounted
   filesystems) and make the hygiene pass cover all of it.
9. **Fencing design** (§3): allowed-host-NQN flipping on the per-replica
   subsystems vs. NVMe persistent reservations; interaction with the node
   agent's startup re-export; verify a severed zombie cannot reconnect before
   the allowed-host list is updated on a node that was unreachable during
   re-stage.
