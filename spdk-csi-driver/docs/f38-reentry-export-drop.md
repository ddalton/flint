# F38 — assembly re-entry drops the live consumer-chain export (design)

**Status: designed from live runaa evidence, not implemented** —
v1.19.0 material. Evidence: runaa drill 3.6c, 2026-07-22 (driver log on
the resurrected server node; recovery was a manual `flint-nfs` pod
delete). Code sites named below are unchanged; this document is the
implementation spec.

## The finding (runaa drill 3.6c, 2026-07-22)

After a ~9-minute kubelet-stop outage of the NFS server's node, the
resurrected node entered a ~90s livelock: a monitor loop re-ran
`create_raid_from_replicas` (driver.rs:1735) for a volume that was
**already being served**. Each re-entry ran `attach_replica_base` for
the local surviving leg (driver.rs:2256), which calls
`drop_stale_local_exports` (driver.rs:2233, the 2026-06-12 runf
regression fix) — and that **deleted the volume's own live
consumer-chain export** (`nqn.2024-11.com.flint:volume:nfs-server-<pv>`),
severing the data chain under the mounted filesystem. The NFS server
pod crashlooped on the F30 identity-marker check (exit 57, I/O-error
class — nfs_main.rs:114-158), nothing escalated, and the repair loop
fired again. Forever.

Observed: alternating `Dropping stale local export
nqn…nfs-server-pvc-… of <lvol-uuid>` and `SINGLE-SURVIVOR DIRECT
SERVE` every ~90s; `ublk_get_disks` empty between cycles;
`bdev_raid_get_bdevs` empty (direct serve — no raid exists by design).
Recovery: delete the flint-nfs pod → fresh NodeStage → stable.

The F36c code already carries this loop's fingerprint: the risk-marker
clear guard commented "rc1's ~90s amnesia, found live on runaa 3.6c"
(driver.rs:1926-1932) patched the *writer-set* side effect of the same
re-entry storm. The export-drop side is F38.

## Root cause (three stacked defects)

### 1. The destructive step: a falsified invariant

`drop_stale_local_exports` deletes every flint `:volume:` subsystem
whose namespace addresses the lvol being claimed. The safety argument
is written down at `flint_subsystems_exporting_bdev`
(driver.rs:3154-3159): *"The raid loopback subsystem exports the raid
bdev, not the lvol, so it can never match."*

**Single-survivor direct serve (driver.rs:2173-2195) falsified that.**
With no raid layer, the consumer chain's head IS the lvol — the
volume's own loopback export namespaces the lvol directly — so the
match rule classifies the live serving export as stale and deletes it
on every re-entry. Call sites: the local branch of
`attach_replica_base` (driver.rs:2272) and the forced-stale re-attach
(driver.rs:2072). Note the inversion at driver.rs:2272-2274: drop
*failure* is non-fatal ("continuing"); drop *success* is what severs
the chain.

`ensure_raid1_bdev`'s converge-don't-recreate idempotence
(driver.rs:2646-2663, reuse-if-ONLINE) arrives too late to help: it
runs AFTER the destructive attach phase, and never runs at all in
direct-serve mode. Latent sibling hazard: driver.rs:2666-2673 deletes
any non-ONLINE raid it finds, without checking for consumers above it.

### 2. The caller that never converges: the consumer-blindness monitor

The ~90s caller is `detect_lost_data_paths` (node_agent.rs:4611) on
the 60-second monitor loop (node_agent.rs:271-307). Its loss predicate
is **raid presence**: `attached && !raids.contains(raid_name(handle))`
(node_agent.rs:4690, 4751). Three strikes → `repair_data_path`
(node_agent.rs:4418) → `create_raid_from_replicas`
(node_agent.rs:4452), then every tick while the predicate holds.

Direct serve never satisfies that predicate BY DESIGN — which is
exactly why the `flint.io/degraded-direct` exemption exists
(node_agent.rs:4668-4681). **It never fires for RWX backing chains,
because setter and reader disagree about which PV object holds it:**

- Written: `create_raid_from_replicas` patches
  `record_volume_id = storage_id_of_handle(volume_id)`
  (driver.rs:1746, 2180) — for the backing handle
  `nfs-server-pvc-X` that resolves to the **user PV** `pvc-X`.
- Read: the monitor checks annotations on the PV it is examining
  (node_agent.rs:4673-4681) — for the backing chain that is the
  **synthetic backing PV** `flint-nfs-pv-pvc-X` (name/handle asymmetry,
  identity.rs:71-98), which passes every earlier skip (it carries the
  copied replica attributes, rwx_nfs.rs:271-283, and is RWO so
  `is_rwx_pv` does not skip it).

