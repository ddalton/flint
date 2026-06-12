# Incremental replica rebuild (snapshot-epoch delta resync)

**Status:** design / proposal — **revision 5**
**Author:** rev 1 drafted with Claude (Opus 4.8); revs 2–5 revised with Claude (Fable 5), 2026-06-10/12
**Scope:** Flint CSI multi-replica volumes (SPDK `bdev_raid` RAID1 over lvols)

**What changed in rev 5** (2026-06-12, after the full e2e cluster campaign):
phases 0–5b were **validated end-to-end on a live 4-node cluster** — epoch
cadence + retention, stale→standby heal in seconds, chase, fenced no-rebuild
admission (§5 cornerstone re-pinned), §11 snapshots/restore/tombstones, and
the phase-5 full build from empty with user snapshots preserved; the standard
8-test kuttl suite passed before and after (see `e2e-campaign-2026-06-12.md`
for the full evidence ledger). The campaign surfaced and fixed three driver
bugs: §11 snapshot names overflowed SPDK's 63-char lvol-name limit (suffix
now budget-clamped, retry idempotency preserved); re-staging onto a
replica-hosting node failed `bdev_raid_create` EPERM because the replica's
own stale NVMe-oF export still write-opened the local base (stale local
exports are now dropped at local attach); and restoring from a multi-replica
snapshot silently **destroyed the data** — the clone carried the source
raid's superblock, so bare-lvol staging found no filesystem at LBA 0 and
formatted. That last bug was root-fixed by answering §10-7: **raids are now
created `superblock: false`** (flint never used examine-based auto-assembly;
the control plane is the sole membership authority per §2), which puts the
filesystem at LBA 0 of every base lvol, makes restores work with zero
special-casing, and structurally eliminates the §3 phantom-assembly hazard
class for new volumes. Pre-release layout break: lvols written by
superblocked builds are incompatible — recreate, don't upgrade in place.
NodeStage additionally refuses to format any volume marked
`filesystem-initialized` (loud failure instead of silent data loss). New
open questions: §10-13 (re-replication of restored volumes — restores are
single-replica clones today, SC `numReplicas` ignored) and §10-14
(deletion-path lvol reaping — observed orphaned heads/epochs/exports after
volume deletion with a stale replica, and clone-pinned snapshot copies when
the PV is deleted before tombstone reconcile). Also observed, minor: epoch
cadence halves under record-patch contention; epoch GC retries noisily while
chain bases pin retired epochs.

**What changed in rev 4** (same day, after a live-cluster reproduction):
both §3 hazards were **reproduced end-to-end** on a 3-worker AWS cluster running
the released v1.0.0 images (see `phase0-hazard-repro-2026-06-10.md` for the full
evidence and validated recovery runbooks). The examine/phantom/-EEXIST mechanism
behaves exactly as predicted, but the failure chain fires *earlier* through
driver-layer bugs the repro surfaced: NodeUnstage leaves a zombie raid whose
claim blocks re-export; `nvmf_subsystem_add_ns`/`add_listener` are not
idempotent (bricking even return-to-origin restage and making NodeStage retry
loops non-convergent); and `reconcile_replica_targets` queries a PV label that
CreateVolume never sets, so post-reboot re-export is dead code. Leg failure is
detected only by I/O, PV replica health is never updated, and `autoRebuild` is
a no-op. §9 phase 0 and §10-1 updated accordingly; the Tier-1 cornerstone
(`bdev_raid_create` admits equalized bases as in-sync, zero copy) was also
**demonstrated live** during recovery validation.
Post-review clarification (2026-06-11): §5 retention now explicitly scopes
epoch cleanup to internal rebuild-owned snapshots only; user-created CSI
`VolumeSnapshot`s remain governed by Kubernetes snapshot lifecycle and must not
be garbage-collected by the rebuild scheduler.
The same review also tightened §5's correctness note: the catch-up proof now
rests explicitly on three pieces — backed-off base, revert, and copying
`E_b`'s **own** snapshot (the piece that handles ordinary cut skew, including
the `E_latest = E_b` case that the final delta does *not* cover) — plus
withholding `in_sync` until the fenced final delta at reassembly, and §5
step 4 now marks the copy loop `E_b`-inclusive as load-bearing. The Tier-1
cornerstone (`bdev_raid_create` admits equalized bases, no rebuild) is called
out as a regression test the phase-3/4 cluster suite must pin. The review
file was folded into these revisions and removed.

**What changed in rev 3** (same day, after a four-lens adversarial review):
§3's mitigation was corrected — deleting a phantom raid is *not* enough, the
on-disk superblocks must be cleared or fresh creation fails `-EEXIST` (this
motivates bumping shipped SPDK to v26.05.x for `clear_sb`); the §5 epoch-skew
correctness argument was repaired for the failure-transition window (back off
one epoch past the I/O timeout) and gained a mandatory revert-of-the-stale-head
step; the §3 fencing lever was rewritten to match reality (everything is
`allow_any_host: true` today); the §6 RWX ride-through and §7 esnap-backfill
claims were scoped honestly; quiesce leasing and unwind were added to the Tier-2
patch. Rev 1 was an unpublished working draft and is not preserved in git.

**What changed in rev 2.** Rev 1 concluded that incremental rebuild requires adopting
a thin SPDK fork carrying Longhorn's raid patches. Rev 2 replaces that with a
**two-tier design**: Tier 1 needs **zero SPDK changes** and eliminates blind full
rebuilds in every case except hot rejoin into a live array; Tier 2 (optional,
data-driven) covers hot rejoin with a **single ~250-line local patch** in the patch
pipeline Flint already runs — not a fork, no Longhorn branch tracking, no delta
bitmap. Rev 2 also corrects several current-state facts (shipped SPDK version,
controller-operator status, NFS data path) and documents a newly found
**superblock-examine hazard** (§3) that any design — and possibly today's code —
must address. All SPDK citations below were re-verified on 2026-06-10 against
stock v26.05 (`/Users/ddalton/github/spdk`, `v26.05-1-gbb2b757ac`); line numbers
may differ slightly in the shipped v26.01, but every cited behavior predates
both. Flint citations are against `main` plus the thin-provisioning default
flip committed alongside this revision.

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
  `controller_operator.rs`, which is **not compiled at all**: its `[[bin]]` is
  commented out (`Cargo.toml:7-9`) and it is not referenced from `lib.rs`;
  `Dockerfile.csi` builds only `csi-driver` and `flint-nfs-server`, and the
  chart's `spdk-controller-operator` Deployment runs the `flint-driver` image
  with its default `csi-driver` entrypoint (no `command:` override). The volume
  stays degraded until the pod is re-staged. The node agent *intends* to
  re-expose the returned replica's lvol over NVMe-oF at startup
  (`reconcile_replica_targets`, `node_agent.rs:1657-1751`), but the live repro
  showed this is dead code — it selects PVs by a label CreateVolume never sets
  (§3, rev 4) — and no live code ever calls `bdev_raid_add_base_bdev`.
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

## 3. The superblock-examine hazard (pre-existing; REPRODUCED on a live cluster)

> **Resolved for new volumes (rev 5, 2026-06-12):** raids are now created
> `superblock: false` (§10-7 answered), so new base lvols carry no raid
> metadata and the examine hook has nothing to assemble — this hazard class
> is structurally eliminated rather than defended against. The convergence
> defenses below (`ensure_raid1_bdev`, `wait_for_examine`, `clear_sb`
> version-gating) are retained as hardening for any pre-rev-5 lvols and as
> §3 history. Pre-release layout break: superblocked lvols put the
> filesystem 1 MiB in; superblock-less lvols put it at LBA 0 — recreate
> volumes, don't upgrade in place.

Discovered while verifying §7 and serious enough to stand alone. The mechanism
is fully verified in stock SPDK, and **both consequences below were reproduced
end-to-end on a live cluster on 2026-06-10** (v1.0.0 images, SPDK v26.01; full
procedure, evidence, and validated recovery runbooks in
`phase0-hazard-repro-2026-06-10.md`). The repro also showed that in the shipped
driver the §3 mechanism is *masked behind* earlier driver-layer failures —
zombie raids left by NodeUnstage and a non-idempotent export path — which brick
restage before `bdev_raid_create` is ever reached; the phantom/-EEXIST layer
was demonstrated by issuing the driver's exact attach + create sequence
manually. Fixing this hazard class therefore means fixing the §9 phase-0 bug
list as a unit, not just sb hygiene.

**Mechanism.** The raid module registers an `examine_disk` hook
(`bdev_raid.c:1497` → `raid_bdev_examine`, `:4124`) that runs on **every bdev
registration** with no per-module opt-out (the only switch is the global
`bdev_auto_examine=false`, which also disables lvolstore auto-discovery and
would force Flint to own examine ordering via explicit `bdev_examine` calls —
a viable but invasive alternative mitigation). It reads block 0; on a valid
raid superblock with no matching raid bdev it **auto-creates a CONFIGURING raid
named from the sb** (`raid_bdev_create_from_sb`, called at `:3932`) and
**claims the base with the exclusive module claim**
(`spdk_bdev_module_claim_bdev`, `:3519`).

**Consequence (a) — replica re-export after node reboot fails (reproduced).**
After the first assembly writes superblocks onto the replica lvols, a replica
node's SPDK restart re-registers the lvol carrying that sb → phantom raid
claims it → the write-mode open inside `nvmf_subsystem_add_ns` fails
(`-32602`) → the replica cannot be re-exported. The replica can't rejoin *at
the transport level*, independent of any rebuild question. Live repro: after
rebooting the replica node, both replica lvols were claimed by auto-assembled
phantoms within seconds of lvolstore load; the consumer raid silently ran
un-redundant (leg failure detected only on the next real I/O; PV health still
`online`; no rebuild attempted anywhere). Worse, today the re-export is not
even *attempted*: `reconcile_replica_targets` selects PVs by the label
`flint.csi.storage.io/replica-{node_uid}=true` (node_agent.rs:1666) which
CreateVolume never applies — reconcile runs against an empty set.

