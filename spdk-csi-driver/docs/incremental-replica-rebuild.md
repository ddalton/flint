# Incremental replica rebuild (snapshot-epoch delta resync)

**Status:** design / proposal — **revision 8**
**Author:** rev 1 drafted with Claude (Opus 4.8); revs 2–5 revised with Claude (Fable 5), 2026-06-10/12; rev 6 with Claude (Fable 5.1), 2026-10-28; revs 7 and 8 with Claude (Fable 5.1), 2026-10-29/30
**Scope:** Flint CSI multi-replica volumes (SPDK `bdev_raid` RAID1 over lvols)

**What changed in rev 8** (2026-10-29, the user's decision on §5's open
question; amended through 2026-10-30): record-before-ack is taken, in the
on-disk shape, as §12. raid1 writes a **membership stamp** on every
surviving leg before it releases the completions of the writes a base
failure degraded, so the disks carry the raid's membership history and
the next assembly reads it before serving. One correction to rev 7's
sketch: a stamp on the disks alone is not enough, because a fenced-off
consumer can still stamp the leg on its own node. A stamp written on a
**majority** of the assembly is one a zombie cannot forge; a continuation
on a minority (two legs lost at once, or any loss on a two-replica
volume) also waits for the sync record, whose write is gated on the
VolumeAttachment's node, so a zombie never acknowledges on an
unconfirmed continuation (one the record confirmed before the move keeps
its leg, and the next stage waits for that leg). §6's survivor
divergence closes for a crash and for a pod move. The unstage capture
and the health monitor become belts, the monitor copies the stamp's
clock into `since` so `T_back` covers the I/O timeout alone, the tie's
"attached" conjunct becomes "listed in the newest valid stamp", and
per-epoch membership stays for the scheduler and the catch-up, since the
record still trails a majority drop by one poll. Costs: one stamp write
per base failure in the ack path of the writes then in flight, nothing
on the healthy path, one cluster per leg, a carried raid1 patch of §7's
size. The model ran ahead of the code (FlintCatchupS, flint-e4) and its
traces fixed the stage's read, §12 step 2 to 4: a read is conclusive
only from a majority of the stamp's size, or at an even size by the
record's vouch; every stamp carries the last released view; the decree
on the previous consumer's leg is written only by a read conclusive
without it; a torn leg, one whose newest stamp is an unconfirmed
continuation, is cleaned at the stage beside a clean member and never
serves alone on a vouch; and the writes in flight when a fenced writer
finds the fence are the write hole, not a stamp gap. The user dropped
F36c's transient serve-with-risk bound under the stamp (2026-10-30,
step 3): its timer is an alert, and serve-with-risk stays for node-gone
and the operator's cut. The raid1 hook has five traps (flint-91). The
verdicts, one pinned model md5 per batch, are the table at the end of
§12; nothing in §12 is built.

**What changed in rev 7** (2026-10-29, after the per-epoch trust model
`formal/FlintCatchupT.tla` reached a code-faithful revision and the unstage
capture and the fail-closed fence shipped): §5 gains a subsection on what
the trust rule rests on and what it leaves open. The rule makes a trusted
epoch **complete** (it holds every acknowledged write); it does not make it
**clean**, which rests on §3's fence and on the assembly's stale write.
Two premises carry every HOLD, and the interim source tie needs neither:
raid1 drops a base at its first failed write, and a fenced-off consumer
cannot write a leg the authoritative raid configures. The model found two
seams in the code as it stands: the assembly's stale write is load-bearing
and best effort, and F36c's last-writer defer lets hygiene return a
zombie's co-located leg to service unmarked. §5 then records the two
closures modelled for §6's survivor divergence: a write quorum does not
close it; record-before-ack (the failed base is marked stale before the
ack of the first write it missed) closes it alone, at the cost of one mark
in the ack path of a degraded write. That choice is the user's and is not
taken here; under it the unstage capture becomes a belt and the tie's
unattached-source demotion goes. Status: the capture (`ef3a6f48`) and the
fail-closed fence (`044f2010`) shipped on 2026-10-28 and the monitor gate
(`eb7c28fd`) on 2026-10-30; the fence-flip mark has not, and §12 makes it
the stage's decree.

**What changed in rev 6** (2026-10-28, after the three-replica model and a
live reproduction): §5's correctness note bounded only the *destination's*
divergence and assumed the copy source holds every acknowledged write. At
three replicas that assumption fails inside the source's own failure
window: raid1 acks a write via one surviving leg, the record keeps the
failed leg `in_sync` for the monitor's detection lag (or for good, when the
stale mark never lands), the scheduler cuts a "common" epoch on it over the
control plane, and a replica rebuilt from it is admitted missing an acked
write (`formal/FlintCatchupN.tla`; reproduced on kind 2026-10-28, an
O_DIRECT write acknowledged and then unreadable). §5 now states the source's
trust condition explicitly and moves trust from the leg to the **epoch**:
an epoch on a leg is trusted iff the leg was a configured base of the
authoritative raid at the cut and still configured at the first observation
after every pre-cut write completed; the record carries that membership per
epoch, written by the consumer node. The interim guards shipped first
(source tie, base floor, forced re-revert) are a conservative encoding of
the same rule. §7's hot-rejoin sequence takes its window source from the
raid under the quiesce rather than from the record, bases the landing pad
on its own newest trusted epoch, and reaps at its revert as §5 does. The
record gains three positive writes (an unstage capture gated on being the
consumer, the monitor gated on the consumer node, the fence flip marking
the co-located leg stale) and names its residual: a consumer crash inside a
failure window leaves survivor divergence (§6). Review and session asks:
`docs/plans/catchup-design-review-2026-10-28.md`.

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
`disk.chert.us/replica-{node_uid}=true` (node_agent.rs:1666) which
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
  than the I/O timeout. **Rev 6:** that back-off is one instance of the
  per-epoch trust rule ("Source trust" below): `E_b` is R_dst's own newest
  *trusted* epoch, and every epoch copied from R_src must be trusted on
  R_src.
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

### Source trust: the per-epoch rule (rev 6, 2026-10-28)

The argument above bounds R_dst. It assumes R_src holds every acknowledged
write, which the failure-window paragraph itself shows is false for a leg
inside its own window: a write acked via another leg alone is missing from
R_src, and an R_src epoch cut during the detection lag records that gap as
"common". At two replicas the assumption is free, since the source is the
sole survivor; at three it is not. `formal/FlintCatchupN.tla` (rev 7)
violates admission faithfulness under both zombie premises at two failures,
and the loss was reproduced live on kind on 2026-10-28.

**The ground truth is the raid's base list.** raid1 fails a base at its
first failed write (the `raid1.c` write-error path; the repro saw the leg
fail as the stuck write completed), so a configured base of the
authoritative raid has completed every acked write. The record's `in_sync`
is a lagging proxy for that: written on the monitor's tick from any node
that holds a raid, written at assembly from reachability alone, and never
written at NodeUnstage, the one place a clean pod move still has the raid's
last observation.

**The rule.** The record carries, per retained epoch, the legs that were
configured bases of the consumer's raid at the cut (the scheduler reads
`bdev_raid_get_bdevs` on the VolumeAttachment's node as part of the cut).
An epoch `E` on leg `s` is **trusted** iff `s` was configured at `E`'s cut
**and** still configured at the first observation after every write
submitted before the cut completed. The next cut or monitor tick after
`T_back` is such an observation; a drained NodeUnstage capture is another,
valid at once. Trust is per (leg, epoch), fixed at the cut, never revoked.
An epoch copied onto a leg inherits trust only when the source epoch was
trusted and the destination's base was trusted. Under this rule:

- **Source selection.** A copy session (bulk, chase, final delta, the §7
  window and backfill) uses a source only for epochs trusted on it. The
  check runs before the copy starts; admission then checks only the final
  delta's source.
- **Base selection.** `E_b` is R_dst's own newest trusted epoch. For a
  returning leg that is the `T_back` back-off made exact; for a head that
  holds copies it is the base of the last trusted copy, which the interim
  *base floor* approximates from below.
- **Admission at stage.** The final delta's source must hold every acked
  write through the end of the previous raid lifetime, which only a
  drained observation shows: the unstage capture after a clean unstage, or
  the quiesce in §7. After a consumer crash no such observation exists, for
  any survivor. The design does **not** wait for one. The assembly that
  serves a head is itself the observation that defines the volume's history
  from that point, so a standby equalized by the final delta to a leg the
  new assembly admits as a member joins at that member's standing, no
  better and no worse. After a crash that standing is §6's survivor
  divergence, which the standby neither widens nor narrows. Refusing it
  would cost a raid lifetime of redundancy for everything else on the
  volume and would tie rebuild latency to the next clean unstage, which
  the performance envelope below forbids. The capture is what makes the
  members' heads provably complete *since that assembly began*, rather
  than conventionally so; a leg that lost its mark in a crash and was
  re-admitted is captured configured at the next clean unstage while still
  missing the pre-crash write, which is the same residual, not a new one.
  The capture is not a precondition of admission.
- **§3's exclusion, enforced.** A leg co-located with a zombie consumer is
  not configured in the authoritative raid after the fence flip, so its
  later epochs are untrusted and it is neither cut nor copied from.
- **The writer set (F36c)** is the configured set at the last observation.

**Observations the record needs** (three positive writes): the base list
captured at NodeUnstage before `bdev_raid_delete`, marking the
non-configured legs stale in the same write, **only when this node is still
the volume's consumer** (the attached VolumeAttachment names it and the
raid's base list is within the record's writer set) — a late unstage on a
former consumer sees a raid missing exactly the healthy legs fenced off
from it and must write nothing; the raid-health monitor's stale marks
gated on the VolumeAttachment's node; and the fence flip marking the
co-located leg stale. **Residual:** a consumer that crashes inside a
failure window leaves no observation after the last tick; the survivors
can disagree and no survivor is provably complete. That is §6's survivor
divergence, accepted there, and this rule does not close it.

**Liveness cost.** The rule makes every cut depend on one read of the
consumer node's raid. A cut whose membership read fails yields an epoch
trusted on no leg: safe, but every catch-up then waits for the next
observation, where today's scheduler cuts regardless of the consumer node.
The scheduler should treat a failed membership read as it treats a failed
snapshot on a target, a failed cut that records nothing, so the cost is one
missed cadence rather than a retained epoch nobody can use. A volume whose
consumer node stays unreachable gets no new trusted epochs until it is
restaged, which is also when its record is next observed; modelled as a
FlintCatchupT arm (flint-e4).

**Performance envelope (a requirement, not an estimate).** The rule must
not touch the data path or the rebuild's copy rate, and as designed it does
not: raid1, the NVMe-oF exports and the lvols are unchanged; the copy is the
same shallow copy of the same epoch snapshots; the rule only decides which
epochs and sources are eligible. What it adds is control-plane only: one
read-only `bdev_raid_get_bdevs` per cut on the consumer node, which the
health monitor already issues every 60 s and which the scheduler should
batch per consumer node rather than per volume; one list of leg uuids per
retained epoch in the record (K × N uuids); and one record write per clean
unstage. The one latency effect is on the chase, which copies an epoch only
after its second observation, so a standby's lag bound grows by at most one
`T_back` (the I/O timeout plus margin), never its throughput; a drained
unstage capture removes even that wait for the final delta. Against the
interim guards it copies less, not more: a demoted standby re-copies from
its floor today, while under the rule its trusted copies stay.

**Status.** Interim, shipped first: the *source tie* (each copy session
records its source before copying; admission refuses unless every source is
still `in_sync`, unchanged since the copy, and attached), the *base floor*
(no base above the stale episode's first revert base) and a *forced
re-revert* after any demotion. That is a conservative encoding of the rule;
its cost is that a complete standby loses its copy whenever a source is
merely unattached at the next stage (`spdk-csi-driver/src/catchup.rs`,
`replica_sync.rs`; FlintCatchupN arms Tie, TieDemoteAsCode,
TieFloorFirstOnly). Per-epoch membership itself is modelled ahead of code
(`formal/FlintCatchupT.tla`, next subsection); once it lands, the floor and
the forced re-revert are redundant and the tie reduces to the belt described
there. One piece of the rule is already exact in code: a cut stamps a
leg's mark only when the leg is still `in_sync` when the cut is recorded
(`3cc7ccbe`, 2026-10-29). FlintCatchupT rev 14's `CutStampsAll` arm
showed the shipped stamping, which marked every planned leg when the
record write landed, moving a standby's mark past an epoch its chase
never copied, so a leg was admitted without an acknowledged write
(`Inv_Catchup` at depth 23); with the fix the arm is clean to depth 35.

### What the rule rests on, and what it leaves open (rev 7, 2026-10-29)

`formal/FlintCatchupT.tla` (flint-e4; stage-only, recursion-free; on the
branch `csi-rebuild-tlaps` until pushed) models the rule beside the shipped
policy and the interim tie, with a knob for each thing the code does that
the earlier models assumed away: catch-up never waits for a cut, the
stage's assembly write marks the legs it excludes, the local-leg guard,
F36c's last-writer defer, and crash-torn values counted as legal (raid1's
write hole). Rev 13, md5 `2dbea4a0`, tla-mc; bound D = 3 replicas, 2
failures, 1 write, 2 epochs; bound C = 3 epochs. No HOLD below is vacuous
(probe run). Arms are code-faithful with the capture on unless noted.

