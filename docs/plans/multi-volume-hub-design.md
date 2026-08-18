# Multi-volume hubs + fork-from-barrier — the agentic-harness topology

Status: **design sketch v2 — RESHAPED after ultracode review** (2026-08-18).
Step 0 — the two-level-lease TLA+ module (`formal/FlintTierSession.tla`,
7 gate runs, gate 165→172) — is DONE; no code exists yet. The ultracode
review (14 agents, 8/8 findings adversarially CONFIRMED, all critical)
validated the lease protocol and the topology and **refuted the v1 reuse
claims** — the shipped tier code is owner-only / etag-only /
process-scoped / single-db in exactly the places v1 said "reused
untouched". Every confirmed finding is folded in below and marked
**[Fn]**. Design of record for the tier machinery this builds on:
`docs/plans/s3-tier-l2-design-review.md` (L2).

## 1. The workload, and what it changes

The target is an **agentic harness**: fleets of agents on multiple k8s
clusters, each running ordinary tools (git, build systems,
sqlite-embedding tools, grep) against workspace files whose durable home
is S3, through a real POSIX interface. Three workload facts drive
everything here:

- **Tool workloads are metadata storms.** `git status` / builds / test
  runs are thousands of `stat`/`readdir`/`open` per second over small
  files — served sub-millisecond from a hub's local namespace, unusable
  over S3-passthrough mounts.
- **Agents run arbitrary tools, so the POSIX surface must be genuine.**
  sqlite transactions, fcntl locks, atomic rename — the multicluster
  campaign's canaries (934ae78).
- **Writes are naturally partitioned.** Each agent writes its own
  workspace; sharing is read-mostly plus published results. The harness
  scheduler enforces write affinity for free.

Drawbacks answered: MDS-per-volume bottleneck (many hubs, many volumes,
sharded); cross-cluster traffic (**S3 is the only inter-cluster
channel**); cold reads (§7 — required work); volume lifecycle at session
rate (volumes become registry rows, not helm releases).

## 2. The semantics contract (what is and is not weakened)

**Within a volume: nothing changes.** Every writer of a volume goes
through the volume's owning hub. Concurrent writers to one file — same
pod or different clusters mounted to the owner — get exactly today's
flint-lite semantics: enforced byte-range locks, close-to-open
coherence, atomic rename, full NFSv4.2.

**Across the volume boundary, concurrency is refused, not relaxed:** a
second would-be writer is fenced at claim time, loudly; forks are
divergent copies by construction; satellites are read-only.

**The satellite consistency contract, stated honestly [F8]:** a
satellite advances **barrier-to-barrier** — its *namespace* is always
some manifest's consistent cut, never a mix of cut N and cut N+1 across
*newly opened* files. But NFS gives no snapshot isolation over a live
mount: a refresh applies per-file rename-over updates, so a reader
holding an fd across a refresh keeps the OLD bytes for that file
(pin-until-close — the shipped F17b/c OPEN-anchored fd machinery already
provides this) while a fresh `open()` of a neighbor sees the NEW cut. A
read *window* spanning a refresh therefore sees old-through-held-fds and
new-through-new-lookups. That is close-to-open coherence, the same
promise NFS makes everywhere else — the v1 phrase "never a torn mix" is
kept only in the namespace/new-opens sense above.

## 3. Topology invariant

One hub (or several — §8) per cluster. Consumers mount their **local**
hub over in-cluster NFS: stock kernel clients, zero footprint, zero
credentials. Hubs talk to S3 in-region. **No hub ever addresses another
hub.** The complete inter-cluster channel inventory: volume cells, hub
session cells, DR manifests, per-volume identity objects (§4), fork
markers, data objects — all in the bucket. Owner death interrupts no
satellite's reads (frozen at last refreshed cut, not down), and no
consumer mount ever changes across an ownership migration.
*(Survived review unpierced.)*

## 4. The multi-volume hub

A volume is **a row in a hub's registry**: *(bucket prefix, local
subtree, volume cell, state partition, class, role ∈ {owner, satellite,
frozen})*.

- **Namespace**: one NFSv4 pseudo-root, `hub:/volumes/<name>`; kernel
  clients mount subtrees natively.
