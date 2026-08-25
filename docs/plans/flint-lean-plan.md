---
title: flint-lean — checkout/publish sidecar + control-plane gateway
status: PLAN OF RECORD, v2 (post-adversarial-review 2026-08-24) — lean is
  the default front end for the agent-harness architecture; the FUSE track
  is DEPRIORITIZED (docs/flint-fuse-architecture.html stays as the designed
  escalation for too-large-to-materialize workspaces). The hub remains the
  escalation for live-shared POSIX.
type: design-impl-spec
created: 2026-08-24
architecture: docs/flint-lean-architecture.html
lineage: extracted from the 14-agent FUSE review; v2 incorporates its own
  12-agent adversarial review (6 dimensions x dedicated skeptic; 18
  CONFIRMED + 5 DOWNGRADED-but-real, 1 refuted — record in §10)
governs:
  - spdk-csi-driver/src/bin/flint-sync.rs (new — the lean sidecar binary)
  - spdk-csi-driver/src/tier/ (subtree lease re-scope, claim identity, manifest split + MERGE writer, materializing import)
  - spdk-csi-driver/src/s3_surface/ (new — the one S3-dialect library)
  - flint-hub-gateway/ (control plane: publish intents, inbox, sync diffs, HITL writes)
  - src/lite_operator/ (lean injection, layout allocation, claim stamping, principal split)
---

# flint-lean: checkout/publish + gateway — implementation plan (v2)

