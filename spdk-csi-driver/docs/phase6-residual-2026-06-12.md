# Phase 6 — Tier-1 residual measurement and cutover validation

**Date:** 2026-06-12 (single session, follows `e2e-campaign-2026-06-12.md`)
**Cluster:** trove-provisioned `runh`, 4× AWS spot workers + 1 CP (one worker
replaced mid-session after a spot retirement), Kubernetes v1.34.9, chart
`flint-csi-driver-chart:1.2.0`, driver `:latest` dev builds from `main`
(bounded umount + orphan sweep + pin-until-admission + pin-advance).
**Knobs:** `FLINT_EPOCH_SCHEDULER=enabled` (30 s), `FLINT_CATCHUP=enabled`,
`FLINT_CUTOVER=enabled` — cutover's first cluster run ever.
**Workload:** 2-replica RWO volume (`numReplicas: 2` SC), Deployment with a
2 s fsync writer, PV annotated `flint.csi.storage.io/rejoin-bounce: enabled`.
**Method:** kill the replica-hosting flint node pod (spdk-tgt dies with it),
track the PV sync record + workload pods at 10–20 s granularity, reconstruct
from controller logs and PV events.

## Headline numbers

The fully autonomous heal (round 3, raid live on the consumer, bounce
rescheduled cross-node):

| T+ | event |
|---|---|
| 0:00 | replica leg killed |
| 0:57 | health monitor marks `stale` (60 s tick) |
| 1:17 | catch-up → `standby`, pin at the revert base |
| ~1:50 | chase advances; pin advances with the mark; epochs hold at K=6 |
| 2:09 | cutover bounces the workload (52 s after standby-ready) |
| ~2:50 | reassembly on the new node; fenced final delta; admitted **no rebuild** |
| 2:58 | `in_sync`, pin released, epochs settle at K |

**Tier-1 residual, effective bounce: ~3 minutes**, composed almost entirely
of detection latency (three independent 60 s ticks: health monitor,
catch-up, cutover planner) — data movement is seconds at this volume size.
The floor is tunable: tighter ticks buy a sub-minute residual at the cost of
control-plane chatter.

**Tier-1 residual, ineffective bounce: unbounded.** When the scheduler
re-places the workload pod on the same node (round 2: a single-pod
Deployment with no spread constraints — a coin flip or worse), kubelet
reuses the staged raid, no reassembly happens, no admission runs, and the
standby chases indefinitely. The verifier emits `CutoverIneffective` at the
900 s cooldown (validated live, correct diagnosis text) and retries — same
odds each round. Round 2 needed a manual cordon (standing in for the §6
scheduling escalation, deliberately unimplemented) before the retry bounce
admitted in 44 s. **The residual distribution is bimodal: ~3 min or
cooldown-multiples, decided by the scheduler.**

## What was validated live (first time for each)

- **Cutover RWO bounce** (`rejoin-bounce` annotation): plans within one tick
  of standby-readiness, bounces only the claim's pods, and the post-bounce
  verification correctly distinguishes success from no-op.
- **`CutoverIneffective`** path end-to-end, including retry eligibility.
- **Admission at reassembly** on three distinct topologies: remote node,
  the standby's own node (local attach + `clear_head_sb`), and the
  fresh-replacement node — all "in_sync, no rebuild".
- **Pin lifecycle (both 2026-06-12 fixes)**: held through standby (no GC
  grind — zero retention warnings over a 15-minute standby soak), advanced
  with the chase mark (epochs never exceeded K+1; before the advance fix
  the same scenario grew 23 epochs in 18 minutes), released at admission.