**Consequence (b) — re-staging on a new node fails (reproduced).** At NodeStage
the driver attaches the remote replicas (`bdev_nvme_attach_controller`); each
attached nvme bdev carries the sb → examine auto-assembles a phantom
`raid_{volume_id}` → the driver's own `bdev_raid_create` with the same name
fails, and the error propagates to a NodeStage failure (`node_agent.rs:856-868`
returns HTTP 500 on RPC error; `driver.rs:1753` propagates with `?`; no EEXIST
tolerance). Live repro confirmed both the instant phantom assembly
(`configuring`, attached bdev claimed `exclusive_write`) and the `-17 File
exists` on the driver-equivalent create. In the shipped driver the restage
actually dies two layers earlier — the zombie raid left ONLINE by NodeUnstage
holds an `exclusive_write` claim on the old node's replica lvol (its export
fails `-32602`), and the still-exported other replica fails the non-idempotent
`add_ns` — so the pod sticks in `ContainerCreating` on *every* node, including
the one it came from. A volume is bricked by a single reschedule (data intact,
RTO = ∞ without manual surgery).

Note: first-time staging is unaffected (fresh lvols carry no sb; examine runs at
registration, *before* the first sb write), which is why this can hide from
basic testing.

**Mitigation — the superblocks must be *cleared*, not just the phantom
deleted.** A fresh `bdev_raid_create` over bases that still carry an old
on-disk sb **fails outright**: the new-bdev configure path always reads the
base's superblock (`raid_bdev_load_base_bdev_superblock`, `bdev_raid.c:3626`),
and a valid sb whose raid uuid differs from the new raid's returns `-EEXIST`
("Superblock of a different raid bdev found", `:3433-3434`). Flint passes no
uuid to `bdev_raid_create`, so the fresh raid gets a random uuid and every base
add fails; passing the old uuid doesn't help (the uuid-match branch diverts
into `raid_bdev_examine_sb` at `:3429`, which fails `-EINVAL` against a fresh
CONFIGURING raid whose `sb == NULL`, `:3876-3880`). So the hygiene pass is:

1. Attach/register all base bdevs, then **`bdev_wait_for_examine`** (upstream
   RPC, `bdev_rpc.c:102`) — examine is asynchronous, and deleting a phantom
   while another base's examine is still in flight lets the phantom re-create
   itself between the delete and our create.
2. Delete any phantom raid **with `clear_sb: true`** (`bdev_raid_delete` param,
   present since v26.05 — this is the concrete reason §9 phase 0 bumps the
   shipped SPDK image from v26.01 to v26.05.x; the interim v26.01 fallback —
   zeroing the first sectors of each replica through a temporary export — is
   ugly but **validated end-to-end** during the 2026-06-10 recovery, see the
   repro doc).
3. `bdev_raid_create` over the intended members; treat `-EEXIST` as "a phantom
   re-appeared" and loop from step 1.

The same `wait_for_examine` + clear discipline applies on **R_src's node during
catch-up** — the attached R_dst bdev carries a valid sb (§5's caveat) and will
spawn a phantom there too. Data alignment across delete+re-create is safe but
conditional: the recomputed default `data_offset` is 1 MiB rounded up to the
base's `optimal_io_boundary` (`bdev_raid.c:3542-3558`), which stays exactly
1 MiB only because Flint creates lvolstores with a 1 MiB cluster size
(`minimal_disk_service.rs:182-186`) — the reassembly path should assert
`recomputed offset == previous offset` so a future cluster-size change cannot
silently shear the data. For completeness: with **no** hygiene at all, stock
behavior on re-stage is phantom-assembly-plus-blind-rebuild — the phantom
assembles under the volume's raid name, the returning stale replica is re-added
by ONLINE examine with a full rebuild (`:3953-3987` → `:3371` → `:3399`), and
Flint's own `bdev_raid_create` then fails NodeStage with `-EEXIST`.

The hygiene pass must also reap **orphaned raids and per-replica nvme
controllers on previously-used nodes** (fact 3 in §2) so no stale writer can
touch the lvols during catch-up or after reassembly.

**Fencing (the zombie-consumer case).** Hygiene by RPC assumes the old node is
reachable. The dangerous variant is a node that goes *NotReady* (kubelet dead)
while its SPDK and mounted filesystem stay alive: the pod is rescheduled and the
volume re-staged elsewhere, but the old node still holds an ONLINE raid over the
replicas **and a mounted kernel filesystem that can keep issuing writes**
(journal commits, writeback) — two active writers, classic split-brain. This
exposure exists in Flint today independent of this design, but the design must
not *assume* "no writers" without enforcing it. The natural lever is NVMe-oF
host filtering — but be clear about the current state: **today every Flint
subsystem is created `allow_any_host: true`** (`driver.rs:1798` per-replica
targets, `node_agent.rs:1861` reconcile re-export, `node_agent.rs:1182`
volume-level, `driver.rs:885`) **and `nvmf_subsystem_add_host` is never
issued**, so there is no list to flip. The fence must be built as *persistent
desired state*, not an after-the-fact flip: create all replica subsystems with
`allow_any_host: false` and an allowed-host list derived from PV annotations,
applied at creation in **both** export paths — otherwise a returning node's
`reconcile_replica_targets` re-exports wide open and reopens the door while the
zombie's kernel initiator is still auto-reconnecting (default `ctrl_loss_tmo`
is ~10 minutes). Two further sharp edges: SPDK's host-removal disconnect is
asynchronous and does not wait for qpairs to drain (`lib/nvmf/subsystem.c:1413`),
so assembly must poll `nvmf_subsystem_get_controllers` until the old host is
actually gone; and a replica **co-located on the zombie node itself** is reached
over loopback through that node's own SPDK and can never be fenced externally —
it must be treated as contaminated and excluded (`stale`) until hygiene runs
there. NVMe persistent reservations are the heavier alternative. See §10-9.

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
- **Tier 2 (optional, one ~250-line carried patch):** *hot* rejoin into a live
  array via a `skip_rebuild` flag on `bdev_raid_add_base_bdev` plus a leased
  quiesce mechanism (§7 — we choose a quiesce/unquiesce RPC pair) plus an
  upstream esnap ("external snapshot") clone — a thin clone whose parent is an
  arbitrary external bdev (§5) — functionally what Longhorn's fork `grow`
  primitive does.
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
scheme** — its engine (`longhorn/longhorn-spdk-engine`) walks the healthy
replica's snapshot tree (`getRebuildingSnapshotList`, `engine.go:1428-1456`)
and rebuilds the whole chain on the destination; its `fastSync` additionally
relies on **fork-only lvol RPCs** (fragmap, range shallow copy, snapshot
checksums). Our epoch scheme is deliberately shaped so the data movement needs
**only upstream primitives**.

## 5. Common machinery and tunables (delta primitives upstream; contracts verified)

Used by both tiers. The delta RPCs below (`shallow_copy` + `check`,
`set_parent`, `set_parent_bdev`) are upstream since v24.05 (CHANGELOG lines
877-882; authored by Damiano Cipriani, SUSE, 2023-07–2024-02), `clone_bdev`
(esnap) since v23.05 (CHANGELOG ~:1236); all present in shipped v26.01. The
snapshot RPCs are far older.

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
    multiple of the destination's (`:7497-7503`); the destination is
    write-claimed for the copy's duration — sole writer, readers still allowed
    (`vbdev_lvol.c:2047-2055`).
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
- **Catch-up sequence** (R_src = an in-sync replica; R_dst = the returning
  stale replica). The catch-up **base epoch `E_b`** is *not* simply the last
  recorded common epoch: it is the newest common epoch whose cut completed at
  least `T_back` (the maximum NVMe-oF I/O timeout plus a clock-skew margin)
  **before** the raid declared R_dst failed — see the correctness note below
  for why. In practice: step back one extra epoch whenever `T_snap` is smaller
  than the I/O timeout.
  0. **Revert R_dst to its own `E_b`** — discard everything R_dst's head holds
     beyond that snapshot (its own unacked in-flight tail, or zombie loopback
     writes): there is no in-place revert RPC, so delete the head and re-create
     it as a clone of R_dst's local `E_b` (`bdev_lvol_clone`), updating the
     replica's lvol uuid in the PV record. The correctness argument below is
     only valid after this step.
  1. Hygiene + fencing pass on R_dst's node and any previous consumer node (§3).
  2. Re-expose R_dst over NVMe-oF (the `reconcile_replica_targets` path, once
     phase 0 fixes its PV-label selection and makes the export idempotent).
  3. Attach R_dst as a bdev on R_src's node (`bdev_nvme_attach_controller`),
     then run the §3 examine discipline **on R_src's node too** — the attached
     R_dst bdev carries a valid raid sb and will spawn a phantom there.
  4. For each epoch `E_b → … → E_latest` — **`E_b` inclusive**; copying
     `E_b`'s own snapshot is load-bearing, not an off-by-one (see the
     correctness note) — `bdev_lvol_start_shallow_copy`
     of the epoch snapshot to the attached R_dst bdev; poll
     `bdev_lvol_check_shallow_copy`. Online; the volume keeps serving degraded.
     A destination-side allocation failure (R_dst's lvolstore ENOSPC) surfaces
     as `state=error`: abort the chase, mark the replica `stale`, raise an
     event — don't retry into a full pool.
     **Amended by §11:** once user `VolumeSnapshot`s can interleave in the
     chain, iterating *epochs by name* splits deltas (the slice held by an
     interleaved user snapshot's blob is silently skipped) — the iteration
     must walk the source's actual blob lineage from `E_b`; see §11.
  5. Align R_dst's snapshot chain (`bdev_lvol_snapshot` on R_dst +
     `bdev_lvol_set_parent`) so both replicas carry the same epoch lineage.
  **Retention pinning:** while a catch-up is active or pending, the scheduler
  must not delete any epoch ≥ the oldest `E_b` in use (record the pin in the PV
  annotation so it survives orchestrator restarts). Deleting epochs *older*
  than every replica's base is always safe — deletion merges the snapshot's
  clusters into its descendant.
  **Ownership boundary:** this retention rule applies only to internal
  rebuild-owned epoch snapshots (`epoch-<vol>-<seq>`). User-created CSI
  `VolumeSnapshot` snapshots are governed by the Kubernetes
  `VolumeSnapshot`/`VolumeSnapshotContent` lifecycle and deletion policy, and
  must never be garbage-collected by the rebuild scheduler. The scheduler should
  identify its own snapshots explicitly (name prefix plus PV annotation/state)
  and delete only those; if an internal epoch participates in a lineage that a
  user snapshot still depends on, cleanup must rely on the blobstore's
  snapshot-delete merge semantics rather than raw removal assumptions.
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

### Correctness note: epoch skew, the failure window, and the revert

Epoch snapshots are cut per-replica while writes flow through the raid, so
replica A's `epoch-N` and replica B's `epoch-N` are **not byte-identical** — a
write racing the cut can land inside one replica's snapshot and after the
other's. That alone is harmless, but only because of how the pieces compose.
The catch-up proof rests on **three pieces** — the **backed-off base** `E_b`,
the **revert** of R_dst to its own `E_b`, and **copying R_src's `E_b`
snapshot itself** before the subsequent deltas (step 4 is `E_b`-inclusive) —
and even then the standby is consistent only up to the latest copied epoch:
it must not be admitted `in_sync` until the fenced **final delta** at
reassembly (§6) completes. No two of these pieces are sufficient. The honest
argument, including the parts a naive version gets wrong:

- *Steady state and cut skew (why `E_b` itself is copied):* raid1
  acknowledges a write only after all live bases complete it, so R_dst holds
  every write acknowledged while it was healthy — but after the revert,
  "holds" means "held at R_dst's own `E_b` cut", and the two replicas' cuts
  are skewed. An acked write landing between the cuts sits *inside* R_src's
  `E_b` yet *after* R_dst's: the revert discards it from R_dst, no post-`E_b`
  delta ever contains it (its clusters are allocated in R_src's `E_b`, not
  later), and the final delta at reassembly misses it for the same reason —
  a chain defined as "changes since `E_b`" loses that write permanently,
  including in the degenerate `E_latest = E_b` case where the per-epoch loop
  would otherwise copy nothing at all. Copying `E_b`'s own snapshot (its
  allocated clusters — the `E_(b-1) → E_b` delta) is what re-supplies it.
  With `E_b` included, the chain covers every cluster R_src changed since
  its `E_(b-1)` cut — a window that brackets R_dst's `E_b` cut by a full
  `T_snap` on each side, swamping both cut skew and I/O lifetimes. Torn
  multi-cluster writes at a cut are benign for the same reason: the argument
  is per-cluster, not per-IO.