- **Control plane**: an admin API on the hub (separate port,
  Secret-token auth): `POST /volumes` (create/fork/claim),
  `/barrier`, `/release`, `DELETE`, `GET /volumes`. **The API's claim/
  release verbs are step-1 scope, not step 6 [F5]** — the session
  lifecycle (§9) is unusable without them.
- **State partitioning — decided against the shipped code, not asserted
  [F4].** The shipped tier speaks to ONE `StateBackend` (state.db)
  carrying NFS state (clients/sessions/stateids/locks) *and* every tier
  durable verb, threaded as a single Arc through flush/evict/hydrate/
  import/reporter; the pre-ack capture guarantee is one batched
  transaction to that backend; capture marks are `(dev, ino)` and the
  disk is shared, so ino cannot select a database. v1's
  "one sqlite per volume" contradicted all of that. **Decision: option
  (a) — one state.db, tier rows gain a `volume_id` column and every
  tier verb becomes volume-scoped.** This preserves the
  single-transaction pre-ack drain (the L2 A3 property v1 silently
  reversed), needs no ino→db routing, and keeps NFS-state cohabitation
  coherent. Costs accepted: drop-volume is a scoped delete lane (tier
  rows + NFS-state purge for the subtree), not a file unlink; capture's
  path-less lanes resolve volume_id from the mount-subtree map at mark
  time. Per-volume dbs (option b) stay recorded as the fallback if the
  single WAL measurably serializes capture at fleet scale — with the
  pre-ack guarantee re-derived over N commits before any such move.
- **Registry durability — the bucket stays sufficient [F7].** v1 made
  `registry.db` the sole authority for prefix binding, role, and class,
  silently dropping the shipped "bucket alone is restorable" DR
  property. Fixed: (1) every create/claim mirrors the registry row into
  `<prefix>/.flint/volume` (name, class, created-by, fork base); (2)
  the volume's prefix is stamped into its own durable tier rows; (3)
  hub identity comes from **configuration** (helm value/Secret), never
  from PVC-persisted `server_id` — after PVC loss the sweep must be
  able to say "owner == me"; (4) DR = claim-time bucket sweep rebuilds
  the registry from `.flint/volume` objects (the multi-prefix runbook
  is step-1 scope); (5) satellite rows are harness-re-issued by
  declaration — they exist in no bucket and that is documented, not
  accidental. A **never-yet-flushed workspace** has no prefix binding
  in the bucket by definition: the identity object is written at
  volume *creation* (before first mount), so the binding exists from
  minute zero even though no data has flushed.
- **Shared machinery, per-volume accounting**: worker pools over
  per-volume queues; knobs two-tier (per-volume-class settings + hub
  global caps); one NOSPC reserve, one watermark, eviction coldest-first
  across volumes.
- **Reserved namespace**: `.flint/` per prefix (now incl. `volume`
  identity object); `.flint-hubs/<hub-id>` at bucket scope (§5);
  `<prefix>/.flint/forks/<fork-id>` markers (§6).

## 5. The two-level lease (modeled: `FlintTierSession.tla`)

Per-volume heartbeats do not scale (1,000 volumes × one PUT/10s ≈ 8.6M
req/day ≈ **$1,300/month**). The Chubby/ZooKeeper move:

- **Hub session cell** `.flint-hubs/<hub-id>`: ONE token-rotating
  heartbeat per hub.
- **Volume cell** per prefix: `{owner hub-id, session generation,
  claim generation}`. Claims/releases are per-session CAS events;
  heartbeats are O(hubs); idle volumes cost zero requests.
- Liveness = the owner's **session** quiet-time, judged by store tokens.

**Depose-first, machine-checked.** S3 CAS conditions one object; nothing
binds "the session is quiet" to "the volume cell is mine". The naive
protocol (claim the volume cell off the quiet count) leaves the loser's
session cell untouched — its heartbeats keep succeeding and it publishes
forever. The `NoDepose` mutation finds exactly this **immortal
multi-volume zombie** lasso: the protocol must first CAS the
quiet-observed token into a `deposed` flag on the owner's SESSION cell
(flaky watcher evidence → stable store state), then claim volume cells.
The loser's next beat 412s ⇒ fence, **hub-scoped** in evidence terms.