| Arm | Shipped | Tie | Trust |
| --- | --- | --- | --- |
| Full invariant, bound D | HOLD 6,301,545 | HOLD 10,275,876 | HOLD 34,814,740 |
| Full invariant, bound C | **VIOLATED** d22 (2,083,303) | not run | no violation to d26 (70,298,735; stopped for memory, not a pass) |
| Catch-up's claim (`Inv_AdmitNoLoss`), lost mark | HOLD 6.6M | HOLD 10.5M | HOLD 53.6M |
| Catch-up's claim, consumer crash, torn values legal | HOLD 14,331,651 | HOLD 23,027,196 | no violation to d30 (103,013,286; 1 h cap) |
| Zombie move, local-leg guard, assembly write | | HOLD 23,114,358 | HOLD 26,855,211 |
| Zombie move, guard, **no** assembly write | | | **VIOLATED** d17 (3,842,866): a dirty epoch admitted |
| Catch-up's claim, zombie writes a configured leg (no fence) | VIOLATED d15 | HOLD 11,659,490 | **VIOLATED** d15 (395,431) |
| Trust without the drop premise | | | `TrustComplete` **VIOLATED** d12 (rev 3, 99,094); `Inv_AdmitNoLoss` HOLD 50.1M |
| F36c defer on, zombie move | `Inv_MembersAgree` **VIOLATED** d4, every policy | | `Inv_AdmitNoLoss` **VIOLATED** d17 |
| §6 residual, `Inv_NoNewLoss`, capture gated | **VIOLATED** d9, every policy, no copy or admission in the trace | | |

Bound C is the first bound that separates the policies. The shipped loss
is rev 6's failure-window trace with the capture on: the leg in its
detection lag is already a bulk-copy source when the capture marks it.

**Two premises, which the tie does not need.** Every trust HOLD rests on:

1. *Drop at the first failed write.* raid1 removes a base when a write to
   it fails (`raid1.c:52-70` → `bdev_raid.c:2440-2444`), so a configured
   base has completed every acknowledged write. A write that hangs
   without failing keeps the base configured until the I/O timeout, which
   is what `T_back` covers. Without the premise the rule's completeness
   claim fails at depth 12 while catch-up's own claim still holds. It is
   stock behaviour that an SPDK bump could change silently: the phase-3/4
   cluster suite must pin it beside the equalized-bases cornerstone (a
   write that fails on one base leaves `bdev_raid_get_bdevs` without that
   base by the time the write completes).
2. *The fence.* A consumer fenced off a leg cannot write it once the
   authoritative raid configures it. A zombie writing a configured member
   is raid-level split brain under every policy; the tie's HOLD in that
   arm says only that its claim is about admission. In code the fence
   flip's probe failed open until `044f2010`; the zombie's co-located leg
   stays out of the new raid through the lvol's claim and the assembly's
   stale write (kind, 2026-10-28).

The tie needs neither, because it trusts nothing it did not copy and
refuses at admission; its price is the demotion described under *Status*
above.

**Both premises observed live** (flint-91, 2026-10-30). Each held once on a
three-node cluster (gate1613, 3 × i4i.2xlarge, us-east-2, 21:27–21:56Z;
spdk-tgt 1.7.3 as shipped, digest `9e31bea6…`; flint-driver 1.61.3-rc1
built from `eb7c28fd`). Premise 1, on the real failure path: a QD1
O_DIRECT writer ran on a raw three-replica volume while one leg's NVMe-oF
port was cut from the consumer by iptables. Under a 90 s cut the raid
dropped the leg after 49 s; scanned at the drop, the leg held sequence
0..77,946 with no gap while acknowledgements went on to 543,423, and the
two configured legs held every acknowledged write, so no write reached the
dropped leg after the one it missed. Under a 21 s cut the leg stayed
configured and all three legs were complete. Two arms report nothing: the
spdk-tgt kill arm never killed the target (a script bug), and the
rejoined leg's region after the 90 s cut was not scanned (an export bug).
Premise 2, a real zombie: kubelet stopped on node A with its writer
running, the pod was force-deleted, and the volume moved to node B after
the six-minute force-detach. B wrote all 16,384 blocks once while the
zombie kept overwriting; both legs of B's raid hold 16,384 of 16,384 of
B's records and none of the zombie's. The control fired: the zombie's raid
lost both remote legs and kept its own local one, on which it went on
acknowledging and landed all 16,384 blocks after B's pass. The monitor
gate (`eb7c28fd`) was live: the zombie's monitor logged "recording nothing
for it" six times, B's and C's legs stayed `in_sync`, and A's leg was
marked stale by B's stage. The run's scripts are not committed
(`bench/results/2026-10-30-ec2-1613/chaos/run-1613.sh`, functions `p1` and
`p2`, and `p2man.sh`), so this is one observation each, not the pin the
code ask in §12 names.

One finding beside premise 1, liveness and not safety. After an spdk-tgt
restart on a raid host the driver's repair attaches controllers before it
sets the dead-target options (`bdev_nvme_set_options`), which SPDK then
refuses, since `spdk_bdev_nvme_set_opts` returns `EPERM` once any
controller is attached ("RPC not permitted with nvme controllers already
attached", `module/bdev/nvme/bdev_nvme_rpc.c`, v27.01-pre; the I/O timeout
and its action are among the refused fields); a black-holed leg under
that host never faults, the raid hangs, and the first failed write never
comes. Seen live: more than ten
minutes with no drop on a node whose target had restarted four times,
against 49 s before them. The premise's completeness claim is intact,
since a write that hangs is never acknowledged; what goes is the I/O
timeout `T_back` assumes and the volume's availability. The fix is on
flint-91's branch `p4-first-attach`, not pushed.

**Completeness, not cleanliness.** The rule says a trusted epoch holds
every acknowledged write. It does not say the epoch holds only writes the
volume acknowledged: a zombie's writes on an excluded leg, or a crash-torn
write, make a leg *dirty* without making it incomplete. Cleanliness rests
on §3's fence and on the stage's assembly write marking stale every leg it
excludes (`record_assembly_sync_state`). That write is load-bearing and
best effort: a stage whose write fails serves anyway. The model's trace
needs exactly that failure: two pod moves inside one monitor window, the
first zombie's leg never marked and joined to the next raid, its epoch
complete but dirty, admitted by trust at depth 17. The write's own comment
calls it record-before-writes; it should be one, and a stage whose
assembly write fails should not serve. That is a new refusal and ships
with its retry caller (the CO's NodeStage retry). It closes the no-write
trace only: the defer seam below reaches the same depth with the write
landed, and belongs to the defer or to record-before-ack.

**The defer seam (F36c).** F36c defers a degraded assembly for up to
`FLINT_F36C_DEFER_SECS` (180 s) when a leg of the last serving assembly is
transiently unavailable, so that the freshest leg can rejoin
(`docs/f36c-assembly-freshness-gate.md`). Its premise, that every acked
write lives on every leg of the last assembly, is the premise rev 6 found
false at three replicas. And inside its wait, hygiene on the old consumer
can free the zombie's co-located leg, which then attaches at the retry as
an `in_sync` member carrying the zombie's writes: `Inv_MembersAgree` at
depth 4 under every policy, and the junk reaches a trusted epoch by depth
17. Rev 6's third positive write, marking the co-located leg stale at the
fence flip, was the candidate closure that leaves the ack path alone.
Run (batch c16, FlintCatchupT rev 15 `8ac3cf00`, 2026-10-30, bound D,
every arm a full pass, the defer on, no record-before-ack, the cut's
mark fix in), it closes the seam's catch-up claim and nothing more:
under trust `Inv_AdmitNoLoss` holds over the full space with the mark
(226,393,635 states, depth 48) and fails at depth 17 without it, the
control killed; under the tie it holds with or without the mark
(46,886,451 and 28,953,576 states), so there the mark earns no credit;
and `Inv_NoNewLoss` fails within eight steps with the mark and nine
without, under both policies, which is §6's residual below and needs
record-before-ack. Under record-before-ack every `in_sync` leg holds
every acked write at ack time, the defer has no case left, and it goes.

**The residual's two closures (a decision for the user, not taken here).**
§6's survivor divergence, a consumer that crashes or whose pod moves
inside a failure window, violates `Inv_NoNewLoss` at depth 9 under every
policy with the capture gated and no copy or admission in the trace: a
write lands on one leg, the raid fails the other two before either stale
mark lands, the write is acked, the consumer leaves, and the new assembly
takes the two legs that missed it. Two closures were modelled, both on rev
13, trust policy, bound D, capture on unless noted; "defer dropped" is
F36c's last-writer defer off.

| Arm | `Inv_NoNewLoss` | `Inv_MembersHold` | `Inv_RaidHolds` |
| --- | --- | --- | --- |
| (b) record-before-ack, defer dropped, zombie move | HOLD 88,624,143 | HOLD | HOLD |
| (b), defer dropped, consumer crash | HOLD 108,524,676 | HOLD | HOLD |
| (b), defer dropped, lost mark, **capture off** | HOLD 40,693,795 | HOLD | HOLD |
| today, lost mark | HOLD 53,577,694 | **VIOLATED** d8 | |
| today, consumer crash | HOLD 126,035,757 | **VIOLATED** d8 (write hole) | |
| today, defer dropped, zombie move | **VIOLATED** d8 | **VIOLATED** d7 | |
| (a) write and assembly quorum 2 of 3, no (b), defer dropped, zombie move | **VIOLATED** d18 | **VIOLATED** d7 | **VIOLATED** d18 |

- **(a) A write quorum** (acknowledge only after two legs complete;
  assemble only with two) does not close it: a lost mark lets catch-up
  copy the gap onto a second leg, after which the quorums no longer
  intersect. It also costs availability. With a quorum of one, an
  acknowledgement on a single landing and a one-leg stage are both
  reachable; with two, neither is (11,968,041 states). (a) is a
  durability policy, "no acknowledgement on one copy", and would sit on
  top of (b), not in its place.
- **(b) Record-before-ack** closes it alone, provided the defer goes.
  When raid1 fails a base, the completions of the writes degraded by that
  failure are held until the base's stale mark is durable, then released.
  Only the failure path pays: one mark per base failure, in the ack path
  of the writes in flight at that moment; the healthy path and the copy
  rate are untouched. It removes the cause behind every rev 6 finding,
  the record lagging the raid, rather than a symptom. A degraded write
  still acknowledges (`LiveAckB`, a full pass on rev 15, 23,212,330
  states, depth 39; c15); a mark
  that can never land blocks it, and that is the cost: a leg failure
  while the record is unreachable stalls the volume's writes where today
  it serves degraded. The data path is spdk-tgt alone, so (b) is a
  carried raid1 patch of the §7 kind (hold and release). Rev 8 takes the
  on-disk shape, §12: the mark is a membership stamp the raid writes on
  the surviving legs before it releases, and the sync record confirms
  only a continuation on a minority of the assembly, because a stamp on
  a single leg is one a zombie can write.

What (b) changes in this section is set out in §12 under "What it
changes in §5 and §6". Rev 7's sketch of that list is superseded: it
assumed the record itself is written before the ack, which is the shape
rev 8 did not take, so the record still trails a majority drop, per-epoch
membership stays, and F36c's defer stays with its gate loosened.

**The tie as a belt.** When the rule, or (b), lands, keep the tie's
refusal at admission of a copy whose source was marked stale since the
copy or changed its head: it is exact, costs one record read, and catches
a source that fails inside a copy session. Drop the demotion of a standby
whose source is merely unattached at the next stage. Whether the
attached conjunct earns its place was measured (c16, the tie admitting
on stale-since-copy alone, `FlintCatchupTTieNA` `780c1009`, beside the
full tie on rev 15 `8ac3cf00`, every arm a full pass): no arm kills it.
Without record-before-ack, catch-up's claim holds on the zombie move,
the lost mark and the crash with the conjunct dropped (17,934,639,
7,256,009 and 16,183,287 states) as with it (18,538,320, 7,363,457 and
16,480,908); under record-before-ack with the defer dropped,
`Inv_NoNewLoss`, `Inv_MembersHold` and `Inv_RaidHolds` hold on all
three shapes with it dropped (13,264,413, 6,918,623 and 15,109,620) and
with it kept (13,662,813, 6,975,995 and 15,348,672). The rows credit
the conjunct with nothing and show only that dropping it breaks nothing
at bound D; the full-tie HOLDs are not evidence that it is
load-bearing.

**Status, 2026-10-29.** Of the three positive writes: the unstage capture
shipped (`ef3a6f48`, 2026-10-28; gated on the attached VolumeAttachment
naming this node and every configured base being in the writer set); the
fence flip fails closed (`044f2010`: a probe that errors counts as still
connected); the monitor's gate on the VolumeAttachment node shipped
(`eb7c28fd`, 2026-10-30); the fence-flip mark on the co-located leg is
not written. Hot rejoin: §7's
rev 6 corrections hold in `FlintCatchupN3CHotFix` (195,931,627 states,
depth 48, complete) and shipped in `395b442d` (2026-10-29). The trust
model's rev 14/15 adds the cut's abort on a
re-created head, the `apply_epoch_cut` mark race (reported to flint-91),
and the trust-only rules (base at or below the target; a demoted standby
re-reverts). The confirmation batch on the final rev closed on 2026-10-30
(c15, 67 arms on `8ac3cf00`, tla-mc 0.1.2 then 0.2.1 at the user's word:
50 gave the verdict their arm wanted, a HOLD over the full space or a
control's violation; 16 informational; one with no verdict at its hour,
the ghost-lineage check, no violation to depth 21 over 93,978,850
states, bounded). Its cut setting is to be read with it: 58 arms model
the cut as shipped before the mark fix (`CutStampsAll` true), 53 of
them at two epochs, where no arm finds the race; the race's two traces
need a third epoch (the trust policy at depth 28, 70,721,422 states,
and the capture's bound C arm at 28: a leg goes standby between a cut's
snapshots and its record, the record stamps the epoch on it anyway, and
the leg is caught up past it). The nine arms with the fix hold where
they finish (the shipped and tie policies over the full space,
36,264,885 and 43,181,628 states at depth 43; trust bounded, no
violation to depth 28), and the fixed cut is a premise of the proof.
The TLAPS proof of the rule (clean setting: capture, no crash, no
zombie move, both premises, the fixed cut; target `Inv_Catchup ∧
MembersHold`) has all 7,673 obligations proved on a seeded cache (pass
7, 2026-10-29). The fresh-cache record run is reported done by flint-e4
(2026-10-31, 15:24 to 15:49Z on the box, otherwise idle: all 7,673
proved, 1,539 s, peak 25.4 GB, the three input md5s asserted, run by
`recT4.sh` after `recT.sh`'s summary was found reading the wrong
stream); twelve of its thirteen controls are in, and its results
section is cited here when the last lands. What the controls say so
far: eleven fail only the steps that use their premise or rule; the
crash and zombie-move controls fail only the lemma that those premises
switch the actions off, so the proof is cited for the clean setting and
never for a crash or a zombie; and the select-time bound on the base
(at or below the target) is not killed, since the proof rests on the
copy-time guard alone, so that filter is a progress rule, not safety.

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
  workload it endangers. *Rev 8:* closed by §12's membership stamp for a
  crash and for a pod move; what remains is the raid1 write hole on
  writes never acknowledged, which §12 leaves as it is.

### Cutover opportunities (when does "next assembly" happen?)

- **Naturally**: pod reschedule/restart, node drain, spot churn — largely the
  same events that cause replica outages in the first place. No action needed.
- **RWX volumes — on demand, with caveats**: bounce the `flint-nfs-server`
  pod; its synthetic RWO PVC is re-staged on restart (raid re-assembled with
  the standby included) while clients retry via the stable per-volume Service.
  But "ride through" must be scoped honestly against the shipped server:
  `flint-nfs-server` *held* all NFSv4 state **in memory** (the SQLite state
  backend existed in-tree but was not wired into the binary until
  2026-06-12 — see the closure note below), so a bounce lost
  clientids/sessions/opens/locks, and recovery rested on a 90 s allow-all
  grace window (`lease.rs:22`). Stateless I/O and uncommitted writes ride through
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
  `stable_nfs_instance_id`). *Closed later the same day:* the SQLite state
  backend is wired into the shipped binary with its DB on the exported
  volume (`.flint-nfs/state.db`, `synchronous=FULL`), exactly as
  prescribed above — clientids, stateids, `reclaim_complete`, and an
  instance counter roam with the PVC; sessions are deliberately dropped
  (kernel clients re-`CREATE_SESSION` on `BADSESSION`). Validated live:
  a dirty-state client (one long-lived fd, no fsync) rode through a
  server replacement with its uncommitted writes retransmitted against
  the restored stateid, no grace stall, gap-free file. Getting there
  required fixing a four-deep durability chain (SIGTERM ignored →
  lazy-unmount data loss → `synchronous=NORMAL` → a marker-aliasing bug
  that reformatted RWX volumes at every restage); see
  `phase6-residual-2026-06-12.md`, NFSv4 persistence round. Byte-range
  locks are still memory-only (`LockManager` sits outside
  `StateManager`) — the remaining gap before claiming lock-holding
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