- **Bounded unstage umount (campaign bug #4 fix)** against a genuinely dead
  mount during recovery — returned promptly, node plugin stayed live,
  restage completed.
- **Data integrity** across five failure/restage cycles: every acknowledged
  write before a data-path death survived; writes EIO'd into a dead mount
  were lost (expected; the writer saw the errors).

## Bugs found (the phase's yield)

1. **Consumer-side spdk-tgt restart leaves the workload on EIO and the
   control plane blind.** *(Layers 0+1 fixed and cluster-validated later
   the same day — see "Consumer-blindness fix" below.)* A node-pod roll on the consumer destroys the
   assembled raid and the volume's frontend; the mounted filesystem returns
   I/O errors. Nothing detects it: the health monitor's stale predicate
   requires an *online raid missing a base* — with no raid at all it never
   fires. The record stayed `in_sync` and the epoch scheduler kept cutting
   snapshots of the frozen image for ~15 minutes until manually noticed;
   recovery was a workload pod bounce (restage rebuilt the raid; all
   pre-death data intact). Needed: the node agent (or health monitor)
   should treat *attached volume with no raid bdev on the attachment's
   node* as degraded — emit the event, and plausibly feed the same bounce
   machinery cutover uses. Until then, every consumer-node driver upgrade
   is a silent outage for its staged volumes.
2. **Dashboard backend crash-loops when a registered node is unreachable**
   (spot retirement): disk-fetch and orphan-scan paths hang on
   unreachable-host timeouts long enough that the liveness probe kills the
   backend repeatedly. Needs bounded per-node timeouts (the parallel fetch
   has them; the orphan scan does not).
3. **Dashboard frontend silently falls back to mock data** when the backend
   is down (502) — the same fallback family `c20711e` removed for disk
   status survives at the top-level dashboard fetch. An outage rendered as
   healthy-looking fake data is worse than an error page.

## Scale assessment (hundreds of volumes)

The per-volume machinery is sound at scale: independent state machines in
PV annotations, idempotent/convergent operations, resumable copies. What is
missing is flow control above it, none of which Tier 2 changes:

- **No global catch-up concurrency cap**: a returned node hosting replicas
  of N volumes triggers N concurrent bulk copies onto one disk.
- **Bounce coordination**: many ready standbys → many workload bounces in a
  window; ineffective bounces retry forever at scale (the bimodal residual
  multiplied); the scheduling escalation stops being optional.
- **API-server load**: node-agent reconcile (and the orphan sweep) full-list
  PVs every 60 s per node; the epoch scheduler loop already showed cadence
  stretch (60 s effective vs 30 s configured) at single-volume scale.
- **Fleet observability**: event timelines don't aggregate; lag/pin-age/
  epoch-count metrics (§6, deferred) become prerequisites — an epoch-list
  length metric would have caught the pin-advance bug by itself.

## Tier-2 decision input (§9-6 → §9-7)

Tier 1 with an effective bounce: ~3 min residual, one workload restart per
heal. Tier 1's failure mode: scheduler-dependent unbounded residual, fixable
with scheduling escalation at the cost of more cutover machinery. Tier 2
(`skip_rebuild` hot rejoin) deletes the bounce apparatus entirely — no
workload restart, scheduler-independent, residual = detection ticks + final
delta + a quiesce measured in metadata ops. The catch-up/epoch machinery is
identical under both; Tier 2 replaces only the admission transport.

## Consumer-blindness fix (landed same day, layers 0+1 of 4)

Designing the fix surfaced a deeper bug and a layered plan. An in-place
repair experiment (hand-rebuilding the consumer raid + export after a
lone spdk-tgt kill) failed productively, teaching three things: disk
attach ran only at DRIVER-container startup, so a lone spdk-tgt restart
(liveness kill, OOM) bricked the node's entire storage — replicas
included — until the whole pod was recreated (5.5 min observed just to
reload, and only with help); a PARTIAL export rebuild is worse than none
(a listener over a namespace-less subsystem makes the kernel initiator
conclude the namespace is deleted and kill the device — listener must go
last, which the convergent export module already orders); and the
namespace-identity question (does the kernel reattach a rebuilt
namespace with a new UUID?) remains open, needing deterministic NGUIDs
pinned at stage before it can be retested.

- **Layer 0 — storage-baseline reconcile (landed):** the 30 s discovery
  loop detects the disk-count collapse and re-runs discovery with
  auto-recovery, re-attaching initialized disks and reloading the
  lvstore; reconcile re-exports on its next tick. Validated live: lone
  spdk-tgt kill → baseline recovered in **50 s** (previously: bricked
  indefinitely).
- **Layer 1 — detection (landed):** the 60 s monitor tick flags any
  volume ATTACHED to this node whose raid bdev is missing — the case
  the health monitor's stale predicate (online-raid-missing-a-base)
  cannot see. Three consecutive strikes (rides out in-flight stages) →
  `flint.csi.storage.io/data-path-lost: <node>` PV annotation +
  `VolumeDataPathLost` Warning with the remediation in the message; the
  flagging node clears it (+`VolumeDataPathRestored`) when the raid
  returns or the attachment leaves. Validated live: flag at T+3m07s,
  bounce → restage → flag self-cleared, data intact (the workload
  STALLED rather than EIO'd this time — layer 0 restored the target
  fast enough that the kernel kept queueing within its reconnect
  window).
- **Layer 2 — in-place repair (landed same day):** when detection
  confirms a loss (3 strikes), the agent rebuilds the raid (the same
  sync-record-aware assembly NodeStage uses) and re-exports the
  loopback subsystem via the convergent module — same NQN, stable
  serial, and a **pinned namespace identity** (`stable_ns_identity`:
  deterministic UUID/NGUID from the volume id, set at stage and at
  repair) so the kernel initiator revalidates the namespace and
  reattaches. Guards: refuses ublk frontends (device node dies with
  spdk; restage only), refuses volumes kubelet does not have staged
  here (`vol_data.json` check — a lingering mid-detach VA must not
  spawn a zombie raid; observed live during a handover before the
  guard existed, self-resolved by the in-flight unstage), and tears
  its raid back down if the attachment leaves mid-repair. Detection
  flags only when repair fails, and keeps retrying every tick.
  **Validated end-to-end with forced direct I/O**: lone spdk-tgt kill
  → repair at T+2m23s → 16 KiB `O_DIRECT` write + synced append/read-
  back through the rebuilt path succeeded in the SAME pod, zero
  restarts — a ~3-minute I/O stall the workload slept through.
  Validation lessons, recorded for posterity: (a) cached reads lie —
  two earlier "successful" rounds were page-cache illusions over a
  dead mount; only direct I/O or synced write-then-read-back proves a
  data path; (b) **the pinning migration is itself a hazard for
  volumes staged pre-pinning**: re-exporting a namespace with changed
  identity under a still-connected kernel controller makes the kernel
  delete the old device node (journal abort under any mount on it)
  and mint a new one — each existing volume needs one full
  detach/restage cycle (or a workload bounce) to cross onto pinned
  identity safely.
- **Layer 3 — bounce fallback (landed same day):** the cutover loop
  consumes the annotation for what repair can't reach (ublk frontends,
  aborted filesystems, expired reconnect windows; also reachable via
  the `FLINT_DATA_PATH_REPAIR=disabled` escape hatch). The planner's
  data-path branch bypasses the standby/lag gates — the bounce IS the
  remediation, restage rebuilds from in-sync replicas — with the same
  policy split (RWX: NFS-pod bounce; RWO: `rejoin-bounce` opt-in,
  otherwise surfaced for the operator). A 90 s debounce keeps a
  transient repair failure from costing a bounce. Verification judges
  these attempts by the annotation clearing (the agent clears it when
  the raid is back), with the cooldown and a distinct
  `CutoverIneffective` diagnosis. Validated live with repair disabled:
  flag at T+2m33s → bounce → same-node reuse defeated it →
  `CutoverIneffective` (correct message) → cordon + retry bounce →
  cross-node restage → flag cleared 16 s later → `CutoverSucceeded`
  ("data path restored") → direct-I/O probe green. **The same-node
  reschedule race defeats data-path bounces exactly as it defeats
  admission bounces** — closed the same day by the scheduling
  escalation below.