- *The failure window (why the last common epoch is NOT a safe base):* raid1's
  completion starts at FAILED and a **single successful leg flips it to
  SUCCESS** (`raid1.c:286-305`, `bdev_raid.c:702-714`), with the failed base
  marked asynchronously (`raid1.c:52-70` → `bdev_raid.c:2440-2444`). So a write
  whose R_dst leg dangles on a dying path can be **acked via R_src alone**,
  land *inside* R_src's epoch-E snapshot, and be missing from R_dst — while E
  is still recorded "common" because epoch cuts travel a control-plane path
  independent of the consumer's data path. A catch-up based at E would never
  copy that write: silent divergence. Backing off to `E_b` (cut ≥ `T_back`
  before the failure) restores the superset property: any acked write missing
  R_dst's leg was submitted within the I/O timeout of the failure, hence
  landed on R_src *after* `E_b`'s cut and is in the copied chain.
- *The reverse direction (why the revert step exists):* R_dst can also hold
  data R_src lacks — an in-flight write that completed on R_dst's leg but never
  on R_src's (and was never acked), or zombie loopback writes (§3). R_src's
  chain never touches those clusters, so without step 0 they would survive
  catch-up as permanent divergence. Reverting R_dst's head to its own `E_b`
  discards them; acked writes that the revert also discards are all present
  in R_src's chain and get re-copied — by the back-off argument for writes
  near the failure, and by the `E_b`-inclusive copy for writes merely racing
  the base-epoch cut.
- *The admission boundary:* a fully chased standby still trails by whatever
  landed after the latest copied epoch. The transition to `in_sync` —
  and with it eligibility as a raid member and read source — happens only
  after the §6 reassembly sequence's fenced final delta, never on copy
  completion alone.

With the three pieces composed — and `in_sync` withheld until the fenced
final delta — no quiesce is needed at epoch-cut time. (A Tier-2 raid quiesce
RPC could shrink `T_back` to zero by making cuts atomic, but it is not
required for correctness.) Tier 1 additionally stands on `bdev_raid_create`
admitting equalized bases as in-sync with no rebuild (§6, demonstrated live
during the phase-0 recovery validation); the phase-3/4 cluster suite must pin
that invariant with a regression test — an SPDK version bump could silently
change it.

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
     the lvol was created thick. Flint's default is thin as of the change
     accompanying this revision (`thinProvision` flipped to `true` in
     `main.rs:868-870`, `minimal_disk_service.rs:1666`, the chart
     StorageClass, and the node-agent HTTP API's `CreateLvolRequest` serde
     default); **volumes created before then are thick**, and for those
     the first epoch snapshot quietly revokes the full-allocation guarantee:
     subsequent writes COW into *newly allocated* clusters and can hit
     lvolstore ENOSPC mid-write if capacity was budgeted 1×.
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
  - **Discards do NOT reclaim clusters on epoch-managed heads.** The
    cluster-release-on-unmap path is gated on the blob being backed by the
    zeroes device — i.e., thin with *no parent* (`blobstore.c:3269-3271`,
    `zeroes.c:165-169`). Under this design the head is always a clone of the
    latest epoch, so `fstrim`/`-o discard` only zero ranges inside
    still-allocated clusters. Reclamation comes from **retention rolling**
    (the snapshot-delete merge), and the head reverts to zeroes-backing only
    after its last snapshot is gone (`blobstore.c:8327-8330`).
  - Capacity rule of thumb for thick volumes that must remain snapshot-safe:
    budget `2×` (or convert to thin at the next opportunity). The controller's
    capacity cache should count retained-epoch overhead (`Σ epoch deltas` per
    node) when placing new replicas, or epoch adoption silently overcommits
    existing pools. §10-4 keeps the follow-up to quantify blobstore *metadata*
    pressure (md pages per blob × (K+1) × replicas per node — the lvolstore's
    md region is fixed-size and can ENOSPC before data does).

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
  ≤ `T_snap` + one delta-copy time. It is *not* in the raid, is never a read
  source, and must never be exported read-write to anything but the catch-up
  destination claim. **The trailing bound is conditional on convergence**: the
  chase converges iff a delta copies in < `T_snap`. Under sustained write rates
  above copy throughput, lag grows without bound — export `lag = epochs-behind
  × T_snap` as a metric with an alert, reflect the true bound in PV status
  rather than the nominal one, and respond by raising `T_snap` adaptively
  (fewer, larger deltas amortize better) before the base epoch ages toward the
  retention pin. Multiple standbys chasing one source multiply read and network
  load on that node (§10-5).
- **Retention expiry.** If a `stale` replica's last common epoch ages out of
  the K retained epochs before catch-up completes, it transitions to the
  thin-aware full build below (E = "empty") — same machinery, larger copy.
- **Rejoin at the next assembly.** NodeStage already re-creates the raid
  (`create_raid_from_replicas`). Insert, in order:
  1. Hygiene + fencing pass (§3) on this node **and any previous consumer
     node**: delete phantom/orphaned raids, detach stale per-replica
     controllers, flip replica subsystems' allowed hosts to this node.
  2. If a standby exists: run the **final delta now** — after step 1's fencing
     is *positively confirmed* (every replica subsystem acked the allowed-host
     state and `nvmf_subsystem_get_controllers` shows the old consumer gone)
     there are no writers, so no quiesce is needed; a final snapshot cut here
     equals the head. This copy runs inside NodeStageVolume, so it must respect
     kubelet's CSI timeout (~2 min with retries): include the standby only if
     the remaining delta is below a copy-time threshold (it should be — chasing
     bounds it to ≤ one epoch), make the copy idempotent and resumable keyed
     off PV state so kubelet retries continue rather than restart it, and on
     threshold overrun stage degraded without the standby and let chasing
     finish in the background.
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
- **Assembly after unclean teardown (survivor divergence).** If the previous
  consumer crashed with writes in flight, the *surviving* replicas can disagree
  with each other (raid1 fans out with no journal; each leg completes
  independently), and a fresh create admits all of them as in-sync with raid1
  free to serve either copy of the same LBA — kernel md resyncs after unclean
  shutdown for exactly this reason. Detectable: the previous consumer never ran
  NodeUnstage/hygiene. Handling: pick one survivor as authoritative and run the
  same epoch reconcile on the others (revert to own `E_b` + copy the
  authoritative chain) before assembly — same machinery, small deltas. Until
  implemented, this is an accepted, documented divergence window (it exists in
  Flint today); filesystem journal replay reading mixed old/new metadata is the
  workload it endangers.

### Cutover opportunities (when does "next assembly" happen?)

- **Naturally**: pod reschedule/restart, node drain, spot churn — largely the
  same events that cause replica outages in the first place. No action needed.
- **RWX volumes — on demand, with caveats**: bounce the `flint-nfs-server`
  pod; its synthetic RWO PVC is re-staged on restart (raid re-assembled with
  the standby included) while clients retry via the stable per-volume Service.
  But "ride through" must be scoped honestly against the shipped server:
  `flint-nfs-server` holds all NFSv4 state **in memory**
  (`StateManager::new_in_memory`, `server_v4.rs:66` — the SQLite state backend
  exists in-tree but is not wired into this binary), so a bounce loses
  clientids/sessions/opens/locks, and recovery rests on a 90 s allow-all grace
  window (`lease.rs:22`). Stateless I/O and uncommitted writes ride through
  (the per-boot write verifier forces clients to resend); clients that miss the
  remaining grace — which the unstage + reassembly + final delta all eat into —
  get `NFS4ERR_NO_GRACE` → application errors. Required to make the claim
  solid: wire the SQLite backend with its DB on the exported volume (state then
  roams with the PVC) and run the final delta *before* deleting the old pod so
  the outage is just the pod restart. Also note the bounce is racy: if the
  replacement pod lands on the same node before kubelet unstages, the staged
  volume is reused — no NodeStage, no reassembly, clients ate a restart for
  nothing. The orchestrator must verify `sync_state` actually flipped and
  retry with a scheduling hint (cordon/anti-affinity) if not.
  *Validated 2026-06-12 (see `phase6-residual-2026-06-12.md`, RWX round):*
  the live blocker wasn't grace at all — it was upstream of it. Four
  composable identity-aliasing bugs (zombie raid at unstage, RWX data-path
  false positive, dual control streams from the synthetic backing PV, and
  boot-time file-handle instance ids → permanent `EBADHANDLE` after every
  bounce) made the RWX bounce a repeating outage. All four fixed; handle
  ids are now pinned per volume (`PNFS_INSTANCE_ID` =
  `stable_nfs_instance_id`). The in-memory-state/grace caveat above still
  stands for long-lived opens/locks and remains the gap to close (SQLite
  state backend on the exported volume) before claiming lock-holding
  workloads ride through.
