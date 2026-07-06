# pNFS durable-DS plan — lvol-backed data servers + fleet operations

**Goal**: graduate flint-pNFS from the explicitly-ephemeral scratch tier
(the 2026-07-05 durability decision in `pnfs-performance-plan.md` Phase 4)
to durable, k8s-native shared storage: DS data survives node loss and pod
rescheduling, and the operations a real fleet needs (DS replacement, MDS
restart, eventually drain and MDS failover) are drilled, not assumed.

**Why this matters for the CSI driver's use cases**: today pNFS honestly
serves only reconstructible data (dataset cache, dataloader staging,
shuffle/scratch). Durable DSes extend the RWX story to data of record —
Spark/analytics warehouse output committed by rename, ML checkpoints and
model registries, HDFS-migration targets where flint replaces the
NameNode/DataNode pair with a standards-based MDS/DS pair, and generally
any many-pod shared-filesystem workload that currently forces a choice
between single-server NFS bandwidth and S3 semantics. The durable tier is
what makes "HDFS-shaped storage on your own cluster, standard kernel
client, bandwidth linear in DS count (ADR 0004: 6.02× read, 4.00× write
at N=4)" a claimable product surface instead of a bench result.

**Why this is cheaper than it sounds**: the hard machinery exists on both
sides. Block side: Tier-2-validated 3-replica raid1 lvols with
incremental rebuild, hot rejoin, and fenced-delta admission — a DS
writing to ext4-on-a-flint-PVC inherits replica-failure handling
*underneath* the pNFS layer, which is why FFL mirroring (production-
readiness "Phase C") stays unbuilt. pNFS side: deviceid is a stable hash
of the `device_id` string (`mds/device.rs::generate_binary_id`), DS
config already env-expands `device_id` (`pnfs/config.rs:512`),
stale-heartbeat → `recall_layouts_for_device` is wired and drilled
(`mds/server.rs:837,909`), the write verifier is boot-derived (correct
retransmit semantics on DS restart), and the sqlite backend persists
clients/sessions/stateids/locks/layouts plus a stable `server_id`.

**Scope guarantee**: same modularity discipline as ADR 0001 — code
changes live under `src/pnfs/`, `src/nfs/`, `src/state_backend/`, and
the chart. The SPDK block path is consumed as-is (DS pods are ordinary
PVC consumers); nothing in the block path imports pNFS code.

---

## Phase 0 — per-file placement stability (P1, prerequisite for everything)

### The finding (2026-07-06 investigation)

`LayoutManager::generate_layout` (`mds/layout.rs:330`) builds each
layout's stripe map from `device_registry.list_active()` **at LAYOUTGET
time**, and `list_active()` (`mds/device.rs:146`) iterates a DashMap
with **no ordering guarantee and no persistence**. There is no per-file
placement record anywhere: a file's stripe→DS mapping is whatever the
active-device list happened to be, in whatever order the map iterated,
when that particular layout was granted.

Consequences, in increasing severity:

1. **Adding a DS re-maps every existing file.** `num_devices` changes,
   so `(offset / stripe_size) % num_devices` sends readers to the wrong
   DS for data written under the old map. Silent wrong-data, not an
   error.
2. **Removing (or losing) a DS does the same** — today's DS-death drill
   passed because the DS came *back* with the same identity; a
   permanently smaller fleet re-maps survivors' stripes.
3. **Even a stable fleet is exposed**: DashMap iteration order is not
   contractual. A re-registration (DS pod reschedule — the *normal case*
   this milestone creates) or an MDS restart can permute the device
   list, flipping the stripe map for files whose layouts get re-granted.

ADR 0004 and every drill to date ran a fixed DS set registered once in
a stable order — the hole was never crossed, which is exactly why it
must be closed before DS pods start moving.

### What to build

- **Placement table** in the sqlite backend (new table rides the
  existing schema-batch `CREATE TABLE IF NOT EXISTS` migration path):
  `file_placement(file_id PRIMARY KEY, stripe_size, device_ids TEXT /*
  ordered JSON array */, created_at)`. Written once at first LAYOUTGET
  for a file; every subsequent LAYOUTGET for that file reuses the
  recorded list verbatim (order included).
- **Grant-time rules**: new file → record `list_active()` *sorted by
  device_id* (kill the iteration-order dependence at the source);
  existing file → if any recorded device is not currently Active,
  refuse the layout (`NFS4ERR_LAYOUTUNAVAILABLE` → client falls back /
  retries) rather than silently re-mapping. MDS-proxy I/O fallback
  remains explicitly unimplemented — refusal is honest.
- **`list_active()` returns sorted** regardless, as defense in depth.
- **Stripe-size pinning**: `stripe_size` is per-file from the placement
  record, so a config change to `layout.stripeSize` affects only new
  files (today it would re-map old ones — same bug class).

