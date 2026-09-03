# Flint-lite operator — development plan

Status: **BUILT (steps 0–4, 2a, 2b, 8) — 2026-08-18, post-v1.27.0.**
Code: `spdk-csi-driver/src/lite_operator/` (crd, render, conflict,
reconcile, bootstrap), bins `flint-lite-operator` + `crdgen`, chart
`flint-lite-operator-chart/`, docs `docs/flint-lite-operator.md`.
Step 7's kind e2e lane is IN (`tests/regression/operator-kind-e2e.sh`,
8 legs, all green: CRD accepted by a real API server, kernel client
mount, CEL + undeclared-knob refusals, one-roll settings edit,
cross-namespace nested-prefix conflict, schema self-repair, reclaim
Retain/Delete, in-place adoption with the AdoptionBlocked fence).
Outstanding: step 5 (hub status endpoint feeding the deep conditions)
and step 6's `Hibernated` (blocked on verifying final-flush-on-SIGTERM;
`Suspended` shipped). Two deviations from the
plan below, both recorded in place: the CRD needed a structural-schema
pass (schemars emits `anyOf` junctors that the API server refuses), and
the operator re-applies its CRD on every start (not only on a version
bump) so a hand-edited schema is repaired.

Prerequisite decision recorded: the
**hub-per-volume topology stays** — this operator is the fleet control
plane for many single-volume hubs, not a step toward multi-volume. The
API below is deliberately **volume-shaped, not release-shaped**, so if
the multi-volume design (docs/plans/multi-volume-hub-design.md) is
ever adopted, only the controller's reconcile changes — the CRD
survives as-is.

## Why an operator (and why now)

The lite chart is the right packaging at 1s–10s of hubs. At fleet
scale (bucket-per-tenant), helm-release-per-volume is the bottleneck:
release-secret sprawl, imperative per-release upgrades, no drift
repair, and the `--reuse-values` failure class this project has been
bitten by on real clusters (runbr). An operator gives:

- `FlintShare` as the unit of fleet management (`kubectl apply` a
  volume; `kubectl get flintshares` is the fleet dashboard).
- Structural immunity to stale values: every reconcile re-renders from
  CR + operator defaults — there is no reusable release state.
- A home for volume lifecycle that runbooks cannot host: suspend
  (scale-to-zero keeping the PVC), hibernate (delete the PVC; wake =
  DR import + the warm fill), waved image rollouts.
- Config validation as schema: the CRD's OpenAPI is DERIVED from the
  same serde types the server parses, retiring the chart's hand-listed
  `$known` guard and its drift test as a class.

## What already exists in-tree (reuse, don't invent)

- **kube-rs 3.0 (client + runtime) is a dependency**, and
  `src/controller_operator.rs:132` already runs a
  `kube::runtime::Controller` + watcher loop with the
  requeue/error-policy shape — the reconcile skeleton is established
  house style, not new ground.
- **Lease-based election** (`src/orchestrator_role.rs`) for
  single-active-operator.
- **`TierConfig`** (`src/pnfs/config.rs`) is the knob schema, already
  serde with camelCase renames and per-field defaults.
- **The lite chart's templates** (`flint-lite-chart/templates/hub.yaml`)
  are the object-shape spec: ConfigMap (rendered server YAML), RWO PVC,
  Service, single-replica Recreate Deployment with startupProbe and
  Secret envFrom. The operator renders the SAME four objects.
- **NOT present**: any first-party CRD (`crds/` in the full chart is
  the external VolumeSnapshot set) or schemars (removed from
  Cargo.toml when CRD generation was dropped) — step 1 re-adds it.

## The API (v1alpha1, namespaced)

```yaml
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata:
  name: tenant-a
  namespace: workspaces
spec:
  bucket: my-team-flint          # must exist; versioning on
  keyPrefix: tenant-a/           # one prefix = one volume = one hub
  endpoint: ""                   # non-AWS stores
  credentialsSecretRef: ""       # "" = ambient (IRSA / node role)
  persistence:
    size: 20Gi
    storageClassName: ""         # "" = cluster default
    reclaim: Retain              # Retain | Delete — what CR deletion
                                 # does to the PVC. The BUCKET is never
                                 # touched by the operator, ever.
  service:
    type: ClusterIP              # LoadBalancer for cross-cluster
    annotations: {}
  image: ""                      # "" = operator default (fleet-wide)
  settings: {}                   # typed TierSettings — schema derived
                                 # from TierConfig, NOT free-form
  lifecycle: Active              # Active | Suspended | Hibernated
                                 # (Suspended/Hibernated land in step 6)
status:
  phase: Ready                   # Pending|Claiming|Importing|Ready|
                                 # Suspended|Hibernated|Failed
  conditions: []                 # Ready, PodScheduled, EpochHeld,
                                 # ImportComplete, WarmFillComplete
  observedGeneration: 3
  address: "10.0.4.7:2049"       # what consumers mount
```