> **Decision 2026-06-12** (`tier2-evaluation-2026-06-12.md`): GO as a
> bounded spike (phase 7a — patch + one manual hot rejoin, quiesce window
> and crash drills measured); orchestration (7b) gated on the spike. Note
> the evaluation's rev-5 deltas to this section: superblock-less raids
> moot the sb-flip completion sequence and the examine-divert hazard, and
> crash safety rides the control-plane record instead of on-disk slot
> state.

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

> **Scope note (added 2026-07-24):** the "grow" / spdk/spdk#3349 discussed in
> this section is **replica-count growth** (add a base to a live array *without*
> triggering a rebuild — the `skip_rebuild` primitive below). Do **not** read
> it as "SPDK cannot grow a raid's *size*." Online raid **volume-size** growth
> is a separate, already-shipped feature: `raid1_resize`
> (`module/bdev/raid/raid1.c:587-630`, upstream since v24.09) fires
> automatically on a base-bdev `SPDK_BDEV_EVENT_RESIZE`
> (`bdev_raid.c:2466-2544`), and for flint's `bdev_nvme` legs the
> lvol→NVMe-oF-AEN→`bdev_nvme`→raid chain propagates with zero SPDK patches.
> The two are orthogonal. Volume-expansion analysis:
> `volume-expansion-status-2026-07.md` §2.1.

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

