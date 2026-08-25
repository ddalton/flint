---
title: flint-lean — checkout/publish sidecar + stateless write gateway
status: PLAN OF RECORD (user-ratified 2026-08-24) — lean is the default
  front end for the agent-harness architecture; the FUSE track is
  DEPRIORITIZED (docs/flint-fuse-architecture.html stays as the designed
  escalation for too-large-to-materialize workspaces; no FUSE work
  scheduled). The hub remains the escalation for live-shared POSIX.
type: design-impl-spec
created: 2026-08-24
architecture: docs/flint-lean-architecture.html (extracted from flint-fuse-architecture page 5)
lineage: 14-agent adversarial review of 2026-08-24 (findings digested in flint-fuse-architecture.html); the preconditions below are re-derivations of that review's findings for the lean shape
governs:
  - spdk-csi-driver/src/bin/flint-sync.rs (new — the lean sidecar binary)
  - spdk-csi-driver/src/tier/ (subtree lease re-scope, claim identity, manifest split, materializing import)
  - spdk-csi-driver/src/s3_surface/ (new — the one S3-dialect library, three placements)
  - flint-hub-gateway/ (grows the grade-2 publish-grant arbiter + grade-1 S3 proxy)
  - src/lite_operator/ (variant: lean injection, layout allocation, claim stamping)
---

# flint-lean: checkout/publish + gateway — implementation plan

## 1. Summary and scope

The lean variant is a third front end over the unchanged bucket format:
an **unprivileged** sidecar materializes the workspace as plain files at
pod start, the app runs against a real local filesystem with zero
interception, and changed files publish at the flush cadence. The
deployment is **two planes**: a full S3 proxy (existing component, holds
the real S3 credentials — agents contact it only) is the DATA plane;
the stateless flint gateway is the CONTROL plane (publish arbitration,
HITL writes, sync diffs, auth). No FUSE, no privilege, no kernel floor,
no ENOTCONN class. Pods hold zero bucket credentials.