### Verification

- Unit: same file, two LAYOUTGETs with the registry re-populated in
  reverse order → identical segment lists. Device missing → refusal,
  not re-map.
- Lima e2e: write file with DS set {A,B}; register C; re-mount (drop
  layouts); read back — content intact and layout still {A,B}. New
  files stripe over {A,B,C}.
- pynfs regression gate unchanged (171/171 + extras).

**Effort**: ~1 week including the lima drill. **This phase gates all
others** — nothing below is safe to drill without it.

### Status: IMPLEMENTED 2026-07-06

Everything above landed, plus one gap the implementation pass found:
**GETDEVICEINFO ignored composite deviceids entirely** — the striped
arm (`mds/operations/mod.rs`) returned the *current* `list_active()`
in registry-iteration order for ANY unknown deviceid, so even a pinned
layout would have resolved to a shuffled/regrown device list on
re-fetch. Fixed by a stripe-group registry (composite deviceid →
placement-ordered device ids) populated at grant/load time; unknown
deviceids are now NOENT, and the composite id + per-file stripe unit
are computed MDS-side and carried on each `Layout` (the dispatcher no
longer re-derives them; the global `stripe_unit()` trait method is
deleted). Placement key = export-relative path (matches DS storage
keying; CSI volume_id for CSI volumes). `DeleteVolume` drops the pin.
sqlite schema v5 (`file_placement` table).

Gates run (all green):
- 623 lib tests incl. 6 new placement tests (restart+reorder via the
  full persist→list→load loop, fleet growth, refusal, stripe-size
  pinning, stripe-group registration, forget-and-repin).
- `make test-pnfs-smoke` — kernel-client mount, 24 MiB write,
  checksum round-trip, real 2-DS striping.
- `make test-pnfs-placement` (NEW fleet-growth drill,
  `tests/lima/pnfs/placement-drill.sh` + `mds-growth.yaml`/`ds3.yaml`):
  A written under {DS1,DS2}; DS3 started; remount; A's sha256
  identical, zero A-bytes on DS3; B stripes 3-wide onto DS3. MDS log
  pins: `striped-A.bin: 2 DSes`, `striped-B.bin: 3 DSes`.

Notes for later phases: the MDS pre-registers every configured
`dataServers` entry as Active at boot (`mds/server.rs:148`) — a
configured-but-dead DS is Active until the heartbeat timeout, which
the growth drill has to sleep past. Phase 3 (restart hardening)
should revisit boot-time device state.

---

## Phase 1 — chart: MDS and DS as first-class citizens (~1 week)

Today the chart's entire pNFS surface is the controller env hook
(`controller.yaml:93-100`, `pnfs.enabled`/`pnfs.mdsEndpoint`). MDS/DS
have only the docker-compose-era sketches in `docker/README-pnfs.md`
(which proposes a DaemonSet — superseded here).

- **DS StatefulSet** (not DaemonSet: identity and PVC binding are the
  point) with `volumeClaimTemplates` on the flint StorageClass
  (3-replica raid1, ext4). `device_id: ${POD_NAME}` via the existing
  env-expansion. `data_dir` = the PVC mount. Replica count =
  `pnfs.dataServers` value; scaling *up* is safe post-Phase-0 (new
  files use the wider set); scaling *down* is refused in docs until the
  drain milestone.
- **Per-pod Services** (one ClusterIP Service per StatefulSet ordinal,
  templated). This is the endpoint-mobility decision: GETDEVICEINFO
  hands kernel clients raw IPs cached per deviceid, and
  `CB_NOTIFY_DEVICEID` is only a protocol constant in the tree. A
  stable ClusterIP per DS makes pod IP churn invisible — zero protocol
  code. (Fallback design if ClusterIP NFS traffic misbehaves under
  kube-proxy: mint a generation-suffixed deviceid on endpoint change +
  recall — protocol-side, ~3-5 extra days; keep in reserve.)
- **MDS Deployment**: replicas=1, `strategy: Recreate`, stable
  ClusterIP Service for both 2049 and gRPC 50051; export dir +
  `state.db` on its own flint PVC; sqlite backend **mandatory** in the
  chart (memory backend is test-only per Phase 4 findings — the
  errno-524/SEQ_MISORDERED wedge).
- **Bootstrap ordering**: DS/MDS PVCs need the flint controller up.
  Same-chart with readiness gating; document the order; kuttl-test a
  cold `helm install` from nothing.
- `pnfs.mdsEndpoint` defaults to the MDS Service DNS when the chart
  deploys the MDS itself.

