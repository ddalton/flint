# Tier-2 hot rejoin — operator runbook

**Status:** first edition 2026-07-03, written against `main` at suite 592
(image `tier2-7b4.4`). Every procedure here was earned by a live incident
during the 7b validation campaigns; the narrative evidence lives in
`UnansweredOn7b.md`. Audience: an operator running a Flint cluster with the
Tier-2 orchestrators enabled — no familiarity with the design docs assumed.

---

## 1. The thirty-second model

A Flint multi-replica volume is a consumer-side RAID-1 over per-node NVMe-oF
exports, one lvol replica per storage node. Recovery is layered:

- **Tier 0 — transparent absorption.** `spdk-tgt` restarts in seconds; the
  node agent re-exports and the kernel initiator's nvme-tcp reconnect
  resumes queued io before any write fails. Most single-leg blips never
  even mark a replica stale. No operator action, ever.
- **Tier 1 — epochs + catch-up.** The epoch scheduler cuts a common
  snapshot (`epoch-<vol>-<n>`) across in-sync replicas on a cadence. A
  failed replica goes `stale`; catch-up reverts its head to a shared base
  epoch and replays deltas from a healthy source until it is `standby`
  (converged, chasing each new epoch). A standby rejoins at the next
  natural raid reassembly (pod restart, restage).
- **Tier 2 — hot rejoin.** For attached volumes with no reassembly in
  sight (the restart-intolerant class), the controller admits a converged
  standby into the ONLINE raid without any consumer disruption: a leased
  quiesce window (`--skip-rebuild` carried SPDK patch), typically
  100–200 ms. Two window shapes, chosen per rejoin by a delta estimator:
  - **inline fenced-final-delta** (small delta, the common case): the
    standby leg itself is equalized to the final snapshot `E_f` inside the
    quiesce and added. Window is O(delta); no follow-on phase.
  - **esnap** (large/unestimable delta): an esnap-clone head is added in an
    O(1) ~150 ms window, then a background **localization** replays the
    data and re-parents the head locally. Until localization completes the
    head depends on the source node's `E_f` export — a short, observable
    exposure window (typically 2–5 s; O(delta) for cold volumes).

**The single most important fact:** the volume's sync record — the
`flint.csi.storage.io/replica-sync-state` PV annotation — is the
**authority**. SPDK state is derived from it, never the other way around.
Every autonomous decision (assembly membership, catch-up source selection,
rejoin eligibility) reads it, and every manual remediation in §6 is a
record edit, not an SPDK edit. A replica head's canonical lvol name
(`vol_<vol>_replica_<idx>`) is **not** stable across recoveries — after an
esnap rejoin the live head is a `_hr`-named clone with a fresh uuid. The
record's `active_lvol_uuid` is how everything finds the live head; code or
operators addressing heads by canonical name will be wrong exactly when it
matters.

## 2. Enabling and configuring

All Tier-1/2 machinery is dark by default. Enable **as a set** on the
controller — hot rejoin admits what catch-up converges, and catch-up
replays what the scheduler cuts:

```
FLINT_EPOCH_SCHEDULER=enabled
FLINT_CATCHUP=enabled
FLINT_HOT_REJOIN=enabled
```

Running with `FLINT_CATCHUP` disabled on a multi-replica cluster is
actively hazardous: a restage with a never-healed stale replica falls back
to the 1.3.0 forced-stale-admission path (mixed stale/current reads,
evented as a divergence hazard). This was observed live during 7b-0
validation.