**v1 scope:** full checkout (no partial/lazy), one writer per workspace
subtree, GiB-scale workspaces, AWS S3 + MinIO behind the deployment's
full S3 proxy (grade 1 PRIMARY — presigned grade 2 is an optional later
optimization), the `sync` verb (HITL). **Non-goals for v1:** FUSE (separate deliverable, its own doc),
multi-writer coherence (the hub's job, forever), eviction/lazy
hydration, cross-subtree online refresh while serving, GCS/Azure
tenancy.

**The headline simplifications relative to the FUSE variant** — each one
is a review finding that narrows or disappears under the lean shape:

- **No online refresh in v1.** The FUSE review's hardest new machinery
  (inverting import's pre-listener + local-wins invariants) is out of
  scope: a lean pod writes its own subtree and refreshes foreign
  content only at explicit points (checkout at start; an on-demand
  `sync` verb that applies the manifest diff to files with no local
  dirt — local-wins preserved, no invariant inversion).
- **No durable removal journal in v1.** With FULL checkout, local
  absence = deletion, so the publish barrier's scan diff carries
  deletes symmetrically with writes: a pod that dies with unpublished
  deletes loses them exactly as it loses unpublished writes — the RPO
  contract, not the resurrection bug. (The FUSE finding — tombstones
  dying with emptyDir while import's sweep re-ingests the key forever —
  required a journal because interception-mode capture has no
  full-tree diff. The journal becomes REQUIRED again the moment
  partial checkout ships; see §8.)
- **No sqlite state backend in the sidecar.** The checkout manifest
  snapshot is the baseline; dirty tracking is in-memory between
  barriers; a crash degrades to re-checkout (RPO). No durable dirty
  bit is needed because there is no protocol ack to honor — the app's
  ack is the local filesystem's.
- **No eviction engine.** Space handling is a checkout-time budget
  check (refuse if the manifest Inventory exceeds the emptyDir budget)
  plus `sizeLimit` — not the watermark/ballast machinery.

## 2. Components

### 2.1 flint-sync (new bin target, `spdk-csi-driver/src/bin/flint-sync.rs`)

An ordinary process. Reuses `tier::` as a library:

- **checkout** — `tier::import`'s manifest lane with a new
  `materialize: bytes` mode (today it materializes evicted stubs;
  lean wants the bytes, fetched with the hydrate fan-out). Applies
  mode/uid/gid/mtime and symlinks from the manifest as import already
  does. Refuses if claim identity mismatches (§4 P1) or the budget
  check fails.
- **publish barrier** — every `floorSecs` (and on preStop): walk the
  workspace, diff (path, size, mtime, optionally xxhash for
  mtime-unreliable trees) against the checkout-baseline manifest plus
  in-memory dirt; upload changed/new files whole via the existing
  guarded `put_whole` path (If-Match per key); issue deletes for
  absent paths; CAS-rewrite the subtree manifest (seq++). Object
  traffic rides the deployment's S3 proxy (the `ObjectStore` s3
  backend pointed at the proxy endpoint — the MinIO path); each
  barrier is validated by a gateway publish-intent first (§2.2). The
  sidecar holds a proxy token + a gateway bearer, never S3
  credentials.
- **lease lifecycle** — claim the subtree cell at start, heartbeat,
  **clean release in preStop** (the measured 17s replacement path);
  the fence and self-recognition semantics come from the §4 re-scope.
- **preStop drain** — final barrier + release, sized against
  `terminationGracePeriodSeconds` ≥ 120 and the documented drain rate
  (8–13 s/GiB measured); a dirty-set pressure valve forces an early
  barrier when unpublished bytes exceed `dirtySetCapGiB`.
- **sync verb (v1, HITL)** — on invocation (exec/signal/HTTP on
  localhost, harness-triggered, never background): ask the gateway for
  the manifest diff since the local baseline seq; apply under the
  decided policy — locally-dirty files win, remote deletions honored
  only on locally-clean paths, adds/changes fetched via the proxy;
  write a machine-readable conflict report + exit status for the
  harness; advance the local baseline to the synced seq. Runs only
  between publish barriers (the sidecar serializes sync against its
  own barrier; the model gate's sync arm covers the cross-writer
  interleavings).

### 2.2 Gateway (extend flint-hub-gateway — it is already the fleet's one door)

Stateless; N replicas; everything durable lives in the bucket.

- **Grade 2 — publish grant (the one non-S3 verb):** the sidecar POSTs
  a publish intent {share, subtree, generation, keys+ETags}; the
  gateway validates (a) lease holdership against the subtree cell,
  (b) the declared layout (§4 P3), (c) claim identity, (d) every key
  is inside the subtree and no key touches `.flint/` except the
  manifest/lease cells the engine owns, then mints short-lived
  presigned PUTs pinned to exactly those keys with the conditional
  headers signed in. Bytes go pod → S3 direct. Non-holders are
  refused, not detected later.
- **Grade 1 — S3 proxy:** the same policy enforced with bytes
  transiting the gateway; served by the `s3_surface` library (§2.3) so
  it is also the fleet's S3-compatible read/write door where presigned
  flows are awkward (browser uploads, CI).
- **DECIDED (user, 2026-08-24): grade 1 is PRIMARY.** The deployment
  runs an S3 proxy that holds the real S3 credentials; agents talk
  ONLY to the proxy. Consequences:
  - Presigned minting (grade 2) demotes to an optional optimization —
    v1 needs no presigning at all. The publish-grant validation
    (lease, layout, claim, key scoping) runs inline as allow/deny on
    the proxied PUTs; non-holders are refused at the request.
  - Flint's arbitration must live IN (or in front of) that proxy —
    a credential-hiding proxy alone leaves writes cooperative
    (If-Match detection, not enforcement). The natural shape: the
    proxy IS the flint gateway (`s3_surface` + grant policy), or the
    proxy chains to it.
  - **Hard requirement — conditional-header fidelity:** the proxy
    MUST pass If-Match / If-None-Match:* / ETags / x-amz-meta-flint-*
    / checksum headers through untouched, both directions. Every
    fencing mechanism (epoch CAS, manifest barrier, strict journal)
    rides those headers; a proxy that strips or synthesizes them
    breaks fencing SILENTLY. The store-conformance probes run
    THROUGH the proxy, as a startup gate, not a doc note.
  - Prefix isolation moves wholesale into the proxy's authz: IAM no
    longer separates agents (they share the proxy's credential), so
    the proxy's policy must scope each workspace identity to its
    subtree — this is now a P-grade precondition, not advice.
  - Topology: ALL bytes transit the proxy (unlike presigned
    direct-to-S3), so checkout fan-out is bounded by proxy
    throughput before S3's — the Phase 0 fan-out/burst measurements
    must run through a proxy-shaped rig, and the proxy is per-cluster
    + replicated (it is stateless; the decided per-cluster gateway
    placement applies to it).
  - Failure domain grows: proxy down now pauses CHECKOUTS as well as
    publishes (presigned reads no longer bypass it). Running pods
    keep serving local files.
- **The HITL write path (v1):** a user editing/uploading through the
  projects UI mid-session is a SECOND WRITER entering an agent's
  subtree. UI writes therefore go through the gateway, which applies
  them lease-aware: validates against the layout, stamps ETags for
  If-Match round-trips (the v1.30 conditional-write machinery),
  refuses or queues while the holder's publish barrier is in flight,
  and records the write so the next `sync` diff carries it. UI writes
  never race the sidecar invisibly.
- **Auth:** agents authenticate to the proxy (bearer v1; the proxy's
  own mechanism otherwise); SigV4 *verification* in `s3_surface`
  serves proxy-side validation, and §9 Q3 shrinks to "which token
  the sidecar presents".
- **Failure mode by construction:** gateway/proxy down ⇒ checkouts
  and publishes pause and RPO grows while pods keep serving local
  files.

### 2.3 s3_surface (new module — ONE dialect, three placements)

The decision of record: **every REST/file surface in the system is the
S3 dialect** — the hub's file API converges onto it (live view), the
gateway speaks it (fleet door), the localhost sidecar (libflint §8
adapter 3) is the same library placed a third way, and direct-mode
non-mount access (FUSE and lean alike) is plain S3 against the bucket
(published view). Same client code everywhere; the consistency
contracts (docs/flint-*-consistency.pdf) say which view you are
reading.

Floor (per libflint §8, which this consolidates): SigV4 header +
presigned validation, ListObjectsV2, Get/Put/Head/Delete + Range,
conditional writes (If-Match, If-None-Match:*), DeleteObjects, full
multipart (parts 1–10000, 5 MiB min, out-of-order, re-uploadable).
The three documented traps are requirements, not surprises: the ETag
is md5-of-part-md5s + `-N` (the s3proxy #338 breakage class);
CompleteMultipartUpload's 200-with-error-in-body must be implemented;
complete-by-concatenation costs 3× IO — the reserve-and-copy decision
is made explicitly, not inherited.

Extensions the dialect adds (each one deliberate):

- **RenameObject** — the S3 Express One Zone verb, served on general
  buckets by us: atomic on the hub's live view (a real rename under
  the coherence authority — something no real GP S3 offers), and on
  the bucket view implemented as guarded copy+delete at the gateway.
- **posix metadata** — `x-amz-meta-flint-*` stamps, the A12 convention
  the tier already writes.
- **activity classification** — data ops count as activity; HEAD and
  status/listing polls do not, configurable — a polling S3 browser
  must not pin a share awake (the idle-ladder lesson: a conditional
  GET answering 304 pins exactly as hard as a 200).

Migration: the hub dual-serves `/files` and the S3 dialect for one
release; `/files` deprecates after the front door moves.

### 2.4 Operator / webhook

- CR: `mode: direct` + `variant: lean` (flush profile: floorSecs,
  dirtySetCapGiB, quiesceBoundSecs — a durability contract, per the
  consistency docs).
- **Injection shrinks to:** an emptyDir (sizeLimit from the workspace
  budget) + an ordinary sidecar container + env + preStop +
  startupProbe (checkout complete). No privileged, no /dev/fuse, no
  mount propagation, no broker. failurePolicy: Fail, two replicas,
  opt-in selectors — unchanged from the FUSE design.
- **The operator remains the single allocator:** it writes
  `.flint/claim` (If-None-Match:* at share birth) and `.flint/layout`
  (the declared, non-overlapping subtree set) with the provisioner
  principal; the webhook refuses a pod whose subtree is not in the
  layout; the gateway enforces the same facts independently at grant
  time — two unforgeable layers, the webhook is not the boundary.

## 3. What v1 deliberately does NOT solve

- **Cross-pod visibility inside a live workspace**: a foreign reader of
  a lean subtree reads the bucket (RPO-consistent snapshots) or asks
  for a `sync`. No flush-to-open promise in v1.
- **Files > 5 GiB** publish via presigned multipart or are refused in
  v1 (decide in §9) — a presigned single PUT caps at 5 GiB.
- **mtime-granularity blindness**: a same-size same-mtime rewrite
  inside one mtime tick can evade the scan. The barrier records
  (size, mtime, ctime where available) and optionally hashes small
  files; the drill battery must include a leg that TRIES to evade the
  scan and the doc states the residual honestly.

## 4. Preconditions (re-derived from the review for the lean shape)

- **P1 — claim identity** (`.flint/claim`, If-None-Match:* at share
  birth; verified by checkout, every grant, every publish). Mandatory
  before anything else: fresh-state pods run the adopt path on every
  boot, so prefix-reuse adoption is an every-boot hazard without it.
  This also closes the standing B12 defect for hub mode.
- **P2 — per-subtree manifests + a designated root owner** (the
  operator/provisioner writes the root manifest; subtree manifests at
  `<subtree>/.flint/manifest`). Required: N writers per prefix and one
  CAS document is last-writer-wins amputation on the first concurrent
  barrier.
- **P3 — declared layout** (`.flint/layout`, operator-owned, CAS):
  subtree assignment is allocation, not discovery — racing S3-side
  claims cannot arbitrate overlap, and a parent-scope claim's takeover
  sweep would fence every child holder's in-flight publish.
- **P4 — subtree lease re-scope** of `epoch.rs`: cell at
  `<subtree>/.flint/epoch` (the key is already prefix-parametric; the
  MPU fence is already scoped by data_prefix), takeover sweep scoped
  to exactly the subtree, self-recognition semantics decided (a fresh
  emptyDir cannot self-recognize; preStop release is the fast path;
  do NOT reuse workspace-stable holder identity — it deposes a live
  mid-flush writer; an operator-side fencing actor that has witnessed
  pod deletion finality is the only safe accelerator).

**Formal model gate:** P3+P4 (layout + subtree lease, including the
grant protocol's interaction with lease handoff) get a spec module
before code — the FlintTierSession precedent refuted the naive lease
design pre-code, and "the abstraction was the bug" is a three-time
repeat offender in this repo.

## 5. Phases

Each phase lands with observed-red tests (every leg proven to fail
against the reverted defect or with a failing control — the
cluster-drill discipline: 24 of 41 naive legs would have passed if
broken).

- **Phase 0 — models + measurements** (gates, ~days):
  0a. Formal model: subtree lease + declared layout + grant protocol.
  0b. Measure checkout with the hydrate fan-out against real S3
      (72.5 s/GiB measured pre-fan-out vs 1–13 theoretical — sizes
      the cold-start claim; publish rate is already measured).
  0c. Burst rig: N≥1000 correlated checkouts against an unpartitioned
      bucket (503 SlowDown ramp; jitter + manifest-GET-first).
  Both 0b and 0c run PROXY-SHAPED (a proxy in front of the store) —
  all bytes transit the deployment proxy, so its throughput is the
  binding constraint before S3's.
- **Phase 1 — bucket format**: claim cell, layout cell, manifest
  split (subtree + root), epoch re-scope per the model. Conformance
  tests against MinIO and real S3 (the store trait's MinIO listing
  workaround gets its server-side Prefix back — its "buckets are
  per-volume" justification is invalidated by multi-workspace
  buckets).
- **Phase 2 — flint-sync**: materializing checkout, scan-diff publish
  barrier, deletes-as-diff, preStop drain + release, budget check.
  Test battery: crash-replacement legs (resurrection bounded by RPO
  and ONLY by RPO; lease lockout measured; clean-release fast path),
  scan-evasion leg (§3), git/pip/sqlite single-pod workload e2e on a
  real workspace.
- **Phase 3 — gateway**: publish-intent verb + policy (non-holder
  refused; `.flint/` protected), sync-diff verb, HITL write path,
  bearer auth + revocation drill, proxy conformance gate (conditional
  headers through the deployment proxy, probed at startup),
  gateway-outage drill (publish-pause, pods keep serving, RPO growth
  observed and bounded). Presigned mint: deferred (§8).
- **Phase 4 — operator/webhook**: `variant: lean` injection, layout
  allocation + claim stamping at share birth, verify pass +
  stale-sidecar reporting, kind e2e (the idle ladder is N/A for lean
  shares — assert nothing renders standing).
- **Phase 5 — s3_surface on the hub**: file API v2 dual-serve,
  RenameObject, posix meta, activity classification; validate with
  rclone/boto3/aws-cli against a live share and against the bucket;
  front-door migration notes.
- **Phase 6 — cluster drill** (runb* tradition, anti-vacuity guards on
  every leg): spot-reclaim drain within the 2-minute notice
  (interruption handler + grace wiring as deployment preconditions),
  hard-kill loss bound = RPO exactly, claim-mismatch refusal,
  S3-console foreign-write fates, burst wave, gateway kill mid-fleet,
  mixed posture (hub share + lean shares, one bucket, different
  prefixes).

Dependency note: Phases 1–2 need only P1–P4; Phase 3 can proceed in
parallel after the Phase-0 model lands; Phase 5 is independent of 2–4
and can start any time (it also pays down the file-API convergence
regardless of lean's fate).

## 6. Economics (shapes, not precision)

- Idle = S3 only, structurally (nothing standing). Lease cells exist
  only while a writer pod runs and releases on preStop — the FUSE
  variant's 3000-heartbeating-cells number does not arise.
- Publish costs: whole-file re-upload of touched files per barrier —
  the interval-capture precision (UploadPartCopy clean runs) is
  deliberately given up; bound with `dirtySetCapGiB` and the 60s
  floor. The fsync-churn pathology cannot occur (no interception —
  the barrier is the only publisher).
- Checkout costs: full-workspace GET per pod start — the Phase-0b
  number times workspace size is the cold-start bill; per-NODE cache
  reuse is FUSE/mount-pod territory, out of scope here.

## 7. Failure model deltas vs the FUSE variant

Same loss bounds (RPO per pod, preStop drain, hard-crash drains
nothing) with three rows improved: no ENOTCONN/daemon-crash class at
all (there is no daemon in the data path); no recovery-order contract
(nothing to restart in place — the files are just there); gateway
outage is publish-pause only. One row unchanged and still owed to the
CRD surface: RPO stated per mode, measured event rate on the fleet.

## 8. Explicit deferrals

- Partial checkout (brings back the removal journal AND a
  checked-out-set record — design before implementing).
- Presigned grade-2 grants (direct-to-S3 bytes; only if the proxy
  becomes a measured bottleneck).
- On-demand foreign-subtree refresh beyond local-wins `sync`.
- SigV4 pod identity / TokenReview auth at the gateway.
- Per-NODE shared checkout cache.
- GCS/Azure surface parity (new store impls + tenancy appendix).

## 9. Open questions (user decisions)

1. Gateway home: extend `flint-hub-gateway` (recommended — it is
   already the fleet door with the credential and the bearer
   machinery) or a new binary?
   **DECIDED (user, 2026-08-24): deployment is per-cluster.** The
   gateway is stateless — every validation input (lease, layout,
   claim) lives in the bucket — so per-cluster replicas need no
   coordination; grant latency stays cluster-local and each cluster
   can bind the gateway to its own ServiceAccount/OIDC trust. The
   binary-home half (extend vs new) stays open.
2. v1 large files: presigned multipart, or refuse > 5 GiB in lean
   mode (routing those workspaces to hub/FUSE)?
3. Auth v1: per-share bearer only, or bearer + SigV4 from day one?
4. ~~Does the `sync` verb ship in v1?~~
   **DECIDED (user, 2026-08-24): the `sync` verb ships in v1.**
   Rationale: the agent harness supports human-in-the-loop — a user
   uploads or edits files mid-session through the project UI and a
   long-lived agent must pick them up without being restarted.
   Design constraints (from the review + this discussion):
   - Explicit and quiescent only: the agent/harness invokes it at a
     moment it chooses; there is NO background refresh (online
     refresh inverts import's pre-listener + local-wins invariants).
   - Conflict policy v1: locally-dirty files win; remote deletions
     are honored only on locally-clean paths; conflicts are
     surfaced to the harness (exit status + report file), never
     silently merged.
   - The Phase 0 formal-model gate grows a sync arm: the subtree
     lease × sync interaction (a sync during another writer's
     publish window) must be modeled before the verb is coded.
5. Does the projects-service UI read through the deployment proxy or
   through the flint gateway? (Writes go through the gateway either
   way — the HITL path; reads could go either.)
6. Does the deployment proxy support per-tenant/prefix authz (scoping
   each workspace token to its subtree)? Decides the enforcement
   grade: proxy-scoped (strong) vs gateway-validated cooperative.