**Done when**: cold `helm install` on a kuttl cluster yields a mounted
pNFS PVC striping across all DS pods; `helm upgrade` rolls MDS and DSes
without client I/O errors (ordered, one DS at a time).

### Status: LIVE-VALIDATED 2026-07-06 on runn — see validation record below

Landed in-session:
- `templates/pnfs-mds.yaml`: ConfigMap (sqlite mandatory, state.db +
  exports on one PVC at /data, `dataServers: []` — dynamic gRPC
  registration only, deliberately avoiding the boot pre-registration
  wart), PVC on the flint SC, stable ClusterIP Service (2049 + 50051),
  single-replica Recreate Deployment. MDS now `create_dir_all`s its
  export root (fresh-PVC boot).
- `templates/pnfs-ds.yaml`: DS StatefulSet (`podManagementPolicy:
  Parallel`, soft node anti-affinity), `volumeClaimTemplates` on the
  flint SC, `deviceId: "${POD_NAME}"`, headless Service + one
  ClusterIP Service per ordinal named exactly like its pod, so
  `FLINT_DS_ADVERTISE_ADDR="$(POD_NAME).$(POD_NAMESPACE).svc.cluster.local"`
  is that Service's DNS name.
- Endpoint mobility resolved with ZERO protocol code: the DS
  advertises the per-pod Service DNS name (new
  `FLINT_DS_ADVERTISE_ADDR` env, precedence over the existing POD_IP
  path in `ds/server.rs`), and the MDS already resolves hostnames to
  IPv4 at GETDEVICEINFO-encode time (`endpoint_to_uaddr`). The
  ClusterIP behind the name is stable across reschedules, so kernel
  clients' cached device info never goes stale.
- values: `pnfs.server.*` block (image, stripeSize, mds storage/
  timeout, dataServers count/storage/anti-affinity); controller's
  FLINT_PNFS_MDS_ENDPOINT defaults to the in-chart Service when
  `pnfs.server.enabled`; scale-DOWN documented as unsupported until
  the drain milestone.
- PNFS_INSTANCE_ID is deliberately NOT set: the sqlite `server_id`
  gives MDS FH stability, and the DS-side check is skipped when the
  env is unset (validated rig shape).

Gates run: helm lint clean; render validated (10 objects at count=3;
pnfs-disabled render emits zero pNFS objects); 624 lib tests;
test-pnfs-smoke re-run PASS after the registration-path changes.

### Live validation record — runn cluster, 2026-07-06

Validated on a fresh 5-node trove cluster (runn: i4i.large CP + 3
workers + c5d.4xlarge builder; chart revs 2–4 over the trove-installed
1.8.0 release). Every Phase 1 "Done when" gate passed, and the drill
found four k8s-only bugs the lima rig structurally cannot reach (no
external-attacher, no stage/unstage flow, no host-vs-pod DNS split).

Cold install (gate PASS): `helm upgrade` with `pnfs.server.enabled=true`
brought up MDS + 2 DSes with PVCs Bound on flint-spdk in seconds —
the fleet dogfoods flint block volumes (`/dev/nvme*` inside pods).
Bonus live capture: the DS pods raced the MDS at boot, initial
registration failed, and the heartbeat-NACK → re-register path
recovered both DSes ~20 s later with their per-pod Service DNS
endpoints (`FLINT_DS_ADVERTISE_ADDR` beat POD_IP as designed) —
Phase 3's "partially prebuilt" claim is now live-proven.