| Variable | Default | Meaning |
|---|---|---|
| `FLINT_EPOCH_SCHEDULER` | disabled | Common epoch snapshot cuts. |
| `FLINT_EPOCH_INTERVAL_SECS` | 300 | Cut cadence (T_snap). Test clusters run 30. |
| `FLINT_EPOCH_RETAIN` | 6 (min 1) | Epochs retained (K). `K × T_snap` bounds the longest outage healed incrementally; older marks fall back to a full build. |
| `FLINT_CATCHUP` | disabled | Stale catch-up + standby chase. |
| `FLINT_CUTOVER` | disabled | Tier-1 RWO rejoin-bounce / RWX cutover planner. |
| `FLINT_HOT_REJOIN` | disabled | The Tier-2 trigger loop + reconciler. Turning this on is the deliberate acceptance of the `skip_rebuild` patch. |
| `FLINT_HOT_REJOIN_MAX_LAG` | 1 | A standby must trail by ≤ this many epochs to be rejoined (localization/final-delta stays small). |
| `FLINT_HOT_REJOIN_RETRY_SECS` | 300 | Per-volume back-off after an **unwound** window — every attempt costs the consumer a quiesce. |
| `FLINT_HOT_REJOIN_WINDOW_TARGET_MS` | 2000 | Committed windows slower than this emit `HotRejoinWindowSlow`. |
| `FLINT_HOT_REJOIN_INLINE_DELTA_MAX_MIB` | 64 | Estimated deltas at or below this take the inline window; larger take esnap. 0 disables inline entirely. See §7 for tuning. |
| `FLINT_HOT_REJOIN_LEASE_MS` | 10000 | Quiesce lease (auto-release bound on writer stall if the controller dies inside a window). |
| `FLINT_HOT_REJOIN_AER_WAIT_MS` | 3000 | Bound on each in-window namespace-appearance wait (esnap path). |
| `FLINT_CONTROLLER_REAP` | **enabled** (node agent) | Dead NVMe controller reaping; set `disabled` to opt out. `FLINT_CONTROLLER_REAP_STRIKES` default 3. |

**Trigger policy (what gets hot-rejoined automatically):** any attached
multi-replica RWO volume that did *not* opt into the disruptive
`rejoin-bounce` path, is not a synthetic-RWX backing PV, and has no
per-volume opt-out — whenever its most-converged standby trails by
≤ `FLINT_HOT_REJOIN_MAX_LAG`. RWX volumes stay on Tier-1.

**PV annotations:**

| Annotation | Who writes it | Meaning |
|---|---|---|
| `flint.csi.storage.io/replica-sync-state` | controller/node agent | **The sync record** (JSON; §4). Operator-writable only per §6. |
| `flint.csi.storage.io/hot-rejoin: "disabled"` | operator | Per-PV opt-out from hot rejoin. |
| `flint.csi.storage.io/rejoin-bounce` | operator | Opt-in to the Tier-1 disruptive bounce; such volumes are never hot-rejoined (disjoint classes). |
| `flint.csi.storage.io/replica-health` | node agent | Present only while degraded (or `localizing` during esnap exposure); cleared when healthy. |
| `flint.csi.storage.io/data-path-lost` | node agent | Cutover-flagged data-path loss marker. |

## 3. What normal recovery looks like — and when NOT to intervene

Expected timeline for a leg failure (numbers from the live drill record,
30 s epoch interval, 60 s orchestrator ticks; scale detection/tick items
up proportionally for longer intervals):

| Stage | Typical | Signal |
|---|---|---|
| Failure detected, replica marked `stale` | 4–35 s | `ReplicaStale` |
| Catch-up (revert + delta replay) → `standby` | tens of seconds (delta) to minutes (full build) | `ReplicaCatchupStarted`, `ReplicaStandby` |
| Trigger fires (lag gate + 60 s tick) | 1–2 ticks | intent visible as `stale` + `hot_rejoin` marker |
| Quiesce window | 100–200 ms esnap; ~1–4 s inline | `HotRejoinSucceeded` (per-step timings in the event) |
| Localization (esnap path only) | 2–5 s warm; O(delta) cold | `HotRejoinLocalized` (carries exposure duration) |
| Kill → fully redundant, single failure | 2–4 min | record all `in_sync` |
| Kill → fully redundant, multi-failure (N≥3) | 7–12 min | see below |

**Multi-failure recovery is a deliberate cascade, not a parallel sprint.**
All failed replicas chase epoch deltas concurrently, but bulk catch-up
runs one replica per volume-cycle and admission is serialized per volume
by a shared claim. Rejoin order is cheapest-catch-up-first, **not** kill
order. Each rejoiner immediately becomes a preferred (non-consumer)
source for the rest, fanning load off the survivor.

Consequently, all of the following look alarming and are **normal —
do not intervene**:

- A stale replica sitting for minutes with its lag counting up while
  another replica rebuilds: cascade pacing. Watch for forward progress
  events, not for the lag number.
- Standby lag oscillating 1↔2: the chase and the cut cadence interleaving.
- `ReplicaCatchupBaseUncovered` followed by `ReplicaFullBuildStarted`: the
  replica's shared base epoch was retired while it sat stale (a stale
  replica pins retention only once its catch-up session starts). The
  full build is thin-aware (holes skipped) and self-selected.
- `ReplicaChaseSourcesExhausted`: a chasing standby was demoted to stale
  because no live source chain covers its mark (typically after the
  sources themselves rebuilt). The bulk/full-build path owns it from
  there. Investigate only if it repeats on the same replica.