- **RWO volumes — by policy**: an opt-in knob (per StorageClass or PV
  annotation) to bounce the workload pod during a maintenance window, for
  workloads that tolerate restarts. The same same-node race applies — verify
  the outcome, don't assume it. Otherwise wait for a natural event.

### Trade-off, stated honestly

Until cutover the array remains degraded: the standby bounds *data-loss
exposure* (if all in-sync replicas were subsequently lost, the standby is
behind by at most `T_snap` + the last delta — **provided the chase is
converging**; see the lag metric above) but it is **not synchronous
redundancy**.
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
bitmap we deliberately **do not want** (in-memory — only the enable flag is
persisted in the superblock, `bdev_raid_sb.c:83` on the fork branch, not the
bitmap itself; so it only helps faults within one raid lifetime and cannot
survive a roaming raid — §2's governing principle).

So Tier 2 is: an optional **`skip_rebuild` flag on `bdev_raid_add_base_bdev`**,
carried as one more `.patch` in `Dockerfile.spdk`.

### Verified patch shape (traced on v26.05; port to shipped v26.01 analogous)

- Plumbing: hand-edit the decoder table and call site in `bdev_raid_rpc.c`
  (`:258-261` — decoder structs are hand-written, not generated; update
  `schema/schema.json` too so the `genrpc.py` lint/doc pass stays green),
  prototype in `bdev_raid.h`, flag stored **on `raid_base_bdev_info`** — it
  must survive the silent divert into `raid_bdev_examine_sb` when the added
  bdev carries a matching old sb (`bdev_raid.c:3429`), which it will after a
  shallow-copy catch-up (§5).
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
  raid-quiesce RPC is independently useful — e.g. it would shrink the §5
  back-off window to zero by making epoch cuts atomic, and it gives
  `lvol-flush` a clean pre-snapshot sync point (§10-6).
- **The quiesce must be leased.** The window spans several control-plane RPCs
  across three nodes; if the orchestrator dies mid-window, an unleased quiesce
  leaves guest IO hung until the initiator above the raid escalates to resets.
  The RPC takes a timeout and auto-unquiesces unless renewed — orchestrator
  death then degrades to "rejoin attempt failed", not an availability incident.
  The orchestrator also needs an explicit unwind per step: add fails →
  unquiesce immediately, delete the esnap clone, and either promote `E_f` to a
  real common epoch (it qualifies if all survivors cut it) or delete it on all
  survivors so it cannot pollute the epoch lineage.
- Estimated ~200–250 lines of C **including the leased quiesce RPC pair**,
  ~250–300 total with schema/CLI. Crash safety is fail-*safe*: a crash between
  channel install and sb write leaves the slot FAILED on disk → next assembly
  treats the replica as stale (a redundant catch-up, never corruption).

### Correct hot-rejoin sequence (one short quiesce window, metadata ops only)

1. Bulk catch-up R_dst to the latest epoch (§5) — online, hours if need be.
2. Quiesce the raid → take final snapshot `E_f` on survivors → expose R_src's
   `E_f` over NVMe-oF → create R_dst's new head as an **esnap clone** of it
   (`bdev_lvol_clone_bdev`) → `bdev_raid_add_base_bdev … skip_rebuild=true` →
   unquiesce. All steps inside the window are metadata operations.
3. From unquiesce: new writes fan out to R_dst's head; reads of not-yet-local
   clusters forward through the esnap to `E_f` — **correct from the first I/O**.
4. Backfill the remaining epoch deltas via `shallow_copy`, then
   `bdev_lvol_set_parent` to localize the chain and drop the esnap dependency.
   **The backfill window is not "at leisure" — it is a dependency window.**
   Until `set_parent` completes, R_dst's reads of non-local clusters traverse
   NVMe-oF to R_src's node (double-hop latency on guest reads raid1 routes to
   R_dst, plus read-modify-write amplification on COW), and R_src's node is a
   **single point of failure for that data**: if it dies, the esnap degrades
   and the only base holding those clusters is gone. Run the backfill at high
   priority, do not report the volume fully redundant until localization
   completes, and on R_src death mid-backfill transition R_dst back to `stale`
   (its consistent epoch chain survives) rather than letting esnap read errors
   surface through the raid.

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

- **lvol/blobstore delta primitives — UPSTREAM** (shallow_copy/set_parent since
  v24.05, clone_bdev/esnap since v23.05; all in shipped v26.01). Verified
  present and contracts as in §5.
- **raid "add as in-sync" (+ a leased quiesce RPC) — NOT upstream anywhere**
  (verified 2026-06-10): fork-only in `longhorn/spdk`; Tier 2 carries the
  minimal ~250-line equivalent as a local patch in the existing
  `Dockerfile.spdk` pipeline.

Rejected alternatives:

- **In-raid in-memory write-intent bitmap:** incorrect for a roaming raid (§2).
- **Persisted md-style on-base WIB:** correct but a large crash-consistent
  format change; duplicates dirty info the blobstore already persists.
- **Superblock surgery / examine tricks:** *proven impossible* — see §7's
  evidence; every path ends in rebuild or rejection.
- **Porting Longhorn's fork branch wholesale (rev 1's recommendation):**
  superseded — it imports the delta bitmap and fastSync surface we don't want,
  plus a fork-tracking obligation, for a primitive that reduces to ~250 lines.
- **Custom replication vbdev (drop bdev_raid):** most work; reinvents raid1's
  write fan-out. Reserve for if we outgrow raid1.

## 9. Phasing

0. **Fix the §3 examine/orphan hazards + the restage/reboot bug cluster.**
   ~~Repro~~ **done 2026-06-10** (see `phase0-hazard-repro-2026-06-10.md`):
   replica-node reboot → phantoms claim replica lvols, re-export dead;
   restage → bricked on every node; manual attach → phantom + `-EEXIST`
   confirmed; both recovery runbooks validated on stock v26.01. Fixes, all
   confirmed necessary by the repro:
   - **bump the shipped SPDK image v26.01 → v26.05.x** (for `bdev_raid_delete
     clear_sb` — the sb-clearing the hygiene pass needs; the validated v26.01
     dd-over-temp-export fallback is operational, not programmatic);
   - the `wait_for_examine` → delete+clear → create discipline in the node
     agent reconcile and pre-assembly path;
   - **full teardown in NodeUnstage**: `bdev_raid_delete` + per-replica
     controller detach + kernel loopback disconnect (the leftover zombie raid
     and its claims were the first blocking layer in the repro);
   - **idempotent, convergent staging**: treat already-present namespace /
     listener / controller / raid as success-or-reuse in both export paths and
     NodeStage (`add_ns` and `add_listener` duplicates returned `-32602` and
     made retry loops permanently non-convergent; a partial stage that fails
     after `bdev_raid_create` re-writes sbs and re-arms the hazard);
   - **fix replica-PV labeling**: CreateVolume must apply
     `flint.csi.storage.io/replica-{node_uid}=true` (or reconcile must select
     by volumeAttributes) — today `reconcile_replica_targets` matches nothing;
   - **health truthfulness**: update PV `replicas[].health` on leg failure,
     emit events, and lengthen/handle the 3 s kernel-device wait (stale kernel
     controllers need an async rescan); grant the node SA the PV-update RBAC
     it already assumes;
   - subsystems created `allow_any_host: false` with host lists from
     PV annotations in **both** export paths, plus post-fence verification (§3).
   *Independent bug fix; prerequisite for everything below; ships on its own.*

   **Implementation status (2026-06-10):** all of the above implemented on
   `main` — convergent export module (`nvmeof_export.rs`, check-then-act with
   unit tests for every poison state the repro produced), staging as a
   reconcile loop (`ensure_raid1_bdev`: reuse-if-online / delete-phantom /
   retry-on-EEXIST + `wait_for_examine`), full NodeUnstage teardown
   (loopback subsystem, raid delete with `clear_sb` when supported, replica
   controller detach, kernel disconnect fallback), replica-side phantom
   hygiene in reconcile, PV-label fallback scan + opportunistic labeling
   (reconcile now also runs every 60s, not just at startup), raid-aware
   `NodeGetVolumeStats` + PV `replica-health` annotation + `VolumeDegraded`
   events, 20s device wait with explicit ns-rescan, node-SA RBAC fix, host
   fencing via stable per-node host NQNs (`nqn.2024-11.com.flint:node:{node}`,
   consumer derived from the VolumeAttachment, default-closed, post-fence
   controller-drain verification, `FLINT_NVMF_FENCING=disabled` escape
   hatch), and the spdk-tgt image bumped to v26.05 (all carried patches
   verified; the old inline ctrlr.c seds became `nvmf-hostlog.patch` — they
   would mis-apply on v26.05). Remaining: cluster acceptance test = re-run
   the repro scenarios and observe convergence instead of bricking.

   **Functional validation (2026-06-11, v1.1.1):** the standard kuttl suite
   (`tests/system/kuttl-testsuite.yaml`, 8 tests: cross-node RWO migration,
   pvc-clone, volume-expansion, snapshot-restore, ROX multi-pod, RWX/NFS,
   ephemeral-inline, multi-replica raid assembly) is green on a live 4-node
   i4i.large cluster running the 1.1.1 images. The suite caught one phase-0
   regression before release: the controller-path export passed its own
   `node_id` (the controller pod) as the fencing consumer, so every
   cross-node single-replica NodeStage was rejected at
   `bdev_nvme_attach_controller` with EIO — five of eight tests failed on
   1.1.0. Fixed by threading the consumer node (`req.node_id`) through
   `setup_nvmeof_target_on_node` (commit `cdbd213`); the node-side
   multi-replica path was already correct. Still open: re-run the §3 repro
   scenarios (replica-node reboot → phantom raid; restage → EEXIST) on the
   fixed build and observe convergence, plus the isolated clean-shutdown
   suite (`kuttl-testsuite-clean-shutdown.yaml`).
