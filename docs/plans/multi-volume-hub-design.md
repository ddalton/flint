# Multi-volume hubs + fork-from-barrier — the agentic-harness topology

Status: **design sketch** (2026-08-18). Step 0 — the two-level-lease TLA+
module (`formal/FlintTierSession.tla`, 7 gate runs) — is DONE; no code
exists yet. Design of record for the tier machinery this builds on:
`docs/plans/s3-tier-l2-design-review.md` (L2) and the flint-lite chart
(`flint-lite-chart/`).

## 1. The workload, and what it changes

The target is an **agentic harness**: fleets of agents on multiple k8s
clusters, each running ordinary tools (git, build systems, sqlite-embedding
tools, grep) against workspace files whose durable home is S3, through a
real POSIX interface. Three workload facts drive everything here:

- **Tool workloads are metadata storms.** `git status` / builds / test
  runs are thousands of `stat`/`readdir`/`open` per second over small
  files. This is the regime that makes S3-passthrough mounts unusable and
  is served sub-millisecond from a hub's local namespace (sqlite).
- **Agents run arbitrary tools, so the POSIX surface must be genuine.**
  sqlite transactions, fcntl locks, atomic rename — the multicluster
  campaign's canaries (934ae78). No harness can audit every tool for
  S3-mount compatibility; "real NFSv4.2" is the only durable answer.
- **Writes are naturally partitioned.** Each agent writes its own
  workspace; sharing is read-mostly (repo bases, toolchains, weights) plus
  published results. The harness has a scheduler — the one component that
  can enforce write affinity for free.

The drawbacks this design answers, from the same conversation: the MDS is
a per-volume bottleneck (answer: many hubs, many volumes, sharded);
cross-cluster traffic (answer: **S3 is the only inter-cluster channel**);
cold reads (answer: §7 — required work, not an option); volume lifecycle
at session rate (answer: volumes become registry rows, not helm releases).

## 2. The semantics contract (what is and is not weakened)

**Within a volume: nothing changes.** Every writer of a volume goes
through the volume's owning hub. Two agents writing the same file
concurrently — same pod, same cluster, or different clusters mounted to
the owning hub — get exactly today's flint-lite semantics: enforced
byte-range locks, close-to-open coherence, atomic rename, full NFSv4.2.
The concurrency domain is the volume, served by one hub, unchanged.

**Across the volume boundary, concurrency is refused, not relaxed:**

- A **satellite** is a read-only snapshot at barrier granularity
  (staleness ≤ owner flush floor + refresh interval; cross-file
  consistent — it refreshes manifest-to-manifest, never a torn mix).
- A **fork** is a divergent copy by construction; it never merges back.
- A second would-be *writer* of a volume is **fenced at claim time** —
  loudly, by the epoch machinery — never silently given weaker semantics.

This matches how the harness already wants to work: the scheduler places
write workloads on the owning cluster; truly-concurrent cross-cluster
writers to one volume were never in the workload.

## 3. Topology invariant

One hub (or several — §8) per cluster. Consumers mount their **local**
hub over in-cluster NFS: stock kernel clients, zero footprint, zero
credentials. Hubs talk to S3 in-region. **No hub ever addresses another
hub.** The complete inter-cluster channel inventory: volume cells, hub
session cells, DR manifests, fork markers, data objects — all in the
bucket. Owner death interrupts no satellite's reads (they degrade to
"frozen at last barrier", not "down"), and no consumer mount ever changes
across an ownership migration — the epoch moves, not the endpoints.

## 4. The multi-volume hub

A volume stops being "a hub" and becomes **a row in a hub's registry**:
*(bucket prefix, local subtree, epoch/volume cell, state partition, role ∈
{owner, satellite, frozen})*.

- **Namespace**: one NFSv4 pseudo-root, `hub:/volumes/<name>` per volume;
  kernel clients mount subtrees natively.