## Scheduling escalation (landed same day)

Every bounce now applies a self-expiring NoSchedule taint
(`flint.csi.storage.io/bounce`) to the bounced workload's node before
deleting pods, so the replacement cannot reuse the dead/stale staged
volume. A taint rather than cordon (operator cordon state is never
touched) or pod anti-affinity (RWO replacements come from the
workload's own controller template, which flint cannot mutate). The
application time is encoded in the taint value, so expiry
(`FLINT_CUTOVER_TAINT_SECS`, default 120 s) survives controller
restarts — the tick sweeps expired taints even with escalation
disabled (`FLINT_CUTOVER_ESCALATION=disabled`). On a cluster with no
alternative node the taint still works: it outlives kubelet's unstage,
so even a same-node landing must restage. Validated live with repair
disabled and NO manual cordon: flag → taint at the debounce boundary →
replacement steered off-node → restage → flag cleared **on the first
bounce attempt** → taint auto-expired and swept. The bimodal residual
is closed: bounces are now deterministic for both admission and
data-path cases. As an unplanned bonus, the post-validation DaemonSet
roll restarted the consumer's spdk-tgt mid-probe: a 16 KiB `O_DIRECT`
write stalled 148 s, the re-enabled in-place repair rebuilt the path,
and the write completed — no error, no restart — the full layer stack
riding through a routine driver upgrade.

**Recommendation:** implement the cheap Tier-1 hardening now (consumer
blindness layers 2+3 above; pod anti-affinity hint or cordon-lite escalation on
`CutoverIneffective`), and proceed with the Tier-2 patch evaluation —
the bimodal residual and the per-heal workload restart are structural to
Tier 1, both disappear with the one verified primitive, and the §7 patch
shape is already traced. RWX (NFS-pod bounce, detach-awaited — should not
have the same-node mode) remains to be validated before the RWX story
rides on cutover.

## RWX cutover round (run same day) — four composable bugs, all fixed

The first-ever live RWX cutover exercise (2-replica `flint-r2` RWX volume,
NFSv4.1 hard mount, synced write/read-back writer, replica degraded by a
tight spdk-tgt kill loop on the remote-leg node) found the bounce machinery
structurally broken for RWX. Root cause of everything: an RWX volume exists
under **three identities** — the user PV (`pvc-X`), the synthetic backing PV
(name `flint-nfs-pv-pvc-X`, volumeHandle `nfs-server-pvc-X`), and the
volumeHandle itself — and different components derived names from different
ones. Observed live, in firing order:

1. **Zombie raid at unstage (the headline).** `NodeUnstageVolume` strips the
   `nfs-server-` prefix before calling `teardown_volume_spdk_state`, but
   stage names every SPDK object from the full handle. Teardown no-ops on
   names that don't exist; the raid survives with its exclusive claim on
   the local replica lvol, and every later export of that replica fails
   `-32602` — a cross-node restage can never assemble. RWO never hit this
   (no prefix; and in the phase-6 RWO rounds the failure itself had already
   destroyed the old raid). Each bounce strands a new zombie on the departed
   node. **Fix:** teardown by full `volume_id`; plus the reconcile's phantom
   raid deletion now keys on the volumeHandle, so a stray zombie is also
   swept within a tick (defense in depth).
2. **Permanent data-path false positive on RWX PVs.** RWX consumers
   NFS-mount the volume: the workload node holds a VolumeAttachment but by
   design no raid, so layer-1 detection flags `data-path-lost` ~3 min after
   any RWX workload attaches, and the flag can never clear (the expected
   raid name never exists anywhere). Layer 3 then bounces the NFS pod every
   `FLINT_CUTOVER_COOLDOWN_SECS` forever — combined with bug 1, a permanent
   ping-pong with the volume unavailable throughout. **Fix:** detection
   skips RWX PVs; the synthetic backing PV (whose handle names the real
   raid) keeps full coverage on the NFS server's node, and the cutover tick
   now folds the backing PV's flag into the parent volume's view — RWX
   keeps its layer-3 fallback, with verification that actually converges.
3. **Dual control streams corrupting the snapshot lineage.** The epoch
   scheduler, catch-up, and cutover iterate "flint multi-replica PVs" by PV
   name; the synthetic backing PV carries the same replica attributes, so a
   second epoch family (`epoch-flint-nfs-pv-pvc-X-N`) and a second sync
   record ran against the same lvols. The interleaved snapshots broke the
   real record's chase (`lineage element …-4 missing — broken chain`),
   which blocked standby admission (`N epochs behind (limit 4)`) — restage
   then refused assembly even after the legs were unblocked. The export
   reconcile had the same disease: replica exports attempted under three
   NQN aliases, squatting the lvol under the wrong subsystem and starving
   the canonical one (and the orphan sweep deleted the canonical subsystem
   as PV-less while protecting the alias squatters). **Fix:** orchestrators
   skip the backing PV (`replica_sync::nfs_backing_parent`); the reconcile
   skips RWX PVs and derives all SPDK names from the volumeHandle.
4. **EBADHANDLE after every NFS server bounce.** With the data path fully
   restored, every client write failed with errno 521: the server's
   `FileHandleManager` embeds a boot-time-nanos instance id in every file
   handle and rejects foreign ids — deliberately invalidating all handles
   on restart. NFSv4.1 session recovery succeeded; handle resolution never
   did. Permanent client-side failure until remount (pod restart). The
   handles are otherwise self-describing (path embedded), and the
   `PNFS_INSTANCE_ID` override already existed for pNFS. **Fix:** the NFS
   server pod now pins `PNFS_INSTANCE_ID` to a stable per-volume hash
   (`stable_nfs_instance_id`), so any incarnation of the server resolves
   any predecessor's handles. Pre-fix RWX server pods mint volatile ids
   until they are recreated once (same migration shape as the pinned
   namespace identity note above).

Manual surgery used during the round (for the record): delete the zombie
raid, move the lvol namespace into the canonical subsystem, delete the six
alias epoch snapshots on both replicas, then race the reconcile's re-squat
until the stage's raid create won the claim. None of it is needed
post-fix.

What the round validated despite the bugs: the bounce taint landed on the
right node both times and auto-expired; the off-node reschedule worked;
`await_detached` + capture/recreate of the bare NFS pod worked;
catch-up→standby on the RWX volume's record converged once the lineage was
clean (chase ~60 s/epoch tick); admission-on-parity marked a fully
caught-up standby in_sync while the volume was quiesced; and the RWO canary
(phase6-writer) rode through every spdk-tgt kill via layer-2 in-place
repair with zero restarts — the RWO stack was never disturbed by any of
the RWX chaos.

### RWX rounds 2-3 (post-fix validation, same day)

With the four fixes deployed, two more injection rounds ran. Round 3 (all
components post-fix) delivered the first clean RWX cutover chain end to
end: replica stale 75 s after injection → catch-up → standby at T0+4 min →
**standby-gate `BounceNfsPod` decision** (no data-path flag involved) →
taint on the NFS pod's node → replacement steered to the standby's node →
restage admitted the standby (`CutoverSucceeded`, both replicas in_sync) —
the deciding/steering/admission machinery is validated for RWX. The
handle-stability fix also validated hard: a writer blocked 12 minutes on
its hard mount resumed through a server-pod replacement onto a different
node with zero restarts, on handles minted before the bounce (the 1.2.0
server binary honors `PNFS_INSTANCE_ID`, so the fix is effective without
an image bump; NFS pods pick up `:latest` — and the BadHandle→STALE
defense-in-depth — at their next fresh creation since `NFS_IMAGE_TAG` is
now set).

Two more defects surfaced and one was fixed on the spot:

- **RWX-consumer unstage detached live raid legs (fixed).** The catch-up's
  replica copy exports are named under the parent PV identity
  (`…:volume:<pv>_N`), and a restage can attach raid legs through them.
  An RWX workload pod's NodeUnstage on the same node as the volume's raid
  consumer ran the SPDK teardown for that same PV identity — its
  per-replica controller sweep detached the raid's live remote leg within
  seconds of a scale-down (EIO on the writer, leg → stale). RWX staging is
  an NFS mount; its unstage is now unmount-only (`main.rs`,
  `is_rwx_nfs_stage` via findmnt fstype).
- **NFSv4 open-state recovery is the remaining RWX production blocker
  (open).** After a server replacement, a client with a pre-bounce open
  resumed issuing writes that were acked locally but never landed in the
  file (writeback against dead open state; `sync(2)` hides the error, the
  app-level read-back caught it). File handles survive now; open/lock
  state does not — the in-memory `StateManager` + 90 s allow-all grace is
  insufficient for write-holding clients. Required as already traced in
  the design doc §cutover-opportunities: wire the SQLite state backend
  with its DB on the exported volume, and run the final delta before
  deleting the old pod. Until then, RWX cutover bounces are transparent
  only to clients without dirty open state (read-mostly, or
  open-write-close patterns with fsync verification).

Operational notes from the rounds: (a) ~5 rapid spdk-tgt sidecar
crash-restarts wedged kubelet's new-pod admission on that node (existing
pods, exec, and heartbeats unaffected; `systemctl restart kubelet` via SSM
healed it; a node reboot was not authorized for the rolesanywhere role).
(b) A pre-fix `data-path-lost` flag on an RWX PV is orphaned by the new
agents (they skip RWX PVs entirely) and held cutover verification open
until cleared — the cutover tick now clears such stale flags itself.
(c) Deleting a bare NFS server pod while its volume stays attached leaves
no recreation path until the next ControllerPublish — worth a controller
reconcile eventually.