This is the F32 bug class reincarnated — identity.rs:85-98 documents
`pv_name_of_handle` for precisely this asymmetry; the degraded-direct
writer doesn't use it. Consequence sharper than the incident
narrative: **any RWX volume in single-survivor direct serve enters the
destructive repair loop within three monitor ticks, outage or not.**
The 9-minute outage was merely the path INTO direct serve.

Why each cycle re-breaks what it "repairs": the re-entry's drop
severs the live export mid-repair; the filesystem above takes EIO
(F30 refusal, exit 57); whether the tail of the repair then restores a
chain (re-export at node_agent.rs:4477-4501, or `ensure_ublk_disk` at
4468-4475) is irrelevant — the raid-presence predicate is still false
next tick, so the monitor strikes again. The 10s detectors join the
churn: `reconcile_exports_if_lost` (node_agent.rs:3234) resurrects the
deleted export from its registry within 10s (the restore half of the
observed alternation); their own repair fast paths (3308, 3606) gate
on `parse_raid_name` and mostly no-op in direct serve.

Cadence: there is no 90s timer anywhere in the driver. The monitor
tick is 60s; tokio's default Burst interval semantics make the
effective period `max(60s, repair-body duration)`, and the body is
inflated by remote setup/attach attempts against the still-absent peer
(`call_node_agent` HTTP with no explicit timeout, driver.rs:668;
`try_attach_remote_replica`, driver.rs:2430) plus F36c probes and
annotation writes — ≈90s observed. The exact decomposition should be
pinned from the runaa log timestamps during validation.

### 3. No escalation actor reaches the proven recovery

The proven recovery (delete the flint-nfs pod → fresh NodeStage)
exists in code twice, and neither instance could fire:

- **F35 liveness reconciler** (rwx_nfs.rs:626 `classify_liveness`,
  rwx_nfs.rs:951 decision table, 30s tick at rwx_nfs.rs:1196):
  classifies by pod phase and deletionTimestamp only. A crashlooping
  pod is phase `Running` → `Present` → `Skip("server pod present")`.
  The exit-57 refusal — restartCount climbing,
  `lastState.terminated.exitCode == 57` — is invisible to it.