- **Control plane**: a small admin API on the hub (separate port,
  Secret-token auth): `POST /volumes` (create / fork / claim),
  `POST /volumes/X/barrier`, `POST /volumes/X/release`,
  `DELETE /volumes/X`, `GET /volumes`. Volumes are created at session
  rate; they cannot be helm values. Durable registry = `registry.db`; on
  restart the hub re-claims owned volumes and re-imports satellites.
- **State partitioning**: **one sqlite per volume** (`state/<vol>.db`) +
  the registry — not volume_id columns in one db. Drop-volume is a file
  delete, capture WALs don't contend across volumes, crash corruption is
  isolated, and it mirrors the per-prefix partition on the S3 side. Keep
  an LRU of open handles; ~1k live volumes is a measurement gate (fd,
  WAL, memory), not a design assumption.
- **Shared machinery, per-volume accounting**: capture/planner/flush/
  hydrate/evict become worker pools over per-volume queues. Knobs go
  two-tier: per-volume-class settings (a *transcripts* class gets a long
  `flushFloorSecs` — the econ gate's billing firewall, now per class; a
  *code workspace* class a short one) + hub-global resource caps. Disk is
  shared: one NOSPC reserve, one watermark, eviction picks victims
  coldest-first **across** volumes.
- **Reserved namespace**: `.flint/` per prefix as today; plus
  `.flint-hubs/<hub-id>` at bucket scope (§5) and
  `<prefix>/.flint/forks/<fork-id>` markers (§6).

## 5. The two-level lease (modeled: `FlintTierSession.tla`)

The one piece of today's design that does NOT scale to thousands of
volumes is per-volume heartbeats: 1,000 volumes × one PUT/10s ≈ 8.6M
requests/day ≈ **$1,300/month of pure heartbeat**. The fix is the
Chubby/ZooKeeper move — leases attach to *sessions*:

- **Hub session cell** `s3://bucket/.flint-hubs/<hub-id>`: ONE heartbeat
  per hub, token-rotating (the real-S3-gate-bug-1 lesson, inherited).
- **Volume cell** (per prefix, where the epoch cell lives today):
  `{owner hub-id, session generation, claim generation}`. Claim/release
  are per-session CAS events, not timer traffic. Heartbeats are O(hubs).
- A volume's liveness = its owner's **session** quiet-time, judged by the
  store's tokens (never wall clocks — the cross-cluster skew posture we
  already drilled).

**The subtlety the model exists for**: S3 CAS conditions apply to ONE
object. A takeover cannot atomically bind "the session is quiet" to "the
volume cell is mine" — different keys. The naive protocol (claim the
volume cell straight off the quiet count) leaves the loser's session cell
untouched: its heartbeats keep SUCCEEDING, the beat-fail fence never
fires, and it believes — and publishes — forever. **The immortal
multi-volume zombie.** The protocol therefore **deposes first**: the
watcher CAS-writes a `deposed` flag into the owner's session cell
(If-Match the quiet-observed token), converting its flaky local evidence
into *stable store state*, and only then claims volume cells naming that
session. The loser's next beat is a CAS mismatch ⇒ fence ⇒ `exit(70)`,
hub-scoped: one failed beat forfeits ALL its volumes at once (correct,
because the quiet evidence indicts the hub, not one volume).

Model results (7 runs, wired into `scripts/check-tla.sh` — gate now 172):