Driver bugs found + fixed (all in the CSI driver, none pNFS-server):
1. **pNFS PVCs could never attach** — ControllerPublishVolume fell
   into the SPDK PV-metadata lookup ("PV found but missing flint
   metadata") because attachRequired is driver-global. Fix: no-op
   publish branch keyed on `pnfs.flint.io/mds-ip` in volume_context,
   plus a NodeStage no-op branch and a NodeUnstage unmount-only
   classification via new `Driver::pv_is_pnfs` (context-free RPC reads
   the PV's attrs).
2. **Mount port was the gRPC port** — `pnfs_csi::create_volume`
   stamped the dialed endpoint's port (50051) into
   `pnfs.flint.io/mds-port`; the kernel would mount NFS against the
   gRPC listener. Fix: `CreateVolumeResponse.nfs_port` (proto field 5,
   MDS reports its bind.port; 0 → 2049 fallback for older MDSes). The
   lima csi-e2e masked this by using its own port variable for mounts.
3. **Service DNS unresolvable at mount time** — the kernel mount runs
   in the node's network context (csi-node is hostNetwork, host
   resolver): `mount.nfs4: Failed to resolve server
   flint-pnfs-mds.<ns>.svc.cluster.local`. Fix: resolve the MDS host
   to its (stable) ClusterIP at provision time in create_volume —
   same convention as the RWX path's raw server IPs; unresolvable
   dev-rig names pass through.
4. **DS-outage reads returned silent zeros — data-corruption P1.**
   With a DS pod gone, its per-pod Service has no endpoints and
   connections fail fast (ECONNREFUSED); the kernel client immediately
   falls back to READ-through-MDS, and the MDS served the sparse
   size-only stub: full-speed reads of zeros, correct length, no
   error (observed live: wrong sha256 during the outage window, data
   on DSes intact throughout). Phase 4's old DS-death drill missed
   this because a same-endpoint restart just retries TCP; the k8s
   Service shape converts the outage into instant-refusal, which is
   what triggers the fallback. Fix: **stub-IO guard** — in MDS mode,
   READ/WRITE on placement-pinned files return NFS4ERR_DELAY
   (`refuse_stub_io` in the dispatcher + `PnfsOperations::
   is_pnfs_managed` + `LayoutManager::has_placement`); non-striped
   files keep full MDS I/O. Re-drill: wrong data eliminated.

Data path (gate PASS): 64 MiB write at 107 MB/s from a busybox pod,
stripes landed ~36M/~35M across the two DS PVCs, sha256 identical
across pod delete + fresh mount. LAYOUTGET/LAYOUTCOMMIT/LAYOUTRETURN
clean in MDS logs; placement pinned on first layout.

MDS restart with live state (gate PASS): image roll (Recreate) cleanly
unstaged/restaged the PVC; boot reloaded 2 placements + 4 layouts from
sqlite; all DSes re-registered ≤20 s; data readable after.

Fleet growth 2→3 (gate PASS, the Phase 0 story end-to-end on k8s):
`dataServers.count=3` → DS-2 with own PVC on a third node (soft
anti-affinity spread). fileA's sha unchanged and its placement still
2-wide (zero bytes ever land on DS-2); new fileB stripes 3-wide
(~16 MiB/DS). Placement refusal never triggered — no re-mapping.

Operational findings (write these into the operator runbook):
- **csi-node rolls kill mounted flint PVCs.** A chart upgrade that
  changes the csi-node DaemonSet restarts spdk-tgt in the same pod;
  every mounted flint volume on the node loses its block device (ext4
  goes EIO), including the pNFS fleet's own PVCs. After any csi-node
  roll: restart DS pods, and scale-cycle the MDS (0 → 1). A bare `kubectl
  delete pod` on the MDS races its ReplicaSet and the replacement
  inherits the dead staging mount (CrashLoop on EIO); scale-to-zero
  forces the clean unstage. If the replacement pod then sticks in
  ContainerCreating with `bdev_nvme_attach_controller … Input/output
  error` (v1.9.0 gate, 4× on runn), the volume's NVMe-oF *target* died
  with spdk-tgt and mount retries alone never re-create it — delete
  the volume's VolumeAttachment (`kubectl get volumeattachment` →
  match .spec.source.persistentVolumeName): the external-attacher
  re-runs ControllerPublishVolume, which re-publishes the target, and
  the next kubelet mount retry succeeds.

  The durable fix is **restart-survivability**, not a chart
  reshuffle. Splitting spdk-tgt into its own DaemonSet only stops
  driver-image bumps from restarting it — it does nothing when
  spdk-tgt itself restarts (its own upgrade, crash, OOM), and any
  spdk-tgt restart is fatal today because staged volumes don't
  survive it: the NVMe-oF subsystems NodeStage created are process
  runtime state (lvolstores reload from disk; the export objects
  don't), nothing re-publishes them on boot, and the consumer's ext4
  journal-aborts to permanent EIO in the meantime. Surviving a
  restart needs two pieces: (1) a reconcile-on-boot pass that
  re-creates staged volumes' subsystems/listeners with identical
  NQNs, and (2) initiator ride-through — kernel nvme-tcp queues I/O
  for ctrl_loss_tmo (~600 s) on connection *loss*, so if the target
  returns with the same identity before the filesystem sees an
  error, consumers get a stall instead of EIO. Caveat for (2): a
  graceful SPDK shutdown may delete namespaces explicitly, which the
  initiator treats as removal rather than loss — the shutdown path
  must look like a crash to the initiator. nvmeof backend only; ublk
  devices die with the process and have no ride-through story. With
  both pieces in place the DaemonSet split becomes a mere
  exposure-reducer. Phase 4 should add a "spdk-tgt restart under
  live pNFS load" drill to validate the ride-through end to end.
- **In-flight I/O at DS-outage time hangs until client state resets.**
  The stub-IO guard converts the corruption into NFS4ERR_DELAY
  retries, but the kernel never re-drives an already-fallen-back RPC
  through the pNFS path, even after the DS recovers — and kernel NFS
  client state is PER NODE (shared superblock): recreating the pod on
  the same node inherits the wedge; force-deleting wedged pods leaves
  a detached-superblock retry loop (~220 rps at the MDS ≈ 1% CPU)
  until node reboot. New I/O from a clean node is unaffected
  (verified: correct checksums in 0.6 s from another node while the
  zombie loop ran). Durable fix = **MDS proxy I/O** (MDS serves
  READ/WRITE by proxying to DSes) — added as a follow-up work item;
  until then a DS blip can require app-pod rescheduling to another
  node to unstick mid-flight readers.
- `helm upgrade --reuse-values` skips NEW chart defaults → nil
  `pnfs.server.*` template errors. Always pass the full values file.
- PVs provisioned by a pre-fix driver carry broken volumeAttributes
  (gRPC port / bare DNS name) and cannot be repaired — attrs are
  immutable. Not a production concern (the chart shape never worked
  before the fixes); re-provision.

Validation images (uncommitted-fix builds from this session, built on
the runn builder): `dilipdalton/flint-pnfs:pnfs-p1.2` and
`dilipdalton/flint-driver:pnfs-p1.2`. The proper `flint-pnfs` release
image still publishes at the next release like every other image.

---

## Phase 2 — DS identity ↔ PVC binding guard (~2–3 days)

Convention (pod name → device_id → PVC follows the pod) gives 90%. The
missing piece is refusing the other 10%: on first boot the DS stamps
`<data_dir>/.flint-ds-identity` (device_id + creation stamp); on every
boot it verifies the marker matches its `device_id` and **refuses to
start** on mismatch. Cheap insurance against the `_hr`-style
identity-aliasing bug class the replica drills kept finding, and
against a human re-pointing a PVC.

Registration additions: DS reports the marker's creation stamp;
MDS logs identity+endpoint transitions at WARN on re-registration
(`device.rs:98` today just says "updating").

**Done when**: unit tests for marker create/verify/mismatch; lima drill
mounts DS-B's volume into DS-A's pod and observes startup refusal.

### Status: IMPLEMENTED 2026-07-06

`ds/identity.rs` (stamp/verify/refuse, temp+rename write, 4 unit
tests); `RegisterRequest.identity_created_at` (field 8) reported on
both register paths; MDS re-registration WARNs endpoint transitions
and — loudly — identity-stamp transitions. Gate:
`tests/lima/pnfs/identity-drill.sh` (pure host: stamp →
verify-with-stable-stamp → foreign-device refusal, exit nonzero,
marker untouched) via `make test-pnfs-identity`, wired into
`test-pnfs-all`. 629 lib tests.

Shipped alongside (same day, driver side): **NodeStage self-heal for
dead NVMe-oF targets** — the attach-failure path re-ensures the
export on the storage node (convergent ensure_export, fencing
preserved) and retries once, turning kubelet's mount-retry loop into
the reconciler. This is restart-survivability piece (1); the manual
VolumeAttachment-delete recipe below is now only needed on pre-heal
drivers. Piece (2), initiator ride-through for already-staged
volumes, remains open.

LIVE-VALIDATED 2026-07-06 on runn (images p2heal.0): a full csi-node
roll reproduced the landmine and the fleet recovered with ZERO
VolumeAttachment surgery — both failure shapes healed
(`bdev_nvme_attach_controller` EIO → "Connected after target
re-ensure (self-heal)" on DS-0's volume, the same PV that needed 4
manual VA-deletes in the v1.9.0 gate; and a mid-roll local-agent
refusal that converged on kubelet's next retry). Identity markers
stamped on all three DS volumes, stamps stable across restarts and an
FS-error episode, registrations carry them, data shas intact
throughout. New residual for the runbook: a pod bounce that RACES the
lazy unmount can inherit a stale read-only staging mount (DS-2, one
occurrence) — delete with `--wait=true` and let termination finish
before the replacement schedules; the durable fix is NodeStage
revalidating an already-mounted staging path (writable? remount/fsck
if not) — future work alongside ride-through.

---

## Phase 3 — MDS-restart and re-registration hardening (~3–5 days)

The device registry is in-memory only (deliberate — DSes are the source
of truth). Post-restart correctness therefore depends on re-registration
being prompt and on the MDS not acting before it happens:

- **DS re-registers on heartbeat NACK.** Today `heartbeat()` logs
  "not acknowledged" and carries on (`ds/registration.rs:158`); an MDS
  that restarted has forgotten the DS, so a NACK must trigger a full
  `register()` retry loop.
- **Boot grace before stale-device recalls**: the stale-device sweep
  (`mds/server.rs:837`) must not fire for the first
  `max(heartbeat_interval × 3, 30s)` after MDS boot, or a restart
  recalls every layout in the cluster while healthy DSes are still
  re-introducing themselves. Aligns with the existing 90s NFS grace
  period (`state/lease.rs`) during which clients reclaim state anyway.
- **Layout-vs-placement reconciliation at boot**: persisted layouts
  reference devices the registry hasn't seen yet — resolve lazily
  (recall/refuse only on actual staleness), not eagerly.

**Done when**: lima drill — MDS process killed and restarted under fio
load; DSes re-register within one heartbeat; clients reclaim through
grace; zero recalls fired for healthy DSes; I/O resumes with no errors.

### Status: IMPLEMENTED 2026-07-06 — core drill green, one clause open

All three items landed:
- **Heartbeat NACK → immediate re-register** (was 3-strike ≈ 30 s):
  a NACK means "the MDS answered and doesn't know us" — re-register on
  that very tick. Transport errors keep the 3-strike path (an MDS
  mid-restart isn't helped by hammering register()).
- **Boot grace before the stale-device sweep**: the sweep's first
  check waits max(heartbeatTimeout, 30 s) after MDS boot so
  re-introducing DSes are never swept; aligns with the 90 s NFS grace.
- **Lazy layout reload documented as an invariant** — the boot reload
  does no registry validation and must not (comment at the reload
  site); staleness is only judged per-file at grant time.

**Bonus P1 the drill caught (the "under load" clause earning its
keep): DS heartbeats starved under write load.** The data path's
block_in_place I/O tiering can occupy all 4 runtime workers for tens
of seconds, silently starving a tokio::spawn'd heartbeat — the MDS
would mark a healthy, BUSY DS stale and recall its layouts (load-
triggered self-DoS). The heartbeat sender now runs on a dedicated OS
thread with its own current-thread runtime AND its own gRPC channel
(a shared channel's I/O driver would still live on the starvable main
runtime). Liveness signalling never shares a scheduler with the data
path.

Drill: `tests/lima/pnfs/mds-restart-load.sh` (make
test-pnfs-restart-load) against a NEW dynamic-only MDS config
(`mds-restart-dynamic.yaml` — the chart shape; the config-listed rigs
repopulate the registry from config at boot and never exercise the
NACK path). Green: dynamic boot registration, kill -9 mid-load,
**both DSes re-registered 10 s after restart via the NACK fast
path**, zero stale detections, zero recalls. OPEN (drill kept strict,
not yet wired into test-pnfs-all): the final error-free-client-I/O
clause fails — post-restart the kernel client's DS **session
trunking** fails ("Session trunking failed" in dmesg; MDS and DSes
share server_owner AND, on the lima rig, one IP), the client abandons
the pNFS path, its fallback MDS writes get stub-IO-guard DELAY (61
refusals observed), and async writeback eventually surfaces EIO to
the app. Needs its own fix wave: revisit the DS-shares-MDS
server_owner trunking design (RFC 8881 allows distinct DS identity)
and/or make the DS reject BIND_CONN_TO_SESSION for unknown sessions
in a way that steers the client to a separate session. Note the k8s
shape (distinct ClusterIPs per DS) may not hit the same-IP trunking
edge at all — verify on a cluster before concluding the failure mode
is real in production.

**VERIFIED ON K8S 2026-07-06 (runn, flint-pnfs:p3.0): the failure
does NOT reproduce on the distinct-IP shape — it is a lima same-IP
rig artifact.** Drill: `pkill -9 flint-pnfs-mds` via nsenter mid-load
(60 × 4 MiB files from a pod on another node). Results: container
restart in 2 s, 64 placements reloaded from sqlite, boot-grace line
held the sweep, all 3 DSes re-registered at +4/+8/+12 s (each on its
next 10 s heartbeat via the NACK fast path), zero stale detections,
zero recalls, writer completed with zero errors, all 60 checksums
correct, client-node dmesg clean (no trunking messages, no
timeouts). The Phase 3 "Done when" bar is met on the production
shape end to end. The trunking fix wave is deprioritized to
rig-hygiene: the lima drill's final clause stays strict as a canary,
and the drill stays out of test-pnfs-all until the rig shape gets
per-DS IPs (lima vmnet aliases) or the DS drops the shared
server_owner.

---

## Phase 4 — k8s failure drills (~1 week, the real cost)

Extend the lima + kuttl suites (pattern: the RWX-teardown and Tier-2
drill campaigns):

1. **DS pod reschedule under load**: cordon+delete the DS pod
   mid-fio; StatefulSet reschedules to another node; PVC follows
   (NVMe-oF reattach); per-pod ClusterIP unchanged; clients stall ≤
   lease-scale seconds, retransmit UNSTABLE data (boot verifier), zero
   errors, integrity clean across cache drops.
2. **Node death** (the ugly one): kubelet gone, pod stuck Terminating —
   StatefulSet will not reschedule without operator action. Drill the
   `out-of-service` taint / force-delete path; runbook entry with exact
   commands and expected client-visible stall (bounded by lease + drill
   measurements). This is the k8s-mechanics twin of the Tier-2
   quiesce/rejoin runbook.
3. **Replica failure underneath a DS**: kill one leg of the DS's lvol
   while pNFS writes flow; Tier-2 rebuild runs under the filesystem;
   assert no pNFS-visible effect (this is the payoff drill for
   lvol-backing — record numbers).
4. **MDS pod roll mid-workload** (k8s version of Phase 3's drill):
   `kubectl rollout restart`, sqlite recovery, 90s grace reclaim,
   measure end-to-end client stall.

**Done when**: all four drills scripted and green twice consecutively;
runbook sections landed in `docs/tier2-operator-runbook.md` or a new
`docs/pnfs-operator-runbook.md`.

### Status: ALL FOUR drills green twice, runbook landed (2026-07-06)

Scripted suite: `tests/k8s/pnfs-drills/` (lib.sh + one script per
drill; kubectl-only, cluster-agnostic, KUBECONFIG + CLIENT_NODE env).
Runbook: `docs/pnfs-operator-runbook.md` with measured numbers.

- **Drill 1 (DS reschedule under load): GREEN ×2** on runn —
  cross-node reschedule 49–54 s, per-pod ClusterIP unchanged,
  identity stamp stable (PVC followed), zero errors, all checksums,
  **max client stall 1 s** (graceful termination keeps the old DS
  serving while the replacement attaches; the in-flight-wedge
  residual did not trigger on the graceful path).
- **Drill 2 (node death): GREEN ×2** — kubelet stopped via nsenter,
  NotReady in ~37 s, out-of-service taint force-detaches and the
  StatefulSet replaces the DS on another node **64–70 s after kubelet
  death**, ClusterIP + volume follow, zero errors, stall 1 s. Node
  restore automated via SSM (instance resolved by InternalIP — trove
  nodes have no providerID).
- **Drill 4 (MDS roll mid-workload): GREEN ×2** — rollout ~40 s,
  placements reload, DSes re-register on their next heartbeat, zero
  recalls, zero errors, **stall 1 s** (the MDS is out of the data
  path; kill -9 variant separately proven in the Phase 3 k8s drill).
- **Drill 3 (replica failure under a DS): GREEN ×2** (interactive +
  scripted `replica-under-ds.sh`) — DS-3 added on a new flint-spdk-r2
  SC (claim template is immutable: orphan-delete the STS + helm
  upgrade recreates it; existing ordinals keep their r1 PVCs), remote
  raid1 leg detached initiator-side mid-write: **raid degrades to 1/2
  but stays online and the DS keeps serving — files kept landing on
  it through the whole degraded window, zero client errors, stall
  1 s, checksums clean**. Leg reattach + `bdev_raid_add_base_bdev` →
  2/2. This is the lvol-backing payoff measured.

Two findings from drill 3's first attempt (both now in the runbook):
placement pins are per file-key forever, and pNFS pods share the
export-root namespace — the drill initially reused file names pinned
hours earlier under the 3-DS fleet, so ZERO stripes landed on the new
DS (Phase 0 doing its job; drills/benches must use unique names). And
**NFS REMOVE does not forget placements** — only CSI DeleteVolume
does; a same-name recreate inherits the old stripe map. Follow-up:
wire forget_placement into the REMOVE op.

Script gotchas baked into lib.sh for posterity: `grep -q` SIGPIPEs
producers under pipefail (let grep consume the stream); a readiness
wait right after pod delete can match the OLD Terminating pod (wait
for the UID to change first).

---

## Phase 5 — re-bench and ADR 0005 (~1–2 days cluster time)

The durability claim is not advertisable until re-measured: every DS
write now fans out 3× over NVMe-oF and pays raid1 + rebuild-machinery
overheads. Re-run the ADR 0004 rig (recipe is routine) with lvol-backed
DSes:

- Same phases (seq 1M, 4k randread, small-file dataloader), N ∈ {1,2,4}.
- **Expectation to confirm**: reads ~unchanged (served from the local
  attached leg); write scaling still ~linear per-DS but each DS's write
  ceiling lower by replication amplification — quantify the constant.
- One replica-degraded phase: numbers during an active rebuild.
- Record as ADR 0005; only then update README/chart docs to claim
  durability, with the measured write cost stated plainly.

**Release gate hygiene** (rides whichever release ships this):
`ds_sequence::test_highest_slotid` was stale (asserted pre-RFC-fix
semantics; `sr_highest_slotid` = highest slot the server *accepts*, RFC
8881 §18.46.3) — **fixed 2026-07-06**, suite 10/10. Note the main
server's `state/session.rs` tracks max-slot-in-use instead; pynfs
accepts both, but unifying on the DS's reading is a small follow-up.

---

## Follow-on milestone A — DS drain/decommission (investigated, not in scope)

**What Phase 0 buys**: with placement persisted per-file, drain becomes
a data-movement problem with exact bookkeeping, instead of being
impossible to even define.

**Cheapest primitive first — no-copy "drain"**: because a DS's data IS
a flint PVC, replacing a DS *pod/node* never copies data (Phase 4 drill
1). Drain-with-copy is only needed to retire a PVC or change fleet
shape.

**Swap-replace (the supported operation, ~1.5–2 weeks when scheduled)**:
1. MDS gains a `Draining` device state: excluded from *new* placements,
   still serves I/O.
2. Data mover: copy the draining DS's path-nested sparse tree to the
   replacement DS. Two options — `rsync --sparse` job (zero new code,
   needs both PVCs mountable) or a DS-to-DS "pull from peer" gRPC verb
   (cleaner, ~1 week extra). Start with rsync.
3. Cut over: quiesce via `recall_layouts_for_device(draining)` (exists),
   final delta copy, atomically rewrite placement records
   old-device→new-device (one sqlite UPDATE), mark new DS Active, retire
   old.
4. Client-visible cost: one recall + re-LAYOUTGET per affected file —
   the same path the DS-death drill already exercises.

**Shrink (N→N-1 without replacement): defer indefinitely.** It is a
full re-stripe (read every affected file under the old map, rewrite
under the new), an order of magnitude more I/O and new code. Workaround
is always swap-replace onto fewer, larger DSes.

---

## Follow-on milestone B — MDS HA (investigated, not in scope)

**Foundation is better than expected.** sqlite persists clients,
sessions, stateids, locks, layouts, the instance counter, and a stable
`server_id` (`get_or_init_server_id`) — so a *successor* MDS process
presents the same server identity, and the 90s grace + RECLAIM_COMPLETE
enforcement (`dispatcher.rs:1098-1134`) is exactly the RFC 8881 restart
story. Kernel clients retry TCP against a stable Service ClusterIP
indefinitely.

**Tier 1 — restart-HA (this milestone's Phases 1+3 deliver it)**:
single-replica Recreate Deployment + state.db on a 3-replica flint PVC
+ stable ClusterIP. RTO = pod reschedule (seconds to ~1 min; node death
needs the taint/force-delete runbook) + boot + up-to-90s grace →
**~1–3 min of client stall, zero errors, zero state loss**. The RWO
PVC attach is the fencing lock — k8s will not attach it to two nodes,
and sqlite is single-writer anyway.

**Tier 2 — warm standby (~2–3 weeks when scheduled)**: pre-scheduled
standby pod + leader election; on takeover, attach PVC, replay sqlite,
enter grace. Cuts reschedule latency out of RTO (→ roughly the grace
period). Requires solving fast RWO detach-reattach and making the boot
grace-vs-recall interaction (Phase 3) airtight. No protocol work.

**Tier 3 — active-active: not on the roadmap.** Requires replacing
sqlite with replicated state and coordinating layout grants across
MDSes; RFC 8881 allows it but nothing in the current workload demands
it — pNFS's whole design keeps the MDS out of the data path (measured
0% CPU), so a single MDS is not a bandwidth bottleneck, only an
availability one, and Tiers 1–2 bound that.

---

## Sequencing and effort summary

| Phase | What | Effort | Gates |
|---|---|---:|---|
| 0 | Per-file placement persistence (P1) | ~1 wk | everything below |
| 1 | Chart: MDS Deployment, DS StatefulSet, per-pod Services | ~1 wk | Phases 2–4 |
| 2 | DS identity marker guard | ~2–3 d | — |
| 3 | MDS-restart / re-registration hardening | ~3–5 d | Phase 4 drill 4 |
| 4 | k8s failure drills + runbook | ~1 wk | Phase 5 |
| 5 | Re-bench → ADR 0005, durability claim | ~1–2 d | release |

Total ≈ 4–5 weeks. Code deltas are modest (placement table + grant
rules, identity marker, re-register-on-NACK, chart templates); the
schedule is dominated by drills and the bench — which is the correct
shape for a milestone whose entire promise is "we will not claim
durability until the drills say so."

Provision note: all live phases need a fresh trove cluster (runk/runl
are deleted); the ADR 0004 bench-rig recipe covers Phase 5.