- One wasted `FLINT_HOT_REJOIN_RETRY_SECS` cycle after `HotRejoinUnwound`:
  two known-benign leaks self-heal on the following pass (a leftover
  `E_f` export the second unwind removes; a record epoch pointing at the
  unwound cut that the next scheduler cut moves past).
- A single ~1–4 s writer pause with no other symptoms: an inline window's
  quiesce. At 5 writes/s the esnap window is invisible.
- `HotRejoinScrubbed` after a controller restart: the reconciler disposing
  of an uncommitted window's artifacts. That is the crash contract
  working.
- `kubectl` shows the `spdk-tgt` container in CrashLoopBackOff after
  repeated kills in a short span: kubelet backoff (~2.5 min at 8
  restarts), not a product issue. It revives on its own.

**The golden rule:** before touching anything, pull the events (§4) and
check whether *anything* changed in the last two orchestrator ticks
(~2 min). The autonomous machinery converged every drill scenario it was
designed for; the procedures in §6 are for the specific states listed
there, each of which is distinguishable by the record + events.

## 4. Observability

### Reading the sync record

```sh
PV=pvc-xxxxxxxx-...
kubectl get pv "$PV" -o go-template='{{index .metadata.annotations "flint.csi.storage.io/replica-sync-state"}}' | jq .
```

Key fields per replica entry:

- `sync_state`: `in_sync` | `stale` | `standby`.
- `last_epoch`: newest epoch known present on that replica; lag = distance
  from the record's `current_epoch`.
- `active_lvol_uuid`: set when the live head's uuid differs from the
  immutable identity in volumeAttributes (i.e. after any revert or
  rejoin). **This — via `live_lvol_uuid` = `active_lvol_uuid` falling back
  to `lvol_uuid` — is the only correct way to address a replica head.**
- `hot_rejoin`: the in-progress rejoin marker (holds the `E_f` epoch).
  Decode: `stale` + marker = window intent written, not yet flipped;
  `standby` + marker = window committed, localization in progress (esnap
  exposure). While set, the replica belongs exclusively to the hot-rejoin
  reconciler.
- `since` / `reason`: when and why the last transition happened.

Volume-level: `current_epoch`, `epochs[]` (retained, oldest first),
`retention_pin` (oldest epoch a catch-up still needs — the scheduler will
not retire at or past it).

### Events

All orchestrators event against the PV:

```sh
kubectl get events -A --sort-by=.lastTimestamp \
  --field-selector involvedObject.kind=PersistentVolume,involvedObject.name=$PV
```

| Event | Severity | Meaning / action |
|---|---|---|
| `ReplicaStale`, `ReplicaStandby`, `ReplicaAdmitted`, `ReplicaCatchupStarted`, `ReplicaFullBuildStarted` | info | The Tier-1 pipeline progressing. No action. |
| `ReplicaCatchupBaseUncovered` | info | Full-build fallback selected (base retired). No action. |
| `ReplicaChaseSourcesExhausted` | warn | Standby demoted to stale — no covering source. Self-heals via bulk path; investigate if repeating. |
| `ReplicaNeedsFullRebuild` | warn | Full build needed but withheld/disabled. Needs attention. |
| `ReplicaCatchupFailed` | warn | One failed cycle; tolerated. **Every 60 s persistently → §6.5.** |
| `HotRejoinSucceeded` | info | Window committed; carries per-step timings. |
| `HotRejoinLocalized` | info | Esnap exposure closed; carries exposure duration. |
| `HotRejoinUnwound` | warn | Window aborted cleanly; retried after back-off. One repeat is normal (§3); persistent → investigate the step named in the event. |
| `HotRejoinWindowSlow` | warn | Committed window exceeded the target. Tune per §7. |
| `HotRejoinScrubbed` / `HotRejoinAdopted` / `HotRejoinDemoted` | info | Crash-decode arms resolving an interrupted rejoin. |
| `HotRejoinReconcileFailed` | warn | Localization resume failed a cycle. Self-heals (scrub + fresh window); persistent on a current image → §6.3. |
| `VolumeDataPathLost` / `VolumeDataPathRepaired` / `VolumeDataPathRestored` | **page** | Consumer raid vanished under a live attachment / came back. §6.6 — the workload usually needs a bounce even after repair. |
| `EpochCutFailed` | warn | One missed cut; tolerated. Persistent = source node trouble. |