1. **Persistent replica sync-state** in PV annotations (`sync_state` ∈
   `in_sync`/`stale`/`standby`, `last_epoch`, current epoch name). *Control
   plane.*

   **Implementation status (2026-06-11):** implemented on `main` as
   `replica_sync.rs`. The record lives in one PV annotation
   (`flint.csi.storage.io/replica-sync-state`): per-replica `{node_name,
   node_uid, lvol_uuid, sync_state, last_epoch, since, reason}` plus
   `current_epoch` (null until phase 2 cuts epochs). Immutable identity
   stays in volumeAttributes; the annotation is the mutable companion.
   Writers: the controller seeds all-`in_sync` in the same patch as the
   replica node labels (lazy rebuild from volumeAttributes covers the
   PV-not-yet-created race); the consumer node's health monitor marks
   replicas `stale` when an *online* raid lacks a configured base for them
   (set difference against healthy bases — SPDK nulls a failed slot's
   name+uuid, so the failed slot itself is unidentifiable; bases match by
   lvol uuid, which the NVMe-oF target propagates, with the deterministic
   remote bdev name as fallback); NodeStage marks replicas excluded from a
   degraded assembly `stale` and emits `ReplicaStale` /
   `StaleReplicaAdmitted` Warning events. Updates are read-modify-write
   merge patches guarded by `resourceVersion` with conflict retry. By
   design nothing transitions a replica back to `in_sync` yet (that is the
   phase 3/4 catch-up + admission path) and nothing consumes the state for
   membership — phase 1 records truthfully, changes no behavior: a stale
   replica re-admitted at reassembly stays `stale` in the record and the
   admission (today's documented divergence hazard) becomes observable.
   RBAC was already sufficient (node SA has PV patch/update since phase 0).
   Unit-tested (wire-format stability, transitions, membership reconcile,
   failed-slot/never-attached/non-online raid matching).
2. **Snapshot scheduler** (common epochs + retention). *Control plane.* Decide
   hosting: revive the controller-operator binary (currently dead per §1; its
   raid-status/replace RPCs also route to `localhost:5260` instead of the
   per-node agent and need fixing) or fold into existing loops (node agent's 30s
   interval; controller's capacity-cache refresh loop).

   **Implementation status (2026-06-11):** implemented on `main` as
   `epoch_scheduler.rs`; unit-tested, not yet cluster-validated (e2e is
   deferred until phases 3/4 complete, then exercised together). Hosting
   decided: a background loop in the **controller process** (single
   coordinator that already reaches every node agent; the operator binary
   stays dead). **Default-disabled** via `FLINT_EPOCH_SCHEDULER=enabled`
   (+`FLINT_EPOCH_INTERVAL_SECS`, default 300; `FLINT_EPOCH_RETAIN`,
   default 6) until the phase-3/4 consumers exist — epochs cost snapshot
   space (up to 2× on pre-1.1 thick volumes, §5) and heal nothing alone.
   Mechanics: 60s tick over this driver's multi-replica PVs; cuts
   `epoch-<vol>-<seq>` via each replica node's agent on **attached**
   volumes only (detached = no writes to capture), on **in-sync** replicas
   only (per the phase-1 record — degraded volumes keep cutting on
   survivors, which is exactly the delta a stale replica will need).
   All-or-abort: the epoch is recorded common (appended to the record's
   new `epochs[{name, recorded_at}]` list, `current_epoch` advanced,
   `last_epoch` stamped on cut replicas only) only when every in-sync
   replica's snapshot succeeded; failures roll back best-effort and emit
   `EpochCutFailed`. "Already exists" converges (a leftover from an
   aborted attempt is the same head cut earlier — §5's skew argument
   tolerates that; `recorded_at` is then an upper bound on the true cut
   time, which errs the phase-3 `T_back` back-off toward an older, safer
   base). Retention retires oldest-first, record-first; node-side
   snapshots are reaped by a convergent GC pass that deletes only epochs
   **below** the retained window (a record rebuilt after annotation loss
   has an empty epoch list and GCs nothing). GC observes §5's ownership
   boundary: only names parsing strictly as `epoch-<vol>-<seq>` are ever
   candidates, so user CSI `VolumeSnapshot`s are never touched, and a
   delete blocked by a user clone is left to the blobstore's
   snapshot-delete merge semantics and retried. The record gains
   `retention_pin` — phase 3 sets it; `retire_epochs` re-checks it at
   write time, refusing the pinned epoch and everything newer. Sequence
   numbers never reuse retired epochs'. Deferred: write-volume-adaptive
   `T_snap` (§10-4), per-StorageClass opt-in, lag metrics (§6) — and the
   §10-3/§10-4 measurements need the live cluster.
3. **Catch-up orchestrator**: detect returned replica → hygiene → bulk
   shallow-copy → epoch chasing (warm standby). *Control plane.*

   **Implementation status (2026-06-11):** implemented on `main` as
   `catchup.rs`; unit-tested (planning functions plus the full RPC
   choreography against a fake node transport and record store), not yet
   cluster-validated (e2e deferred until phase 4, then exercised together).
   Hosted next to the epoch scheduler in the controller process;
   **default-disabled** via `FLINT_CATCHUP=enabled`
   (+`FLINT_CATCHUP_TBACK_SECS`, default 120 — `T_back` must cover the
   NVMe-oF I/O timeout *plus* the 60s health-monitor tick, because `since`
   is stamped at detection and trails the true leg failure;
   `FLINT_CATCHUP_POLL_SECS`, default 2). Mechanics, mapped to §5:
   - **`E_b` selection:** newest recorded epoch that is verified present on
     the returning replica (listed on the node, not inferred from
     `last_epoch` — a replica healed by an earlier catch-up has gaps) and
     whose `recorded_at` is ≥ `T_back` before the replica's stale-marking.
     `recorded_at` is an upper bound on every per-replica cut time, so the
     comparison only errs toward an older, safer base; an unparseable
     failure time degrades to the oldest present epoch. No candidate →
     `ReplicaNeedsFullRebuild` event (phase 5) and the replica stays stale.
     An empty epoch list does nothing at all — a record rebuilt after
     annotation loss must never condemn a healable replica.
   - **Retention pin** set to `E_b` *before* the revert; held through
     standby and released at phase-4 admission, once no replica still
     depends on a pinned base — standby or mid-catch-up write-virgin
     head (changed 2026-06-12 per the §10-14 observation: retiring a
     standby chain's base is data-safe — a retired epoch's delta merges
     into its retained successor — but node-side epoch GC then grinds
     against the chain's clone-parents, warning every cycle until
     admission frees them anyway). The pin also **advances with the
     chase mark** (`advance_retention_pin`, same day, found in the
     phase-6 run): a standby resumes base-inclusively from `last_epoch`,
     so it needs nothing older — without the advance, a standby that
     cannot admit (e.g. an ineffective cutover bounce) holds retention
     at its original base and the epoch list grows unbounded (observed
     live: 23 epochs against K=6 in 18 minutes). The advance is bounded
     by the oldest need across ALL dependent replicas, never moves
     backward, and a mid-flight full build (`reverted_to: "empty"`)
     blocks it entirely.
   - **Revert** deletes the head and re-clones it from the replica's own
     `E_b`, keeping the lvol *name* (the stable `lvs/name` alias makes the
     revert idempotent across crashes). The new uuid is recorded as
     `active_lvol_uuid` in the sync record — volumeAttributes stay
     immutable — and `reverted_to` marks the head as a write-virgin clone:
     resume skips the re-revert only while that exact base stands, and
     **phase 4 must clear `reverted_to` when it admits the replica
     `in_sync`** (from then on the head takes raid writes and a later
     catch-up must revert again).
   - **Superblock hygiene before every export** (the linchpin): the
     reverted head reads its clone parent's raid sb at block 0, so the
     orchestrator force-examines it on the replica node, lets the phantom
     assemble, and deletes it with `clear_sb` — the bdev later attached on
     the source node then presents a zeroed block 0 and examine finds
     nothing. Without this, attach on a non-consumer source spawns a
     phantom; attach on the *consumer* source gets the §3 ONLINE-examine
     re-add — a stock blind full rebuild. If a raid does claim the attached
     destination: CONFIGURING → released (no `clear_sb` — its bases can
     include the source's own live lvols); ONLINE → loud abort, never
     fight it. An ONLINE raid on the *replica's* node likewise refuses the
     catch-up (zombie consumer; §2 fact 3).
   - **Fenced re-export:** the per-replica subsystem's host list converges
     to exactly the source node (the copy writer) — a previous consumer's
     auto-reconnecting initiator is locked out; phase-4 staging re-flips
     the fence to the new consumer.
   - **Copies are base-INCLUSIVE in every session** — bulk (§5 step 4's
     load-bearing rule) *and* chase: re-copying the base's own delta from
     the current source also closes the cut-skew window when the source
     replica changes between sessions, so source selection can stay
     stateless (any in-sync replica, preferring one off the consumer
     node). Interrupted copies re-run the whole chain; epoch snapshots are
     immutable so the re-copy converges. The destination head is
     re-snapshotted as the newest copied epoch (§5 step 5) — the standby's
     consistent resume point — except in the degenerate `E_latest = E_b`
     case, where the name already exists on the destination and the head
     is consistent without it.
   - **Scheduling:** 60s tick; each volume runs as its own task behind an
     in-flight set (a multi-hour bulk copy on one volume must not stall
     other volumes' chases); one stale replica per volume per cycle
     (§10-5's two-simultaneously-stale question stays open). "Returned" is
     detected by the replica node answering the lvol listing; an
     unreachable node is silent, real failures emit
     `ReplicaCatchupFailed`.
   - **Known phase-3 asymmetry (resolved by phase 4):** NodeStage used to
     export the *identity* uuid from volumeAttributes, so after a revert a
     stage attempt failed against the recreated head, the replica was
     (correctly) excluded from assembly, and the attempt trampled the
     catch-up export until the next chase cycle converged it back. Phase 4
     made staging sync-state-aware (export `active_lvol_uuid`, run the
     final delta, include the standby, clear `reverted_to`) and ended the
     churn — see the §9-4 status below.
4. **Tier 1 reassembly admission**: final delta at NodeStage + standby inclusion
   in `bdev_raid_create`; RWX NFS-pod bounce; RWO pod-bounce policy knob.
   *Control plane.*

   **Implementation status (2026-06-11):** implemented on `main`
   (`catchup.rs` admission + `cutover.rs` bounces + sync-state-aware
   staging in `driver.rs`/`node_agent.rs`); unit-tested, not yet
   cluster-validated — phases 1–4 are now complete, so the combined e2e
   suite is the next step. Mechanics, mapped to §6:
   - **Sync-state-aware staging** (`create_raid_from_replicas`): the sync
     record is loaded at assembly and *enforced only when the volume has
     epoch history* — without epochs the catch-up cannot heal an excluded
     replica, so legacy attach-everything (with the `StaleReplicaAdmitted`
     warning) remains the lesser hazard. In-sync replicas attach by their
     **live head uuid** (`active_lvol_uuid` after a revert; the identity
     uuid in volumeAttributes addresses nothing post-revert — this ends
     the phase-3 export-trample asymmetry). Stale replicas stay out;
     standbys go through admission. Two fallbacks keep availability:
     stale replicas are force-admitted (loudly, evented) when exclusions
     would drop the assembly below the 2-base minimum.
   - **Final delta** (`admit_standbys_at_stage`, called between the
     survivor attaches and `bdev_raid_create`): every attached survivor's
     export fence now admits exactly the staging node and the raid does
     not exist yet, so **no writer exists anywhere** — one more common
     epoch cut (reusing the scheduler's all-or-abort `execute_cut`,
     targeted at exactly the replicas that attached, addressed by live
     uuid) equals every head with zero skew; one more base-inclusive
     chase session onto the standby equalizes it; `bdev_raid_create`
     admits all listed bases as in-sync with no rebuild. The §5 machinery
     is reused verbatim — the final delta is just a chase session whose
     source is provably frozen.
   - **Ordering is load-bearing**: the final epoch is recorded *before*
     the copy (an interrupted admission leaves a normal common epoch the
     background chase consumes); `in_sync` is recorded — **clearing
     `reverted_to`**, the phase-3 obligation — *before* the consumer-side
     attach and create (the reverse order risks a raid member the chase
     still treats as a standby target; if the attach/create then fails,
     the health monitor re-marks the replica stale once an online raid
     exists without it). An ONLINE raid already on the staging node
     defers all admissions: `ensure_raid1_bdev` will reuse it, and
     add-to-ONLINE is the stock blind rebuild (§7).
   - **Budget** (kubelet's CSI timeout): `FLINT_STAGE_DELTA_BUDGET_SECS`
     (default 60) bounds the copy via a deadline threaded into the poll
     loop; `FLINT_STAGE_MAX_EPOCHS_BEHIND` (default 4) pre-rejects a
     non-converged chase. Overrun = stage degraded without the standby
     (`StandbyAdmissionDeferred` event), replica keeps chasing, next
     reassembly retries — the §6 resumability comes free from the
     idempotent chain re-copy.
   - **Health-monitor truthfulness fixes** so admission sticks:
     `replicas_missing_from_raid` now matches bases by live uuid (an
     admitted reverted replica exposes `active_lvol_uuid`) and reports
     only `in_sync` replicas (a chasing standby is *expected* to be
     missing from the raid — previously the monitor would demote it back
     to stale every tick). The node agent's reconcile skips stale/standby
     exports entirely (the catch-up orchestrator owns those fences) and
     exports the live uuid for in-sync replicas. RWX identity split
     fixed: everything keyed off a volumeHandle resolves through
     `record_pv_name` (`nfs-server-<vol>` → user PV).
   - **Cutover** (`cutover.rs`, controller loop, default-disabled via
     `FLINT_CUTOVER`): plans a bounce only when every standby is *ready*
     (lag ≤ `FLINT_CUTOVER_MAX_LAG`, default 1 — so the final delta is
     small). RWX: the bare `flint-nfs-<vol>` pod is captured → deleted →
     the synthetic PV's detach is awaited (closes the §6 same-node
     staged-volume-reuse race) → recreated from the sanitized spec with
     `nodeName` cleared. RWO: strictly opt-in via the PV annotation
     `flint.csi.storage.io/rejoin-bounce: "enabled"`; the claim's pods
     are deleted and their controller reschedules them. Every bounce is
     **verified**: standbys that flip → `CutoverSucceeded`; still standby
     after `FLINT_CUTOVER_COOLDOWN_SECS` (default 900) →
     `CutoverIneffective`, then eligible to retry. The §6 scheduling-hint
     escalation (cordon/anti-affinity) is deliberately not implemented —
     an ineffective bounce is surfaced, not silently fought.
   - **Known windows, accepted and documented:** a concurrent chase can
     overlap the final delta (both copy the same immutable epochs onto
     the same head — convergent, merely wasteful; bounded to one cycle by
     the in-flight set). The final epoch is briefly recorded while a
     failed-attach survivor is still marked in_sync (stale-marking is
     post-create by design); a catch-up sourcing that replica in the
     window fails its copy and retries clean. The §6
     survivor-divergence-after-unclean-teardown reconcile remains future
     work, as does the NFS server's SQLite state backend for solid
     bounce-through (§6 caveats).
5. **Thin-aware full build** for new/replaced replicas. *Control plane.*

   **Design note — user `VolumeSnapshot` preservation (2026-06-11):** the
   implemented machinery (phases 2–4) preserves user snapshots by
   construction: the only bulk-delete path is epoch GC, gated by the strict
   `epoch-<vol>-<seq>` name parser (a user snapshot can never become a
   candidate — pinned by test), and the catch-up revert deletes only the
   writable *head*; snapshots are independent read-only blobs the
   blobstore keeps (SPDK refuses to delete any blob with clones besides),
   so a user snapshot cut between `E_b` and the failure simply becomes a
   branch point — still restorable. The final delta only creates
   snapshots. The full build must keep that property, but can only do so
   in a weaker sense: reverting to `E = "empty"` necessarily orphans the
   replica's local user-snapshot chain — the blobs stay intact and
   restorable (restore clones from the snapshot, not the head) but are no
   longer the head's ancestry, so their clusters remain allocated as
   standalone space; on a physically *replaced* replica they are gone with
   the disk. Two obligations for the implementation: (a) the full build
   must never reap the old chain itself — user snapshots are not ours to
   delete, and the orphaned *epochs* below the retained window are already
   the GC's job; (b) the §10-11 question (multi-replica `VolumeSnapshot`
   support, **now designed in §11**) is a soft prerequisite — today
   `CreateSnapshot` resolves its source via the singular
   `node-name`/`lvol-uuid` volumeAttributes, which multi-replica volumes
   do not set, so snapshotting them fails outright and user snapshots are
   single-node objects. §11 also amends the shape of this phase: with user
   snapshots in the chain, the full build is specified as **lineage replay
   from empty** (copy the oldest element's allocated set, then each delta,
   aligning at user-snapshot elements), which re-creates the user-snapshot
   copies on the rebuilt replica — the old local chain still orphans, but
   snapshot coverage is preserved; with no user snapshots it degenerates
   to the single flattened copy sketched above.

   **Implementation status (2026-06-11):** the *same-identity* full build
   is implemented on `main` in `catchup.rs` (unit-tested, not yet
   cluster-validated):
   - **Automatic transition** per §6 retention expiry: when
     `select_base_epoch` finds no usable shared epoch on a returned stale
     replica, the orchestrator runs the full build instead of merely
     classifying (the classification-plus-`ReplicaNeedsFullRebuild` path
     remains behind `FLINT_CATCHUP_FULL_BUILD=disabled`).
   - **Revert to `E = "empty"`**: delete the head (tolerating absence — a
     wiped lvolstore has no head at all) and create a fresh empty thin
     lvol under the same name, sized to the source head. Only the head is
     deleted: the old chain's snapshots stay as orphaned, restorable blobs
     (obligation (a) above). The resume marker is `reverted_to: "empty"`
     (can never collide with an epoch name); the standard write-virgin
     resume argument applies and phase 4 clears it on admission unchanged.
   - **Lineage replay from the root** (`lineage_chain` with no base): the
     root element's blob holds every cluster written before its cut, so
     replaying root → target reproduces the source's target image from
     nothing — thin by construction (holes never copied), and the
     destination re-acquires every non-tombstoned user snapshot in the
     chain per §11. The retention pin anchors at the OLDEST retained epoch
     for the build's duration; a user snapshot deleted mid-build merges
     into its descendant and the convergent re-run picks up the merged
     chain.
   - **Requires at least one recorded epoch** (the replay target): a
     volume with the scheduler disabled never full-builds — consistent
     with the empty-record rule everywhere else.
   - **NOT implemented — replica replacement (new node / new identity):**
     identity lives in immutable volumeAttributes, so replacing a replica
     needs a membership-change channel (record-level identity overrides
     consumed by ControllerPublish/NodeStage, or PV re-creation) plus
     placement logic. Recorded as open question §10-12; the implemented
     full build covers the wiped-disk / retention-expiry / recreated-lvol
     cases where the identity (node + lvol name) survives.
5b. **Multi-replica `VolumeSnapshot` support** (§11): cut on every in-sync
   replica via the epoch machinery (`execute_cut` + live uuids,
   all-or-abort); lineage-walk chain discovery in the catch-up (replaces
   the name-derived epoch chain — closes the §11 delta-split hazard);
   align-at-user-snapshot on heal so healed replicas re-acquire snapshot
   copies; presence-verified restore source selection; tombstone-driven
   deletion. *Control plane; soft prerequisite for phase 5 wherever user
   snapshots exist.*

   **Implementation status (2026-06-11):** implemented on `main`
   (`snapshot/multi_replica.rs`, the lineage walk in `catchup.rs`, CSI
   wiring in `snapshot_csi.rs` and the restore path); unit-tested, not
   yet cluster-validated. See the §11 status note for deviations from the
   design sketch and known limitations.
6. **Measure** the Tier 1 residual: time degraded with a ready standby and no
   reassembly event. *Decides Tier 2 with data.*
7. *(Conditional)* **Tier 2**: `skip_rebuild` patch + esnap-clone hot rejoin
   (§7).
8. **Tests:** offline→rejoin delta resync; roam-during-catch-up (no
   corruption); outage past epoch retention → thin-aware full build; reboot →
   phantom-raid repro; restage → EEXIST repro; power-cut during final delta;
   Tier 2: quiesce-window bound (set a target); crash between channel install
   and sb write (must trigger rebuild-or-recatchup, never serve stale reads).
   Adversarial set (build a fault-injection bdev early — delay/error on one
   raid leg; it exercises several of these): write acked during base failure
   straddling an epoch cut (validates the §5 back-off); consumer crash with
   in-flight writes → reassembly read-consistency across survivors (§6
   survivor divergence); NFS bounce with open files + locks under load past
   the grace window; orchestrator kill inside the Tier-2 quiesce window
   (lease must fire); R_src node kill during esnap backfill (R_dst must
   revert to `stale`, no esnap errors through the raid).

Phases 0–5 (including 5b) are pure Rust control plane against upstream RPCs.
The SPDK patch decision moves from "gating dependency, sequence first"
(rev 1) to "phase 7, decided by phase 6's data."

## 10. Open questions to validate

1. ~~**Repro the §3 hazards** on a live cluster.~~ **Answered 2026-06-10:
   both consequences reproduce** (`phase0-hazard-repro-2026-06-10.md`).
   Multi-replica volumes cannot heal at the transport level after a
   replica-node reboot, and a pod reschedule bricks the volume on every node.
   Phase 0's priority is confirmed above this design — it is a production
   availability bug in shipped v1.0.0.
2. **Longhorn's snapshot→grow atomicity** — read `engine.go` `ReplicaAdd`
   (~:824) and `replica.go` `RebuildingDstStart` (~:3020): is engine IO
   suspended across snapshot→grow, or does grow's internal quiesce suffice?
   Informs whether our §7 single-window sequence can be relaxed.
3. **Shallow-copy locked-op interplay**: the §5 retention-pinning rule covers
   deletion; remaining question is cadence — confirm the scheduler never needs
   to snapshot/delete a blob mid-copy (EBUSY) under normal epoch rhythm.
4. **Blobstore metadata pressure + empirical held-space validation**: md pages
   per blob × (K+1) epochs × replicas per node against the lvolstore's
   fixed-size md region (the likely binding constraint at high volume counts);
   validate §5's held-space model at target sizes; pick `T_snap`/`K` from
   measurement; consider write-volume-adaptive `T_snap`.
5. **Two replicas simultaneously stale:** catch-up ordering; which is
   authoritative; do we chase both as standbys concurrently?
6. **`lvol-flush` patch interaction with epoch snapshots:** epoch and
   final-delta snapshots are crash-consistent only — does the data path need a
   flush (and does the `lvol-flush` patch provide the right hook) immediately
   before a snapshot cut, so the snapshot captures all completed-and-acked
   writes from the guest's perspective?
7. ~~**`superblock: false` for new volumes**~~ **Answered and adopted
   2026-06-12** (rev 5): raids are created without superblocks; the §3
   hazard class is structurally eliminated and the multi-replica restore
   data-destruction bug (filesystem hidden at the raid data offset) is
   root-fixed. Split-brain: unchanged — the sb never prevented stale
   assembly (per §3); the control plane plus §10-9 fencing remain the sole
   authority, and the lost sb base-identity check is replaced by the
   driver's own record-vs-live-uuid verification at attach. Migration:
   moot pre-release (dev volumes recreated); the layout break is called
   out in §3.
8. **Orphan reaping completeness**: enumerate everything a dead consumer node
   can leave behind (raid bdev, nvme controllers, ublk/nvmf frontends, mounted
   filesystems) and make the hygiene pass cover all of it.
9. **Fencing design** (§3): default-closed allowed-host lists (from PV
   annotations, applied at subsystem creation in both export paths) vs. NVMe
   persistent reservations; post-fence verification via
   `nvmf_subsystem_get_controllers` (SPDK host removal is async); the
   co-located-replica case (cannot be fenced externally — contaminated until
   hygiene); verify a zombie's auto-reconnecting initiator (`ctrl_loss_tmo`)
   cannot win a race against a returning node's re-export.
10. **NFS state persistence** (§6): wire the existing SQLite state backend into
   `flint-nfs-server` with its DB on the exported volume; grace-period
   semantics after restore (RFC 8881 stable-storage reclaim gating vs. today's
   allow-all-in-grace); bound the bounce-cutover outage so clients reliably
   reclaim within grace.
11. ~~**Multi-replica `VolumeSnapshot` support**~~ **Designed and
   implemented 2026-06-11 — see §11** (cut on every in-sync replica via the
   epoch machinery; lineage-walk chain copy closing the delta-split hazard;
   align-at-snapshot on heal; presence-verified restore; tombstone-driven
   deletion). Phase 5b, unit-tested; **cluster-validated 2026-06-12**
   (`e2e-campaign-2026-06-12.md`): cut-on-both with identical clamped
   names, exact-to-the-write restore, align-at-snapshot on a chasing
   standby, full-build replay preserving the user snapshot, and
   tombstone-driven deletion including the clone-pinned-pending case
   (copy held while a restore clone existed, reaped and tombstone cleared
   once the clone was deleted). Still open: the §10-3 locked-op cadence
   including user cuts under load; lineage-replay full-build data movement
   vs. a flattened copy under rewrite-heavy workloads.
12. **Replica replacement (new node / new identity)** — the part of §9-5
   deliberately not implemented. Replica identity (`node_name`, `node_uid`,
   `lvol_uuid`, lvs) lives in immutable PV volumeAttributes; every consumer
   (ControllerPublish → publish_context → NodeStage, the reconcile label
   scan, the health monitor) reads it from there. Replacing a dead node's
   replica therefore needs: (a) a mutable membership-override channel —
   most plausibly record-level identity entries in the sync annotation,
   consumed everywhere volumeAttributes are read today (the
   `active_lvol_uuid` pattern generalized to whole-replica identity), or
   PV object re-creation with updated attributes; (b) placement (reuse
   CreateVolume's node selection) + lvol provisioning for the new replica;
   (c) `reconcile_membership` semantics for the swapped identity (today a
   new identity enters `in_sync`, which is wrong for a replacement — it
   must enter stale/empty and full-build); (d) the §6 question of when
   replacement is triggered (operator action vs. node-gone timeout). The
   implemented full build (§9-5) then does the data movement unchanged.
13. **Re-replication of restored volumes** (found 2026-06-12): a volume
   restored from a multi-replica snapshot is a single bare clone on one
   node — the SC's `numReplicas` is silently ignored, because a raid1
   cannot assemble over one real base plus empty siblings (reads from a
   blank leg corrupt instantly) and CSI `CreateVolume` cannot block on
   seeding copies. The machinery for the right answer already exists:
   create N replica lvols, then replay the snapshot's lineage into each
   via the §9-5 full-build path, asynchronously after create —
   record-level states (stale/empty → standby) gate admission exactly as
   for heals. Needs: async orchestration ownership (catch-up loop is the
   natural home), `numReplicas` honored in the restore CreateVolume, and
   the §10-12 identity channel if the clone node ≠ desired placement.
14. **Deletion-path lvol reaping** (observed live 2026-06-12): DeleteVolume
   of a volume with a stale replica left both replica heads, six epoch
   snapshots, a user-snapshot copy, and the NVMe-oF export subsystems
   behind (the export's write-open blocks head deletion; chained
   snapshot-of-clone elements then unwind only leaf-first); DeleteSnapshot
   fan-out converges by tombstone while the PV exists, but a copy pinned
   by a restore clone whose PV is deleted later has no reconciler left to
   reap it. Epoch GC also retries forever (once per minute, warning each
   time) on epochs pinned as a standby chain's base — the retention pin
   should arguably be held until admission instead of until copy
   completion. Wants a node-agent orphan sweep (extends §10-8's hygiene
   pass to lvols and exports keyed by absent PVs) rather than more
   ordering cleverness in the deletion paths.

   **Implemented 2026-06-12** (`orphan_sweep.rs` + the node agent's 60s
   monitor tick; default-on, `FLINT_ORPHAN_SWEEP=disabled` escape hatch,
   `FLINT_ORPHAN_SWEEP_STRIKES` default 3). Safety model: strict
   flint-shape parsers (the epoch-GC rule generalized — `vol_*`,
   `epoch-<vol>-<seq>`, `snap_<vol>_<u64>`, `temp_pvc_clone_*`, `eph_*`,
   `:volume:` NQNs; anything else is invisible); PV absence proved only
   by a successful full list; ordered candidacy so inline-ephemeral
   volumes (PV-less by design) are condemned only via verified frontend
   absence (their own live export or ublk attachment protects them, and
   an unverifiable ublk listing in ublk mode skips them); a subsystem
   namespace referencing any present non-condemned bdev (e.g. an
   assembled raid) keeps the subsystem alive — raid teardown stays
   NodeUnstage's; three consecutive condemned cycles before deletion
   (rides out the CreateVolume→PV-object window). Reap order is
   subsystems first, then lvols in retry passes (leaf-first emerges);
   clone-pinned copies defer quietly at debug level — the no-reconciler
   case is thereby closed: the pin's eventual release leaves the copy
   condemned and the next cycle reaps it. Deliberately out of scope: a
   `temp_pvc_clone_*` under a live PV (deletion-path bug, not an
   orphan), an ephemeral leak with its frontend intact (indistinguishable
   from in-use). The pin-until-admission change landed separately
   (2026-06-12): `mark_in_sync` releases the retention pin only when no
   replica still depends on a pinned base (standby or mid-catch-up
   write-virgin head; a merely-stale replica holds no claim — its next
   catch-up pins fresh), and `record_standby` no longer clears it, so
   retention never retires a chain the GC would grind against. **Cluster-validated 2026-06-12** on a fresh 4-node
   cluster: planted orphan head/epoch/snapshot lvols and an orphan
   subsystem next to a live volume — all four reaped on exactly the
   third cycle; the live volume's lvol, subsystem and data untouched
   throughout; normal PVC deletion still converges to zero without the
   sweep's involvement.

## 11. Multi-replica user snapshots (`VolumeSnapshot`) — design

*Added 2026-06-11; answers §10-11. Implementation is phase 5b (§9).*

**Implementation status (2026-06-11):** implemented on `main` as designed;
unit-tested (lineage walks over forests with interleaved user snapshots,
cut targeting/rollback by live uuid, align-at-user-snapshot replay,
tombstone convergence incl. unreachable-replica retention, restore source
selection by verified presence). **Cluster-validated 2026-06-12**
(`e2e-campaign-2026-06-12.md`): cut/restore/tombstone all live-verified;
restores are exact to the snapshot cut; restored volumes are single-replica
clones (re-replication → §10-13). Deviations from the sketch below, all
deliberate:
- **The name suffix is not a timestamp**: multi-replica snapshot names are
  `snap_<vol>_<fnv64(csi_request_name)>` — external-snapshotter retries
  carry the same CSI `name`, so the lvol name is stable and the cut
  converges via EEXIST instead of piling up snapshots under fresh
  timestamps (the single-replica path's known idempotency wart, left
  unchanged there). The suffix parses as the same `u64` the strict-name
  discipline expects. **Live fix (2026-06-12):** the full 20-digit decimal
  rendering overflowed SPDK's 63-usable-char lvol-name limit on `pvc-<uuid>`
  ids (65 chars → `-32602` on every cut); the suffix is now reduced modulo
  the digit budget left by `snap_<vol>_`, preserving determinism and the
  strict-`u64` parse.
- **`CreateSnapshot` reports `size_bytes: 0`** (unspecified) for
  multi-replica snapshots — copies are thin per-replica deltas with no
  single meaningful size.
- **`DeleteSnapshot` with the volume's PV already gone** falls back to a
  best-effort all-node sweep by name (the legacy uuid scan stops at the
  first hit, which would orphan the other copies); with the PV present, a
  failed tombstone write fails the delete retryably — success without the
  tombstone would leak copies.
- **Known limitation — `ListSnapshots`**: the node-side listing still
  reports each copy separately under its per-node uuid, so a multi-replica
  snapshot appears once per replica and not under its name-shaped CSI id.
  Create/delete/restore never use the listing; revisit only if the e2e
  shows external-snapshotter caring.
- **Pre-existing, unchanged**: `DeleteVolume` does not reap the volume's
  snapshot copies (true for single-replica today as well); the post-PV
  deletion sweep above covers later `DeleteSnapshot` calls.

### Current state (verified in code)

`CreateSnapshot` resolves its source through `get_volume_info`
(`snapshot_csi.rs:118`), which requires the singular
`flint.csi.storage.io/node-name`/`lvol-uuid` volumeAttributes — set only for
single-replica volumes (`main.rs:1046-1073`; multi-replica volumes set the
`replicas` JSON instead). Snapshotting a multi-replica volume therefore
fails "metadata not found" today. User snapshots are single-node objects
named `snap_<vol>_<timestamp>` (`snapshot_csi.rs:131` — strictly parseable,
the same shape as epochs), cut on one node and restored by cloning on that
node; restored volumes are always single-replica (`main.rs:551`).

### Semantics: one snapshot, N equivalent copies

A multi-replica `VolumeSnapshot` is cut on **every in-sync replica** (per
the phase-1 record), under the **same name** on each replica's lvolstore —
the epoch pattern exactly, with the same machinery: `execute_cut`
(all-or-abort, EEXIST converges a retry, partial failure rolls back),
targets addressed by **live head uuid** (`EpochTarget.snapshot_source`; a
reverted-then-admitted replica's identity uuid addresses nothing). The CSI
call succeeds — and `ready_to_use` is true — once every in-sync replica
holds the cut. Stale and standby replicas neither block nor participate;
they acquire their copy through healing (below).

**Per-copy skew is the accepted semantic.** Cuts are not simultaneous.
raid1 fans every acked write to all legs, so each replica's cut is a valid
crash-consistent image of the volume; two copies of the same snapshot can
differ by writes acked inside the skew window. A restore sources exactly
one copy, so every restore is crash-consistent — but two restores from the
same snapshot may differ within that window. This is the semantic class
§5's epoch-skew argument already accepts, and unlike epochs, user
snapshots play no role in the delta-resync correctness proof: the skew is
a documented property, not a correctness input. (Crash-consistency only,
as today — no filesystem freeze; §10-6's `lvol-flush` question applies to
user cuts identically.)

### The lineage problem — the one real change to the §5 machinery

A user snapshot interleaved into a replica's chain **splits the epoch
delta**: with `epoch-4 → snap_X → epoch-5 → head`, epoch-5's blob holds
only the writes since `snap_X` — the writes between epoch-4 and `snap_X`
live in `snap_X`'s blob (shallow copy transfers only clusters allocated in
the source blob itself, `blobstore.c:7445-7451`). The phase-3 chain copy
derives its chain from the record's *epoch names* (`chain_from`) and would
copy epoch-4 then epoch-5, silently losing `snap_X`'s slice — data loss on
the healed replica. **The copy chain must therefore be the source's actual
blob lineage**, discovered live at copy time: walk from the source head
through `bdev_get_bdevs → driver_specific.lvol.base_snapshot`
(`vbdev_lvol.c:768-778`) back to `E_b`, reverse, and copy every element's
delta in order — still `E_b`-inclusive (§5 step 4), still stopping at the
session's target epoch.

- The lineage walk is strictly more robust than name-derived chains even
  with no user snapshots involved (epoch retirement's merge into the
  descendant is reflected by construction), so phase 5b replaces
  `chain_from` unconditionally rather than special-casing.
- **Destination alignment generalizes:** the destination head is
  snapshotted (EEXIST-tolerant, exactly today's `align_head`) at every
  *user-snapshot* element it replays, plus the target epoch. A healed
  replica thereby re-acquires a copy of every user snapshot in the chain —
  a **bit-identical** one, since it replays the source's exact images
  (stronger than the cut-time skew equivalence). Intermediate *epoch*
  elements keep today's behavior — no realign; `select_base_epoch`
  already tolerates epoch gaps via verified presence.
- A destination's own pre-failure copy of `snap_X` survives the revert
  (snapshots are read-only blobs; the revert deletes only the head) and
  blocks the realign with EEXIST — correct: it is already a valid
  equivalent copy of that snapshot.
- A user snapshot newer than the newest recorded epoch is not copied by a
  chase (the chase's consistency target is that epoch); the first session
  whose target epoch is newer picks it up — including the phase-4 final
  delta, whose fresh cut is newer than everything by construction.
- **Eventual completeness invariant:** a user snapshot exists on every
  replica that was in-sync at cut time, plus every replica subsequently
  healed through a chain containing it. The §9-5 full build preserves the
  invariant by being specified as **lineage replay from empty** — copy the
  oldest element's full allocated set, then each delta in order, aligning
  at user-snapshot elements. Cost honesty: replay moves *at most the sum
  of the chain's deltas*, which exceeds a flattened single copy when
  clusters were rewritten across elements — that excess is the price of
  re-creating the snapshot copies; with no user snapshots in the chain it
  degenerates to the flattened copy.

### Restore: source selection by verified presence

Restore resolves the snapshot's copies by listing the candidate replicas'
lvols (the `select_base_epoch` discipline: presence is verified, never
inferred from records), prefers an in-sync replica off the consumer node
(`pick_source`), and clones there. The snapshot's recorded "home node"
degrades to a hint. Restored volumes remain single-replica (today's
semantic); a multi-replica restore is a single-replica clone plus phase-5
full builds for the remaining replicas — explicitly out of scope here.

### Deletion: tombstone-driven convergence, never absence-driven

Deletion removes the copy from every replica that holds it (absent =
success, idempotent). An unreachable replica gets a **tombstone** in the
volume sync record (`deleted_snapshots: [name, …]`), executed convergently
— by the catch-up at heal time (it is already touching the replica) and by
a slow snapshot-service reconcile — and cleared once every current replica
confirms absence. Ownership discipline mirrors the epoch GC exactly: only
names parsing strictly as `snap_<vol>_<timestamp>` are ever candidates,
and reaping is driven by a positively recorded delete, **never** by a
name's absence from a snapshot listing (an empty or unreadable list must
not delete data — the epoch GC's empty-record rule, same reasoning). A
copy pinned by a restore clone on its node refuses deletion (`-EPERM`,
blob has clones) and is left to the blobstore's snapshot-delete merge plus
retry — the epoch GC behavior; the other replicas' copies delete
independently, so the pin is local to the node serving the clone.

### Costs and limits, stated honestly

- **Space:** one delta-sized copy per in-sync replica — the §5 space model
  × N, the price of surviving any single disk loss. Counts toward the
  §10-4 metadata-page pressure measurement.
- **Locked-op serialization (§10-3):** user cuts, epoch cuts, and shallow
  copies of the same head must serialize per blob (EBUSY); the per-volume
  in-flight set plus EBUSY-retry covers it, but the §10-3 cadence
  validation must now include user-snapshot traffic.
- **Equivalence, not identity:** copies of one snapshot differ within the
  cut-skew window (above); healed replicas' replayed copies are
  bit-identical to their source's. Both are valid crash-consistent images.

### Implementation sketch (phase 5b)

1. `CreateSnapshot` multi-replica path: resolve replicas + sync record,
   cut via `execute_cut` on in-sync replicas' live uuids, roll back on
   partial failure. No copy registry — presence is always verified live,
   like epochs.
2. `catchup.rs`: replace `chain_from` with the lineage walk (one
   `bdev_get_bdevs` sweep of the source per session) and align at
   user-snapshot elements.
3. Restore: presence-verified source selection.
4. Deletion: per-replica fan-out, sync-record tombstones, heal-time
   reconcile.
5. Tests — unit: lineage walk over fake bdev forests (interleaved user
   snapshots, retired epochs); cut targeting and rollback;
   align-at-user-snapshot choreography; tombstone convergence including a
   clone-pinned copy. E2e (§9-8 set gains): snapshot cut while degraded →
   replica heals → restore sourced from the healed replica serves the
   snapshot's content.