Naming (decided): **`FlintShare`**. "FlintVolume" was rejected because
"flint volume" already means a CSI-provisioned SPDK volume in this
project's vocabulary — the CRD must not overload it. "FlintObjVolume"
was rejected because it keeps the overloaded noun, adds an
abbreviation k8s API conventions discourage, and becomes untrue for
tier-off shares (the object store is an optional backend, not the
identity). "Share" is what the consumer actually gets — a mountable
NFS export — and stays truthful across tier on/off, any object-store
backend, and both topologies (dedicated hub today, shared hub under
multi-volume). The bucket-backed nature is surfaced via
`additionalPrinterColumns` (BUCKET, PREFIX, PHASE, ADDRESS) and a
`fsh` short name. (`FlintExport` is the recorded runner-up.)

Design rules:
- The spec never names a Deployment, PVC, or Service — those are the
  CURRENT reconcile's outputs, not the API. (The multi-volume hedge.)
  Child object names are CR-DERIVED for fresh CRs (the chart's fixed
  `flint-lite-*` names cannot coexist as a fleet); adopted CRs may
  carry non-canonical child names, recorded in status.
- `settings` is a typed knob schema, but **an all-`Option` mirror of
  the knob subset with NO schema defaults** (review major 6): deriving
  the schema directly from `TierConfig` would embed every
  `#[serde(default)]` value, and CRD structural defaulting
  MATERIALIZES defaults into stored CRs at admission — the operator
  could never distinguish "user pinned 60" from "defaulted", and a
  later server-side re-pricing of a default (an explicit expectation
  of the economics gate) would leave the fleet running old values as
  if user-pinned: stale-values-by-construction, the exact
  `--reuse-values` class this operator exists to kill. The operator
  renders ONLY `Some` knobs into `tier:`; unset knobs keep taking the
  server default (the chart's documented contract). Parity with
  `TierConfig`'s knob surface is enforced by a crdgen unit test that
  ALSO asserts the generated settings schema contains zero `default`
  keys. Identity fields (bucket/keyPrefix/endpoint/credentials) are
  first-class spec, exactly as the chart splits them today — which
  requires splitting `TierConfig` into identity + `TierKnobs`
  (`#[serde(flatten)]` keeps the mds.yaml shape and existing config
  tests green).
- CEL validation rules on the CRD for what CEL CAN express:
  bucket/keyPrefix immutability via transition rules
  (`self == oldSelf`, wired through kube's CEL validation derive
  attributes — plain `JsonSchema` emits no x-kubernetes-validations),
  keyPrefix syntax. Fleet uniqueness is controller-enforced (see
  Architecture) — CEL cannot see other objects. No admission webhook
  in v1 (cert lifecycle for little gain).

## Architecture

One new binary `flint-lite-operator` in the existing crate ([[bin]]
beside the other seven): a `Controller` on `FlintShare`, server-side
apply with a fixed field manager for every write (drift repair for
free), Lease election, exponential requeue on error, slow steady
requeue (~5 min) when settled.

**Child ownership (review critical 1 — the PVC is special):**
Deployment/Service/ConfigMap carry ownerReferences (`owns()`, die by
owner GC). The PVC **NEVER carries an ownerReference** — an ownerRef'd
PVC is garbage-collected the moment the finalizer releases the CR,
and GC knows nothing about `persistence.reclaim`; Retain-by-default
must be fail-safe by construction, not by reconcile correctness (for
a tier-off share the PVC is the ONLY copy of the data). PVC events
arrive via `watches()` + a label mapper on the operator's SSA labels.
Finalizer: reclaim=Retain ⇒ remove the finalizer, done.
reclaim=Delete ⇒ issue the PVC delete FIRST, then remove the
finalizer — deletionTimestamp is durable and
`kubernetes.io/pvc-protection` holds the PVC until owner GC kills the
pod, so a crash between the two steps retries idempotently (the
reverse order orphans the PVC forever on operator crash). The bucket
is out of scope by invariant.

**Input-change rollout (review critical 3 — nothing restarts the hub
otherwise):** the hub parses `--config` ONCE at boot (no reload path
exists in `src/pnfs/`), and credentials ride `envFrom` (fixed at
container start). The pod template therefore carries
`checksum/config` (sha256 of the rendered mds.yaml) and, when
`credentialsSecretRef` is set, `checksum/creds` — any input change
rolls the Deployment through Recreate. `Controller::watches()` on
Secrets maps rotations to referencing FlintShares, converting
rotation from a failure-driven self-fence/exit(70)/restart blip the
operator cannot explain into one deliberate observable rollout. The
same checksum annotations go into `flint-lite-chart` FIRST (the chart
has the identical latent bug today: `helm upgrade` with changed
`tier.settings` never reaches the running hub), which also keeps the
render-parity golden test exact. A `ConfigCurrent` condition
distinguishes rendered-from-running while a roll is pending; whether
a settings edit rolls immediately or waits for an ack
(`restartPolicy: Immediate|Manual`) is a spec field — the roll is a
client-visible Recreate bounce (same-PVC epoch re-claim is instant,
per docs/flint-lite.md, so the window is the bounce, not a lease
wait).

**Fleet uniqueness (review major 5 — CEL cannot express it):** at
most one FlintShare per (endpoint, bucket, prefix-SUBTREE) across all
namespaces, enforced at reconcile time from the controller cache —
CEL and ValidatingAdmissionPolicy see one object and cannot check
cross-object invariants. Conflict predicate is normalized OVERLAP
(endpoint None=="", trailing-slash-normalized, prefix NESTING counts:
`tenant-a/` vs `tenant-a/sub/` collide — sweeps and `.flint/` control
objects span the subtree). Winner = oldest creationTimestamp (UID
tiebreak); loser gets phase=Failed + a `Conflict` condition naming
the winner + an Event, no Deployment — and an already-running hub
that BECOMES a loser (endpoint is mutable) is scaled to 0, not merely
skipped. Any change/delete re-queues the whole conflict set, so
deleting the winner promotes the survivor. The store-side epoch
protocol stays defense-in-depth, NOT the arbitration mechanism: two
live claimants do not flap (a live foreign holder is waited out — the
loser crash-loops pre-listener), but when the winner dies for a lease
window, the other CR's hub executes TAKEOVER and serves the prefix's
data at ITS address — cross-tenant exposure on routine pod churn if
duplicates are ever allowed to reconcile.

Status v1 derives from what Kubernetes already knows (Deployment
availability, pod startupProbe progress ⇒ Pending/Claiming/Ready).
Deep conditions (EpochHeld, ImportComplete, WarmFillComplete, RPO age)
need the hub to SAY so — step 5 adds a read-only status endpoint on
the existing `monitoring.health` port serving a small JSON (epoch
state, import report, warm-fill report, reporter gauges), and the
operator polls it into conditions. No log scraping.

## Build order

0. **Decision gate (this document)** — reviewed; the topology
   commitment and API shape above are the contract.
1. **CRD scaffolding** (~2–3 days, re-estimated per review): enable
   kube's `derive` feature and add schemars AT THE MAJOR kube 3.0
   PINS (kube-derive is absent from the lock and the removed schemars
   line predates the 0.8→1.x derive break — this is "add correctly",
   not "restore"); split `TierConfig` into identity + `TierKnobs`
   (`#[serde(flatten)]`, existing config tests stay green);
   `FlintShare` via `#[derive(CustomResource, JsonSchema)]` with the
   all-`Option` `TierSettings` mirror; bucket/keyPrefix immutability
   via kube's CEL validation derive attributes; a `crdgen` bin
   printing the CRD YAML (checked in under the new chart's `crds/` as
   install-time bootstrap ONLY — see step 2a); the parity unit test:
   settings block matches `TierKnobs`' serde surface AND contains
   zero `default` keys.
2. **Core reconcile** (~4 days, +1 per review): CR →
   ConfigMap/PVC/Service/Deployment per the Architecture section's
   ownership rules (PVC never ownerRef'd; `watches()` + label
   mapper); finalizer + reclaim ordering (Delete: PVC-delete THEN
   finalizer-removal); `checksum/config` + `checksum/creds`
   pod-template annotations — **added to
   `flint-lite-chart/templates/hub.yaml` FIRST** (fixing the chart's
   own latent settings-never-apply-on-upgrade bug and keeping parity
   exact); `Controller::watches()` on Secrets; the fleet-uniqueness
   conflict arbitration; the **render-parity golden test**: for a
   matrix of specs (including a sparse-settings case proving omitted
   knobs stay omitted), operator output == `helm template
   flint-lite-chart` output, where "normalized" is SPECIFIED in the
   test (helm-managed labels, object names, namespace, ordering —
   nothing else). Tests with teeth: reclaim=Retain PVC survives CR
   delete; operator killed between PVC-delete and finalizer-removal
   still converges.
2a. **The operator owns its CRD** (~1 day; review critical 2): helm
   never touches `crds/` on upgrade, so a chart-only CRD freezes at
   first install and structural pruning then SILENTLY STRIPS every
   knob added in later releases — the exact failure class the CRD
   exists to retire. At startup, before the Controller, the operator
   SSA-applies its compiled-in CRD (`FlintShare::crd()` — the same
   artifact crdgen prints), guarded by a
   `chert.us/crd-schema-version` annotation compared before apply
   (bare SSA would let a briefly-restarted OLD operator stomp a newer
   schema mid-fleet-upgrade). RBAC gains CRD get/list/watch/create/
   patch. Degraded path is LOUD: apply denied or served schema older
   than the operator ⇒ operator not-Ready condition + warning Events
   on CRs whose spec would be pruned. (Rejected alternative,
   recorded: templated CRD + `resource-policy: keep` also upgrades
   but breaks non-helm installs and leaves the schema hostage to
   whoever helms last.)
2b. **Chart→CR migration** (~2 days; review critical 4 — previously a
   parenthetical in the docs step, actually the riskiest operation
   every existing user must perform): the chart's children have
   fixed, release-unprefixed names, and a second Deployment on the
   same RWO PVC can land on the SAME NODE (RWO is node-granular;
   WaitForFirstConsumer/local PVs FORCE it) — two sqlite writers on
   one state.db, and the epoch cannot fence them because both pods
   read the same `state.db` and self-recognize on the same
   `hub-{server_id}` holder. Mechanism, in the plan not deferred:
   adoption = `persistence.existingClaim` for the PVC + SSA
   field-manager force-claim of the existing fixed-name
   Deployment/Service/ConfigMap IN PLACE — never a second Deployment;
   an `AdoptionBlocked` condition holds reconcile while any foreign
   pod mounts the claim (operator-enforced fence — a doc saying
   "scale down first" will be skipped); the documented helm endgame
   (delete the release secrets, or `helm.sh/resource-policy: keep`
   on all four objects) because `helm uninstall` after adoption
   deletes adopted objects, ownerRefs notwithstanding. Migration
   ordering also covers the conflict-arbitration blind spot: the
   controller cache cannot see chart-installed hubs, so the rule is
   release-teardown-or-adoption BEFORE the CR exists.
3. **Status v1 + events** (~1–2 days): phase/conditions from
   Deployment/pod state; Events on claim-wait, import-running (the
   startupProbe window a naive operator would misread as failure —
   the chart's startupFailureThreshold logic must carry over).
4. **Operator packaging** (~1–2 days): `flint-lite-operator-chart`
   (crds/, RBAC, election Lease, single-replica Deployment);
   multi-arch image via the established zigbuild/prebuilt recipe;
   `release.sh` gains the image+chart gating.
5. **Hub status endpoint** (~2 days, server-side): JSON status on the
   health port (epoch/import/warm-fill/reporter gauges); operator
   polls it into the deep conditions. Doubles as a human debugging
   surface (`kubectl port-forward` + curl).
6. **Lifecycle** (~3–4 days): `Suspended` (verify-or-add final flush
   on SIGTERM FIRST — the known RPO-at-sleep gap — then scale to 0
   keeping PVC) and `Hibernated` (delete PVC; wake reconciles a fresh
   one and sets `hydrateWarmAfterImport` for the DR import — the warm
   fill is the wake-up half). Both are conditions-visible and
   idempotent through operator restarts.
7. **e2e** (~3 days, +1 for the review legs): a kind lane beside the
   existing two — install operator chart, apply a FlintShare, wait
   Ready, mount from the Lima VM, write/read, delete CR, assert
   children gone + PVC per reclaim policy; a tier variant on
   in-cluster MinIO re-using lite-kind-tier-e2e's plumbing. Review
   legs: **upgrade** (install vN, apply a share, upgrade to vN+1
   whose schema adds a knob, set it, assert it survives admission and
   reaches the ConfigMap); **conflict** (two CRs, cross-namespace +
   nested-prefix variants ⇒ exactly one hub pod, loser carries
   `Conflict`; delete the winner ⇒ survivor promotes);
   **settings-edit** (edit a knob on a Ready share ⇒ exactly one
   Recreate roll, new value live via the step-5 endpoint);
   **rotation** (rotate the Secret ⇒ proactive roll, no
   EpochRenewFailures burst); **migration** (helm-install, write
   data, adopt via CR ⇒ at most one pod replacement, same PVC + same
   server_id, then the helm endgame with the hub surviving);
   suspend/wake leg once step 6 lands.
8. **Docs** (~1 day): docs/flint-lite.md gains the operator path; the
   migration runbook documents step 2b's mechanism (the mechanism
   itself is step 2b, not a doc).

Total: ~3 weeks to a shippable v0 (steps 1–4 incl. 2a/2b), ~2 more
days for deep status, ~1 week for lifecycle + e2e + docs. (+4–5 days
over the pre-review estimate — the review's forced edits, priced in.)

## Out of scope (recorded)

- Multi-volume topology (the API is compatible by construction; the
  reconcile swap is that design's step, not this one's).
- Conversion webhooks (single served version until a v1beta1 exists).
- PVC resize, cross-namespace consumers, autoscaling, transparent
  wake-on-SYN (explicit wake by harness/operator only — the activator
  was rejected in the scale-to-zero analysis).
- Replacing the chart: it remains supported; the operator consumes the
  same images and renders the same objects (the golden test enforces
  it).

## Risks

- **CRD lifecycle**: v1alpha1 must be honest about instability;
  bucket/keyPrefix immutable via CEL from day one (mutating them
  under a live hub is split-brain by construction).
- **Startup-vs-liveness misread**: the operator must treat a
  minutes-long pre-listener startup (fence + import) as progress, not
  failure — port the chart's startupProbe budget, surface it as a
  condition.
- **Deletion defaults**: Retain-by-default PVC + never-touch-bucket
  are invariants a reconcile bug must not be able to cross — the PVC
  one is now structural (no ownerReference exists to mis-handle), and
  both get explicit tests.
- **Fleet uniqueness of (endpoint, bucket, prefix-subtree)** is the
  third named invariant: controller-enforced (CEL cannot express it);
  the store-side epoch protocol is defense-in-depth, not the
  arbitration mechanism — an unarbitrated duplicate becomes
  cross-tenant data exposure on routine pod churn (the takeover
  path).
- **Two sources of object truth** (chart + operator) until one wins:
  the render-parity golden test is the mitigation, and it fails the
  build when they drift.

## Review record

Ultracode-reviewed 2026-08-18 (11 agents: 3 adversarial dimensions →
top-7 verify → synthesis). Verdict: **not buildable as written;
mandatory edits, no rework** — all folded in above. 4 critical
(Retain-PVC killed by owner GC; helm-frozen CRD resurrecting silent
knob loss; no input-change rollout path — including the chart's own
latent settings-never-apply-on-upgrade bug, fixed chart-first in step
2; migration's same-node dual-sqlite-writer window that the epoch
provably cannot fence) + 2 major (fleet uniqueness inexpressible in
CEL — with the takeover cross-tenant-exposure consequence; TierConfig
default-materialization breaking the unset-takes-server-default
contract, forcing the all-Option mirror + struct split). Attacked and
HELD: the volume-shaped CRD, single-replica reconciler, no-webhook-v1,
the golden-test concept, the epoch protocol as defense-in-depth (two
live claimants wait, not flap), and self-fence-then-exit(70) as a
self-healing (if blind) rotation path. Net +4–5 days.