> **Status (2026-08-25):** Phase 0a SHIPPED — `lean/formal/LeanSubtree.tla`,
> its own 20-run TLC gate (`lean/formal/check.sh`), deliberately separate
> from flint's `formal/` corpus; the model's findings are recorded in
> `lean/formal/README.md`. Phase 1+2 CORE SHIPPED, and Phase 3
> GATEWAY VERBS SHIPPED — the `flint-lean` crate (`lean/sidecar/`,
> deliberately its own crate over the extracted `crates/flint-store`
> layer, so the lean test loop never builds the hub crate): flint-sync
> (checkout/restart matrix, 7-step barrier, inbox/window cell,
> merge-capable manifest writer, 412 park/AdoptOwn policy, guarded GC,
> claim/rotation lease, sync verb) + flint-lean-gateway (HITL
> PUT/GET with the window gate, snapshot/status, and the sidecar verbs
> with PER-REQUEST epoch validation — P5's teeth on every
> gateway-mediated write). 17-leg battery (`lean/sidecar/src/tests.rs`)
> against MemoryStore's full conditional semantics. **Phase 0b FIRST
> NUMBERS MEASURED** (`docs/plans/flint-lean-0b-measurements.md` —
> loopback floors; tentative v1 file-count cap 250k; fan-out +
> HEAD-tick levers landed and rig-validated; occupancy lock added).
> **Phase 4 CORE SHIPPED** — `spdk-csi-driver/src/lean_operator/` +
> `flint-lean-operator` bin (separate controller, shared image):
> FlintLeanWorkspace CRD, claim/adopt both arms, derived-probe
> injection brain, admission webhook (TLS, cert Secret, registration),
> `flint-lean-chart`, and two kind e2e suites (raw 7/7, chart 8/8).
> **PHASE 6, KIND HALF SHIPPED** — `lean/e2e/run-chaos.sh`, 12 legs
> green, record in `docs/plans/flint-lean-chaos-drill.md`. Two
> corrections it produced, both recorded there: the P5 data-plane
> residual is now MEASURED (a deposed straggler landed 7.6k further
> data PUTs after rotation — control plane held, data path did not),
> and **§2.2's four failure effects are PROXY effects, not gateway
> effects** (C8 vs C12: a gateway outage costs only "HITL writes
> fail"), which locates P5 enforcement at the proxy — where the
> sidecar's writes actually go, and where the epoch already rides in
> GenerationStamps. Still open: multipart compose for > whole_put_max
> files, routing sidecar barriers through the gateway verbs by default
> (the deferral the drill measures the cost of), proxy conformance gate
> + proxy-shaped 0b/0c re-measure, the three Phase 6 legs kind cannot
> run (real spot NODE reclaim, burst N≥1000, real-proxy rates),
> per-workspace bearers/SigV4.

## 1. Summary and scope

The lean variant is a third front end over the unchanged bucket format:
an **unprivileged** sidecar materializes the workspace as plain files at
pod start, the app runs against a real local filesystem with zero
interception, and changed files publish at the flush cadence. The
deployment is **two planes**: a full S3 proxy (existing component, holds
the real S3 credentials — agents contact it only; each project has a
dedicated bucket or prefix) is the DATA plane; the stateless flint
gateway is the CONTROL plane (publish arbitration, the HITL inbox, sync
diffs, auth). No FUSE, no privilege, no kernel floor, no ENOTCONN class.
Pods hold zero bucket credentials.

**v1 scope:** full checkout (no partial/lazy); one writer per workspace
subtree; workspaces bounded in BOTH axes — bytes (emptyDir budget) and
**file count (checkout refuses above the measured Phase-0b cap;
"GiB-scale" alone is not a scope statement)**; files > 64 MiB publish
via **streaming multipart compose** (the existing `Plan::Compose`
machinery — proxied, no presigning; `put_whole` is never fed past
`whole_put_max`, matching the code's own routing); AWS S3 + MinIO behind
the deployment proxy; the `sync` verb (HITL); **Kubernetes ≥ 1.29
(native sidecar containers are mandatory, §2.4)**.

**Simplifications kept from v1 of this plan** (each a FUSE-review
finding that narrows under the lean shape): no online refresh (sync is
explicit); no durable REMOVAL journal (full checkout keeps deletes
RPO-symmetric — but see the emptyDir bookkeeping below, which is NOT
that journal); no eviction engine.

**Revised in v2 (review):** "no sqlite in the sidecar" survives, but
"no durable sidecar state at all" does not — the sidecar persists
**plain-file bookkeeping on the emptyDir**: a checkout-complete marker,
the baseline manifest snapshot (seq + per-entry ETags, rewritten at
every barrier and sync), a pre-barrier intent journal of (key, new-ETag)
pairs, and a pod-incarnation holder id. A container restart over a live
tree is a first-class state (§2.1), not "re-checkout".

## 2. Components

### 2.1 flint-sync (new bin target)

An ordinary process reusing `tier::` as a library.

**checkout** — `tier::import`'s manifest lane with `materialize: bytes`
(hydrate fan-out). Verifies claim identity (§4 P1) and BOTH budgets
(bytes vs the emptyDir sizeLimit; file count vs the v1 cap — the
manifest `Inventory` already carries both). Writes the
checkout-complete marker + baseline snapshot LAST. The agent container
cannot start before the marker exists (§2.4 native-sidecar gating).

**publish barrier** — every `floorSecs` and on preStop:

1. **Consume the inbox first** (§2.2): if the subtree's
   `.flint/inbox` cell is non-empty, run the forced mini-sync (fetch
   the listed foreign adds/edits under the sync conflict policy) and
   advance the baseline. A barrier NEVER runs against an unconsumed
   inbox — this is what makes HITL uploads structurally
   un-amputatable.
2. Scan-diff (path, size, mtime, ctime; optional small-file hash)
   against the **persisted baseline snapshot — never a re-seeded
   bucket manifest**. Deletion basis: a path is delete-eligible only
   if absent in THIS scan AND the previous scan (absence must survive
   two consecutive scans — the rename-vs-walk race guard) AND present
   in the sidecar's own baseline.
3. Write the intent journal (keys + expected ETags) to the emptyDir.
4. Upload changed/new files: ≤ 64 MiB via guarded `put_whole`
   (If-Match per key); larger via streaming multipart compose. After
   each upload, re-stat the source; on drift, re-queue for the next
   barrier (torn-upload guard — residual risk for forced barriers is
   documented, §3).
5. **Manifest CAS** — a MERGE-capable variant of `write_at_barrier`
   (new code; the reused writer rewrites the whole document from the
   local walk and cannot merge): three-way merge of baseline vs local
   vs bucket-current. Foreign entries (present in bucket-current,
   absent from baseline and local) are PRESERVED and queued into the
   inbox for the next sync — never dropped, never deleted.
6. **Deletes last**, after the CAS, as garbage collection of keys the
   new manifest no longer references — **etag-guarded** (HEAD +
   compare the baseline ETag; a key whose ETag the sidecar does not
   recognize is never deleted; note S3 DELETE itself is
   unconditional, hence the HEAD guard). Reordered from v1
   (upload→delete→CAS dangled the manifest on crash).
7. Rewrite the baseline snapshot + clear the intent journal.

**Per-key 412 policy (was unstated):** on an If-Match 412 during
upload, HEAD the key and compare the `flush_uuid`/GenerationStamps
metadata the store already stamps: my own crashed/torn earlier PUT ⇒
adopt the ETag and continue; foreign (a HITL write) ⇒ **park the path,
emit it in the conflict report, never If-Match-overwrite an ETag the
sidecar did not itself publish** — the inherited flush arbitration is
LOCAL-WINS-overwrite (`flush.rs:1391`) and is explicitly NOT reused
here. The arbitrate loop is re-derived for flint-sync (it cannot lift
`flush.rs` wholesale); the same rule covers ambiguous proxy errors
(response lost after S3 applied the PUT).

**sync verb (v1, HITL)** — harness-invoked (exec/localhost HTTP),
never background. **Sync BEGINS with a full scan-diff** — "locally
dirty" means dirty per THAT scan against the baseline, never per the
last barrier's snapshot (otherwise sync honors a remote delete over
the agent's un-scanned latest work). Then: gateway manifest+inbox diff
since the baseline seq; apply — locally-dirty wins, remote deletions
only on locally-clean paths, adds/changes fetched via the proxy;
machine-readable conflict report + exit status; baseline advances.
Serialized against the sidecar's own barrier.

**Restart matrix (first-class, was the review's top theme):**

| State on wake | Meaning | Action |
| --- | --- | --- |
| No marker, empty tree | fresh pod | checkout |
| No marker, partial tree | checkout crashed (agent never started — gated) | resume checkout (import local-wins = natural resume) |
| Marker present | container restart over a LIVE tree | **never re-materialize**: reload the persisted baseline, rescan to rebuild dirt, self-recognize the lease via the persisted incarnation id, continue barriers |

Re-checkout over a live tree is forbidden: import's local-wins
protects only PRESENT paths, so it would resurrect the agent's
unpublished deletes into a running session — an implicit non-quiescent
sync. The persisted incarnation id is emptyDir-scoped (a replacement
pod gets a fresh emptyDir ⇒ fresh identity), so it is NOT the
workspace-stable identity P4 forbids; only the same pod's restarted
container inherits it, which is exactly when self-supersede is safe.
The claim-wait bookkeeping {last_token, quiet_polls} also persists so
container restarts resume the takeover observation instead of
resetting it.

**lease lifecycle** — claim the subtree cell at start, heartbeat,
clean release in preStop (the 17 s path). Unclean-death lockout
(~60–110 s) is ACCEPTED for v1 and the replacement pod's probes are
budgeted for it (§2.4); the P4 fencing actor is NOT a v1 deliverable
(its "pod deletion finality" trigger is unsound — force-delete does
not SIGKILL and a partitioned node's process outlives its pod object;
deposing on it would manufacture the §2.2 straggler).

**preStop drain** — early drain begins at observed
`deletionTimestamp`; final barrier after the agent exits (native
sidecar ordering). Sizing is now explicit and operator-enforced:
`dirtySetCapGiB ≤ (grace − agentExitBudget − release) ×
proxyMeasuredDrainRate / maxCoScheduledLeanPodsPerNode` — the 8–13
s/GiB figure predates the proxy and is re-measured in Phase 0b;
spot reclaim is a NODE event, so co-drained pods share the proxy.
`dirtySetCapGiB` counts only files smaller than itself — a single hot
file larger than the cap must not convert the valve into a
continuous-republish forcing function; such files get a per-file
publish cooldown instead (the amplification pathology, §6).

### 2.2 Gateway (control plane; per-cluster, stateless, N replicas)

- **Publish intent + the barrier-window/inbox cell.** Statelessness
  needs a durable coordination substrate: `<subtree>/.flint/inbox` — a
  CAS cell that is BOTH the HITL inbox and the barrier-window token.
  The sidecar's intent CAS-marks the window open (with a deadline +
  epoch, so a dead sidecar cannot wedge HITL past lease expiry) and
  clears it after the manifest CAS; every replica checks it before
  admitting a UI write, closing the two-replica race the review proved
  (a barrier through replica A was invisible to replica B). Starvation
  bound: after N consecutive HITL refusals, the next barrier admission
  defers to the pending write.
- **The HITL write path.** UI writes land as objects plus **inbox
  entries — never direct manifest edits** (a gateway manifest bump is
  amputated by the sidecar's next whole-document CAS; the review
  reproduced this end-to-end three ways). The sidecar consumes the
  inbox at every barrier (§2.1 step 1) and at every sync; a HITL
  upload therefore survives any number of barriers without an explicit
  sync. Refuse-vs-queue during an open window is pinned: refuse with
  Retry-After derived from the window deadline.
- **Takeover fence rotation (the straggler fix).** A successor's claim
  path CAS-rewrites the subtree manifest (seq++, content-identical)
  BEFORE serving, so a deposed predecessor's in-flight manifest CAS
  412s — the inherited fences do not cover lean (the MPU sweep aborts
  only multipart; per-key If-Match has no purchase because a
  checkout-only successor rotates no data ETags; the epoch residual
  note in `epoch.rs:28` assumes a PUBLISHING successor). Additionally
  every sidecar PUT carries its epoch in the GenerationStamps and the
  gateway's validation is **per-request** (granularity now stated),
  rejecting writes whose epoch is below the subtree cell's.
- **Proxy posture (decided):** grade-1 primary; conditional-header
  fidelity is a conformance GATE, hardened per the review: probes have
  **must-FAIL arms** (a stale If-Match that MUST 412; If-None-Match:*
  over an existing key that MUST 412 — stripping is only observable as
  a forbidden success), enumerate **every proxy replica** (headless
  Service), re-run on a cadence and on observed proxy version change
  (refusing barriers while drifted), and cover every op class a fence
  rides: conditional multipart Complete + its 200-with-error-body,
  copy-source-if-match, DeleteObjects, ETag stability across
  PUT/multipart/HEAD/GET.
- **Auth:** sidecar holds a proxy token + a gateway bearer; SigV4
  verification arrives with `s3_surface`.
- **Failure mode (corrected §7):** gateway/proxy down ⇒ publishes
  pause AND checkouts/restarts wedge AND sync is unavailable AND HITL
  UI writes fail loudly. Running pods keep serving local files.

### 2.3 s3_surface (ONE dialect, three placements)

Unchanged from v1 of this plan: the S3 dialect everywhere (hub live
view, gateway door, localhost adapter later); floor = SigV4 +
conditional writes + full multipart with the three documented traps
(multipart ETag shape, 200-with-error-in-body, 3× IO concat);
extensions: RenameObject, `x-amz-meta-flint-*`, activity
classification. Hub dual-serves `/files` + the dialect for one release.

### 2.4 Operator / webhook

- **Native sidecar containers are MANDATORY** (initContainer with
  `restartPolicy: Always`, K8s ≥ 1.29): the startupProbe on a plain
  container gates nothing the design needs — kubelet starts siblings
  on `started`, and pod deletion SIGTERMs regular containers in
  parallel. Native sidecars give both the start gate (agent cannot
  start before checkout-complete) and the stop ordering (drain scans a
  quiescent tree after the agent exits). Without the start gate, an
  early agent's scaffold files are silently clobbered by rename-
  materialize or published over user data by local-wins — both
  observed in the review.
- **Probes derived, never fleet constants:** the webhook computes the
  sidecar's startupProbe budget from the workspace Inventory × the
  Phase-0b proxy-measured checkout rate + the unclean-death lockout
  (~110 s) + headroom; "checkout complete" moves to readiness where
  claiming states need to hold longer. (The hub's 600 s default kills
  a 20 GiB checkout at the only measured rate.)
- **Principal split for bucket-admin ops:** bootstrap (versioning /
  lifecycle read-write / bucket-wide MPU probe) runs ONLY under the
  operator/provisioner principal — never in flint-sync. The takeover
  MPU sweep's `list_uploads` is bucket-wide on the wire
  (`s3.rs:352` MinIO workaround): through a correctly project-scoped
  proxy it is DENIED and would fail every claim — the sweep becomes an
  operator-side job (or the proxy serves a scoped MPU view), and the
  conformance gate gains an anti-vacuity arm proving a
  directory-prefixed probe upload is FOUND on MinIO (do not restore
  the raw server-side Prefix while MinIO is in scope).
- **Claim adopt arm (was undefined at its hard case):** the claim
  carries a durable, user-declared project identity from the CR SPEC
  (stable across CR delete/recreate — never the CR UID). On a 412 at
  share birth the operator GETs the standing claim: identities equal ⇒
  adopt (recreate-over-own-data is a designed lifecycle: DR, GitOps,
  cross-cluster moves); different ⇒ refusing status condition, never
  on-the-fly adoption. Checkout/intents verify the injected identity
  against the cell by value. BOTH drill legs ship (adopt-own must
  succeed; foreign-on-reused-prefix must refuse), each proven to fail
  against the other arm's naive implementation.
- CR surface: flush profile (floorSecs, dirtySetCapGiB, per-file
  cooldown, quiesceBoundSecs) as a durability contract; emptyDir
  sizeLimit + file-count budget; `failurePolicy: Fail`, two replicas,
  opt-in selectors.

## 3. What v1 deliberately does NOT solve (stated honestly)

- Cross-pod live visibility (snapshots + sync only).
- **Torn whole-file uploads of files written DURING a forced barrier**
  (preStop under a SIGTERM-ignoring agent, pressure valve): two-pass
  scan + re-stat + re-queue shrink the window; the residual is
  documented in the CRD contract. A file hotter than
  `quiesceBoundSecs` faces the stated dilemma: publish possibly-torn
  or defer (growing RPO) — the knob picks, the report says which.
- mtime-granularity scan evasion (unchanged from v1; drill leg tries
  to evade and the residual is stated).
- **Empty directories do not round-trip** (the manifest is file-based;
  observed on the 0b rig's git e2e — `.git/objects/{info,pack}` etc.
  vanish across checkout). Benign for git, which recreates them
  lazily; a dir-marker entry is the v2 lever if a workload needs it.
- Within-project enforcement is gateway-validated + CAS-cooperative
  (the proxy's tenancy is project-granular — §9 Q6); bucket
  versioning makes clobbers recoverable.

## 4. Preconditions

- **P1 — claim identity** with the §2.4 adopt semantics.
- **P2 — per-subtree manifests + designated root owner.**
- **P3 — declared layout** (operator-allocated, CAS).
- **P4 — subtree lease re-scope** of `epoch.rs` + the §2.2 takeover
  fence rotation; fencing actor explicitly deferred (v1 accepts the
  lockout).
- **P5 (new) — proxy conformance + scoping:** the hardened gate of
  §2.2, plus the principal split of §2.4, as startup-enforced facts.
- **Formal model gate:** P3+P4 (lease + layout + grant/handoff,
  including the deposed-straggler barrier), the **inbox/window cell**,
  and the **barrier × HITL-write product** (the sync arm alone was too
  narrow — the review's worst finding lived exactly in the un-modeled
  product).

## 5. Phases (observed-red discipline throughout)

- **Phase 0 — models + measurements:**
  0a. Formal model per §4.
  0b. Checkout AND publish rates through a PROXY-SHAPED rig, on two
      axes: bytes (s/GiB) and **file count (100k and 1M entries: walk
      wall-time, digest cost, manifest bytes, sidecar peak RSS,
      barrier duration, HITL-refusal window)** — acceptance numbers,
      and the v1 file-count cap falls out of them.
  0c. Burst rig N≥1000 proxy-shaped; acceptance criterion is the
      BINDING constraint: sustained GB/s per proxy replica (both NIC
      directions) + the replica-sizing formula
      `ceil(N×W×2/(T×replica_GBps))`; checkout admission (a
      gateway-issued concurrency token) so bursts queue instead of
      timing out probes. A measured proxy bottleneck is the explicit
      trigger that un-defers grade-2 presigned reads.
- **Phase 1 — bucket format:** claim (+adopt), layout, manifest split
  + **merge-capable barrier writer**, inbox/window cell, epoch
  re-scope. Lifecycle at fleet scale: shared buckets get ONE
  bucket-wide MPU-abort rule (S3 caps 1,000 rules/bucket);
  bucket-per-project keeps the per-share rule. Conformance vs MinIO +
  real S3, through a proxy, with the must-fail arms.
- **Phase 2 — flint-sync:** checkout/resume, restart matrix, barrier
  (order, merge, guards, 412 policy, multipart), sync-with-scan,
  drain. Battery: container-restart-with-unpublished-delete (file must
  NOT reappear; delete publishes next barrier), crash-replacement,
  scan-evasion, directory-rename-during-barrier, hot-file-through-
  preStop, proxy-response-lost mid-barrier (AdoptOwn convergence),
  git/pip/sqlite e2e.
- **Phase 3 — gateway:** intent/window/inbox verbs, HITL path, sync
  diff, per-request epoch validation, bearer + revocation, hardened
  conformance gate, outage drill asserting ALL FOUR effects (publish
  pause, checkout wedge, sync dead, UI writes fail with a distinct
  error), observability deliverables: per-pod RPO gauge
  (seconds-since-last-barrier + unpublished bytes), publish-failure
  and checkout-wedged alerts, conflict-report surfacing.
- **Phase 4 — operator/webhook:** native-sidecar injection, derived
  probes, layout/claim (+both adopt legs), principal-split bootstrap,
  operator-side MPU sweep, kind e2e (incl. agent-starts-after-
  checkout with a plain-container failing control).
- **Phase 5 — s3_surface on the hub** (unchanged; independent).
- **Phase 6 — cluster drill:** spot NODE-level reclaim (all pods near
  cap + one SIGTERM-ignoring agent as the control), hard-kill loss =
  RPO exactly, claim legs (both arms), HITL-upload-survives-two-
  barriers-no-sync, UI-edit + agent-edit conflict leg (both versions
  recoverable, conflict surfaced, never a silent winner), straggler
  barrier after takeover, burst wave with admission, gateway kill
  mid-fleet, mixed hub+lean bucket.

## 6. Economics (amplification stated, not hidden)

- Idle = S3 only, structurally. Lease cells exist only while a writer
  runs.
- Publish amplification is REAL and bounded by policy, not physics: a
  2 GiB hot file re-uploads whole per barrier (~2.9 TiB/day at 60 s
  cadence) — hence the per-file cooldown for > `whole_put_max` files,
  `dirtySetCapGiB` excluding over-cap files, and the CRD stating
  bytes/day expectations. UploadPartCopy of unchanged ranges for the
  large-file class is the designed v2 lever.
- Manifest cost scales with FILE COUNT (whole-document rewrite per
  changed barrier): the 0b axis measures it; skip-digest-on-no-diff
  and per-entry serialization caching are v1 mitigations.
- Checkout: full-workspace GET per pod start through the proxy —
  Phase 0b/0c numbers × workspace size; per-NODE cache is FUSE/
  mount-pod territory, out of scope.

## 7. Failure model (corrected)

Loss bounds: RPO per pod; preStop drain best-effort within the §2.1
arithmetic; hard crash drains nothing; torn-hot-file residual per §3.
No ENOTCONN class, no recovery-order contract, no daemon in the data
path. **Gateway/proxy outage: publishes pause + starting pods wedge +
sync unavailable + HITL writes fail** (running pods serve local files
throughout). Container restart: bounded by the restart matrix — no
resurrection, lease self-recognized, dirt rebuilt by rescan.

## 8. Explicit deferrals

- Partial checkout (returns the removal journal + checked-out-set).
- Presigned grade-2 (un-deferred only by a measured 0c proxy
  bottleneck).
- On-demand foreign-subtree refresh beyond sync.
- SigV4 pod identity / TokenReview at the gateway.
- Per-NODE shared checkout cache; GCS/Azure.
- UploadPartCopy range publish for large files.
- P4 fencing actor (needs node-level fencing semantics + the takeover
  rotation in place first).

## 9. Decisions and open questions

1. Gateway home: extend `flint-hub-gateway` (recommended) or new
   binary — open. **Deployment: per-cluster (DECIDED).**
2. Large files: **DECIDED by review — streaming multipart compose in
   v1** (no presigning needed); >5 GiB works; checkout refuses only
   what Phase 0b says is unpublishable.
3. Auth v1: bearer only vs + SigV4 — open.
4. **Sync verb ships in v1 (DECIDED)** — semantics per §2.1.
5. UI read path: through the proxy or the gateway — open (writes go
   through the gateway either way).
6. **Proxy tenancy: project-granular, dedicated bucket or prefix per
   project (DECIDED).** Cross-project isolation proxy-native;
   within-project residual accepted for v1 (versioning recovers).

## 10. Review record (2026-08-24, 12 agents, 6 dimensions × verify)

18 CONFIRMED + 5 DOWNGRADED-but-real; 1 refuted. One line each; all
folded above.

Critical: HITL upload amputated/deleted by the barrier's whole-document
CAS-rewrite — found independently by THREE dimensions (→ inbox cell +
merge writer + baseline-pinned deletes, §2.1/§2.2); barrier order
dangled the manifest + stranded ETags (→ upload→CAS→GC-deletes + intent
journal + 412 policy); publish-side both-writers collision silently
LOCAL-WINS overwrites the UI edit via inherited arbitration (→ park +
conflict report, never overwrite foreign ETags); sync consults dirt
that cannot exist yet (→ sync begins with a scan); deposed straggler's
barrier is unfenced — MPU sweep/If-Match/manifest CAS all fence the
wrong party (→ takeover manifest rotation + per-request epoch
validation); >64 MiB/5 GiB files check out but can never publish (→
streaming multipart, `put_whole` never past `whole_put_max`); plain
sidecar + startupProbe gates neither start nor stop (→ native sidecars
mandatory, K8s ≥1.29); container restart over a live tree resurrects
unpublished deletes + 60 s lease lockout (→ restart matrix + emptyDir
bookkeeping + incarnation id).

Major: two-replica gateway cannot serialize the barrier window (→ the
window cell); replacement pod crash-loops through the lockout — probe
budgets reset the observation clock (→ derived probes + persisted
{last_token, quiet_polls}); claim adopt arm undefined — wedge vs
adoption dilemma (→ declared identity + both legs); conformance probe
was startup-only/one-replica/wrong op classes (→ hardened gate);
bucket-admin ops through a scoped proxy fail the claim path (→
principal split + operator-side sweep + MinIO anti-vacuity arm);
scan-vs-mutation races (→ two-pass absence + etag-guarded deletes +
re-stat); barrier cost scales with file count (→ 0b axis + cap +
mitigations); whole-file amplification + dirtySetCap inversion (→
cooldown + cap semantics); drain arithmetic unsound (→ §2.1 formula +
early drain + node-level spot leg); gateway-outage story
self-contradictory + probe budgets (→ §7 + derived probes); proxy
failure mid-barrier spec gap (→ 412/ambiguous policy); 0c oracle
measured the non-binding constraint (→ replica-sizing acceptance +
admission).

Refuted (correctly): "partial checkout mass-deletes on container
restart" — the barrier cannot run without a completed checkout's
baseline, and import's local-wins makes re-checkout a natural resume;
the surviving arms (resurrected deletes, lease lockout) are folded.
