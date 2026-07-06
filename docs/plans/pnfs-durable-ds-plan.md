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