> **Rev 6 amendment (2026-10-28; §5's per-epoch rule).** Three corrections
> to the sequence above, found by `formal/FlintCatchupN.tla`'s hot-rejoin
> arms and the kind repro, where an inline rejoin put a leg rebuilt from a
> record-`in_sync` but raid-failed source into the live raid:
>
> 1. **The window's source is read from the raid, not the record.** R_src in
>    step 2 must be a *configured base* of the live raid as read under the
>    quiesce; `in_sync` per the record is not sufficient, since a leg in its
>    failure window is `in_sync` and missing acked writes. `E_f` cut under
>    the quiesce is trusted on every configured base: the quiesce is the
>    observation.
> 2. **The landing pad's base is its own newest trusted epoch**, not its
>    chain top: a stale leg's chain top was never backed off, and a demoted
>    standby's chain top is a copy. Step 4's backfill uses only epochs
>    trusted on the backfill source, per §5.
> 3. **The pad's revert reaps as §5's does**: epochs newer than the base,
>    and for a full build every retained epoch name. Localization today
>    reaps nothing, so a stale leg's old epoch survives a full-build
>    localization and a second rejoin can re-base on it.
>
> The bound-C run holds (`FlintCatchupN3CHotFix`, 195,931,627 states,
> depth 48, complete, 2026-10-28), and the code shipped in `395b442d`
> (2026-10-29; `hot_rejoin.rs`, `replica_sync.rs`): the window's source is
> a configured base of the live raid, re-read under the quiesce; every
> copy source is written ahead and the tie re-checked before the inline
> promotion, the localization replay and `in_sync`; the pad's base follows
> `select_base_epoch`; and localization reaps after the pad's revert.

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
     `disk.chert.us/replica-{node_uid}=true` (or reconcile must select
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

   > **Note 2026-10-02:** this PASS came from the kuttl suites, several of whose steps could not fail (`$patch: delete` deleted nothing; absence was "asserted" with `--ignore-not-found`; see tests/system/README.md, "Migration from kuttl"). "Cross-node RWO migration" in particular was never exercised: its writer was never deleted. The Chainsaw suites that replaced them passed on real nodes on 2026-10-02 (tests/system/results/2026-10-02-aws-spdk/).

1. **Persistent replica sync-state** in PV annotations (`sync_state` ∈
   `in_sync`/`stale`/`standby`, `last_epoch`, current epoch name). *Control
   plane.*

   **Implementation status (2026-06-11):** implemented on `main` as
   `replica_sync.rs`. The record lives in one PV annotation
   (`disk.chert.us/replica-sync-state`): per-replica `{node_name,
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
     fight it. An ONLINE raid on the *replica's* node refuses the catch-up
     only if it holds THIS head as a configured base (zombie consumer, §2
     fact 3, or the forced-stale fallback serving the leg locally), and it is
     never deleted. An ONLINE raid there that does not hold the head is the
     consumer's own, serving the other legs: the catch-up proceeds beside it
     (2026-10-28; until then any ONLINE raid refused, and a stale leg on the
     consumer's node stayed unbuilt while the pod stayed). Since flint raids
     are created with `superblock: false`, no head carries a raid sb today,
     and the examine above cannot re-add one to a live raid.
   - **Served-locally guard, before the revert** (FlintLocalLeg,
     2026-10-28): F36's `head_live_consumer` finds a served head through
     its nvmf export, and a head the consumer's raid holds LOCALLY has none.
     So the bulk catch-up and the chase also read the replica node's raids
     and defer (`ReplicaHeadIsLocalBase`; fail closed on a probe error)
     while any raid holds the head as a configured base. Without that, the
     revert's `bdev_lvol_delete` hot-removed a served base (SPDK checks no
     claim), and with the raid's other leg already faulted out, the acked
     writes only that head held went with it (`formal/FlintLocalLeg.tla`).
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
     `disk.chert.us/rejoin-bounce: "enabled"`; the claim's pods
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
`disk.chert.us/node-name`/`lvol-uuid` volumeAttributes — set only for
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
degrades to a hint. A multi-replica restore (since 2026-10-27,
`snapshot/restore.rs`) is exactly that single-replica clone plus phase-5
full builds for the remaining replicas: the clone is leg 0 (`vol_<vol>_
replica_0`), the other legs are empty placeholders on the freest other
nodes, seeded STALE both in the sync record and in volumeAttributes
(`disk.chert.us/replicas-seeded-stale` — the record is patched
best-effort after CreateVolume, and a record synthesized without the
list would read an empty leg as in_sync), and the catch-up cuts the
volume's first epoch on the clone (`ensure_epoch_for_rebuild`, below)
before the thin-aware full builds. The placeholders are also marked
`empty_head` in the record until they reach standby. Two staging rules
keep them out of service meanwhile: a record-stale leg is excluded at
stage whether or not epochs exist yet, and the forced-stale fallback never
admits an empty or half-built head. So the clone serves alone until the
others are built. A first version had neither; a WaitForFirstConsumer pod
staged the restore before any epoch, the raid was assembled from the clone
and both empty legs, and the node formatted it (dash1, 2026-10-27; the
model's RestoreLoss and EmptyHeadRestore). Too few nodes with room refuse
the create, as a plain multi-replica create does. Cloning each node's own
copy was rejected: copies are equivalent, not identical (cut skew), and
a raid of legs that differ is not a raid.

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

## 12. Record-before-ack: the membership stamp (rev 8, 2026-10-29)

§5's rev 7 subsection left one decision open: how to close §6's survivor
divergence, a consumer that crashes or whose pod moves inside a failure
window. The model showed a write quorum does not close it and
record-before-ack does. The user took record-before-ack in the on-disk
shape. This section is the design pass for it, with one correction to the
shape as rev 7 sketched it.

### The guarantee, and the correction

The invariant the raid must keep: **at every acknowledgement, the set of
legs that completed the write is recorded durably somewhere the next
assembly reads before it serves.** raid1 today acknowledges when every
configured base has responded and one has succeeded
(`raid1_write_bdev_io_completion`, `raid1.c`), then marks the failed base
on the app thread (`raid_bdev_fail_base_bdev`), so the acknowledgement
precedes every record of the failure. Stock SPDK's superblock does not
help even where it is enabled: its update sits after
`raid_bdev_remove_base_bdev_on_quiesced`, and the quiesce has already
completed the in-flight I/O to the upper layer.

The correction. A stamp on the disks alone does not meet the invariant,
because the disks cannot tell who wrote the stamp. After a pod move the
old consumer is fenced off every remote leg (§3), but the leg on its own
node is attached directly (`FlintLocalLeg`) and stays writable to it. Its
raid, failing the fenced legs, would stamp that local leg "sole member
now", and the next stage could not distinguish that stamp from a
legitimate continuation on one leg before the move. The two cases are
the same bytes; what differs is whether the writer was still the
volume's consumer, which only the control plane knows.

So the stamp is paired with one rule: **a stamp is self-validating only
when it names a majority of the assembly.** Placement puts the legs of a
volume on distinct nodes (`driver.rs:566`), so a fenced-off consumer
holds at most one leg, and with the §3 fence in force it cannot stamp a
majority. A continuation on a minority (two legs lost at once on a
three-replica volume, or any loss on a two-replica volume) additionally
waits for the sync record, written by the node agent and gated on the
VolumeAttachment naming its node, before the raid releases. A zombie's
unconfirmed minority continuation therefore never acknowledges: its
record write is refused, and its held writes time out. A legitimate one
pays one record write. The common case, one leg lost of three, pays one
stamp write on the disks and never touches the control plane. One
continuation a zombie can keep is the one the record confirmed while it
was still the consumer: a raid already reduced to that single leg sees
no further failure at the move, since the fence takes nothing it still
holds, and it goes on acknowledging on that leg. That is safe because
the record names the leg as the only `in_sync` one, the next stage
cannot conclude without it and waits for hygiene to free it, and the
assembly that follows carries every write acknowledged there; with the
node gone, only the operator's cut from a stale leg serves without
them, marked (step 3). The model reaches it (c21, `Reach_ZombieAck`, a
permitted trace), and rev 8's first wording, that a zombie never
acknowledges, was too strong by exactly this case.

### The stamp

One block-aligned record at the tail of every leg, in a region the raid
excludes from the volume's LBA range:

| Field | Content |
| --- | --- |
| `magic`, `version`, `crc` | self-describing; a block that fails the check is "no stamp" |
| `volume` | the volume's uuid |
| `seq` | the assembly sequence: allocated by the stage in the sync record's assembly write (§5's record-before-writes), one per assembly of the volume |
| `gen` | the membership generation within the assembly: 0 at assembly, +1 per base lost |
| `size` | the assembly's member count at `gen` 0, so a reader can judge majority |
| `members` | the lvol uuids configured after this change |
| `clock` | the consumer's wall clock at the change, for `since` |
| `rgen`, `rmem` | the last **released** generation of this assembly and its members: the newest membership the raid acknowledged under, by majority or by the record's confirmation, copied forward into every stamp, so a stamp the raid never released still shows what was released before it |

Order is lexicographic on (`seq`, `gen`). The region is one lvolstore
cluster, so its allocation never shares a cluster with data; the lvol is
created one cluster larger than the volume, and raid1 sizes the array
from each base's `data_size` (`raid1_start`, `raid1.c`), which the patch
reduces by the reserve where `data_size` is derived from `blockcnt` and
`data_offset` (`bdev_raid.c`, `raid_bdev_configure_base_bdev`). The
filesystem stays at LBA 0 (rev 5's restore invariant) and never sees the
region.

Written at three moments, always by the consumer's `spdk-tgt`:

- **Assembly.** `bdev_raid_create` takes the stamp option with `seq`, and
  writes (`seq`, 0, `size`, members) to every base before the raid comes
  online. This is the on-disk twin of the assembly's record write and
  happens after it.
- **A base lost.** The barrier below: (`seq`, `gen`+1, survivors) on every
  surviving configured base before any degraded completion is released.
- **Resize.** The region moves with the tail, and the raid follows the
  lvol at once: `bdev_lvol_resize` changes the base's block count,
  `raid_bdev_resize_base_bdev` is told, and `raid1_resize` sizes the
  array as the smallest present base's `blockcnt` less `data_offset`,
  skipping absent bases. The move is made by whoever holds the leg:
  the raid, on the growth event, when the leg is a configured base;
  the driver, through the two RPCs below, after a grow of a leg under
  no raid. Either copies the current stamp to the new tail with the
  old tail's LBA recorded in it (`prev`), accepts the new size, zeroes
  the old region (so the LBAs it vacates read as zeros, the thin-volume
  invariant) and clears `prev`; each step is idempotent, and
  `bdev_raid_create` with the stamp option, and a base's add, finish an
  interrupted move before the base serves: a tail stamp with `prev` set
  has its old region zeroed and `prev` cleared. The reserve comes off
  in `raid1_resize` as it does in `raid_bdev_configure_base_bdev`, or
  the first grow puts the new tail inside the volume's LBA range.

**The resize's two windows and the short leg** (flint-e4, 2026-10-30, at
the user's word). A crash after the grow and before the copy leaves no
valid stamp at the new tail; the reader treats that as "no stamp" and
the stage falls back to the record for that assembly, today's
behaviour. A crash after the copy and before the zeroing leaves a valid
stamp at the new tail and the old stamp's bytes, one cluster (128 KiB,
`LVS_CLUSTER_SZ`) of volume and lvol uuids, `seq`, `gen` and a clock,
inside the volume's LBA range on that leg. No user data is at risk,
because the filesystem is grown by NodeExpandVolume only after
ControllerExpandVolume succeeds, which is only after every leg's grow,
and a partial fan-out is re-driven by the resizer's retry
(`ExpandReplicaFanoutIncomplete`, `expand.rs`); the thin-volume
invariant, though, would rest on that ordering alone, which is why the
assembly finishes the move itself rather than trusting the next retry.
A leg absent during a grow: a leg the record holds stale or standby
never sees one, because the expand's belt refuses the whole fan-out
while any leg lags (`replicas_not_in_sync`, v1.21.0; the drill of
2026-07-27 showed no partial fan-out behind it), so only a lost-mark
leg, this section's subject, can be absent during a grow. Its own grow
fails on its node and is re-driven; meanwhile the raid grows on the
present legs, whose stamps the barrier wrote at the leg's loss with
`gen`+1 and without it, so the short leg's stamp at its old tail is
older by the rule and reads stale, whatever its size. Where no barrier
ran, a grow with the raid unassembled, the short leg's stamp may tie
and count in the read, which is correct, since it holds the
acknowledged history and only lacks room; three existing guards then
keep it from serving or bounding the array: the driver's leg-size
guard drops a shorter leg before the create (`leg_size_guard.rs`, the
largest cohort kept and the PV's capacity the floor, at the stage, at
standby admission and at the node agent's construction boundary),
stock raid1 refuses a shorter base rejoining a grown array (a base's
`data_size` survives its removal, and `raid_bdev_configure_base_bdev`
fails the add when offset and size exceed its capacity, which §7's
hot-rejoin path must expect), and catch-up grows a short standby head
to its source before attaching it (`align_dst_head_size`, F56) and ends
with admission's stamp at the new tail. None of this is in the model:
FlintCatchupS has fixed-size legs (`Blocks = {b1}`), no verdict row
covers the resize rule, and the raid unit tests asked below check it.

Read by the control plane through two small RPCs the patch adds beside
the raid module, `bdev_flint_stamp_read` and `bdev_flint_stamp_write`,
on any bdev: the consumer's attached remote legs and its local lvol at
stage, the node's own lvols for the capture and the monitor.

### The barrier: hold and release

`raid_bdev_io_complete` (`bdev_raid.c`) is the one funnel every raid I/O
completes through. The patch adds a per-raid *pending membership change*
and, per channel, a queue of completions held while it is set:

1. `raid1_write_bdev_io_completion` on a failed base write sets the
   pending flag (an atomic on the raid) **before** it calls
   `raid_bdev_io_complete_part`, so the write that saw the failure is
   itself held, and so is every write that completes on any thread from
   that instant.
2. `_raid_bdev_fail_base_bdev`, on the app thread, writes the next stamp,
   carrying the last released generation as (`rgen`, `rmem`), to the
   surviving configured bases, with the multi-base write pattern of
   `raid_bdev_write_superblock` and a different offset and payload.
   This runs **before** the removal's quiesce: the quiesce waits for
   in-flight I/O to complete, held completions never do, and the two
   would deadlock. The stock sequence (quiesce, superblock, deconfigure,
   unquiesce) follows unchanged once the stamp has landed.
3. A stamp write that fails on a survivor fails that survivor as well:
   the pending change is extended, a further stamp names the rest, and
   the chain ends when a stamp has landed on every remaining base, or
   when none remains, in which case the held writes complete as failed.
   No acknowledgement without a durable stamp, ever.
4. Release. If the survivors are a majority of `size`, the flag clears
   and every channel flushes its queue (`spdk_for_each_channel`). If
   not, the raid reports the change in `bdev_raid_get_bdevs`
   (`pending_membership: {seq, gen, members}`) and keeps holding until
   `bdev_raid_confirm_membership(name, seq, gen)` arrives. The node agent
   polls `bdev_raid_get_bdevs` on its own raids at one second, writes the
   record (the survivors' marks and the sole-continuation flag) only when
   the attached VolumeAttachment names its node, then confirms. A refused
   or failed record write confirms nothing; the held writes reach the
   initiator's I/O timeout and fail, which is the correct outcome for a
   zombie and the accepted one for a legitimate consumer cut off from
   the API server while losing two legs at once.

Reads are held too. Serving a read from a surviving base during the
barrier would be safe, since reads change nothing on the disks, but the
barrier lasts one write round trip and a single queue is simpler. Every
in-flight write is in the barrier, not only the one that failed: a write
submitted after the base failed but before the stamp landed would
otherwise acknowledge on the survivors with the failed base still named
a member on every disk.

**Five traps in the hook** (flint-91, from `bdev_raid.c` and `raid1.c` at
v26.05):

- *Split I/O.* `raid_bdev_io_complete` runs once per part of a split
  write; when the higher part completes it re-submits the lower part,
  resetting the submitted count and the status. The hold gates only the
  final branch, after the split block, and the degraded flag lives in a
  per-I/O field the re-submit does not reset; otherwise a write whose
  failed leg was in the first part acknowledges unheld.
- *The processed channel.* `ch_processed` is allocated per raid channel
  and never registered as an I/O channel; writes into a rebuild's
  processed region and a split's second part run on it, and
  `spdk_for_each_channel` never visits it. The queue keys on the parent
  channel (`spdk_io_channel_get_ctx` of the bdev_io's channel), so the
  release reaches every held completion.
- *Process I/O.* Rebuild writes complete through the process path and
  set nothing, which is right since they acknowledge nothing; rebuild
  reads pass through `raid_bdev_io_complete`, so the hold keys on the
  per-I/O flag only the write completion sets, never on "every
  completion". Held writes inside the next window's range stall that
  window's `spdk_bdev_quiesce_range` until the stamp lands: progress, not
  deadlock, as long as the stamp never waits on the process. §7's leased
  quiesce (patch 0006) behaves the same way, and a stamp that cannot land
  fails closed and lets the lease time out and unwind.
- *Reset.* The raid's reset does not wait for held writes, and
  `raid_bdev_submit_reset_request` resets every base, so a survivor's
  reset can abort a stamp write in flight. The stamp path fails closed:
  an aborted stamp write is retried, bounded; while no stamp has landed
  the hold stays and the raid stays degraded; nothing is released without
  a stamp.
- *The app thread.* The stamp is issued from the app thread, which needs
  its own channels on the configured bases (`spdk_bdev_get_io_channel` on
  their descriptors), as the superblock write path has.

A sixth, from the resize (2026-10-30): `raid1_resize` sizes the array
from `blockcnt` less `data_offset` over the present bases, not from
`data_size`, so the reserve comes off there as well; and
`raid_bdev_free_base_bdev_resource` clears `data_offset` but not
`data_size`, so a base absent through a grow is refused on rejoin by
`raid_bdev_configure_base_bdev` with "size exceeds capacity", which the
patch keeps and §7's hot-rejoin path must treat as a short leg for
catch-up to grow, not as a fault of the base.

Overlap with the carried series: of the twelve patches in
`spdk-csi-driver/spdk-patches/`, only 0006 (skip-rebuild on add, the
leased quiesce) touches `module/bdev/raid`. It changes
`raid_bdev_deconfigure_base_bdev`, `raid_bdev_configure_cb`,
`raid_bdev_configure_base_bdev_cont`, `raid_bdev_add_base_bdev`, the RPC
file and the headers, none of the functions above, so the stamp patch
rebases beside it with textual overlap only in `bdev_raid.h`'s
`raid_base_bdev_info` and in `bdev_raid_rpc.c`.

### Reading the stamps at assembly

The stage, after the fence flip and before `bdev_raid_create`:

1. Legs the record holds **stale** are excluded first, as today. Stamps
   decide only among record-`in_sync` legs. A stale leg's tail may carry
   a copied or reverted stamp that names it a member of some old
   generation; it does not matter, because the record's state is the
   control plane's decree and admission rewrites the stamp.
2. Read the stamp of every attached `in_sync` leg. A stamp is **valid**
   when its `members` are a majority of its `size`, or when the record
   shows the sole continuation it describes. A stamp that is not valid
   stands for the released generation it carries, (`seq`, `rgen`,
   `size`, `rmem`), which is valid by construction. Skipping it instead
   loses evidence, flint-e4's second trace (FlintCatchupS, bound D, lost
   mark): leg 1 drops, generation 1 lands on legs 2 and 3 and releases,
   a write is acknowledged on them; leg 2 drops, generation 2 lands on
   leg 3 alone, a minority held for the record; the consumer unstages
   cleanly before the record confirms. At the restage legs 1 and 3
   attach. Skipped, leg 3's unconfirmed stamp leaves leg 1's generation
   0 as the newest valid, two attached legs of three make it conclusive,
   and leg 1 lacks the write. Read as its released view, leg 3 stands
   for (1, 1, {2, 3}), the newest valid stamp, and leg 3 serves alone
   with leg 1 marked stale. Among the valid stamps and views take the
   greatest (`seq`, `gen`); its `members` are the assembly's candidates.
   The read is **conclusive** only when the attached `in_sync` legs,
   members of that stamp or not, number a majority of its `size`, or
   when the record vouches: a confirmed sole continuation on an attached
   leg, or every unattached leg record-stale, the previous consumer's leg
   counted as the record holds it, never as step 4 would mark it, or no
   majority of the stamp's `size` fits among the legs outside the attached
   `in_sync` set (the attached legs number more than `size` less a
   majority: one of two, two of four) while the record confirms no
   continuation on the legs outside it. The last vouch adds nothing at an
   odd size and everything at two replicas: every generation that
   acknowledged a write either was a majority, which then has a member
   among the attached legs, or was a minority the record confirmed, which
   would have marked the attached legs stale; so the newest acknowledged
   generation sits on an attached leg. Without it a two-replica volume
   whose other leg's node is down would wait for that node at every
   stage, and under the bound dropped in step 3 that wait would be the
   rule, not the exception.
   Counting attached members only
   is also safe, and defers where the count would serve, as in the
   trace above. Two majorities of one size intersect, so
   a newer majority stamp would sit on an attached leg; a read from fewer
   legs can miss one. flint-e4 found the gap before the model ran: a leg
   dropped at generation 1 while the other two stamp (`seq`, 1) and
   acknowledge a write, the consumer crashes inside the poll, both
   survivors go out and the dropped leg alone returns; its (`seq`, 0)
   stamp is the newest it can show, and the stage would serve without
   the write. An inconclusive read defers under step 3, which is what
   record-before-ack gives there, unavailable rather than lost. Every
   attached leg not in the candidates is marked stale in the assembly
   write, with the reason. A leg whose newest stamp is an unconfirmed
   continuation is a **torn leg**: unacknowledged writes may have landed
   on it after the last release, and once the fence is found only on
   such a leg, since a continuation stamps before it writes and a failed
   stamp write fails the survivor. The writes in flight when the fence
   is found land before the stamp, on the fenced assembly's members with
   equal stamps, and nothing on disk tells them from a crash's: they are
   the write hole, bounded by what was in flight, and closing them would
   need the fenced writer to learn of the fence before writing, a race.
   A torn leg counts for the read, its released view being evidence, but
   the assembly write marks it stale whenever a clean member of the
   newest valid generation is attached, and catch-up rebuilds it from
   its last trusted epoch, which predates the junk. It never serves
   alone on a vouch: its junk is everything the fenced writer landed
   while it ran, minutes on a partitioned node, not an in-flight set, so
   the stage waits for a clean member and serves a torn leg alone only
   at node-gone or by the operator's cut, under step 3's serve-with-risk,
   with the marker naming the leg and its stamp's clock. The trace that
   drew the rule, flint-e4's fifth (FlintCatchupS rev 9, two replicas,
   zombie move, bound D, `Inv_AdmitNoLoss` at depth 18): the fenced old
   consumer lands a write on leg 1 as the fence takes leg 2, before any
   stamp; hygiene frees leg 1 before the first stage attempt, so step 4
   never runs; the stage attaches both legs, conclusive, and serves with
   the junk on leg 1; the first cut puts the junk inside a trusted epoch;
   leg 1 drops and returns, is reverted to its own trusted epoch and
   admitted holding a value no member ever held and nobody acknowledged.
   As first read that was a gap. As classed since, the write that finds
   the fence lands before the stamp and is the hole above, legal and
   bounded; the three-replica form of the same trace is batch c21's
   under rev 7 (depth 17, 29,309,763 states), which that revision had no
   way to count as legal and rev 10 does. What the torn-leg rule
   prevents is the other form, the writes a fenced writer lands after
   its stamp: not an in-flight set but everything it writes while it
   runs, preventable, served to an application that never submitted it,
   with the held stamp on the leg as its evidence. The model shows that
   form served when the exclusion is off (c24, depth 17, beside a clean
   leg) and when the never-alone rule is off (c26, depth 7, alone on the
   vouch), and never served unmarked with both on: the full space at two
   replicas, and at three replicas to depth 27 on the zombie move (c26
   to depth 18 on the box, c27 to depth 27 on the 192-core box, bounded;
   the shapes without a fenced writer, the lost mark and the crash, are
   a full pass at three replicas under the same invariants, c27), with
   the expiry off (c26; under the expiry a torn leg is served alone and
   marked, step 3, a full pass at two replicas in the same batch). The rule costs
   a clean move nothing and cleans the crash-held case, a live consumer
   crashing inside a minority hold, the same way. At two replicas the
   read without the torn leg is one of two and needs the even-size
   vouch, so the two rules interlock.
3. If the read is inconclusive, the stage **defers**: the legs that would
   make it conclusive are unattached, and one of them may carry a newer
   stamp than any attached leg shows. F36c carries the wait. Its writer
   set is the record's `in_sync` legs the read lacks, and its gate is
   "the read concludes", not "at least one member attached": any one
   member sufficed for a conclusive list, since every acknowledged write
   landed on every member of the stamp that names them, but a member of
   the record's set can be exactly the leg that lacks the write, leg 3 in
   the second trace under step 4. F36c's premise, that every acked write
   lives on every leg of the last assembly, is now true by construction
   rather than by assumption, and its question is exact: the stamps say
   which legs the stage cannot do without. Its bound was the one
   product decision left, and the user made it (2026-10-30) after the
   model measured it. Today the gate serves the reachable legs after
   `FLINT_F36C_DEFER_SECS` (180 s), or at once when the missing leg's
   node is gone (`FLINT_F36C_NODE_GONE_SECS`, 360 s), and stamps
   `AckedTailRisk` (freshness_gate.rs, `GateDecision::ServeWithRisk`;
   drill 2.4's obligation that a lost writer never manufactures an
   outage). Under the stamp that marker is exact: writes acknowledged on
   the missing leg's newer generation, if it has one, are lost once the
   served assembly stamps a higher `seq`, because raid1 keeps no versions
   to merge them back. The measurement (FlintCatchupS rev 8, md5
   `ab167ffe`, the expiry arm, zombie move, bound D, a sanity run, not a
   HOLD): a write lands on legs 2 and 3 as leg 1 drops it and is
   acknowledged on generation {2, 3}; leg 2 goes out and the raid holds
   at 3; the pod moves before any monitor tick with the old raid still
   claiming leg 3; the new stage attaches leg 1 alone, at generation 0,
   one of three with nothing vouched, and defers rightly; the bound
   expires, the stage serves leg 1 as the next `seq` and marks 2 and 3
   stale, and the write survives only on two stale legs the new `seq`
   outranks: permanent, and marked (`Inv_NoNewLoss` at depth 12). No
   mark or tick is needed, only that the leg holding the write is
   unreachable at the stage, by a zombie's claim or a dead node; a crash
   instead attaches leg 3 and the read concludes on it. The cost is a
   three-replica effect: at two replicas the expiry loses no
   acknowledged write over the full space (c25), because an
   acknowledgement there needs both legs or a confirmed continuation
   whose confirm marks the other leg stale, so every record-`in_sync`
   leg holds every acknowledged write and the expiry serves only such
   legs; its serve of a torn leg is marked (c26); at three replicas it
   may lose a write held only on an unreachable leg, and that loss is
   marked (c22, bounded). The shipped gate
   makes the same serve at once and silently, since any attached writer
   satisfies it. **Decision: the transient bound is dropped and its
   timer kept as an alert; the node-gone serve stays.** A stage whose
   read is inconclusive refuses while the missing legs are transient
   (claim-blocked, or not ready inside the node-gone horizon); at the
   old 180 s it emits an event naming the legs it waits for and the cut
   that would serve without them, and keeps refusing. Serve-with-risk
   remains for two cases only, a node gone past the horizon and the
   operator's explicit cut from a stale leg, so the marker means one
   thing: a dead node or a person accepted the loss. The reasons:
   record-before-ack was taken so that an acknowledged write is never
   lost without someone deciding to lose it, and the stamp tells the
   stage with certainty when it cannot know; a claim-blocked or
   not-ready leg usually returns within minutes, the case the bound was
   sized for, which is exactly when waiting is cheap and right; and
   durable stores already take this posture (an incomplete placement
   group waits until an operator marks it lost). The cost: a Ready node
   whose agent is wedged and never frees its claim leaves the volume
   unavailable until an operator acts, an alerted operator matter
   rather than a data matter. The change is in the driver's stage,
   outside the raid1 patch. With the bound dropped the model's HOLDs,
   which cover the wait, are the product's exact claim, and the expiry
   arm documents what an operator's cut costs. A snapshot of a stale
   leg's head before its revert, which would keep a stranded tail for
   the operator, is not taken: with the bound dropped a tail is stranded
   only by a node returning after the horizon or by a cut, both rare and
   both chosen; revisit if drills show nodes returning after the horizon.
4. The previous consumer's own leg, when it cannot be attached (the old
   raid still claims it), is excluded and marked stale by decree **only
   by a stage whose read is conclusive without it**: the attached
   `in_sync` legs a majority of the newest stamp's `size`, or a confirmed
   sole continuation on an attached leg. Step 2's vouch counts that leg
   as the record holds it, never as this step would mark it. When the
   even-size vouch concludes the read and the stage assembles, the
   assembly write already marks the absent leg stale, as it marks every
   `in_sync` leg it excludes; what the vouch's decree adds is that mark
   on the defer path, when a read concludes but the stage still cannot
   assemble. It is not needed for safety: at two replicas the vouch by
   the read alone keeps every acknowledged write over the full space
   (c23, the completeness invariants; the junk invariants ran with the
   decree form only), and the two forms' counts differ only because a
   decree changes the record. Otherwise
   the stage defers under step 3 and waits for the leg as F36c's last
   writer; when hygiene frees it, it attaches and the read concludes with
   it. This is the enforcement §3 asked for and rev 6 listed as the third
   positive write, gated on the one fact that makes it safe: the attached
   evidence already decides, so the excluded leg holds nothing the
   assembly's members lack, and it rejoins by catch-up. Two traces fixed
   the gate (flint-e4, FlintCatchupS, zombie move, bound D). Rev 4 (md5
   `d32f4f23`, `Inv_NoNewLoss` at depth 8): legs 1 and 2 fail, the raid
   holds at {3} for the record, the monitor's tick marks 1 and 2 stale
   while the hold is up, the pod moves before the confirm, and the decree
   as rev 8 first wrote it marked 3 stale too; every leg record-stale
   with every acknowledged write on leg 3, a strand rather than a disk
   loss, and no path back without an operator. Sparing the leg when it is
   the record's only `in_sync` one closed that and not the next. Rev 5
   (depth 13): a write lands on legs 1 and 2 as leg 3 drops, generation 1
   = {1, 2} releases and the write is acknowledged; leg 1 goes out, the
   raid holds at {2}, the monitor marks 1 stale and not 3; the pod moves
   with leg 2 the previous consumer's leg and the record holding {2, 3}
   `in_sync`. Only leg 3 attaches, with generation 0. One of three is no
   majority, but a vouch that counted leg 2 stale by decree concluded,
   served on leg 3 and marked 2 stale, without the write. The record's
   `in_sync` count cannot gate the decree, because the record trails and
   misses marks by design; only stamps the stage can read, or a
   confirmation the record made, can. FlintCatchupS rev 7 (md5
   `6859c997`) carries the gate; batch c21's verdicts on it, beside the
   two decree controls, are in the table at the end of this section. The
   rev 4 strand's root is a
   monitor mark that is never load-bearing under the stamp: had the
   confirm come it would have written the same marks, and since it never
   came nothing was acknowledged past the last released view, so leg 2
   was complete and could have served at once. A monitor that skips its
   marks while `bdev_raid_get_bdevs` reports a pending membership would
   buy that availability; c21 carries it as an arm, and it is a
   follow-up, not part of this pass. This step also closes the design's
   rev 7 defer seam: a leg hygiene frees during the defer is either the
   one the stage waits for, and the read concludes with it, or already
   stale; a leg hygiene frees before the first attempt carries its held
   stamp and is step 2's torn leg.
5. `bdev_raid_create` with `seq` from the assembly write stamps the
   members, and the raid serves.

### What it changes in §5 and §6

| Mechanism | Under the stamp |
| --- | --- |
| §6 survivor divergence after a crash or a move | closed; a crash leaves the stamps, a move leaves them and the record for a minority continuation |
| the raid1 write hole (writes never acknowledged, landed on some legs) | shrunk to the writes in flight at a crash or at the fence; a torn leg beside a clean member is cleaned at the stage, and alone it waits or serves marked (step 2) |
| unstage capture (`ef3a6f48`) | a belt: it refreshes the record early; the stamps already hold what it reads |
| health monitor | a belt, gated on the VolumeAttachment's node (the gate shipped as `eb7c28fd`, 2026-10-30; a failed lookup still records); it copies the stamp's `clock` into `since`, so `T_back` covers the I/O timeout and skew, not the tick (`FLINT_CATCHUP_TBACK_SECS` can drop from 120 toward the timeout) |
| fence-flip mark on the co-located leg | the decree in step 4 above, written only by a stage whose read concluded without that leg |
| per-epoch membership (§5's rule) | stays for the scheduler and the catch-up: the record trails a majority drop by up to one poll, and a leg in that lag is still record-`in_sync` to `plan_cut` and `in_sync_sources`; the cut-time read from the live raid is what makes an epoch's membership exact |
| the source tie | the "attached" conjunct becomes "named in the newest valid stamp at admission": a source whose node died after a clean unstage is still vouched for and its standby admitted; a source the raid dropped is not, and the demotion stands for that case only |
| base floor, forced re-revert | as §5 says: redundant once per-epoch membership is in the record |
| F36c | stays; its writer set becomes the record's `in_sync` legs the read lacks and its gate "the read concludes"; the defer seam closed by step 4; its transient serve-with-risk bound dropped and its timer kept as an alert, serve-with-risk for node-gone and the operator's cut only (step 3) |
| assembly record write | a precondition of serving (rev 7); it allocates `seq` |
| cleanliness | the fence, the assembly write, step 4, and step 2's torn-leg mark, which covers a leg hygiene freed before the first stage attempt |

### Interactions, stated honestly

- **Snapshots and copies carry stamps.** An epoch snapshot includes the
  stamp cluster, and `bdev_lvol_start_shallow_copy` copies the source's
  allocated clusters, so a rebuilt leg's tail holds the source's stamp
  until admission rewrites it; a revert restores an old one. Both are
  benign: membership is a list of uuids, a copied stamp names the
  source's members and never the destination, and a reverted stamp names
  an old generation on a leg the record holds stale, which step 1
  excludes before any stamp is read.
- **Restore.** A clone of a leg's snapshot is one cluster larger than its
  volume and exposes the tail to the single-replica bare-lvol stage; the
  filesystem at LBA 0 ignores capacity it did not format. Re-replication
  (§10-13) writes a fresh assembly stamp.
- **Capacity.** One cluster per leg, allocated on the first stamp;
  CreateVolume sizes lvols at the request plus one cluster;
  ControllerExpandVolume grows by the same rule.
- **Fencing disabled** (`FLINT_NVMF_FENCING=disabled`) voids the majority
  argument along with everything else §3 rests on; the stamp then
  records history faithfully but cannot refuse a zombie.
- **The SPDK bump.** The patch rides the carried series
  (`scripts/spdk-patches.sh`, never hand-edited) beside §7's hot-rejoin
  patch; the superblock code is its model and its regression suite is
  the raid unit tests plus the kind drills below.
- **Maintenance.** A planned roll drains a leg before its node goes
  down: one record round marks it stale under a leased suppression mark
  (`drain_for_maintenance`, replica_sync.rs), taken only when every
  other leg is in sync, and the roller refuses the consumer's own node
  (docs/maintenance-drain-csi-node-roll.md). A drained leg is
  record-stale, so step 1 excludes it and no stage waits for it; after
  the restart the roller clears the mark and the leg rejoins by catch-up
  as a delta, on its own node, not rebuilt elsewhere. A node taken down
  outside the roller is an unplanned loss to the raid: a three-replica
  volume continues on the majority's stamp, a two-replica one on the
  record's confirm, and a stage during the outage concludes by the count
  or by step 2's even-size vouch. Replacement on another node happens
  only when the node is declared gone, from a serving raid, and never
  meets the wait. Step 3's wait is reached only when the leg holding the
  newest generation is unreachable and the attached legs cannot rule out
  a newer one: one leg of three, or at two replicas a confirmed
  continuation on the absent leg, which then truly holds writes nothing
  else has.
- **What it does not do.** It does not version writes, so two members can
  still disagree on a write that was never acknowledged; it does not make
  the record exact for a majority drop, which is why §5's per-epoch rule
  stays; and it does not help a volume whose fence is off.

### Costs

| Path | Cost |
| --- | --- |
| healthy writes and reads | none: no extra I/O, no extra hop |
| one base lost, majority survives | the writes in flight at that instant wait one base-write round trip to each survivor (sub-millisecond on the network the data already crosses); nothing touches the control plane |
| two bases lost at once, or one on a two-replica volume | the same, plus one sync-record write (tens of milliseconds; seconds under API-server contention); an unreachable API server fails those writes at the I/O timeout, by design |
| rebuild copy rate | none: the copy is the same shallow copy of the same epochs |
| space | one lvolstore cluster per leg |
| control plane | N stamp reads per stage; one `bdev_raid_get_bdevs` per second per node holding raids (the monitor already issues it every 60 s); the confirm RPC on a minority continuation |

### Alternatives considered

- **The record shape** (rev 7's first option): the mark in the sync
  record before every degraded acknowledgement. One mechanism, a smaller
  patch (hold, a `get_bdevs` field, a confirm RPC, no on-disk format,
  no reserve, no read RPC, no resize rule), and an exact record for
  everyone, which would retire per-epoch membership's cut-time read as
  well. Its price is the API server in the ack path of every degraded
  write, a burst of record writes when a node holding many legs fails,
  and a data-path dependency on the consumer's API connectivity. Rejected
  by the user for the on-disk shape; it remains the fallback if the
  patch proves larger than §7's.
- **Stock raid superblock**: at LBA 0, which breaks rev 5's bare-lvol
  restore; auto-assembled by examine, the §3 hazard class; and written
  after the quiesce, so not in the ack path anyway. Three reasons, any
  one sufficient.
- **A mark on the consumer node's local disk**: fast and rebuildable, but
  lost with the node, and the node's loss is the case.
- **A write quorum** (rev 7's option (a)): does not close the residual
  without record-before-ack underneath, and costs availability.
- **The stamp without the majority rule**: the zombie hole above.

### Model verdicts (2026-10-30)

FlintCatchupS (flint-e4; `formal/`, on the branch `csi-rebuild-tlaps`
until pushed) is FlintCatchupT rev 15 with the stamp read as above and
one knob per rule of this section, so a control is the same model with
one rule off. Every batch ran under one pinned model md5 that the run
asserts, on tla-mc 0.1.2 until the user moved the box to 0.2.1 at
16:54Z on 2026-10-30 (TLC is the reference; 0.2.1 was validated against
both on the gate that day with the same verdicts and counts; c22, c24
and c26 ran on 0.1.2 throughout, a row of c21, c23 or c25 finished on
0.2.1 says so in its verdict cell, and c27 ran on 0.2.1), and a verdict
is cited with its batch's md5 and nothing else:
c21 = rev 7 `6859c997` (the stamp, the released view and the gated
decree; rev 1's batch c17 was stopped and superseded by it); c22 = rev 8 `ab167ffe` (the expiry
action); c23 = rev 9 `c41c6986` (the even-size vouch); c24 = rev 10
`2a9567c7` (the torn leg, the pre-stamp write counted as the hole); c25
= rev 11 `46c89207` (the torn leg never alone); c26 = rev 12 `8e54566d`
(the torn junk as a data invariant); c27 = rev 12 again, the user's
full-space run at three replicas on a 192-core spot box (2026-10-30,
m7i.48xlarge, 160 workers, 180 GB fingerprint and queue caps, tla-mc
0.2.1; the batch was cut short at the user's word after its three
safety arms, so its expiry, sole-continuation, barrier and four-replica
arms did not run and nothing is claimed for them). Three replicas and two
failures (§5's bound D) unless the row says two replicas. The rules
column names what is on beyond the stamp itself: *count* is step 2's
majority read over attached `in_sync` legs, *view* the released
generation every stamp carries, *decree* step 4's gate, *vouch* step 2's
even-size vouch, *torn* the torn leg's exclusion beside a clean member,
*hole* the pre-stamp write counted as the write hole, *never alone* the
torn leg refusing to serve alone on a vouch, *expiry* the serve-with-risk
action. **HOLD** means the full state space was exhausted; a run cut by
its time cap is "clean to depth N", not a HOLD; and a violated run's
state count says where the trace was found, nothing about the space. The
*hole class* is step 2's write hole, writes in flight when a crash or the
fence lands them on some legs under equal stamps; every other loss the
model found was a stamp gap, and each got a rule. Two of c21's controls,
the majority read off on the lost-mark and crash arms, reached no
verdict (depth 21 and 19), because the model's last healthy member never
fails and nothing fails after the unstage, so those shapes cannot reach
the gap; a control in the wrong configuration rules out nothing, and
they are not cited. A control not killed within its cap rules out
nothing either (c24's three-replica control without the torn leg's
exclusion), and the rule is then cited by the control that was killed,
at two replicas. A probe that is unreachable with a rule off because
the knob's wiring forbids the step, rather than because the design does,
is left out as well (c23's vouch probe with the vouch off, c25's torn
probe without the expiry, and c25's torn-marked HOLD under the full rule
set, whose invariant restates the guard it checks); what carries weight
is their partners, the situation shown to arise, the control that fails
without the rule, and a HOLD on the data beside it.

| Batch | Arm | Rules | Invariant | Verdict | What it shows |
| --- | --- | --- | --- | --- | --- |
| c21 | lost mark, crash, zombie move, with the stamp off (FlintCatchupT rev 15, capture off; control) | none | `Inv_MembersHold` | VIOLATED at depth 8 on all three | the stamp is load-bearing: without it, rev 7's residual in every shape |
| c21 | lost mark, crash, zombie move, with the released view off (control) | count, decree | `Inv_MembersHold` | VIOLATED at depth 13 on all three | the second gap: an unconfirmed minority stamp erases the majority stamp before it |
| c21 | vacuity probes (FlintCatchupSReach `7d5d053b`) | count, view, decree | reachability | all reached: the barrier at depth 2, a majority release at 4, a sole confirm at 5, a stale leg excluded at step 1 at 4, three assemblies at 4, on the lost-mark shape; a zombie's stamp at 2 and a confirmed sole continuation acknowledging on its leg after the move at 7 on the zombie shape | every rule is exercised; the HOLDs are not vacuous |
| c21 | zombie move | count, view, decree | `Inv_NoNewLoss` with admission | VIOLATED at depth 17 (29,309,763 states) | the hole class: the three-replica pre-stamp write, step 2; rev 7 cannot count it legal, rev 10 does (c24, c25) |
| c21 | lost mark; crash | count, view, decree | `Inv_NoNewLoss`, `Inv_MembersHold`, `Inv_RaidHolds` | lost mark: clean to depth 23 (106,466,304 states, bounded, 2 GB caps); crash: clean to depth 22 (100,322,249 states, bounded at the box's hour, tla-mc 0.2.1) | §6's divergence closed for a crash and a lost mark, to the cap at three replicas |
| c21 | the decree as rev 8 first wrote it (control) | count, view, decree on every move | `Inv_NoNewLoss` | VIOLATED at depth 8 (34,827 states) | the third gap's first trace, the strand |
| c21 | the decree sparing the record's last `in_sync` leg, ungated on the read (control) | count, view, decree spare-last | `Inv_NoNewLoss` | VIOLATED at depth 13 (1,724,093 states) | the third gap's second trace: the vouch counted the previous consumer's leg stale by decree |
| c21 | zombie move with the majority read off, three failures allowed (control) | view, decree | `Inv_NoNewLoss` | VIOLATED at depth 12, with two failures | the first gap: one leg of three attached shows an older stamp as newest; its partner is the zombie arm above with the read on, whose shallowest violation is the depth 17 hole, so with the rule on there is no violation of any kind through depth 16 and the gap's two-failure trace lies inside that space |
| c21 | zombie move; lost mark; crash, three failures allowed, the majority read on | count, view, decree | the three above | zombie move: VIOLATED at depth 17, the hole class, the same 18-state trace as the two-failure arm up to the order of two snapshots, the third failure unused; lost mark: clean to depth 23 (152,014,024 states); crash: clean to depth 21 (93,412,376 states); both bounded at the box's hour, 2 GB caps, tla-mc 0.2.1 | the third failure adds no trace within the cap: the zombie arm finds rev 7's hole and nothing new, and the lost mark and crash reach the two-failure arms' depths; bounded, not a full pass (the full three-replica passes are c27's, at two failures) |
| c21 | `Inv_AckStamped` at three replicas (every acknowledgement with every member stamped) | count, view, decree | `Inv_AckStamped` | clean to depth 23 (106,466,304 states), bounded at the box's half hour (1,800 s cap), 2 GB caps, tla-mc 0.2.1 | the barrier claim itself, to the cap: no acknowledgement found with a member unstamped; bounded, not a full pass |
| c21 | zombie move, `Inv_ZombieNoAck` (a zombie never acknowledges, as rev 8 first claimed) | count, view, decree | `Inv_ZombieNoAck` | VIOLATED at depth 7 (at two replicas at depth 5, c23, where one failure leaves a single survivor) | permitted by design, the claim sharpened: a sole continuation the record confirmed before the move keeps acknowledging on its leg, and the next stage waits for that leg (the correction above) |
| c21 | `Inv_SoleOnly` | count, view, decree | a confirmed sole continuation leaves its leg the record's only `in_sync` one | clean to depth 21 (bounded); a full pass at two replicas (c23, below) | step 4's one clause subsumes sparing the last leg |
| c21 | `Reach_ServeOnZ` | count, view, decree | reachability | reached at depth 7 | an assembly after the move serves on the previous consumer's leg once it attaches: the HOLD is a wait, not a defer forever |
| c21 | two legs lost at once with the record unwritable | count, view, decree | `Inv_NoNewLoss`, `Inv_MembersHold`, `Inv_RaidHolds`, `Inv_AckStamped` | clean to depth 24 (120,460,751 states), bounded at the box's hour, 2 GB caps, tla-mc 0.2.1 | the held writes fail and nothing is lost, to the cap; bounded, not a full pass |
| c21 | the member-only read (stricter than the count) | member, view, decree | the three above | zombie move: VIOLATED at depth 17, the hole class, as the count's arm; lost mark: clean to depth 23 (106,261,050 states, bounded at the box's hour); crash: clean to depth 22 (99,941,057 states, bounded) (tla-mc 0.2.1) | as safe as the count where it reaches: the same hole, the same bounded depth; it defers where the count serves |
| c21 | the monitor silent during a hold, the decree ungated | count, view, monitor skips | `Inv_NoNewLoss` | VIOLATED at depth 17, the hole class by another path: the zombie's pre-stamp junk on its own leg is copied to a returning leg by that leg's catch-up and served after a second move; the ungated decree's depth 13 violation is gone (none at depth 16 or less) (tla-mc 0.2.1, two workers) | the strand's root: the skipped marks remove the strand's own trace, but rev 7's hole masks everything past depth 16, so whether the skip closes the third gap on its own is not shown either way; the test is a rev 10 or later arm with the pre-stamp write legal (a follow-up, step 4) |
| c21 | zombie move, the capture late (SZLate2) | count, view, decree | `Inv_NoLateJunk2` | clean to depth 20 (100,707,189 states), bounded at the box's hour, 2 GB caps, tla-mc 0.2.1, two workers; under rev 3 clean to depth 21 (185,940,255 states, 1 h cap), whose depth 20 trace was the hole | the capture's timing: no late junk served within the cap; bounded, not a full pass |
| c21 | liveness | count, view, decree | `Prop_BarrierEnds`: a majority drop's barrier ends; a minority's ends when the record is writable | no verdict: cut at the cap (1,800 s; depth 20, 26,300,370 states) before the liveness check could conclude, so the run claims nothing; its control with the record unwritable fails as expected, a lasso at depth 9 (tla-mc 0.2.1, two workers) | that the barrier is not a hang is shown by no run: a capped liveness check decides nothing, and the control shows only that the property can fail; it needs a full pass, at two replicas or on a larger box |
| c22 | zombie move with the expiry on (the code's 180 s bound) | count, view, decree, expiry | `Inv_NoNewLoss` | VIOLATED at depth 12 (854,198 states) | the bound's cost, step 3: the write on the unreachable leg is lost, permanent and marked |
| c22 | lost mark; crash, with the expiry on | count, view, decree, expiry | `Inv_NoNewLoss` | clean to depth 19 (16,512,065 states; 25,857,285 states), bounded | no loss found under the bound when the leg holding the write is attachable, to the cap |
| c22 | zombie move, lost mark, crash, with the expiry on | count, view, decree, expiry | `Inv_LossMarked` (no acknowledged write absent from every `in_sync` leg while the risk ghost is unset) | zombie move: clean to depth 20 (112,224,732 states, bounded); lost mark: clean to depth 22 (70,484,362 states, bounded, 2 GB caps); crash: clean to depth 22 (106,316,435 states, bounded, 2 GB caps) | the bound never loses silently: a dead node or a person accepted it |
| c22 | identity with rev 7, its own run (three replicas, lost mark, one failure) | count, view, decree | the three of c21 | 7,286,102 states, depth 36, under both `6859c997` and `ab167ffe` | rev 8 with the expiry off is rev 7; the control as rev 8 first wrote the decree fails at depth 8 under rev 8 as well (33,493 states) |
| c22 | `Reach_Risk` | count, view, decree, expiry | reachability | reached at depth 4 | the expiry's serve is exercised |
| c23 | two replicas, lost mark; crash | count, view, decree, vouch | `Inv_NoNewLoss`, `Inv_MembersHold`, `Inv_RaidHolds` | HOLD, 4,730,836 states, depth 37; HOLD, 5,410,162 states, depth 35, the same counts under either form of the vouch, by the read or by the decree, since without a zombie there is no decree | a two-replica stage serves on one leg with the peer absent, record-`in_sync` and no continuation confirmed on it |
| c23 | two replicas, zombie move, with the vouch on; with it off | count, view, decree, vouch | `Inv_NoNewLoss` with admission | VIOLATED at depth 16 with the vouch; at depth 18 without it (1,905,835 states, tla-mc 0.2.1), the same class, the monitor's mark rather than the decree making the leg stale | the pre-stamp write, hole class, which rev 9 cannot count as legal; the vouch neither causes nor cures it; counted legal from c24 |
| c23 | two replicas, zombie move, the completeness invariants | count, view, decree, vouch | `Inv_AckedHeld`, `Inv_MembersHold`, `Inv_RaidHolds` (no acknowledged write lost; admission's cleanliness half left out) | HOLD, 50,779,768 states, depth 39 | acknowledged data is never lost at two replicas under rev 9; what rev 9 lacks is the hole's accounting and the torn leg's cleaning |
| c23 | two replicas, lost mark; crash, the vouch off | count, view, decree | the three above | HOLD, 4,341,608 states, depth 37; HOLD, 4,907,607 states, depth 35 | safe without the vouch; the vouch's larger space is the serves it adds |
| c23 | identity at three replicas, one failure, with and without the vouch | count, view, decree; the same with vouch | the three above | 7,286,102 states, depth 36, both (tla-mc 0.2.1) | at an odd stamp size the vouch's condition is the majority count, arithmetic, not a run; at three replicas the vouch can act only on a stamp of size two, which takes two failures: with one failure no state differs (this row), with two the vouch is on in c27's three-replica arms |
| c23 | two replicas, zombie move, the vouch off, the completeness invariants | count, view, decree | `Inv_AckedHeld`, `Inv_MembersHold`, `Inv_RaidHolds` | HOLD, 38,019,676 states (tla-mc 0.2.1) | acknowledged data is never lost without the vouch either; the vouch's larger space is the serves it adds |
| c23 | two replicas, zombie move; lost mark, `Inv_SoleOnly` | count, view, decree, vouch | `Inv_SoleOnly` | HOLD, 50,779,768 states; HOLD, 4,730,836 states | step 4's one clause holds over the full space at two replicas |
| c23 | two replicas, lost mark, `Inv_AckStamped` (every acknowledgement happens with every member stamped with the raid's membership) | count, view, decree, vouch | `Inv_AckStamped` | HOLD, 4,730,836 states, depth 37 (tla-mc 0.2.1) | the barrier claim re-checked at two replicas under the vouch, the one place a single-leg assembly serves: a vouch-served stage never acknowledges ahead of its stamp |
| c23 | two replicas, zombie move, the vouch by the read only, the completeness invariants | count, view, decree, vouch (read form) | `Inv_AckedHeld`, `Inv_MembersHold`, `Inv_RaidHolds` | HOLD, 44,074,902 states, depth 39 (full-space rerun on tla-mc 0.2.1, 2 workers, 2 GB caps; the first run on 0.1.2 stopped bounded at depth 25) | the one arm that separates "the vouch concludes the read" from "the vouch also writes the decree", judged by verdict, not count (a decree changes the record, so the two spaces differ): the decree half of the vouch adds only the mark on the defer path and is not needed for safety at two replicas, step 4 |
| c23 | `Reach_VouchSpot` with the vouch off, on the lost mark, the zombie move, the crash | count, view, decree | reachability | reached: lost mark at depth 3, zombie move at 1, crash at 3 | the one-of-two situation arises without the vouch, and no stage serves there: the vouch answers a real wait |
| c23 | `Reach_Vouch` with the vouch on | count, view, decree, vouch | reachability | reached: lost mark at depth 4, zombie move at 2, crash at 4 | the one-of-two serve is exercised |
| c24 | two replicas, zombie move | count, view, decree, vouch, torn, hole | `Inv_NoNewLoss` with admission, `Inv_MembersHold`, `Inv_RaidHolds` | HOLD, 33,637,829 states, depth 39 | the post-stamp junk cleaned and the pre-stamp write counted as the hole: nothing else is lost |
| c24 | the same, the capture late | the same | `Inv_NoLateJunk2` | HOLD, 33,637,829 states, depth 39 | no junk admitted after the first cut; the capture's timing changes nothing under the stamp |
| c24 | two replicas, zombie move, the pre-stamp write counted as loss (control) | count, view, decree, vouch, torn | `Inv_NoNewLoss` | VIOLATED at depth 16 | the write that discovers the fence lands before the stamp: the hole, bounded by what was in flight |
| c24 | two replicas, zombie move, the exclusion off (control) | count, view, decree, vouch, hole | `Inv_NoNewLoss` | VIOLATED at depth 17 | the exclusion is load-bearing: a torn leg assembled beside a clean leg serves its post-stamp junk |
| c24 | two replicas, lost mark | count, view, decree, vouch, torn, hole | the three above | HOLD, 4,452,875 states, depth 36 | |
| c24 | two replicas, a crash inside a minority hold (the crash-held torn leg) | the same | the three above | HOLD, 6,424,612 states, depth 35 | the crash-held case is cleaned the same way |
| c24 | two replicas, zombie move, the vouch off | count, view, decree, torn, hole | the three above | HOLD, 22,983,727 states, depth 38 | safe without the vouch too; the smaller space is the wait the vouch removes |
| c24 | two replicas, zombie move, the capture late, the exclusion alone | count, view, decree, vouch, torn | `Inv_NoLateJunk2` | clean to depth 24 (14,789,186 states, bounded) | no junk admitted after the first cut, with the pre-stamp write still counted as loss; the full-knob form is a full pass in c26 |
| c24 | identity, every knob since rev 8 off | count, view, decree | the three of c21 | 7,286,102 states, rev 8's count | the knobs add nothing when off |
| c24 | three replicas, zombie move; and the control without the exclusion | count, view, decree, vouch, torn, hole; the same less torn | the three above | clean to depth 17 (18,609,637 states, bounded), the depth of rev 7's pre-stamp violation; the control was not killed within its cap (22,577,098 states, depth 17) | the three-replica torn leg, bounded; the unkilled control rules out nothing, and the killed control is the two-replica one at depth 17 |
| c24 | probes `Reach_JunkNoStamp`, `Reach_TornMarked`, `Reach_TornServed` | count, view, decree, vouch, torn, hole | reachability | depth 3, 4, 5; a torn leg marked after a crash at 6 and at three replicas at 4 | junk with no held stamp exists (the hole); a torn leg is marked, in every shape; under rev 10 a torn leg served alone on the vouch, the reason for rev 11 |
| c25 | two replicas, zombie move | count, view, decree, vouch, torn, hole, never alone | `Inv_NoNewLoss` with admission, `Inv_MembersHold`, `Inv_RaidHolds` | HOLD, 29,120,225 states, depth 39 (state storage capped at 1 GB after a memory guard cut the first run; the cap moves states, not the result) | the product's claim at two replicas: a stage waits or serves clean, never serves junk alone |
| c25 | the same, the capture late | the same | the same | HOLD, 29,120,225 states | |
| c25 | two replicas, zombie move, never alone off (control) | count, view, decree, vouch, torn, hole | `Inv_TornMarked` (a torn leg serves alone only with the risk ghost set) | VIOLATED at depth 5 | without the rule an ordinary stage serves a torn leg alone on the vouch |
| c25 | `Reach_TornServed` with the expiry | all with expiry | reachability | reached at depth 5, the ghost set | a torn leg is served alone only by the expiry action, node-gone or the operator's cut, and it is marked; the form without the expiry is unreachable by the knob's wiring and is not cited |
| c25 | two replicas, zombie move, with the expiry | all with expiry | `Inv_LossMarked` | HOLD, 31,631,037 states, depth 39 (the run also checked the torn-marked invariant, by construction, not cited) | every loss under the cut is marked |
| c26 | two replicas, zombie move, the capture late | all | `Inv_TornJunkMarked` (while the risk ghost is unset, no member of the live assembly holds a value a fenced writer wrote after its stamp; the write that finds the fence, the hole, is left out on purpose, and a confirmed continuation's acknowledgements are acked data, not junk), with `Inv_NoNewLoss` and `Inv_NoLateJunk2` in the same run | HOLD, 29,140,655 states, depth 39 (20,430 above rev 11's count for the same knobs: the cumulative ghost tells apart states rev 11 merged, nothing in behaviour) | the never-alone rule's data consequence, measured: no torn junk is ever served unmarked |
| c26 | two replicas, zombie move, the never-alone rule off (control) | all less never alone | `Inv_TornJunkMarked` | VIOLATED at depth 7 | without the rule a torn leg serves alone on the vouch with its post-stamp junk, unmarked (leg 1 fails, the move, leg 1 out, the zombie stamps and then writes on leg 2, hygiene frees it, the vouch serves it alone): the row that separates the rule from its absence |
| c26 | two replicas, zombie move, the exclusion off (control) | all less torn | `Inv_TornJunkMarked` | VIOLATED at depth 5 | the torn leg assembled beside a clean leg serves its junk |
| c26 | two replicas, zombie move, with the expiry | all with expiry | `Inv_TornJunkMarked` | HOLD, 31,651,787 states, depth 39 | a torn leg's junk reaches a member only with the risk marker set: under the expiry it is served alone and marked, step 3, as a data invariant |
| c26 | identity, every knob since rev 8 off | count, view, decree | the three of c21 | 7,286,102 states, rev 8's count | the knobs add nothing when off |
| c26 | three replicas, zombie move | all | `Inv_NoNewLoss`, `Inv_MembersHold`, `Inv_RaidHolds`, `Inv_TornJunkMarked` | clean to depth 18 (31,810,913 states, bounded) | one level past the depth at which rev 7 found the pre-stamp write; the three-replica claim is bounded, not a full pass |
| c25 | two replicas, lost mark; crash | all | the three above | HOLD, 3,627,109 states, depth 35; HOLD, 5,326,386 states, depth 35 | the full rule set on the shapes without a zombie |
| c25 | two replicas, zombie move, the vouch off | all less vouch | the three above | HOLD, 20,481,527 states, depth 38 | safe without the vouch under the full rule set; the smaller space is the wait the vouch removes |
| c25 | two replicas, zombie move, with the expiry on | all with expiry | `Inv_NoNewLoss` | HOLD, 31,631,037 states, depth 39 (tla-mc 0.2.1) | the expiry costs no acknowledged write at two replicas: an acknowledgement needs both legs or a confirmed continuation that marks the other stale, so every record-`in_sync` leg holds every acknowledged write; c22's cost is a three-replica effect |
| c25 | identity, every knob since rev 8 off | count, view, decree | the three of c21 | 7,286,102 states, depth 36 (tla-mc 0.2.1), rev 8's count | the knobs add nothing when off |
| c25 | three replicas, zombie move | all | the three above | clean to depth 19 (54,626,909 states, bounded; tla-mc 0.2.1, 2 GB caps) | the three-replica claim under rev 11 is bounded, not a full pass |
| c27 | identity on the 192-core box, every knob since rev 8 off | count, view, decree | the three of c21 | 7,286,102 states, depth 36, the box's count | the model and the checker agree across machines |
| c27 | three replicas, lost mark, two failures | all | `Inv_NoNewLoss`, `Inv_MembersHold`, `Inv_RaidHolds`, `Inv_NoLateJunk2`, `Inv_TornJunkMarked` | HOLD, 1,774,204,193 states, depth 45 (160 workers, 629 s) | the full rule set over the whole three-replica space on the lost mark |
| c27 | three replicas, crash, two failures | all | the five above | HOLD, 2,325,892,812 states, depth 46 (190 workers, 824 s) | the same on the crash |
| c27 | three replicas, zombie move, two failures | all | the five above | no violation to depth 27 (2,429,420,385 distinct states, bounded: cut at the user's word at 1,186 s with the space still growing by half per level and the queue spilled to disk) | the three-replica zombie space is too large to exhaust; checked ten levels past the deepest three-replica trace the model has found (depth 17, c21), bounded, not a full pass |

What the table claims. At two replicas every arm under the full rule set
is a full pass, and each safety rule has a control that fails without it
in the same configuration, so the HOLD is attributable; the vouch's
control shows the wait it removes, not a loss. The never-alone rule
needed its own data invariant to be attributable: the acked-data HOLDs
of c24 and c25 do not need it, so the torn-junk invariant of rev 12 is
what separates it from its absence, holding over the full space with
the rule and failing at depth 7 without it (c26). At three replicas
the full rule set holds over the whole state space for the lost mark
and the crash (c27: 1,774,204,193 states to depth 45 and 2,325,892,812
to depth 46, rev 12's five invariants in one run, two failures
allowed); with a zombie the space is too large to exhaust and is
checked to depth 27 without a violation (c27, 2,429,420,385 states, cut
at the user's word while still growing), ten levels past the deepest
three-replica trace the model has found (rev 7's pre-stamp hole at
depth 17, c21; the box's bounded zombie arms reached depths 17, 19 and
18 under revs 10, 11 and 12, c24, c25, c26). Batch c21 closed on
2026-10-31 with every remaining rev 7 three-replica arm bounded at the
box's cap (an hour; half an hour for the acknowledgement stamp), at
depths 20 to 24, three failures allowed adding no trace within that
cap, and its liveness arm at no verdict, so the barrier's
termination is claimed by no run. No three-replica control
was killed within its cap (c24) and c27 ran none, so the attribution of
each rule rests on the two-replica controls; the three-replica rows
show the rules holding together, not which rule carries which HOLD.
Every batch here, c21 to c27, ran trust with the fixed cut
(`CutStampsAll` FALSE in every cfg, checked by flint-e4), so §5's cut
caveat on c15 does not apply to these rows. The
hole is legal by definition in the rows that
count it so, and its bound (in flight at the fence or at a crash) is a
property of the raid, not of the model. The stamp model has no TLAPS
proof; the proved rule is §5's trust rule (FlintCatchupT), whose
fresh-cache record run waits for a quiet box, and a proof of the stamp
rule is in the gate under *Delivery order*. The model
files are local to flint-e4's branch until the user says push. Nothing
in this section is built.

Open with flint-e4:

- [ ] c27's arms the cut left unrun (the expiry form, the two
      sole-continuation arms, the barrier at three replicas, four
      replicas), at the user's word.
- [ ] The TLAPS record run on a quiet box; then the branch pushed by PR at
      the user's word.

### Code asks

flint-91, once the gate under *Delivery order* is passed, scoped here
and decided by the user:

- [ ] The raid1 patch in the carried series: the reserve, the stamp
      write, the barrier, the majority release, the `get_bdevs` field and
      the two RPCs, with raid unit tests for the barrier (a completion is
      never delivered before its stamp) and the chain (a survivor whose
      stamp write fails is failed).
- [ ] Driver: lvols one cluster larger; `bdev_raid_create` with `seq`
      from the assembly write; the stage's stamp read and steps 1 to 5;
      the tie's conjunct; F36c's writer set and gate from the read.
- [ ] Driver: the torn-leg mark in the assembly write (step 2), with its
      reason, and the catch-up's revert of a torn leg to its last trusted
      epoch before any copy.
- [ ] Driver, the freshness gate under the stamp (step 3): the transient
      deadline becomes the `AssemblyWaiting` alert naming the legs and
      the cut; `ServeWithRisk` for node-gone and the operator's cut only;
      freshness_gate.rs's `evaluate` returns `Defer` with the deadline
      passed and a transient leg, and its unit tests follow.
- [ ] Node agent: the one-second `get_bdevs` poll on held raids, the
      gated record write, the confirm; the monitor copying `clock` into
      `since`.
- [ ] Kind drills: cut one leg's path under a write stream and read the
      survivors' stamps before the write's acknowledgement is observed;
      cut two paths and confirm the write completes only after the record
      shows the continuation; a fenced old consumer's writes never
      acknowledge; crash the consumer inside a failure window and restage.
- [ ] The resize move in the raid patch: the reserve off in
      `raid1_resize`; the stamp copied to the new tail with `prev`, the
      old region zeroed, the move finished at `bdev_raid_create` and at
      a base's add when `prev` is set; the driver's move through the
      RPCs for a leg under no raid; raid unit tests for a crash at each
      window and for a shorter base's add.
- [ ] Pins: the two premises of §5's rev 7 subsection (both observed once
      on gate1613, 2026-10-30, from uncommitted scripts; the pin is the
      committed cluster test that runs them at every spdk-tgt bump, with
      the kill arm restored and the rejoined leg's region scanned), and
      the barrier ordering as a unit test that an SPDK bump cannot pass
      silently.

### Delivery order

1. The model and the proofs, complete (flint-e4). **The user's gate
   (2026-10-31): no work on the raid1 patch, the driver or the node agent
   starts until every item below is done and cited in the table above
   under its md5.** Each item is a checkbox; flint-e4 confirms the list
   and estimates it, and anything needing a box beyond the Linux one is
   a plan and a cost for the user to approve first.
   flint-e4 reviewed the list on 2026-10-31; the estimates are theirs.
   - [ ] The trust rule's TLAPS record run on a fresh cache, asserting
         every input's md5, with its thirteen controls (§5). Reported
         done (all 7,673 proved; twelve controls in); ticked when its
         results section is cited in §5.
   - [ ] A TLAPS proof of the stamp rule itself, on FlintCatchupS. None
         exists: the proved rules are FlintCatchup and FlintCatchupT. To
         add anything over the trust proof it must run with the crash
         and the zombie move on, in c27's setting (rev 12, trust, the
         fixed cut), for `Inv_NoNewLoss`, `Inv_MembersHold`,
         `Inv_RaidHolds` and `Inv_AckStamped`. Estimate, low confidence:
         one to two days to find an inductive invariant and check it at
         two and three replicas, then four to eight days of proof; the
         stamps, the released view, the majority reasoning, a second
         writer and torn legs are all new over the trust proof, which
         took a day on top of FlintCatchup's. Cached passes fit the box;
         a fresh-cache record likely does not (the trust record peaked
         at 25.4 GB of 30) and would need one 64 GB spot instance, plan
         and cost first. A middle option, two to three days: a proof of
         the read rule alone, abstracted (majority-or-confirmed stamps
         under the released view name members holding every
         acknowledged write, for any replica count and generation),
         which removes the bound where it matters most, with model
         checking keeping the rest.
   - [ ] The three-replica zombie arm as a full pass (c27 bounded it at
         depth 27), or the user's explicit acceptance of the bound. Not
         feasible on the box: depth 27 was 2,429,420,385 distinct
         states, about 20 GB of fingerprints, growing by half per level,
         with no honest total (ten billion or more). First try SYMMETRY
         over the replica ids, after checking the model is symmetric in
         them, validated on the two-replica arms and the two full
         three-replica passes; if still too large, the acceptance names
         its evidence: the two-replica zombie full, the three-replica
         lost mark and crash full, the zombie to depth 27. The stamp
         proof above supersedes this item.
   - [ ] Liveness: `Prop_BarrierEnds` as a full pass with its control
         (c21's arm reached no verdict). Two replicas first, on rev 12,
         for the lost mark, the crash and the zombie move, plus the
         record-down control that must lasso: box work, a few hours per
         arm. Three replicas has no feasible full pass (the safety
         spaces alone are 1.8 to 2.3 billion states): bounded only, or
         the user accepts two-replica liveness. The premise to cite:
         timeouts fire, which is the P4 fix (`c7e5d2fe`).
   - [ ] `Inv_AckStamped` at three replicas as a full pass (c21 bounded
         it at depth 23; c27's arm did not run). Cheapest: add it to the
         two full three-replica arms and rerun both on one m7i.48xlarge
         spot instance, about 25 minutes of checking; not the box, where
         that space is a day and may not fit.
   - [ ] The c27 arms the cut left unrun: the expiry form, the two
         sole-continuation arms, four replicas. The lost-mark sole arm
         shares the full arm's constants and joins the session above;
         the three zombie arms (the expiry form, the zombie sole arm,
         four replicas) run bounded at one-hour caps in the same
         session, four to five hours of instance in all, priced before
         it is asked for.
   - [ ] The resize rule modelled (the model has fixed-size legs and no
         row covers it), or resize excluded from the patch's first cut
         by the user. A resize arm is one to two days to build (per-leg
         size, the stamp's tail offset moving with the lvol, the raid
         following the lvol, the two windows, the short leg,
         `raid1_resize`'s minimum over bases less the reserve), then
         hours on the box at two replicas full and three bounded. If
         excluded, the code refuses expansion of a stamped volume, and
         that refusal ships with its callers.
   - [ ] The monitor-skips question classed by a rev 10 or later arm
         with the pre-stamp write legal (c21's arm neither confirms nor
         refutes it); a follow-up unless the patch adopts the ungated
         decree with the monitor skipping held legs in place of §12's
         gated decree, which is what the rows cover. About an hour on
         the box at two replicas, then bounded at three.
   - [ ] The model files pushed by PR at the user's word, so every md5
         the table cites is on main: FlintCatchupT with its modules,
         inductive invariant, proof and cfgs; FlintCatchupS for each
         cited revision, rev 7 to rev 12, each md5 reproduced or copied
         from its batch directory and verified; the reachability files,
         cfgs, results without generated logs, and the table script;
         FlintCatchup's proof. About half a day to assemble and verify.
   The premises' pins are code and sit in step 4, not here. The
   decisions in this list are the user's: whether to buy the stamp
   proof or the three-replica zombie run, whether two-replica liveness
   is accepted, whether resize is in the first cut, and every EC2 run.
2. The raid1 patch with its unit tests, on the box.
3. The driver and node-agent changes behind one flag, default off until
   the kind drills pass.
4. The kind drills and the pins.
5. Then retire the interim guards in §5's order: the floor and the forced
   re-revert once per-epoch membership is in the record, the tie's
   demotion once admission reads stamps.