- Strict theorems: a beating session is never deposed (rotation's teeth);
  a clean release is a publish barrier (drain's teeth).
- Strict liveness: a hub that lost a volume eventually stops believing
  it owns it; a dead session's volumes are eventually claimed.
- **`NoDepose` mutation finds the immortal-zombie lasso** — the naive
  two-level lease is machine-checked unsound; `NoFence` finds the same
  lasso through the swallowed-412 door; `NoRotate` re-finds real-S3 gate
  bug 1 one layer up; `NoDrain` lands a release straggler under the next
  owner's reign.
- One required-fail probe (`ProbeStale`): the plan/land window exists at
  this layer exactly as `FlintTierEpochProbeStale` states it one layer
  down — bounded by the fence, arbitrated by the data plane's epoch
  stamps (`Inv_NoSuccessorOverwrite` stays the data plane's theorem;
  this module deliberately does not re-model publishes' CAS arbitration).

**Ownership lifecycle**:

- *Clean handoff*: owner drains the volume's flush, writes a final
  barrier + manifest, CAS-writes a **release token** into the volume
  cell. Any hub claims a released cell instantly — no lease wait
  (that's what makes session migration between clusters cheap; the
  drain is what makes it safe — modeled).
- *Crash takeover*: judge the session quiet (store tokens), **depose the
  session**, then claim its volume cells at leisure; MPU abort-sweep per
  claimed prefix as today. RPO = flush floor, unchanged from single-hub
  DR.
- *Zombie owner*: depose ⇒ beat-fail ⇒ `exit(70)`; the restarted hub
  comes back with a fresh session and **re-claims through the ordinary
  claim path** (self-recognition at both layers) — a deposed hub's pod
  restarts as a claimant, not an owner, and should rejoin as satellite
  for volumes it lost.
- *Idle volumes cost zero requests* — no per-volume timer exists at all.

## 6. Fork-from-barrier

Create workspace W from base B at barrier N in O(metadata), zero copy:

1. `POST /volumes {name: W, from: B@N}`: claim W's fresh prefix, write
   `.flint/base = {prefix_B, barrier N, manifest etag}` under W, and drop
   a **fork marker** at `prefix_B/.flint/forks/<W-id>` (so deleting B can
   refuse while forks exist — coordination via S3, never via a registry
   that would need cross-cluster chatter; leaked markers reconcile
   against the fork's cell).
2. Import-as-stubs from B's manifest M_N (existing machinery), except
   each stub's hydration source is **B's object at the versionId recorded
   in M_N**. This is where bucket versioning finally becomes
   load-bearing: forks pin versionIds, so B's later publishes never
   disturb a fork and no reference counting is needed for correctness.
3. Reads of untouched files hydrate from B's pinned versions; writes
   capture and publish under W's prefix, flipping that file's provenance
   base→own. Renames/deletes are W-local metadata + tombstones.
4. W's manifests carry **per-file provenance** (own key, or base
   key+versionId) and stay FLAT: forking a fork copies provenance
   entries, so chains never deepen and reads never walk an overlay.

The one real knot: **noncurrent-version lifecycle vs fork lifetime**.
Rule for v1: the bucket's noncurrent retention floor ≥ max fork age,
plus a `materialize` operation (server-side COPY of base-referenced
objects into W's prefix) to detach any fork that must outlive the floor.
Fork destroy = delete W's prefix + remove the marker; B is never touched.

Scale expectations from the existing drills: a 10k-file base imports in
14s (tier-scale.sh) — a fork lands in seconds materializing nothing.
Very large trees may need lazy dentry materialization (stub files created
on first lookup); that's the escape hatch, not the v1.

## 7. Cold reads — required work, staged by what agents actually touch

Cold-reads-after-fork/refresh are this topology's **steady state**, so
the L4-measured posture (72.5s/GiB: whole-file, sequential 8MiB ranged
GETs) is not shippable here. Two stages, ordered by the workload:

1. **Parallel small-object hydration** (first, mandatory): agent
   workspaces are source trees — thousands of KB-scale files. Pipeline
   the hydration queue, raise `hydrateConcurrency` well past 4, prefetch
   siblings on readdir-then-open patterns. No correctness surface
   changes: whole-file hydration per file stays as-is.
2. **Range-serve for large artifacts** (second): commit chunks
   individually, wake a parked reader when *its range* is present —
   first-byte-cold drops from ~72s to ~one chunk fetch for the
   weights/artifacts volumes. This changes the hydrating flag into a
   present-ranges bitmap and touches the partial-restore truncate-back
   reasoning, so **`FlintTierMarker` must be extended in the same change**
   (the whole-file flag is load-bearing in that model). Pinned to this
   step, deliberately not before.

Upload already parallelizes (13.3s/GiB measured); nothing owed there.

## 8. Scaling shape

- **Reads** scale per cluster (each hub is an edge cache; a changed
  object crosses cluster↔S3 once per cluster per version, then serves at
  NFS speed from PVC + page cache).
- **Metadata** scales per hub (full namespace local to every satellite).
- **Writes** scale per volume — one owner hub per volume, deliberately
  (that IS the consistency point; see §2). The write-scaling dimension
  is volume count, and the harness's workspace-per-agent model supplies
  it naturally.
- **Intra-cluster**: volumes share nothing but disk and S3, so run K
  hubs per cluster and let the harness map workspace→hub. That plus
  per-cluster hubs is the full answer to "the single MDS bottleneck".
- **Satellite refresh signaling**: poll the manifest key with a
  conditional GET (`If-None-Match` last etag). 10s poll × 100 satellites
  ≈ 10 req/s ≈ pennies/month; the interval is the staleness knob. No
  EventBridge/SQS, no cross-cluster channel.
- **Multi-region** (recorded for later): S3 CRR can make satellite data
  reads region-local, but CAS is not coherent across replicated buckets —
  the session cells, volume cells, and manifest authority live in exactly
  ONE home bucket; only data reads may come from replicas.

## 9. The session lifecycle, end to end

1. Harness: `POST /volumes {name: ws-agent42, from: base-repo@latest}` →
   ready in seconds.
2. Pod mounts `hub:/volumes/ws-agent42` (in-tree NFS PV). Full POSIX.
3. Tools run: metadata local; cold reads hydrate in parallel; writes
   flush on the volume class's floor. Turn boundaries may force barriers
   (durable, resumable checkpoints with consistent manifests — also the
   eval-reproducibility pin: any cluster can mount the identical tree).
4. Session end: `release` (drain + final barrier + release token) or
   `DELETE` (purge; retention policy decides audit copies).
5. Resume anywhere: any cluster's hub claims instantly off the clean
   release; only files the agent touches move.

## 10. Build order

0. ~~`FlintTierSession.tla` + 7 gate runs~~ — **done** (this document's
   §5; the gate grows 165 → 172).
1. Volume registry + admin API + multi-volume namespace/serving (the
   biggest single piece; includes per-volume sqlite partitioning and
   cross-volume evict/NOSPC accounting).
2. Two-level lease in code: session cell + depose + hub-scoped fence +
   clean-release token; `successor_check` verifies volume cell + session;
   startup re-verify per volume. (The TLA module is the spec; its
   mutations name the regressions.)
3. Parallel small-object hydration (§7 stage 1).
4. Satellite role: no claim, read-only export (`NFS4ERR_ROFS` on
   mutating ops), manifest-poll + import-refresh loop.
5. Fork-from-barrier: provenance manifests, versionId pinning, fork
   markers, `materialize`; fork/DR drill (fork under churn; base delete
   refused; retention-floor breach surfaced loudly).
6. Ownership migration UX (release/claim API arms, restart-as-satellite)
   + a two-cluster handoff drill (clean, crash, zombie — the chaos-H
   pattern at the session layer).
7. Range-serve + `FlintTierMarker` extension (§7 stage 2).

Not in scope until demand: cross-cluster concurrent writers to one
volume (refused by design), automatic claim-on-first-write (invites
ping-pong; migrations stay API-triggered with a minimum-hold rate limit).

## 11. Open risks (carried honestly)

- sqlite-per-volume ceilings at ~1k live volumes — measure before
  building past the LRU.
- Fork retention vs lifecycle rules — the one correctness-economics knot
  (§6); v1 rule is a floor + `materialize`, and the reporter should WARN
  when a fork's pinned versions approach the floor.
- Stale filehandles on volume delete under a live mount — the API must
  enforce sessions-end-before-delete.
- Fork markers are advisory (crash between marker and cell leaves a
  leak) — reconciliation sweep needed, claim-time, like the MPU sweep.
- The admin API is a new attack surface on the hub — token auth +
  NetworkPolicy at minimum; it never holds bucket credentials beyond
  what the hub already has.