### The dashboard

The controller pod serves a dashboard + JSON API on `:8080`
(`/api/volumes`, `/api/events`, `/api/overview`; per-volume replica sync
states, raid member health, and the engine event timeline). Note: the
admin password is generated fresh at **every controller pod start** and
printed in the pod log (`one-time admin password`); sessions die with the
pod.

## 5. Intervention ground rules

1. **The record is the authority.** Remediate by editing the record and
   letting the orchestrators converge SPDK state to it — never by
   "fixing" SPDK objects to match your expectation.
2. **Never delete an lvol** that is (a) any record's `active_lvol_uuid` /
   identity uuid, or (b) a member of a live raid. Verify both, every time
   (§6.4).
3. **Controller-only rolls.** Ship controller fixes with
   `kubectl -n flint-system set image deploy/flint-csi-controller ...`.
   Do not roll the node DaemonSet casually — it bounces `spdk-tgt` on
   every node, a cluster-wide data-plane event.
4. **Record edits are small and targeted**: one replica entry, set
   `reason` to say it was an operator repair, leave everything else
   untouched.
5. Before demoting any replica, confirm **at least one other replica is
   `in_sync`** — the acked-write guarantee lives there.

## 6. Procedures

### 6.0 Foundation: patching the sync record

Read, transform with `jq`, write back via `kubectl annotate --overwrite`:

```sh
PV=pvc-xxxxxxxx-...
KEY=flint.csi.storage.io/replica-sync-state
REC=$(kubectl get pv "$PV" -o go-template="{{index .metadata.annotations \"$KEY\"}}")
echo "$REC" | jq .   # inspect first, always

NEW=$(echo "$REC" | jq -c --arg node <node-name> '
  (.replicas[] | select(.node_name == $node)) |= (
      .sync_state = "stale"
    | del(.active_lvol_uuid)
    | del(.hot_rejoin)
    | del(.reverted_to)
    | .reason = "operator record repair: <why>"
  )')
kubectl annotate pv "$PV" --overwrite "$KEY=$NEW"
```

Adjust which fields you touch per the specific procedure below. The
controller's next tick (≤60 s) acts on the edit.

### 6.1 Record says `in_sync` but the head lvol is gone — recovery deadlock

**Symptom:** assembly refuses with "in_sync peer unavailable, below
minimum"; the named lvol does not exist on the node; the health monitor
cannot re-label because there is no online raid. (Cause class: the head
was deleted out-of-band — historically a sweeper bug; potentially a human
with an RPC socket.)

**This is deadlock by design** — the record is authority, and it claims a
replica that isn't there. Remediation is the record patch in §6.0 exactly
as written: mark that replica `stale`, drop `active_lvol_uuid` (and
`reverted_to`/`hot_rejoin` if present). Catch-up then rebuilds the leg
from a healthy source. If the volume was down to zero live legs, see §6.6
for the consumer-side tail.

### 6.2 Wedged `standby` + `hot_rejoin` marker after a long controller outage

**Symptom:** a replica sits at `standby` with the marker set; the
reconciler's resume attempts fail wanting epochs that no longer exist
(outage outlasted the GC horizon, `K × T_snap`); meanwhile the leg may
still be serving raid writes with no epoch credit.

**Options, in order:**
1. **Wait.** Local GC drift can move the resume base into the retained
   window; the B1 drill converged this way in ~2 min. Cheap to try for a
   few epochs' worth of time.
2. **Patch the record** (§6.0): clear `hot_rejoin`, set `sync_state` to
   `stale`, drop `active_lvol_uuid`. Confirm another replica is `in_sync`
   first (rule 5). Tier-1 re-chases the leg from scratch; the abandoned
   head is reaped by the chase's revert.

### 6.3 Persistent `HotRejoinReconcileFailed`

Since the coverage-probe hardening (`tier2-7b4.4`), localization backfill
fails over across sources and this event should appear at most
transiently; even before it, the reconciler's backstop (scrub + fresh
window) converged the observed episode in ~4 min unattended. If it
repeats for more than ~10 min on a current image: capture the record +
events (it is a bug worth reporting), then remediate as §6.2 option 2 —
same guard, same effect.

### 6.4 Guarded delete of an abandoned `_hr` head

Normally unnecessary — the chase reaps superseded `_hr` heads
automatically (since `tier2-7b3.2`). Manual fallback, e.g. after
restoring from very old images. An `_hr` lvol is safe to delete **only
if both**:

1. Its uuid is **not** any replica's `live_lvol_uuid` in the record
   (check `active_lvol_uuid` and the identity `lvol_uuid`).
2. It is **not** a base bdev of any raid (check on the consumer node).

Verify and delete through the node agent's SPDK proxy (port 9081 in the
node pod):

```sh
NODE_POD=$(kubectl -n flint-system get pod -l app=flint-csi-node \
  --field-selector spec.nodeName=<node> -o name)
# inspect: uuid + aliases of the stray
kubectl -n flint-system exec "$NODE_POD" -c flint-csi-driver -- \
  curl -s -XPOST localhost:9081/api/spdk/rpc -H 'Content-Type: application/json' \
  -d '{"method":"bdev_get_bdevs","params":{"name":"<lvs>/vol_<vol>_replica_<n>_hr"}}'
# raid membership (run against the CONSUMER node's pod)
... -d '{"method":"bdev_raid_get_bdevs","params":{"category":"all"}}'
# only after both checks pass:
... -d '{"method":"bdev_lvol_delete","params":{"name":"<lvs>/vol_<vol>_replica_<n>_hr"}}'
```

A stranded `_hr` head the record does not reference may hold stale data;
it must never be adopted as a source — delete it, don't repurpose it.

### 6.5 `ReplicaCatchupFailed` every cycle / trigger starvation

Two known causes, both environmental:

- **Stale `Node` objects.** A deleted machine whose Node object lingers
  costs every orchestrator tick a ~3 s TCP timeout, which can starve the
  trigger's tick-race indefinitely (observed: 8/8 eligible ticks lost;
  won the very next tick after cleanup). `kubectl delete node <stale>` —
  first thing to check when a ready standby (lag ≤ max) sits unadmitted
  for many minutes.
- **Source-coverage errors** ("epoch-N not found in the source lineage")
  on images older than `tier2-7b4.2`: that image line lacks coverage-aware
  source failover and can wedge permanently after multi-failure episodes.
  Upgrade the controller; no record surgery needed — the fixed selector
  heals the volume on its first cycle.

### 6.6 Total data-path collapse (raid gone, consumer EIO)

**Symptom:** `VolumeDataPathLost`; writer gets hard EIO; raid
unregistered on the consumer node.

1. **Wait for autonomous reassembly** — the node agent reassembles from
   in-sync + admitted standbys, typically ~2 min (`VolumeDataPathRepaired`
   / `ReplicaAdmitted`).
2. **Then bounce the consumer pod.** A workload that was writing during
   the outage almost always needs it: ext4 gives up on the vanished device
   before the initiator's reconnect window, so the mount points at a dead
   instance even after the raid is back. (The Repaired event text says
   exactly this.) A workload that was *idle* through a short episode may
   resume without a bounce — check before restarting.

Never bounce first: the restage would race the reassembly and buys
nothing.

## 7. Tuning notes

- **Inline ceiling vs. the window target.** The inline window is
  O(allocated clusters) at an observed ~16 MiB/s (small-delta floor;
  improves with size). The 64 MiB default ceiling therefore extrapolates
  to ~4 s windows — past the 2 s default target
  (`HotRejoinWindowSlow` fires). Where the 2 s bound is strict, set
  `FLINT_HOT_REJOIN_INLINE_DELTA_MAX_MIB=32`; where a rare 4 s pause is
  acceptable, keep 64 and treat the event as informational. `0` forces
  every rejoin through the esnap path (O(1) window, but re-introduces
  exposure). Note that allocated-cluster deltas run far above logical
  write volume on metadata-heavy workloads (1 MiB clusters + journal
  scatter: a 5-append/s writer produced 26 MiB epochs).
- **Epoch cadence.** `K × T_snap` (retain × interval) is the longest
  outage healed by delta replay; anything older takes a full build.
  Shorter intervals also mean fresher standbys and smaller
  final-deltas/localizations. Cost: snapshot churn per replica.
- **Retry back-off.** `FLINT_HOT_REJOIN_RETRY_SECS` (300) exists because
  every window attempt quiesces the consumer. Lower it only for drills.