- **Layer-3 cutover bounce**: `plan_cutover`'s `data_path_lost` branch
  returns `BounceNfsPod` (cutover.rs:297-301) — literally the manual
  recovery, automated, with backing-PV flag folding already handled
  (cutover.rs:752-770). But it is default-disabled
  (`CutoverConfig { enabled: false }`, cutover.rs:194; `FLINT_CUTOVER`
  is not wired in the chart's controller env), and even when enabled
  it is starved: the flag is set only when repair FAILS
  (node_agent.rs:4808), the node agent clears it on the next
  "successful" (destructive) repair, and the 90s debounce
  (cutover.rs:947-951) resets on every clear.

## Destructive/mutating steps on re-entry over a live chain

- **D1** `drop_stale_local_exports` deletes the live consumer-chain
  export when the serving head is the lvol (direct serve). The F38
  trigger.
- **D2** `ensure_raid1_bdev` deletes any non-ONLINE raid without a
  consumer check (driver.rs:2666-2673). Latent: a transiently
  CONFIGURING assembly under a live frontend would be destroyed.
- **D3** Record and annotation churn per entry:
  `record_assembly_sync_state` re-stamps the writer set (the F36c
  amnesia, now guarded on the clear side only) and the F36c
  defer/risk markers cycle (driver.rs:1863-2051).
- **D4** `repair_data_path` tail: anti-zombie `bdev_raid_delete` if
  the attachment moved mid-repair (node_agent.rs:4455-4466); the
  re-export/ublk-recover steps are convergent but only after D1
  already severed the chain this cycle.

## The tension (why the drop can't just be removed)

- The 2026-06-12 drop fixes a REAL hazard: a stale export created for
  a previous consumer epoch holds a write-mode open that EPERMs the
  raid module's exclusive claim at `bdev_raid_create`. A fresh
  NodeStage MUST be able to clear it — and the genuinely-stale export
  can itself hold live controllers (the fenced-out previous consumer's
  lingering connection). A blanket "skip any subsystem with live
  connections" re-opens the runf regression.
- The repair machinery exists because chains genuinely vanish
  (spdk-tgt restart). An "already healthy" guard that trusts
  registries instead of live RPC state masks real losses (F33: Ready
  nodes run dead tgts).
- Direct serve is load-bearing for drill 2.4 (permanent loss → serve
  the survivor). Repair can't be blanket-skipped for direct-serve
  volumes: a direct-serve chain whose tgt restarted DOES need the
  rebuild.

## Proposed resolution (layered: a + b + c + d)

**The invariant** (extends F36 guard-a from catch-up-time to
assembly/repair-time):

> No assembly or repair path may delete or replace any component of a
> serving chain — export subsystem, raid bdev, ublk disk, or head
> lvol — while that chain has a live consumer. Re-entry over a
> healthy serving chain must be a read-only no-op that returns the
> existing head. When consumer liveness cannot be verified, treat the
> chain as consumed (fail closed).

F36 guard-a ("never delete a live-consumed head", catchup.rs:953-1015
`head_live_consumer`, applied at catchup.rs:1591-1627, shipped
v1.18.0) is the special case for the head lvol; F38 generalizes it to
every chain component, reusing the same probe.

### (a) Idempotent re-entry guard at the top of `create_raid_from_replicas`

Before ANY attach (the existing `ensure_raid1_bdev` reuse is too
late), consult live SPDK state only:

- **Raid mode:** `get_raid_bdev(raid_name(volume_id))` is ONLINE →
  return it untouched.
- **Direct mode:** the expected head lvol exists AND a serving entity
  is live on it — a ublk disk mapping the volume's id to the lvol in
  `ublk_get_disks`, or the subsystem `volume_nqn(volume_id)` whose
  namespace is the lvol with ≥1 live controller
  (`nvmf_subsystem_get_controllers` — the `head_live_consumer` probe,
  catchup.rs:962).

Guard passes → skip attaches, drops, record stamping, and marker
churn entirely (also closes D3). Failure modes designed against:
half-broken chains (export live, frontend dead) must NOT pass — the
check is end-to-end or falls through to rebuild; the check is
RPC-live, never registry-derived; a fresh NodeStage has no consumer
yet, so the guard naturally falls through and staging is unchanged.

### (b) Self-chain exemption in `drop_stale_local_exports` (host-scoped)

For each matched NQN, fetch its controllers before deleting:

- Any live controller whose `hostnqn == flint_host_nqn(self.node_id)`
  — this node's own initiator, i.e. the chain being served here —
  **skip and warn**. (`nvmf_subsystem_get_qpairs` gives the same
  evidence; controllers is what `head_live_consumer` already uses.)
- Controllers only from OTHER hosts → stale by fence rules (the runf
  regression's shape is *"created while a previous consumer ran on
  another node"*, driver.rs:2265-2271) → drop, as today.
- ublk-frontend addendum: also skip when `ublk_get_disks` shows a
  disk on this lvol — a live ublk chain has no subsystem to probe but
  is equally live-consumed.
- Probe error → skip the drop, log loudly, and let a genuinely stale
  claim surface as the raid-create EPERM (a visible, retryable stage
  failure beats a silently severed live chain).

Rejected variant: a caller-context flag ("repair re-entry vs fresh
stage"). Context does not encode liveness — a repair re-entry after a
real tgt death legitimately needs the drop; the probe answers the
actual question.

### (c) Repair-loop convergence

- **Fix the exemption's annotation target** (independently shippable;
  alone sufficient to stop the runaa loop): write
  `flint.io/degraded-direct` via
  `pv_name_of_handle(volume_id)` — not `storage_id_of_handle` — at
  driver.rs:2180 and the clear at driver.rs:2221, so the monitor's
  read (node_agent.rs:4673) sees it on the PV it examines. Exactly the
  F32 fix, applied to one more writer.
- **Make the monitor's predicate serving-entity-aware**: chain health
  = (raid ONLINE, or degraded-direct with the head lvol live) AND
  frontend entity present (export-with-self-controller or ublk disk)
  — i.e. reuse the (a) guard as the health check instead of raid
  presence alone.
- **Escalate instead of looping**: after N consecutive failed repairs
  (N=3), stop repairing, set the layer-3 flag, and hold it for the
  episode (do not clear on a partial success; the episode closes only
  when the chain verifies end-to-end). That un-starves the cutover
  `BounceNfsPod` path — which requires wiring `FLINT_CUTOVER` into
  the chart controller env, or carrying the bounce in (d) instead.

### (d) F30/F35 interplay: crashloop-aware liveness

`nfs_pod_liveness` (rwx_nfs.rs:636) reads only phase. Extend
classification with container status: restartCount rising with
`lastState.terminated.exitCode == 57` (or
`waiting.reason == CrashLoopBackOff` with last exit 57) observed on N
consecutive reconciler passes (N=4 ≈ 2 min at the 30s tick) → new
class `CrashLooping` → `nfs_reconcile_decision` → Recreate (delete +
recreate; `delete_dead_nfs_pod` already handles the corpse race).
That is the proven recovery — pod recreate forces a fresh NodeStage.

Guard rails: only exit 57 escalates on this path (generic crashloops
— OOM, config — stay F35-Present semantics); recreates are bounded
(cooldown + max attempts per window) because two of the three F30
verdicts (`RefuseMismatch`, `RefuseEmpty`) cannot be fixed by a
restage and would flap forever. To distinguish, nfs_main should write
the refusal verdict to `/dev/termination-log` so the reconciler reads
`lastState.terminated.message` and escalates only the I/O-error class
(the "backing store died under the export" case — F38's case).

### Recommended combination

Ship **a + b + c + d**. Rationale for all four layers: (c)'s
annotation fix removes the false-positive generator; (a) makes any
remaining re-entry convergent and cheap; (b) enforces the invariant at
the destructive site itself, protecting every OTHER re-entry path
(kubelet stage retries, F29-style restage races, future callers) that
(a) does not front; (d) is the terminal escape hatch that turns
"wedged forever" into "recovered within a bounded number of ticks" —
matching what the human did on runaa. Any one layer alone leaves a
hole: a-only trusts one call site's guard; b-only still loops
(non-destructively) and starves escalation; d-only recreates the pod
into the same destructive loop.

## Acceptance (drill 3.6e)

Setup: RWX r2, pvc-backed NFS server on node A holding a local leg.
Drive the volume to single-survivor direct serve (permanent removal of
the peer leg; F36c permanent branch). Confirm
`flint.io/degraded-direct` lands on the **backing** PV
(`flint-nfs-pv-…`). Steady acked-write workload through NFS clients
throughout.

**Provocation 1 — the runaa replay:** kubelet-stop node A ~9 min,
resurrect. Assert:

- (a) convergence within one cycle: at most ONE
  `create_raid_from_replicas` re-entry post-resurrect, then quiescent
  for ≥10 monitor ticks (no further `SINGLE-SURVIVOR DIRECT SERVE` or
  `Dropping stale local export` lines);
- (b) zero export drops while consumed: no `nvmf_delete_subsystem`
  against a subsystem holding a live self-host controller, entire
  drill;
- (c) chain stability: the consumer entity (ublk id / initiator
  controller) identity is unbroken across ticks; NFS pod restartCount
  stops climbing within one cycle; no F30 exit-57 after convergence;
- (d) zero acked-write loss + amcheck clean (the 3.7 bar).

**Provocation 2 — loop provocation without an outage** (regression
test for the annotation-target fix): with the chain healthy in direct
serve, run ≥5 monitor ticks. Assert zero repair entries. (Pre-fix
behavior: destructive loop begins by tick 3.)

**Provocation 3 — escalation:** break the chain unrecoverably in
place (`FLINT_DATA_PATH_REPAIR=disabled` + kill the frontend). Assert
N exit-57 crashloops → reconciler recreates the pod within
N×30s + pod-start budget → fresh NodeStage → clients resume. Assert
recreates are BOUNDED for a manufactured identity-mismatch export (no
recreate storm).

**Non-regression:** 2.4 and 3.6c re-runs stay green — guards (a)/(b)
must not delay first-tick survivor serve, and the F36c gate behavior
is unchanged.

## Scope note

Touches: `driver.rs` (top-of-assembly guard; host-scoped drop
exemption; degraded-direct annotation target via `pv_name_of_handle`),
`node_agent.rs` (monitor health predicate + episode-scoped escalation
strikes), `rwx_nfs.rs` (liveness class + decision table + bounds),
`nfs_main.rs` (termination-message verdict, optional), chart
(`FLINT_CUTOVER` wiring if the layer-3 bounce is chosen over the
reconciler-side recreate). Related family: F36 guard-a shipped v1.18.0
(catchup.rs:1591); F36c gate implemented with this loop's writer-set
amnesia already patched on the clear side (driver.rs:1926); F34/F35
shipped v1.18.0. F38 is the assembly-re-entry member of the same
invariant family: an assembly path may never destroy a chain component
that has a live consumer.