**The publish stamp is CLAIM GENERATION — bound explicitly [F1].** The
residual stale-publish window (ProbeStale) is arbitrated by the data
plane's stamps, and v1 left *which* stamp undefined. The volume cell
carries two numbers; **session generation is per-hub and NOT monotonic
across owners** (a fresh hub deposing a long-lived one claims at sgen 1
vs the loser's 7) — stamping it would resurrect BUG 7 through the
shipped `stamps.epoch <= ours ⇒ ForeignHand ⇒ local-wins re-publish`
compare (flush.rs:1150) as silent data loss. **Claim generation is
per-volume CAS-monotonic by construction** (`ClaimCell` bumps it on
every acquire) and is the one lawful comparand. All three reused fence
legs are specified against it: `successor_check`'s filter and
store-verify compare **claim-gen ordering against the volume cell**;
the fence-on-stamp arbitration arm likewise; per-volume startup
re-verify refuses past a cell claim-gen ahead of the hub's claim.
**Owed in the gate (step 0b): extend FlintTierSession (or a small joint
module) with claim-gen-stamped publishes and port the `NoStampCheck`
mutation, so the wrong binding reproduces the BUG 7 counterexample in
the gate rather than in production.**

**Fencing is per-volume in mechanism [F5].** The shipped fence is
`exit(70)` and the shipped heartbeat treats NotFound as deposed —
correct for one volume per process, fatal for N: a `DELETE /volumes/X`
purging X's cell must not kill the hub, and one volume's deposition
must not forfeit the other N−1 *mechanically* (session-deposition
does forfeit all — that's the evidence semantics — but a per-VOLUME
cell dispute quarantines that subtree only). Re-scoped invariant: **"an
unfenced VOLUME is never served"** — per-volume guards, per-volume
quarantine, listener binding decoupled from any one contested claim
(a contested claim parks that volume as "contested", it does not block
the hub). The startupProbe posture re-derives from this.

Model results (7 runs, in the gate): strict (beating sessions never
deposed; clean release is a publish barrier — the drain is what makes
release's no-lease-wait handoff safe), liveness (lost ownership
resolves via the fence; dead sessions' volumes get claimed), and the
required-fail ProbeStale residual. The review attacked the protocol,
the hub-scoped-evidence rationale, and the economics; all held.

**Ownership lifecycle**: clean handoff = drain → final barrier +
manifest → release token in the cell → any hub claims instantly. Crash
takeover = judge session quiet → **depose the session** → claim cells →
MPU abort-sweep per claimed prefix. Zombie = depose ⇒ beat-fail ⇒
hub-wide forfeit ⇒ restart re-claims through the ordinary path
(self-recognition at both layers) and rejoins as satellite for volumes
it lost. **The release/drain/teardown orchestration is NEW CODE** — the
shipped `epoch_release` has no caller and claim() has no contested
return arm [F5]; FlintTierSession models the drain, nothing implements
it yet.

## 6. Fork-from-barrier

Fork W from base B at barrier N in O(metadata), zero copy: claim W's
fresh prefix; write `.flint/base = {prefix_B, barrier N}` under W and a
fork marker under `prefix_B/.flint/forks/<W-id>`; import-as-stubs from
M_N with hydration source = **B's objects at the versionIds recorded in
M_N**; writes publish under W's prefix and flip provenance base→own;
W's manifests carry flat per-file provenance (chains never deepen).

**versionId is a first-class field end-to-end — this is the step's real
scope [F6].** The shipped `ObjectStore` has no versionId anywhere
(get/head/copy pin by If-Match etag only), manifests record etags only,
and S3 evaluates If-Match against the CURRENT version — so on the
shipped code, the moment B republishes a key, every fork stub's guarded
GET 412s into the adopt-current arm and **silently adopts B's newer
bytes: the inversion of this section's headline promise.** Required:
versionId on the trait surface (both backends + the memory double, with
`GetObjectVersion` probed at bootstrap alongside the versioning check),
versionId in manifest entries and evicted-file metadata, and
version-aware COPY for `materialize`.

**The 412/404 arbitration contract forks by provenance [F2][F6].** The
shipped hydrator's S3-wins adopt-current arm remains correct for
exactly one provenance: **an owner hydrating its own live object**. For
*pinned* provenances — a fork reading its base version, a satellite
reading its manifest's version — adopt-current is the corruption path:
a 412 must be structurally impossible (the GET names a versionId), and
NotFound (lifecycle expired the pinned version) is a **loud park**: the
reader parks, the A12 reporter WARNs with the true cause (retention
breach), nothing adopts. This is new arbitration code, not reuse.

**Retention economics and the corrected bound [F3].** Versioning
becomes correctness-load-bearing here, and the noncurrent tail is a
first-class cost the L2 design of record already mandates pricing:
every barrier publishes a full-size new version, so one 100MiB
full-rewrite-churn file at the default 60s floor accretes ≈4.1TiB of
noncurrent bytes/month ≈ **~$97/mo — one file against the whole
$148–166/mo bill the econ GO was derived on.** Consequences: (1)
noncurrent-bytes-rate is measured in the L2 workload replays **before
step 5 builds**, and the econ gate re-runs with it; (2) per-class
`flushFloorSecs` is re-derived as a storage-tail lever, not only a
request firewall; (3) long retention floors are confined to
explicitly-forkable **base volumes** — scratch workspace prefixes get
short noncurrent expiry (the floor cannot be prefix-scoped at fleet
scale: 1,000 lifecycle rules/bucket); (4) the v1 rule "floor ≥ max fork
age" is **wrong** — S3's NoncurrentDays clock runs from *supersession*,
not fork creation, so forking an old barrier needs floor ≥
barrier-lookback + fork lifetime; fork creation **refuses** when any
pinned version's remaining life is under margin, and the reporter WARNs
as pinned versions approach expiry; (5) `DELETE /volumes` is a
version-enumerating hard delete (DELETE requests are free; the purge
kills the tail); (6) `materialize` (version-aware server-side COPY into
W's prefix) detaches any fork that must outlive the floor.

Fork destroy = delete W's prefix + remove the marker; B is never
touched. Scale: a 10k-file base imports in 14s (tier-scale.sh); lazy
dentry materialization is the escape hatch for very large trees.

## 7. Cold reads — required work, staged by what agents actually touch

1. **Parallel small-object hydration** (first, mandatory): pipeline the
   hydration queue, raise `hydrateConcurrency` well past 4, prefetch
   siblings on readdir-then-open. No correctness surface changes.
2. **Range-serve for large artifacts** (second): chunk-level commit +
   wake-on-range; converts the hydrating flag into a present-ranges
   bitmap ⇒ **`FlintTierMarker` extended in the same change**, along
   with the pinned-provenance hydration arm from §6 [F6].

Upload already parallelizes (13.3s/GiB measured); nothing owed there.

## 8. Scaling shape + satellite refresh as a protocol

Reads scale per cluster (hub = edge cache: one fetch per cluster per
version); metadata per hub (full local namespace); writes per volume
(deliberately — that IS the consistency point; volume count is the
write-scaling dimension); intra-cluster by running K hubs and mapping
workspace→hub.

**Satellite refresh is a coherence protocol, not a re-run of import
[F8].** The shipped `import_refresh` is local-wins in every lane and
has **no delete lane** — reused as-is, a satellite would freeze every
already-imported file forever and never remove an owner-deleted one.
Build item 4 is therefore: manifest-poll (conditional GET on the
manifest key — 10s × 100 satellites ≈ 10 req/s, pennies) + an **update
lane** (etag/generation change ⇒ per-file rename-over to the new
version, held fds keep old bytes until close via the shipped F17b/c fd
anchoring) + a **delete lane** (manifest omission ⇒ tombstone-respecting
unlink) + versionId-pinned hydration (§6) + live-mount concurrency with
capture/evict/hydrate rows + the FlintTierMarker extension for the
overwrite-in-place lane. Enforcement of "sessions end before volume
delete" lives in the **harness/CSI contract**, not NFS (v4 has no
MOUNT protocol to police).

Multi-region: CRR can make satellite data reads region-local, but CAS
is not coherent across replicated buckets — session cells, volume
cells, and manifest authority live in exactly ONE home bucket.

## 9. The session lifecycle, end to end

1. `POST /volumes {name: ws-agent42, from: base-repo@latest}` → ready in
   seconds (identity object written at create — §4).
2. Pod mounts `hub:/volumes/ws-agent42`. Full POSIX.
3. Tools run: metadata local; cold reads hydrate in parallel; writes
   flush on the class floor; turn barriers = durable resumable
   checkpoints (and the eval-reproducibility pin).
4. Session end: `release` (drain + final barrier + release token) or
   `DELETE` (version-enumerating purge).
5. Resume anywhere: any hub claims instantly off the clean release;
   only touched files move.

## 10. Build order (re-cut after review [F5])

0. ~~FlintTierSession + 7 runs~~ **done** (gate 172).
   **0b (owed before step 2 code): claim-gen-stamped publishes in the
   model + the ported NoStampCheck mutation [F1].**
1. **Multi-volume core + ownership lifecycle, merged**: registry +
   admin API (all five verbs incl. claim/release), volume-scoped state
   rows in the one state.db (+ scoped delete lane incl. NFS-state
   purge), per-volume guards/quarantine (listener decoupled from any
   contested claim; NotFound-on-cell ≠ hub death), session cell +
   depose + clean-release drain, claim-gen stamps in all three fence
   legs, registry mirroring to `.flint/volume` + config-stable hub
   identity + the multi-prefix DR sweep/runbook [F4][F5][F7].
2. *(folded into 1 — kept as the review/drill boundary: the two-cluster
   handoff drill — clean, crash, zombie — at the session layer.)*
3. Parallel small-object hydration (§7 stage 1).
4. Satellite role as the coherence protocol of §8 (update/delete lanes,
   versionId-pinned hydration, honest §2 contract) [F2][F8].
5. Fork-from-barrier: versionId end-to-end, provenance-forked
   arbitration, retention bound + refusal + reporter WARN, priced
   noncurrent tail + econ re-run, `materialize`; fork/DR drill [F3][F6].
6. Ownership-migration UX polish (restart-as-satellite, min-hold rate
   limit) — the mechanism itself ships in step 1.
7. Range-serve + FlintTierMarker extension (§7 stage 2 + §6 arm).

Not in scope until demand: cross-cluster concurrent writers to one
volume (refused by design); automatic claim-on-first-write.

## 11. Open risks (carried honestly; review-adds marked)

- Single-WAL capture serialization at fleet scale — the trigger for
  partitioning option (b); measure before ~1k live volumes [F4].
- **Noncurrent-tail vs churn shape**: whether the econ GO inverts is
  conditional on real per-file rewrite churn — settle by measuring
  version-bytes/day in the L2 workload replays before step 5 [F3].
- **Residual ESTALE corners across refresh rename-over**: special
  stateids, lease-recovery CLAIM_FH, LOCK across a refresh — settle
  with a lima rig test holding a lock across a rename-over [F8].
- Fork markers are advisory (crash between marker and cell) —
  claim-time reconciliation sweep, like the MPU sweep.
- `registry.db` is now a **cache** of bucket + config truth — the DR
  sweep is the recovery path and must be drilled [F7].
- The admin API is a new attack surface — token auth + NetworkPolicy;
  it holds no credentials beyond the hub's own.
- Sessions-end-before-delete is enforced harness/CSI-side; the hub can
  only make delete-under-mount safe (ESTALE), not polite [F8].

## 12. Review record

Ultracode review 2026-08-18 (workflow wf_9c23d111-84c: 5 dimension
reviewers → 8 top findings → 8 adversarial verifiers → synthesis; ~1.1M
tokens): **8/8 findings CONFIRMED, all critical, all folded in above**
— F1 stamp binding (§5), F2 satellite tear via adopt-current (§6/§8),
F3 noncurrent-tail economics + wrong retention bound (§6), F4
state-partitioning vs the single StateBackend (§4), F5 build-order
inversion / process-scoped fencing (§4/§5/§10), F6 versionId absent
end-to-end (§6), F7 registry as dropped DR property (§4), F8 satellite
refresh coherence + honest §2 contract (§2/§8). **Held under attack:**
the depose-first lease protocol and all four mutations, claim-gen
monotonicity, the heartbeat/poll economics, the topology invariant,
within-volume semantics, and the F17b/c fd-anchor layer (which defeats
the ESTALE-storm objection and makes the refresh fix cheaper).