- **replicas=2 structural caveat.** During esnap exposure the source node
  is a single failure domain for the whole volume (it hosts both the
  surviving leg and the head's esnap parent) — source death mid-exposure
  is fail-stop (zero acked-write loss, drill-proven) but takes
  availability until §6.6 recovery. With ≥3 replicas an independent leg
  survives. The inline path has no exposure at all — one more reason it
  is the default for small deltas.

## 8. Known residuals (as of 2026-07-03)

Documented, bounded, not yet fixed — so an operator recognizes them:

- **Esnap localization resume walks the source lineage** (B1): after a
  controller crash mid-localization, resume can transiently fail against
  GC until local drift converges it (§6.2 is the backstop). The fix
  (prefer the local chain, which provably contains `E_f`) is owed.
- **Consumer-blind epoch cutting** (phase-6): epochs can be recorded while
  consumer writes cannot flow (e.g. during a repair episode). Harmless
  under revert-first admission, but epoch numbers are not proof of
  consumer progress.
- **Controller crash inside the quiesced sub-span**: live-measured
  2026-07-03 via fault injection. The restarted reconciler's decode
  releases the orphaned quiesce as its first RPC, so the consumer write
  stall is bounded by container-restart + reconcile latency (measured
  3.5 s); if the controller cannot come back at all, the data-plane
  lease auto-expiry still bounds it at `FLINT_HOT_REJOIN_LEASE_MS`
  (measured 10.0 s ± 0.12 s with no controller alive). The writer
  resumes with no error in both cases.
- **`admit_standbys_at_stage` final-delta source selection** assumes fresh
  standby marks (safe today — the chase gates admission — but not yet
  coverage-probed like the other source-selection sites).

## 9. Appendix: drill primitives

For reproducing scenarios on a test cluster (these were the campaign's
standard tools):

- **Leg kill:** `kubectl -n flint-system exec <node-pod> -c spdk-tgt -- kill 1`.
  Repeated kills accrue kubelet CrashLoop backoff (§3).
- **Precision crash (±2 ms):** from the privileged `spdk-tgt` container on
  the target pod's node, write `1` to the victim container's cgroup v2
  `cgroup.kill` file (host cgroup fs is rw there). `cgroup.freeze` is the
  matching pause primitive. `kubectl delete pod --force` has 1.5–6 s of
  jitter and cannot hit sub-second spans.
- **In-window fault injection (drill-only):** spans inside the hot-rejoin
  window (~150 ms) are too narrow even for `cgroup.kill`. Set
  `FLINT_HOT_REJOIN_FAULT=abort_after_quiesce` on the controller to abort
  the process the instant W1 commits, leaving the quiesce lease orphaned
  (the auto-release drill; expect a writer stall of restart + reconcile
  latency, ~3.5 s — the decode's defensive unquiesce — with
  `FLINT_HOT_REJOIN_LEASE_MS` as the no-controller backstop). **Disarm
  immediately after the first fire**
  (`kubectl set env ... FLINT_HOT_REJOIN_FAULT-`) — the restarted
  container still has the env and will fault every rejoin attempt.
  Never set in production.
- **Acked-write-loss check:** run a writer appending `seq timestamp` lines
  with fsync; after the episode, assert the sequence has no gaps
  (`awk 'NR>1 && $1!=prev+1 {print}; {prev=$1}'`). Every drill gate in the
  campaign was "zero gaps".
- **Narration:** poll the dashboard API (`/api/volumes`, `/api/events`) at
  ~2 s cadence for replica-state transitions; grab the admin password from
  the controller pod log after any roll.
- **Integrity scrub (fs-allocated-only):** cut a snapshot of every leg
  under ONE `bdev_raid_quiesce` lease (source each snapshot from the
  record's `live_lvol_uuid`, never the canonical name; a 3-leg cut takes
  ~60 ms). Export the snapshots over NVMe-oF (`:scrub:` NQNs and
  `scrub-*` lvol names are invisible to the sweep/reaper) and attach them
  on one node. **Do not trust the snapshots' fs metadata directly** — a
  quiesced snapshot is crash-consistent, not checkpointed; recent
  allocations live only in the ext4 journal. Instead: clone one leg's
  snapshot, `e2fsck -fy` the clone (journal replay), take the allocated
  block map from the replayed clone (`dumpe2fs` free-block complement),
  then hash the *pristine* snapshot devices over exactly those extents
  and compare. Full-device digests are expected to diverge benignly on
  reused lvstores (unwritten thin-cluster remainders: stale bytes on
  organically-grown legs vs zeros on rebuilt legs); the fs-allocated
  digests must be identical. Clean up: disconnect, delete the scrub
  subsystems, delete the clone before its parent snapshot, delete the
  snapshots. Worked example: `UnansweredOn7b.md`, "fs-allocated-only
  scrub" (2026-07-03).
