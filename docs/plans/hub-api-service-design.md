# Hub file-API Service: addressable, not exposed

**Status:** design of record, superseding the three-way merge. Two of the merged design's load-bearing choices are withdrawn on measured evidence (`{base}-api` naming; `publishNotReadyAddresses`), and the advertise knob is split into a second stage gated on a hub change. **No code has been written.**

**Read first:** `docs/plans/file-api-fleet-auth.md` (the credential half of this problem), `docs/plans/flint-lite-fleet-scale-plan.md` (every blocker in this operator is a per-reconcile rate term), `docs/plans/s3-tier-l2-design-review.md`.

---

## 1. The problem

A hub pod serves NFS on 2049 and an HTTP file API on 8080. The 8080 listener is a bare `containerPort` and nothing else:

- It is in no Service. `render::service()` emits exactly one `ServicePort`, hard-named `nfs`, `targetPort: NFS_PORT` — `render.rs:465-472`, `render.rs:58`.
- It is in no status field. `FlintShareStatus` at HEAD has six fields and `address` is documented as "what consumers mount (`host:port`)" — `crd.rs:644-647`.
- The only supported reader is the operator, and it dials the **pod IP**: `hubstatus::poll` builds `http://{pod_ip}:{port}/status` — `hubstatus.rs:318-324`, called from `reconcile.rs:2275`.

So a client on another cluster has no name to dial, and an in-cluster front door has only an IP that changes on every roll. There is no `backendRef` an Ingress, a Gateway `HTTPRoute` or a mesh export could point at.

That is the whole gap: **there is no stable object naming the hub's HTTP door.** It is not a routing gap — routing across a boundary is and remains a cluster-admin act.

---

## 2. What exists, and why it is shaped that way

The file API is deliberately kept off the consumer Service, and the reason is stated in four places that must stay consistent:

- `render.rs:575-578` — "deliberately NOT added to the Service, which carries NFS and may be a LoadBalancer. The lifecycle controller reaches /status by POD IP."
- `flint-lite-chart/templates/hub.yaml:180-184` — the chart-side twin.
- `crd.rs:330-334` — "Served on its own port, ClusterIP-only, and NEVER added to the consumer-facing Service."
- `reconcile.rs:2216-2219` — why the poll uses the pod IP.

Three facts make that invariant load-bearing rather than fussy:

1. **One socket, three surfaces, one auth filter.** `/health` and `/status` are assembled into the same warp server as `/files*` (`status.rs:278-285`, `:324-332`) and only the file routes carry `auth_filter` (`fileapi/mod.rs:478-563`). Publishing the port publishes an unauthenticated document naming `serverId`, `podName`, uptime, `epoch.held`/`epoch.number`, the import and sweep reports, tier gauges, active NFS lease count, `rpoClean`, and the free-text `importRefused` (`status.rs:201-241`).
2. **The credential is one long-lived shared secret.** `ApiConfig.token` is a single `Option<Arc<TokenSource>>` (`fileapi/mod.rs:120`). No set, no expiry, no audience, no per-caller identity, no rate limit, no lockout — grep for `Semaphore|rate_limit` in `fileapi/` returns three comments and no code.
3. **The mutating power is the whole project tree.** PUT/DELETE/folder/move over any path; `/files/move` does not condition its destination (`fileapi/mod.rs:1312-1314`).

**One correction that must land in this commit.** Commit `c72faea` (ancestor of HEAD) made the token **live**: `TokenSource::current()` is read per request (`fileapi/mod.rs:593-595`) with a 10s background re-read (`token.rs:58`). `docs/flint-lite-operator.md:667-671` and `docs/plans/file-api-fleet-auth.md` §2 and §9 all still say a rotation needs a pod restart. That is wrong at HEAD and a client author reading it will build a bounce-based rotation that costs mounted NFS clients ~90s of grace for nothing. What is still boot-time: whether the routes are mounted at all — no token ⇒ no route table ⇒ **404, not 401** (`config.rs:874-877`, `server.rs:647`).

---

## 3. The design

### 3.0 The structural bet, and the two stages

**The operator renders the object an admin-built front door needs as a backend, and publishes where it is. It never renders the object that assigns a routable address.**

There is no `type` knob, no `nodePort`, no `loadBalancerClass`, no chart value and no operator flag that can make the operator create an internet-facing API Service. The reason is §4: NetworkPolicy cannot be the guard, so the dangerous configuration must be unrepresentable rather than defended.

The work splits into two stages because one adversarial finding is not fixable by documentation:

- **Stage 1 (this change).** A headless, share-scoped, uid-named API Service gated on `monitoring.enabled && fileApi.enabled`; `status.apiEndpoint` in its derived in-cluster form; `status.hubPhase`; the `ApiEndpointPublished` condition; the two CEL rules that close shipped silent-ignores; RBAC for the delete path.
- **Stage 2 (blocked on a hub change).** `spec.monitoring.fileApi.port` — a **second listener carrying only the authenticated `/files*` routes** — and only then `spec.monitoring.advertiseUrl`, the CR field that describes an off-cluster front door.

**The brief asked for an `apiAdvertiseAddress` knob. Stage 1 does not ship it, and that is deliberate.** Advertising an off-cluster door while `/status` rides the same socket makes "forward only `/files*`, deny `/status`" a documentation-enforced invariant with no mechanism — the class this repo distrusts, and the natural Ingress (`path: /`) violates it. `fileApi.port` makes the split enforceable by the socket, so a Service can express it and an admin cannot get it wrong by writing the simplest rule that works. Stage 2 is specified here in full so the shape is not re-litigated.

### 3.1 Spec surface

**Stage 1 adds no spec field.** It adds two CEL rules and one port constraint (§5). This is worth stating plainly: the Service's existence is gated on `spec.monitoring.enabled` and `spec.monitoring.fileApi.enabled` — fields that already exist at schema 6 and can therefore **never be pruned by an operator downgrade**. A new `apiService.enabled` field would be prunable, and "block absent ⇒ delete the Service" would then make a rollback delete the fleet's API Services as a side effect. Gating on a pre-existing field makes that hazard unrepresentable, and that is the recorded reason there is no such field.

**Stage 2** adds two, both on the existing `MonitoringSpec` (which already derives `KubeSchema`, so node-attached CEL is free, and which is the type whose own doc comment states the invariant being amended):

```rust
/// A SECOND listener carrying ONLY the authenticated `/files*` routes.
/// When set, `/health` and `/status` stay on `monitoring.port` and are
/// NOT reachable on this one, and the operator's API Service targets
/// this port instead.
///
/// Setting this adds a line to the hub config, which rolls the pod —
/// ~90s of grace for every mounted client. It is the price of making
/// "do not publish /status" a socket boundary instead of a sentence in
/// a runbook.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub port: Option<i32>,                       // on FileApiSpec

/// Where an off-cluster caller reaches this hub's file API, as an
/// absolute http(s) URL with no trailing slash. A client joins it as
/// `<base>/files`.
///
/// ADVERTISES; DOES NOT PROVISION. The operator renders a HEADLESS
/// Service and never anything routable. This describes a front door
/// YOU built, whose backend is that Service. Setting it changes
/// nothing the operator creates.
///
/// NOT ARBITRATED. Unlike `bucket`/`keyPrefix`, two shares in two
/// namespaces may publish the identical value and nothing detects it.
///
/// NOT VERIFIED. The operator copies it verbatim and has no signal
/// about whether it still routes anywhere. `status.apiEndpoint`
/// reports it under reason `AdvertisedUnverified` for that reason.
///
/// SECURITY: writable by anyone who can write this FlintShare, and
/// copied verbatim into status. It is a display value and a routing
/// hint, NOT a trust anchor. See §4.5.
#[serde(default, skip_serializing_if = "Option::is_none")]
#[schemars(length(max = 2048))]
pub advertise_url: Option<String>,           // on MonitoringSpec
```

Named `advertiseUrl`, not `advertiseAddress`, because the grammar differs: `status.address` is `host:port` (what `mount(8)` takes) and this is a URL (what an HTTP client takes). Someone who pastes a `host:port` gets a CEL rejection whose message names the URL form. Two endpoint grammars on one CR is a wart; it is accepted because a cross-boundary field that cannot say `https` cannot express the only security-relevant fact about a door carrying a rewrite-capable bearer credential.

### 3.2 The Service object

```yaml
apiVersion: v1
kind: Service
metadata:
  # {base truncated to 50}-api-{first 8 hex of metadata.uid}
  name: tenant-a-api-3f9c1b7e
  namespace: workspaces
  labels:                       # labels(share), unchanged
    app.kubernetes.io/name: flint-lite
    app.kubernetes.io/instance: tenant-a
    chert.us/share: tenant-a
  ownerReferences:
    - apiVersion: chert.us/v1alpha1
      kind: FlintShare
      name: tenant-a
      uid: 3f9c1b7e-2a11-4c8e-9d0f-6b5a1c2d3e4f
      controller: true
      blockOwnerDeletion: true
spec:
  clusterIP: None                # HEADLESS. Not a knob, not a default.
  ports:
    - name: api
      protocol: TCP
      port: 8080                 # MUST equal the resolved container port
      targetPort: http           # stage 2: `files`
  selector:
    app.kubernetes.io/name: flint-lite
    chert.us/share: tenant-a
```

Rendered **iff `monitoring.enabled && monitoring.fileApi.enabled`**. `Rendered` gains `api_service: Option<Service>`; `Names` gains `api_service: String`.

**The name is uid-derived, and that single choice closes four findings.**

- **It fits.** A Service name is an RFC 1035 label capped at 63 characters and the CRD sets no `maxLength` on `metadata.name` (verified: the generated `openAPIV3Schema` has no `metadata` node). **MEASURED on k8s 1.36:** a Service with a 60-character name is accepted; the same base with `-api` appended (64) is rejected — `metadata.name: Invalid value: … must be no more than 63 characters`. Under the withdrawn `{base}-api` scheme every existing share named 60–63 characters would have had its Service apply 422 on the first reconcile after upgrade, and because §5's applies propagate with `?` that error returns **before** the Deployment apply — the share stops converging entirely and sits in the failure backoff (up to 900s) forever. `{base[..50]}-api-{uid[0:8]}` is 63 at worst.
- **It is not mintable by another CR's formula.** Under `{base}-api`, a share literally named `foo-api` renders its *consumer* Service to that name and force-applies over share `foo`'s API Service. **MEASURED on k8s 1.36** with field manager `flint-lite-operator` and `--force-conflicts`: `metadata.ownerReferences` is a listType=map owned by that manager, so the apply **replaces** it — one ownerRef after, not two, with ports and selector flipped in the same call. The victim gets no watch trigger, because `.owns(Service)` maps by ownerReference, which now names the attacker. With `spec.service.port: 8080` on the attacker's share (legal — no CEL constrains it), the victim's cached endpoint DNATs to the attacker's NFS listener and the victim's front door presents its bearer token there. Detection waits for the victim's own requeue: 300s live, up to `FULL_APPLY_AFTER` (18000s) parked.
- **It is not computable from `status.conflictWith`.** `conflict::redirect` publishes the winner's `namespace` and `name` unconditionally across namespaces; only `address` is gated on `same_ns` (`conflict.rs:141-152`). Under a name-plus-namespace formula, a conflict loser could compute the winner's door from its own status — voiding the doctrine at `crd.rs:653-668` for the *stronger* door, and doing it by arithmetic rather than by disclosure. `metadata.uid` is not in `ConflictWith`, so the formula stops working and resolving the winner's endpoint again requires reading the winner's CR, which is an authorization check the API server performs for free.
- **It removes the griefing tension.** No CR name can deny another share its API Service by formula collision.

A deliberate attacker who can *read* the victim's CR still learns the uid and can name a share after it. That residual is handled by the guard below, and the outcome is first-writer-wins rather than misroute.

**Both Service applies are owner-verified, and this costs nothing.** §6 already GETs the NFS Service live (`reconcile.rs:1194-1197`). Hoist that GET **before** the §5 apply, add the same GET for the API Service, and for each: if the object exists and its controller ownerRef names a different share uid, **do not apply**. Set `Ready=False, reason: NameCollision` (consumer Service) or `ApiEndpointPublished=False, reason: NameCollision` (API Service), emit a Warning Event, publish no endpoint, and continue. The merged design guarded only the API Service, which fails closed in one ordering and open in the other. Guarding both is symmetric and net-zero in API calls.

**The API Service apply must never propagate its error.** Apply it *after* the Deployment, capture any error into `ApiEndpointPublished=False, reason: ApplyFailed` with the message, and continue the reconcile. A door object must not be able to stop the NFS door from converging.

**Headless, because two ClusterIPs per share breaks the measured envelope.** `docs/plans/flint-lite-fleet-scale-plan.md:424-428` states the envelope explicitly: "3000 ClusterIP Services remain, by design… 3000 cluster IPs is 73% of a GKE-default /20 service CIDR." A /20 is 4096. A second ClusterIP per share puts the validated 3000-share fleet at 6000 against 4096, i.e. exhaustion at roughly 2048 shares — *below* what has already been validated. Service CIDR is a cluster-wide, unquota'd, shared resource, and the documented tenancy model puts many tenants in one namespace. The failure is cross-tenant and lands on the wrong door: once the allocator is dry, a brand-new share's **consumer** Service cannot be created either, so `status.address` is never published and the share is unmountable. One tenant's share count denies every other tenant new shares, reported only as a repeating apply error.

`clusterIP: None` allocates nothing, still creates EndpointSlices, still gives a stable per-share DNS name with the correct share-scoped selector, and is a valid `backendRef` for any controller that routes to endpoints.

> **ASSUMED, NOT MEASURED:** that the target front-door implementations accept a headless Service as a backend. Gateway API and endpoint-routing Ingress controllers do; some LB integrations require a VIP. **This must be verified against the actual front door before landing.** If a VIP is required, the admin creates one Service themselves against the same selector — paying one cluster IP per share they actually expose, not one per share in the fleet. That is the correct place for the cost to fall.

**Headless trap for the implementer:** for a headless Service, `spec.ports[].port` feeds SRV records only; a client resolving the A record dials the **container** port directly. So `port` **must** equal the number `targetPort` resolves to, or the published endpoint is a lie. Both come from the same expression (`monitoring.port ?? 8080`, or `fileApi.port` in stage 2), so keep them derived from one variable and assert it in a unit test.

**`targetPort` by NAME.** The containerPort is named `http` and its number is `m.port.unwrap_or(HEALTH_PORT)` (`render.rs:587`). Naming the target makes it impossible to reproduce the bug already shipped in the operator chart's NetworkPolicy, which hardcodes `port: 8080` against a free `spec.monitoring.port`.

**`publishNotReadyAddresses` is WITHDRAWN.** The merged design set it, justified it on the `Starting` window, and enumerated its costs as "a few seconds of connection-refused" and "a crash-looping pod stays in endpoints". Both of the costs it missed are worse than the benefit.

**MEASURED on k8s 1.36.** Kubernetes computes `ready := PublishNotReadyAddresses || (serving && !terminating)`. A pod with a failing readiness probe, fronted by two Services with identical selectors: the flagged Service reports `{'ready': True, 'serving': False, 'terminating': False}`; the plain one reports `ready: False`. After `kubectl delete pod --wait=false`, with `deletionTimestamp` set: the flagged Service still reports `{'ready': True, 'serving': False, 'terminating': True}`.

Consequences the flag would have created:

1. **The conflict-loser argument was false.** The fence is a merge patch to `replicas: 0` (`reconcile.rs:766-770`) — an ordinary termination — and `terminationGracePeriodSeconds` defaults to 120 (`render.rs:105`). There is no force-kill: `pods/delete` was **tried and reverted in this working tree** (`flint-lite-operator-chart/templates/rbac.yaml`: "Deliberately NOT `delete`. Force-deleting a conflict loser was tried and reverted"). With the flag, the loser's API Service keeps a ready endpoint for the full grace while the hub runs `graceful_shutdown` — and `reconcile.rs:772-776` records that a loser's shutdown publishes land in the **winner's** subtree. A cached endpoint plus a token becomes a write into another tenant's data, with the loser's CR asserting the door is closed.
2. **The `Terminating` clear was an announcement, not a control**, for the same reason.
3. **A node partition becomes a permanent blackhole.** The hub is `strategy: Recreate`, `replicas: 1` (`render.rs:705-709`), so there is no second pod and no replacement can start while the Pod object exists. With the flag, a pod on a dead node stays `ready: true` through the Ready-condition flip at ~40s and through Terminating, because graceful deletion cannot complete without a reachable kubelet. The NFS Service fails cleanly; the API Service SYN-blackholes until someone deletes the Node object — and force-deleting a flint pod is the contraindicated recovery (F29). The door documented as "fails fast rather than hanging" would hang longer than the door that is supposed to hang.

Dropping the flag means the API Service has **no endpoints during the entire `Starting` window**, because the readiness probe is TCP 2049 (`render.rs:740-744`, `:556-558`) while the HTTP listener binds first, before the epoch claim and the DR import (`server.rs:626-631`). That is a real loss and it is compensated in the status, not in the Service — see `status.hubPhase` below, which reaches a caller even when the pod's node is gone. `publishNotReadyAddresses` appears nowhere in this repo today (verified by grep) so nothing exercised it either way.

**Selector is `selector_labels(share)`**, share-scoped, never fixed — a fixed selector would front every hub in the namespace, which `render.rs:18-23` names as "the whole reason the operator can run a fleet in one namespace." This needs its own unit test: `normalize()` strips `selector` recursively from both sides of the golden comparison (`render.rs:1186`), so parity could never catch a wrong one even if the chart rendered this object.

**No annotations and no annotations knob.** A headless Service has nothing an annotation would tune, and inheriting `spec.service.annotations` would put NFS LoadBalancer tuning onto it where it is inert but reads as a promise.

### 3.3 Status surface

```rust
/// Where the file API answers, as an absolute URL. Absent when there
/// is no file API, when the operator could not verify its Service, and
/// on the paths that must not advertise a door (`Failed`,
/// `Reprovisioning`, `Terminating`).
///
/// This says WHERE. `phase` and `hubPhase` say WHETHER. Read them
/// first: a parked share has no pod, so this name does not resolve.
///
/// The intended consumer is a component that also reads this CR and can
/// patch `chert.us/requested-at`. A caller holding only this URL cannot
/// wake a parked share and is not a supported consumer.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub api_endpoint: Option<String>,

/// The hub's OWN phase, from the operator's `/status` poll, when a poll
/// succeeded on this pass. Absent means "not observed this pass" — it
/// is NOT a statement about the hub. Never carried forward.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub hub_phase: Option<String>,
```

```yaml
status:
  phase: Ready
  address: tenant-a.workspaces.svc.cluster.local:2049
  apiEndpoint: http://tenant-a-api-3f9c1b7e.workspaces.svc.cluster.local:8080
  hubPhase: Serving
  conditions:
    - type: ApiEndpointPublished
      status: "True"
      reason: InCluster
```

`apiEndpoint`, not `apiAddress`. No nested object — one string is the whole fact. No printer column: a second address column is noise in a 3000-row listing, and this field's reader is a front door, not a human at a terminal.

**How it is computed.** `render::api_endpoint(share, namespace) -> Option<String>`, in `render` so the Service name and the published name cannot drift:

1. `monitoring.enabled != Some(true)` or `fileApi.enabled != Some(true)` ⇒ `None`.
2. Stage 2 only: `advertiseUrl`, trimmed, non-empty ⇒ that string verbatim. (Empty/whitespace falls through, matching `address_of`'s pinned behaviour at `reconcile.rs:649-651` and its test at `:3474-3479`.)
3. Otherwise ⇒ `http://{names.api_service}.{namespace}.svc.cluster.local:{port}` where `port` is `fileApi.port ?? monitoring.port ?? 8080`.

The derived form spells `http://` explicitly. One type, no guessing, and the in-cluster hop is honest about being cleartext.

**`address_of` is not reused and must not be.** It takes a live Service, reads `spec.ports.first().port` **positionally**, and returns `None` for a LoadBalancer with no ingress. Reusing it would import four absent-windows the API door must not have — LoadBalancer-before-ingress, every idle-ladder transition pass, `HibernateDeferred` (which returns `Phase::Ready` with a 30s requeue and can persist indefinitely under the shipped `rpoClean false forever` condition), and `Terminating`.

**Two verifications gate publication**, both on passes that write status for file-API-enabled shares:

- **The Service exists and is ours.** A pure formula can never observe that its Service was deleted; `status.address` self-heals in one reconcile because §6 GETs the NFS Service live. Without a GET, a hand-deleted or GC'd API Service leaves `apiEndpoint` published and `ApiEndpointPublished=True` for up to five hours on a parked share, and the front door is told yes while DNS returns NXDOMAIN. So: GET it. Missing ⇒ `reason: ServiceMissing`, endpoint withheld.
- **The token actually resolves.** CEL can only check that `tokenSecretRef` is a non-empty *string*. The key the hub reads is hardcoded — `render.rs:307-311` emits `tokenFile: /etc/flint/api-token/token` and `render.rs:639-646` mounts the whole Secret at that directory. A Secret whose key is `api-token` mounts fine, the file does not exist, `resolve_token()` returns `None`, and `status::spawn` assembles only the base routes: `/files*` is **404** while `/status` answers **200 unauthenticated**. The hub is otherwise perfectly healthy — pod Ready, poll succeeds, `HubReachable=True` — so without this check the operator publishes an endpoint whose only reachable surface is an unauthenticated status document. The only existing signal is one hub log line; `StatusDoc` has no field reporting whether the routes are mounted (checked every field). So: GET the Secret, require a non-empty `token` key. Missing ⇒ `reason: TokenUnresolved`, endpoint withheld, Warning Event.

> **Rate cost, stated because the fleet plan demands it.** These are two extra namespaced GETs per reconcile **for file-API-enabled shares only** — not the fleet. The Service GET is net-zero against the merged design's collision guard (which the uid name retired) and is the same call §6 already makes for the NFS Service. If the file-API-enabled subset turns out to be the whole fleet, the Secret GET must move onto a **label-scoped** Secret reflector rather than a per-pass GET — and note the trap: this operator has already shipped a Secret watch that held *every* Secret in the cluster.

**Every `write_status` call site.** `write_status` force-applies the whole struct and every field is `skip_serializing_if`, so **`None` is a removal, not a no-op**, and there is no `..Default::default()` anywhere.

| site (working tree) | phase | `apiEndpoint` | `hubPhase` | `ApiEndpointPublished` |
|---|---|---|---|---|
| ~:928 conflict-rejected | `Failed` | `None` | `None` | `False / Conflict` |
| ~:966 adoption-blocked | `Failed` | `None` | `None` | `False / AdoptionBlocked` |
| ~:1284 idle-ladder short-circuit | any | formula | polled-or-`None` | computed |
| ~:1336 main §6 | any | formula | polled-or-`None` | computed |
| ~:2352 cleanup (in-flight) | `Terminating` | `None` | `None` | `False / Terminating` |

**Conditions carried forward on early-return paths are lies, and the merged design shipped one.** `apply` seeds `conds` from the previous status (`reconcile.rs:741-745`) and `set_condition` upserts by type. The conflict and adoption-blocked paths set only `Conflict`/`AdoptionBlocked` and `Ready` — every other condition passes through verbatim. A share that was serving and then lost arbitration would publish `phase: Failed`, `apiEndpoint` pruned, and `ApiEndpointPublished: True, reason: InCluster`: the field says the door is gone and the condition says it is open, on exactly the CR where a front door is deciding what to do about a conflict. Hence the two new reasons above, set explicitly. **The general rule is worth writing into `reconcile.rs`:** any condition not explicitly set on an early-return path is stale on that path. The other ten condition types on those two paths should be audited in the same change.

**`ApiEndpointPublished` reasons.** Conditions are a free-form `Vec<ShareCondition>` with a `type: String` and no schema enum, so this costs no `SCHEMA_VERSION` bump.

| status | reason | meaning |
|---|---|---|
| True | `InCluster` | published, derived headless form |
| True | `AdvertisedUnverified` | stage 2: published, `advertiseUrl` copied verbatim, **never verified** |
| False | `NotEnabled` | `monitoring.enabled` or `fileApi.enabled` absent/false |
| False | `ServiceMissing` | the API Service is not there |
| False | `TokenUnresolved` | the Secret has no non-empty `token` key |
| False | `NameCollision` | the name exists and is owned by another CR |
| False | `ApplyFailed` | the API Service apply was rejected |
| False | `Conflict` / `AdoptionBlocked` / `Reprovisioning` / `Terminating` | withheld on purpose |
| False | `StrandedService` | a required delete was refused 403 |

There is no `Advertised`-with-a-plain-`True` reason. The operator has no signal about the admin's front door, ever; a bare `True` would be an observation it never made.

`set_condition` preserves `lastTransitionTime` when only the reason changes, which is correct — the door did not open or close, only its posture is being restated — and it also means the timestamp carries no liveness information. Say so in the doc.

**Warning Events amplify, they do not signal.** `PlaintextApiEndpoint` (stage 2, on an `http://` advertise), `ApiTokenUnresolved`, `ApiServiceNameCollision` — all on reason *transition* only, never per pass. The condition reason is the durable record, because this repo has already shipped a release where **every** operator event was 403: kube-runtime publishes `events.k8s.io/v1` while RBAC granted the core group, silently, because publishing is best-effort.

### 3.4 Withdrawal is a control, not an announcement

On three paths the operator does not merely stop advertising — it **deletes the API Service**, owner-verified by name, best-effort, never blocking the path it runs on:

1. **Top of `cleanup()`**, in the same place `status.apiEndpoint` is cleared, *before* any reclaim work. The finalizer window is a window where `GET` succeeds and owner GC has not run, so an advertised HTTP door does not hang — it **succeeds**, and a client `PUT`s into a volume whose PVC is about to be destroyed under `reclaim: Delete`.
2. **The conflict-fence path**, before the `replicas: 0` merge patch, so the loser's door is shut rather than left routable for the 120s grace during which its shutdown publishes land in the winner's subtree.
3. **When `monitoring.enabled` or `fileApi.enabled` flips false**, in the normal apply path.

Paths 1 and 2 matter because those branches return **before** §4 render and §5 apply (`reconcile.rs:833`, `:872`), so a losing share never renders, its fingerprint is never computed, and the object is otherwise unmanageable for as long as the share loses — a rewrite-capable door object with no operator code path that can reach it. The merged design's §4 described the delete path as though it were unconditional; the control flow does not provide that, so the delete is invoked explicitly from those two paths.

RBAC gains `delete` on `services`. On 403: `ApiEndpointPublished=False, reason: StrandedService`, a Warning Event, continue — matching the `ConflictFenceFailed` pattern for "new verb, old chart". Always by name, always after verifying the live object's controller ownerRef is this share's uid, **never by label selector** — a selector delete plus the adoption label story could remove a chart-installed Service. The `rbac.yaml` comment block records the blast radius in the register the existing `pvc/delete` note uses, and should note that the operator can *already* `patch` any Service, which permits retargeting a selector at an arbitrary workload — strictly more dangerous than deleting one.

**The NFS Service is not deleted on the fence path.** That is out of scope, it is a consumer-visible behaviour change to a mounted door, and it deserves its own change with its own e2e leg. Filed.

---

## 4. Safety

### 4.1 The default configuration, analysed

The hazard the brief named — "LoadBalancer API Service + default settings puts a read-write file API on the public internet" — is not defended. The knob is not offered. Here is why a default could not have defended it:

- `networkPolicy.enabled: false` in **both** charts (`flint-lite-operator-chart/values.yaml:76-77`, `flint-lite-chart/values.yaml:137-138`).
- `hubNamespaces: []`, so enabling it protects **no hub**. **MEASURED:** `helm template … --set networkPolicy.enabled=true -s templates/networkpolicy.yaml | grep -c 'kind: NetworkPolicy'` → `1`, the operator's own deny-ingress, and zero hub policies.
- `apiClientCIDRs: []`, `apiClientSelectors: []`.
- `apiClientSelectors` **does not work today**: `nindent 8` where the NFS list correctly uses 10 (`networkpolicy.yaml:90` vs `:67`). A two-key peer is a YAML parse error; the docs' own one-key example renders `podSelector: null` with `matchLabels` hoisted to peer level, which is not a valid `NetworkPolicyPeer`. **Any claim that a front door is admitted via selectors is false in the field.**
- A CNI that does not enforce ignores every rule in silence. kind's default is in that group, which is why no e2e leg asserts enforcement.
- `externalTrafficPolicy` is unset everywhere (zero grep hits across both charts, `render.rs`, `crd.rs`), so it is `Cluster`, so kube-proxy SNATs external traffic to a node IP and an `ipBlock` allowlist **cannot see the real client at all**.

Five independent opt-ins, all default-off, all in a chart owned by the operator installer while the exposure knob would sit in a CR owned by the tenant, with no admission-time link. Nothing in `reconcile.rs` consults NetworkPolicy. **Status publication is not a control.** Safe-by-construction is the only form of that promise this codebase can currently keep.

`NodePort` is refused for the same family of reasons and one more: it would open a cleartext, rewrite-capable port *plus* an unauthenticated `/status` on every node's interface, on many clouds with public IPs, on the strength of one CR line.

**What replaces it.** A cluster admin builds their own front door — Ingress, Gateway `HTTPRoute`, mesh export, or a LoadBalancer they write — with the API Service as its backend, and (stage 2) sets `advertiseUrl` so the CR describes what they built. On a peered VPC with routable service CIDRs the pod addresses behind the headless name are directly reachable and no gateway is needed at all.

**NetworkPolicy is deliberately not a precondition for publication.** The operator cannot evaluate a policy graph, a CNI may ignore every rule silently, and a share in an unlisted namespace is unprotected with nothing detecting it. Gating on it would produce a control that reports success while doing nothing.

### 4.2 Correcting the merged design's "legibility, not reachability" claim

The claim was that a Service in front of a pod port changes nothing, because NetworkPolicy is enforced at the destination pod and a ClusterIP DNATs to the same pod:port. That is true **only when NetworkPolicy is on**, which is not the shipped default — so the entire delta lands in exactly the case that matters.

Today the hub's HTTP surface exists only on ephemeral pod IPs and appears in **no DNS record at all** (the only Service is NFS-only and not headless, so there are no pod A records). Reaching it needs pod-list RBAC in the target namespace or a scan of the pod CIDR, and any address found dies on the next restart.

Under this design a file-API-enabled share gains a stable cluster-DNS name resolvable from any pod in any namespace with zero RBAC. Three things narrow that from the merged design's version and none of them eliminate it:

- The name embeds 8 hex of the CR uid, so it is not enumerable by guessing share names.
- The Service is gated on `fileApi.enabled`, not on `monitoring.enabled` alone — a share that never asked for a file API does not get a VIP in front of an unauthenticated `/status`. (The merged design gated on `monitoring.enabled`, which is required for the idle ladder, i.e. effectively the whole fleet.)
- Headless means no VIP; the name resolves to pod IPs.

**Residual, stated plainly:** anyone who can read a FlintShare can compute the endpoint and reach an unauthenticated `/status`, cross-namespace, from any pod. Reading a FlintShare is a wider grant than reading its Secret in some deployments. There is no rate limit, lockout, concurrency cap or per-source accounting anywhere in `fileapi/` or `status.rs`, and `constant_time_eq` returns early on a length mismatch (`fileapi/mod.rs:619-627`) so token *length* is oracle-able even though content is not. A per-source failed-auth counter and a modest concurrency cap on the auth filter are worth doing independently of this design; they are filed, not built here.

### 4.3 Endpoint impersonation by label squat — analysed and dominated

The attack: any principal that can create a Pod in the share's namespace applies a pod with `app.kubernetes.io/name: flint-lite` and `chert.us/share: tenant-a` and a container port named `http`. It joins the EndpointSlice, kube-proxy or the headless A record spreads traffic across it, and it logs the `Authorization` header of every request a front door sends. There are no admission webhooks in this repo (verified: zero hits for `ValidatingWebhook|MutatingWebhook|admissionregistration` across both charts and all of `src/`), and the operator never reads EndpointSlices, so `ApiEndpointPublished` stays `True` and nothing is logged.

**This is real, and it is not a privilege escalation, because the capability it requires already yields the credential more directly.** A pod may mount any Secret in its own namespace; that requires only pod-create, not RBAC on the Secret. So a principal with pod-create in the share's namespace can mount `flint-api-token` and read every file-API token in that namespace outright — no squat, no interception, no waiting for a request. EndpointSlice selection is namespace-scoped, so a squatter can only join Services whose Secrets it can already mount. The squat gains nothing an attacker did not already hold.

**What this means, and it must be written down because nothing in the charts enforces it:** *a namespace that holds FlintShares is a control-plane namespace. Granting pod-create in it is equivalent to granting read on every file-API token in it.* That belongs in `docs/flint-lite-operator.md` as a deployment requirement, not as a footnote.

**Residual that the dominance argument does not cover:** interception is stealthier and more durable than a Secret read — it survives a rotation, it produces no Secret access in the audit log, and it captures tokens the attacker could not otherwise attribute to a share. That matters for detection and forensics, not for authorization. Two cheap follow-ups are filed: echo the hub's `serverId` as a response header on every file-API call so a caller can detect that it is talking to something other than the hub named in `status.serverId`; and fleet-auth §8, under which a captured token is scoped to one project and expires in minutes.

**The merged design's §5.4 rule is withdrawn.** "A caller presenting the token must resolve the in-cluster Service name itself rather than dialling whatever `advertiseUrl` says" points the victim *at* the vulnerable path. The correct rule: **a component presenting a token must dial an endpoint whose route it controls** — its own Ingress, its own mesh route, or a pod it resolved itself — and must treat both `status.apiEndpoint` and `advertiseUrl` as display values and routing hints.

### 4.4 `monitoring.port: 2049` — the collision the merged design declined to refuse

`spec.monitoring.port` is an unvalidated `int32` (`flintshares.yaml:245-249`: `format: int32, nullable: true`, no minimum, maximum, enum or CEL). Setting it to 2049 puts the read-write file API on the port the consumer Service targets — and `spec.service.type: LoadBalancer` is a first-class enum value with no CEL constraining it and is the **documented** cross-cluster recommendation (`docs/flint-lite-operator.md:159-163`, `flint-lite-chart/README.md:82`, `docs/flint-lite.md:52-53`).

The whole chain is verified: `mds.bind.port` is the constant `NFS_PORT` (`render.rs:216-219`) while `monitoring.health.port` is `m.port.unwrap_or(HEALTH_PORT)` (`render.rs:299`); `status::spawn` binds `0.0.0.0:cfg.port` **first**, explicitly "before the tier, before the listener" (`server.rs:626-668`), ahead of `start_tier` (`:705`) and ahead of `serve_tcp`; both probes are `TCPSocketAction { port: Int(NFS_PORT) }` (`render.rs:556-573`, `:740-744`), so warp on 2049 satisfies them within ~10s and the pod enters the consumer Service's endpoints during the entire minutes-long pre-listener window; the consumer Service's `targetPort` is the constant `NFS_PORT`. Then `serve_tcp` binds and gets `EADDRINUSE`, `nfs_mds_main.rs:161-165` returns `Err`, the container restarts, and the exposed window repeats forever under CrashLoopBackOff.

**This design makes it more likely, not less**, because it removes the supported route to cross-cluster API reach and "put it on the port that already has a LoadBalancer" is exactly the workaround that invites. So it is refused at admission — rule **M4** in §5 — and the merged design's sentence "the named `targetPort` keeps the pod port a single number so this cannot get worse" is deleted, because it is the claim that would stop a reviewer from looking here.

Two adjacent items are filed, not fixed: `serve_tcp`'s bind failure surfaces only as a generic error in a CrashLoopBackOff with no hint that the health listener took the port; and the operator chart's hub NetworkPolicy hardcodes `port: 8080` against a free `spec.monitoring.port`, whose failure mode is denying the operator's own `/status` poll and therefore "an unknown hub that never suspends". The latter is mitigated by a `networkPolicy.apiPort` helm value defaulting to 8080 — a mitigation, not a fix, because helm cannot see CRs.

### 4.5 The tenant-controlled string (stage 2)

`advertiseUrl` is a mutable spec field on a tenant-writable CR, copied verbatim into status. A front door that dials it while holding that project's token can be induced to hand the token to a host — and a scheme — the tenant chose. The same primitive exists for NFS via `spec.service.advertiseAddress`, so this is a widening of a known class, but a credential makes it materially worse and a mount address is dialled without one.

Three specific widenings the merged design missed, all closed by rule **M1** in §5:

- **CRLF.** `[^/?#@]` matches `\r`, `\n`, tab and space. **MEASURED on k8s 1.36** against the merged design's exact rule: `"https://p.example.com\nX: 1"` → ACCEPTED; `"https://p.example.com /x"` → ACCEPTED; `"https://a\tb.com"` → ACCEPTED. Any consumer interpolating the value into a request line, a header, a redirect, an audit log line or a generated config is writing attacker-chosen CRLF.
- **No length bound.** Hence `maxLength: 2048`.
- **`@` was excluded from the path**, which is stricter than RFC 3986 and rules out the email-derived project ids this operator's own docs describe. **MEASURED:** `"https://p.example.com/a@b"` → REJECTED, with a message that names "query, fragment, userinfo or trailing slash" and says nothing about `@`.

**And one that CEL cannot close: `advertiseUrl` is not arbitrated.** `conflict::admit` exists to guarantee at most one live share per bucket subtree, fleet-wide, with a CEL immutability rule on `bucket`/`keyPrefix`, a machine-readable redirect and a two-cluster drill. `advertiseUrl` is a second, stronger endpoint identity with **no uniqueness check of any kind**: two shares in two namespaces may publish the identical value and neither CR shows a conflict signal, because `admit` keys on `(endpoint, bucket, prefix)` and never looks at what a share advertises. A fleet project service that holds every project's token and builds its routing table from published endpoints then has two projects claiming one URL, and fleet-auth §7 already records that "who read which file is answerable only in the project service's own audit log, written BEFORE the API call" — so the misattribution is unrecoverable after the fact.

**Stage 2 therefore does not ship on the strength of a doc sentence.** Either fleet-auth §8 (audience-scoped ServiceAccount tokens, which make a token minted for one project useless at another's door and retire the whole class) lands first, or stage 2 adds a fleet-snapshot duplicate-`advertiseUrl` check publishing a condition and withholding the endpoint — accepting that as a new O(fleet) per-reconcile rate term, which the fleet plan says is the shape of every blocker in this operator. That decision is §10's first open question.

### 4.6 Revocation does not work the way an incident responder would assume

`token.rs:111-128`: `refresh()` on an **empty** file logs "keeping the previous token" and returns false; on an **unreadable or missing** file, the same. So emptying the `token` key, or deleting the Secret, leaves a compromised credential accepted on every request, indefinitely. Only writing a **new non-empty value** revokes. Deleting the Secret is worse than useless: the volume is non-optional (`render.rs:639-646`), so the token stays live until the next restart and the pod then wedges in `ContainerCreating` — taking NFS down with it, hours later, looking like an unrelated outage.

The keep-last-good rationale is sound (the kubelet's atomic symlink swap makes transient reads possible). The gap is that no explicit revocation path was added alongside it and nothing documents that the obvious ones do nothing. `docs/plans/file-api-fleet-auth.md`'s "no revocation short of a Secret write" reads as "a Secret write revokes"; two of the three Secret writes a responder reaches for revoke nothing.

**This design does not change any of that, and it makes it matter more**, because its whole purpose is to put a stable address in front of that credential. Two things must land with it:

- **Document the actual rule** in the CRD doc comment and the operator guide, in one sentence: revocation is writing a *new non-empty value*; emptying the key or deleting the Secret revokes nothing and will wedge the pod on its next restart.
- **Name the working kill switch**: setting `spec.monitoring.fileApi.enabled: false` changes `mds.yaml`, which moves `config_checksum`, which rolls the pod — the routes come back unmounted and every file call is a 404. That is a genuine revocation, it already works, and it is not written down anywhere.

**Rejected: having the operator rewrite the token Secret on `Terminating` or on a conflict fence.** It would work, but it requires granting the operator write on tenant-owned Secrets — from which it could mint any project's token — and `file-api-fleet-auth.md:86-91` is explicit that the operator must not hold that capability, "because it would gain the ability to mint every project's token while having no need to produce any." The Service delete in §3.4 closes the door without touching the credential.

### 4.7 The conflict loser and cross-namespace disclosure

`status.apiEndpoint` is `None` at both `Failed` sites, hardcoded, beside the existing `address: None`. The loser's API Service is **deleted** on the fence path (§3.4), so the door is shut rather than un-advertised.

**`status.conflictWith` gains no API counterpart, ever, in any namespace.** Two independent arguments. *Utility:* a loser's owner holds no token for the winner's file API, so it is a pointer to a door they cannot open — zero utility at strictly wider disclosure. *Doctrine:* the NFS redirect passes the "could already have read it" test (`crd.rs:653-668`) because a same-namespace reader could have found the export by scanning. An API endpoint fails that test in both directions — weaker without the token, far more damaging with it, since `/files/move` does not condition its destination. The uid-derived name is what makes the refusal *hold*: `ConflictWith` carries `name` and `namespace`, so a name-plus-namespace formula would have made the winner's door computable from the loser's own status, turning an authorization decision back into a side effect. Record the refusal in the `ConflictWith` doc comment so it is not re-proposed.

---

## 5. Validation rules

All attached to the **`MonitoringSpec` node**, following the `IdleSpec` precedent (`crd.rs:284-288`) so `self` is the monitoring object and no outer `!has(self.monitoring)` guard is needed. This keeps the spec root flat and puts each failure nearer its field. It does **not** reduce the CRD's cost budget, which is a total.

### Stage 1

**M3 — `fileApi` needs `monitoring.enabled`.** Closes a shipped silent-ignore in the exact block being opened: `render.rs:295` nests the whole `fileApi` branch inside `monitoring.enabled` and no rule guards it, which is the class every other cross-field rule in this CRD exists to refuse.

```
!has(self.fileApi) || !has(self.fileApi.enabled) || !self.fileApi.enabled || (has(self.enabled) && self.enabled)
```
> `spec.monitoring.fileApi.enabled needs spec.monitoring.enabled — they share one listener, so the file API is silently ignored without it`

**M4 — the monitoring port is not the NFS port.** §4.4.

```
!has(self.port) || (self.port != 2049 && self.port >= 1024 && self.port <= 65535)
```
> `spec.monitoring.port must be 1024-65535 and must not be 2049 — 2049 is the port the consumer Service targets, and the health listener binds before the NFS listener, so the file API would take the NFS port and the hub would crash-loop`

Back M4 with a **render-time refusal** as well, so a CR stored before the rule cannot resurrect the collision on an operator upgrade.

### Stage 2

**M5 — the file-API port is distinct.**

```
!has(self.fileApi) || !has(self.fileApi.port) || (self.fileApi.port != 2049 && self.fileApi.port >= 1024 && self.fileApi.port <= 65535 && self.fileApi.port != (has(self.port) ? self.port : 8080))
```

**M1 — `advertiseUrl` shape.**

```
!has(self.advertiseUrl) || (self.advertiseUrl.matches('^https?://[^[:space:][:cntrl:]/?#@]+(/[^[:space:][:cntrl:]?#]*)?$') && !self.advertiseUrl.endsWith('/'))
```
> `spec.monitoring.advertiseUrl must be an absolute http(s) URL with no query, fragment, userinfo, whitespace, control characters or trailing slash — e.g. https://projects.example.com/p/tenant-a. A client joins it as <base>/files`

**M2 — never advertise a door without a lock, a split listener, or a blast-radius decision.** `has()`-only, near-zero cost.

```
!has(self.advertiseUrl) || (has(self.enabled) && self.enabled && has(self.fileApi) && has(self.fileApi.enabled) && self.fileApi.enabled && has(self.fileApi.tokenSecretRef) && self.fileApi.tokenSecretRef != '' && has(self.fileApi.maxDownloadBytes) && has(self.fileApi.port))
```
> `spec.monitoring.advertiseUrl needs monitoring.enabled, fileApi.enabled, a non-empty fileApi.tokenSecretRef, an explicit fileApi.maxDownloadBytes and fileApi.port — advertising a door that also serves an unauthenticated /status is pure-loss disclosure, and 5Gi is not a decision`

`fileApi.port` is in that list because it is the mechanism that makes "deny `/status` at the front door" a socket boundary rather than a sentence. `maxDownloadBytes` is there because one careless browse click on a cold object is real, billed S3 egress.

### Traps for the implementer

- **No backslashes anywhere inside a CEL string literal.** `\[` fails to parse as a *string literal* before RE2 sees it, and the API server then refuses the **whole CRD**, taking every other rule with it (`crd.rs:122-127`). The trap is backslashes, not brackets: `[^:]` and `[0-9]` already ship unescaped, and `[^[:space:][:cntrl:]/?#@]` is a bracket expression containing POSIX classes — no backslash required.
- **No `startsWith('[')` gymnastics here.** The NFS rule needs it because it must locate a port and IPv6's colons make the last-colon rule ambiguous. A URL delegates the default port to the scheme, so `https://[2001:db8::1]:8443` matches the authority class with no special case — none of `/ ? # @` occurs in an IPv6 authority. Copying the NFS regex would reject every legitimate gateway URL.
- **Printer-column jsonPaths: no backslash-dot.** Not an issue here — this design adds no printer column.
- **The CEL cost budget is a per-CRD total, and node placement does not reduce it.** **MEASURED:** a probe CRD carrying the merged design's rule set — 14 rules including two unbounded `.matches()` — installs and evaluates correctly on kind, k8s 1.36. That retires the merged design's "unmeasured budget" tension *at that shape*. Stage 1 is 15 rules; stage 2 is 18 with two `.matches()`. **Stage 2's shape has not been measured and must be**, because exceeding the budget makes the API server refuse the whole CRD at install — the same blast radius as the backslash trap, and it would also take down the operator's self-managed CRD update path. Add "install the `crdgen` output against a real API server and confirm acceptance" to `scripts/release.sh` beside the existing stale-CRD check.
- **POSIX classes inside a CEL `.matches()` must be re-verified on a real API server before landing.** The base regex shape is measured; the POSIX-class variant is not.
- **Explicit `null` on a nullable field is not a CEL runtime error.** **MEASURED:** `has()` treats it as absent, so M2/M3 reject cleanly with their own messages.

---

## 6. Behaviour in every phase

`hubPhase` is present only when the operator's `/status` poll succeeded on that pass. It reaches a caller even when the pod's node is gone, which is the point.

| k8s phase | pod | API Service object | endpoints | `apiEndpoint` | `hubPhase` | `ApiEndpointPublished` | what a caller sees |
|---|---|---|---|---|---|---|---|
| `Pending` | none | exists | none | **set** | absent | `True / InCluster` | DNS resolves to nothing |
| `Starting` | up, pre-listener | exists | **none** (probe is TCP 2049) | **set** | `ClaimingEpoch` / `Importing` | `True / InCluster` | no A record; read `hubPhase` |
| `Ready` | up | exists | 1 | **set** | `Serving` / `Sweeping` | `True / InCluster` | works |
| `Suspended` (admin) | none | exists | none | **set** | absent | `True / InCluster` | no A record; an admin said no |
| `IdleSuspended` | none | exists | none | **set** | absent | `True / InCluster` | no A record; wakes on `requested-at` |
| `Hibernated` | none, PVC gone | exists | none | **set** | absent | `True / InCluster` | no A record; wake is slow, `serverId` will change |
| `Reprovisioning` | either | exists | maybe | **absent** | maybe | `False / Reprovisioning` | withheld on purpose |
| `Failed` (conflict) | scaling to 0 | **deleted** | — | **absent** | absent | `False / Conflict` | gone |
| `Failed` (adoption) | varies | **deleted** | — | **absent** | absent | `False / AdoptionBlocked` | gone |
| `Terminating` | up | **deleted first** | — | **absent** | absent | `False / Terminating` | gone before the reclaim |
| `fileApi` off / `monitoring` off | any | **deleted** | — | **absent** | absent | `False / NotEnabled` | never existed |
| token key wrong | up | exists | 1 | **absent** | `Serving` | `False / TokenUnresolved` | withheld; `/files` would 404 |
| Service hand-deleted | any | missing | — | **absent** | maybe | `False / ServiceMissing` | withheld within one requeue |

**`Reprovisioning` is withheld, diverging from the merged design.** `phase_of` reports it for both halves — including the one where the pod is up, with the comment "a consumer that reads Ready here would mount a hub whose disk is about to be destroyed under it" (`reconcile.rs:618-623`) — and all six `drive_reprovision` arms carry `short_circuit`, so `status.address` is `None` throughout. In `ReprovisionVerifying` the hub is Serving and the file API returns 201 to a `PUT`; `snap.hibernatable()` is evaluated **once**, before the scale-to-zero, with no re-check before `claims.delete`. The only protection is the graceful flush inside `terminationGracePeriodSeconds` (default 120) against a measured publish rate of 13.3 s/GiB. Publishing the endpoint would invite an API client into the window an NFS client is kept out of, and hand it a 201 for a write that is about to be discarded. Withholding is stable, not flickering: `Reprovisioning` is a settled phase reported on a stable short-circuit.

**The cost of that choice, named:** the `ReprovisionDeferred` arm ("stay up and keep flushing", `REQUEUE_BLOCKED` 30s) holds `Phase::Reprovisioning` indefinitely on a completely healthy hub, and this repo has already shipped the `rpoClean false forever` condition that reaches it. Such a share serves perfectly and publishes no endpoint, forever. `hubPhase` (`Serving`) is what lets a consumer tell that apart from a real reprovision. The real fix is the `rpoClean` bug, not this field.

**Filed, adjacent, not fixed here:** `hibernatable()` is not re-checked after the drain and before `claims.delete`, so nothing verifies that writes accepted after the first check were flushed.

**Publishing in the down phases is kept, and its audience is narrowed.** The wake protocol is a `chert.us/requested-at` annotation patch on the FlintShare, cleared as the wake fires (`reconcile.rs:1678-1682`); nothing in the data path is a trigger, deliberately, because a wake-on-call would let a poller resurrect the fleet. So a caller holding only the URL cannot wake a parked share. Because the endpoint is uid-derived, learning it requires reading the CR anyway — which is exactly the audience that *can* patch `requested-at`. The schema says so. A caller handed only the URL is not a supported consumer.

**Headless changes the failure shape in the down phases, for the better.** A headless Service with zero endpoints returns no A records, so a client gets a **resolution failure** rather than a connection refused or a hang. That is a clearer signal than either NFS door state.

---

## 7. Migration, compatibility, parity

### Existing FlintShares

Stage 1 is additive to the spec. Behaviour changes only for shares with `monitoring.enabled && fileApi.enabled`, and only by gaining an object.

**M4 can reject a CR that is valid today** (any share with `monitoring.port` outside 1024–65535, or equal to 2049). Audit before landing:

```
kubectl get flintshares -A -o json | jq -r '.items[] | select(.spec.monitoring.port != null) | select(.spec.monitoring.port == 2049 or .spec.monitoring.port < 1024 or .spec.monitoring.port > 65535) | "\(.metadata.namespace)/\(.metadata.name) port=\(.spec.monitoring.port)"'
```

**M3 can also reject a CR that is valid today** (`fileApi.enabled: true` with monitoring off). Such a CR is already being silently ignored, so behaviour does not change — only loudness — but its next `kubectl apply` fails. Audit:

```
kubectl get flintshares -A -o json | jq -r '.items[] | select(.spec.monitoring.fileApi.enabled == true) | select((.spec.monitoring.enabled // false) == false) | "\(.metadata.namespace)/\(.metadata.name)"'
```

If either audit finds hits in the field, split that rule out and land it with a transition-guarded form.

### The upgrade apply storm is the WHOLE fleet, not the file-API subset

The merged design said "every monitoring-enabled share takes one full apply pass". That is wrong. `render_fingerprint` hashes a fixed array with `DefaultHasher` (`reconcile.rs:122-133`); adding a fifth element changes the digest for **every** input, including shares where `api_service` is `None`, because `Option::hash` writes a discriminant.

So on a 3000-share fleet: every share's stored `chert.us/render-hash` mismatches, `apply_gate_state` returns `None`, `skip_applies` is false for all 3000, and each does ConfigMap + Service + Deployment applies plus a Deployment annotation restamp — roughly 12,000 writes spread over one `REQUEUE_PARKED` window (1800s) at reconcile concurrency 32. That is the write-rate class v1.33.0 spent effort reducing from ~99/s to ~0.24/s, and it should be stated in the release notes.

**No pods restart.** `mds_yaml` is untouched in stage 1, so `config_checksum` does not move, so `checksum/config` does not move, so no mounted client loses its ~90s grace. That is the design's best compat property and it is a direct consequence of refusing to put an addressing knob in hub config — the hub has no notion of its own address by construction (`bind: 0.0.0.0`, `render.rs:216-219`).

**Rejected: hashing `api_service` only when `Some`,** so monitoring-off shares keep their hash and skip the pass. It re-introduces the "a gate that hashes a subset stops noticing" hazard the fingerprint comment exists to warn about. Take the storm; say so.

**Timing, corrected.** `apply_gate_state` returns `None` — which always means apply — the moment the hash does not match the Deployment's annotation (`reconcile.rs:138-147`). The Service is created on the **first reconcile after upgrade**, bounded by one requeue interval (300s live, 1800s parked), **not** by `FULL_APPLY_AFTER`. The 5-hour bound applies only to re-asserting an object whose rendered hash still matches — i.e. repairing external drift on a **parked** share, which is why the existence GET in §3.3 matters: it is what turns a hand-deleted API Service into a withheld endpoint within one requeue instead of a lie for five hours. Note also that the apply gate is restricted to parked shares (`reconcile.rs:1024-1029`, "Live shares are deliberately NOT gated"), so on live shares the applies and their owner-verifying GETs run every reconcile.

### Charts

**Stage 1 does not touch `flint-lite-chart` at all.** Consequences, all measured:

- `chart_sha256()` covers exactly `Chart.yaml`, `values.yaml`, `templates/hub.yaml`, `templates/_helpers.tpl` (`render.rs:1308-1313`). None is touched, so `parity_fixture_matches_the_current_chart` stays green with no regeneration.
- `scripts/check-render-parity.sh` is unchanged. Its `("Service", 1)` count assertion and its kind-index-0 selection both remain correct — which avoids the measured trap that helm orders `flint-lite-api` **before** `flint-lite` when the object lives in its own template file, so bumping the count to 2 would silently record the wrong object and `--write` would bake it in.
- The four golden cases are unchanged, **including `full`**, which stays the recorded proof that a LoadBalancer consumer Service carries only the `nfs` port with the file API enabled. That proof is more valuable intact than overloaded.
- `pnfs::config`'s `hub.yaml` `include_str!` test is unaffected.
- `tests/regression/two-doors-kind.sh` stays valid on the chart path.

**The cost is a third sanctioned divergence.** `render.rs:16-26` enumerates exactly two intended differences (selector labels; `checksum/creds`) under the premise "two sources of truth for the same hub". The API Service joins that list **with its reason**: it is not part of the hub, it is part of the operator's published contract, and a chart-rendered hub has no CR to publish an endpoint into. `Rendered` will hold an object the golden fixture does not describe and cannot describe. Its only guards are the unit tests in §7 below — and because the operator's own `/status` poller deliberately keeps using the pod IP, the API Service has **zero internal consumers**, so nothing in the operator's hot path exercises it either. That is the honest weak point of this whole change.

**Stage 2 does touch the chart**, because `fileApi.port` is a hub-config knob: `values.yaml`, `templates/_helpers.tpl` (the config emitter) and `templates/hub.yaml` (a second containerPort named `files`) all change, so `chart_sha256()` moves and `scripts/check-render-parity.sh --write` must be re-run with the fixture regenerated. The `mds.yaml` line is emitted only when the knob is set, so **only opting-in hubs roll** — but they do roll, at ~90s of grace per mounted client, and `rollout_checksum` strips exactly one boot-only knob so there is no way to avoid it.

**`flint-lite-operator-chart`:** `delete` added to `services` with the blast-radius comment; `networkPolicy.apiPort` value added; **and the `apiClientSelectors` `nindent 8 → 10` fix as an independent prerequisite commit** — the docs point users at that knob and prescribing one that renders invalid YAML is not acceptable.

**Filed, not bundled:** widening `chart_sha256()` from a four-file allowlist to the whole `templates/` directory (it is already blind to `networkpolicy.yaml`, the "passes by not looking" class its own header warns about), and narrowing `normalize()` so Service selectors are compared.

### CRD

`bootstrap::SCHEMA_VERSION` **6 → 7** for stage 1 (M3, M4, `status.apiEndpoint`, `status.hubPhase`), **7 → 8** for stage 2. No new phase value, no new printer column, so the enum-storage hazard does not apply and `crd_prints_the_fleet_columns` is untouched. Regenerate `flint-lite-operator-chart/crds/flintshares.yaml` with `cargo run --bin crdgen` in the **same commit**; `scripts/release.sh:186-192` refuses to push otherwise.

**Ordering.** This builds on the working tree, not HEAD: five `FlintShareStatus` construction sites (not four), `SCHEMA_VERSION` 6 (not 4), `Terminating` already present, `pods/delete` reverted. If the `conflictWith` branch lands second, the ladder entries and site list renumber and nothing else changes — the two touch disjoint status fields and disjoint CEL nodes.

**Downgrade.** The CRD sets no `x-kubernetes-preserve-unknown-fields`, so an operator at 7 writing `status.apiEndpoint` against a CRD still at 6 has it **silently dropped at admission**, and an older operator restarting mid-upgrade re-applies its schema and **prunes the field out of every stored CR**. A rollback disables the feature and **strands** every API Service, because the downgraded operator has no code path that knows about them; they are ownerRef'd, so CR deletion still collects them. Document it in the upgrade section. The class this design is immune to: because the Service is gated on `monitoring.enabled` + `fileApi.enabled` — fields that exist at schema 6 and can never be pruned — **a downgrade cannot flip the gate and cause a fleet-wide delete.**

### Tests

Parity cannot cover this object, so these are the entire guard and none is optional:

1. **API Service shape**: `clusterIP == "None"`, one port, `port` equals the resolved container port **number**, `targetPort == "http"` (stage 2: `"files"`) by name, **no `publishNotReadyAddresses`**. The comment above this test says *why* the flag is absent, so nobody adds it back "because the Starting window is dark".
2. **Name**: `{base[..50]}-api-{uid[0:8]}`, at base lengths 49 / 50 / 63, asserting `<= 63` in all cases.
3. **Selector**: `== selector_labels(share)`. Mandatory — `normalize()` strips `selector` from both sides of the golden comparison.
4. **The consumer Service is unchanged**: exactly one port, named `nfs`, `targetPort: 2049`, regardless of `spec.monitoring`. Nothing asserts this today; the protection is entirely that `service()` happens to emit a one-element vec, while `address_of` reads `spec.ports.first().port` **positionally**.
5. **`render::api_endpoint`**: absent when monitoring or fileApi off; derived form spells `http://` and follows the resolved port; (stage 2) verbatim when advertised, whitespace-only falls through.
6. **Owner-verified apply**, both Services: a foreign ownerRef refuses without applying, sets the condition, and does not return `Err`.
7. **`the_render_fingerprint_moves_when_any_applied_object_moves`** extended.
8. **CRD guards re-run**: `crd_is_structural`, `crd_settings_have_no_defaults`, `flattening_keeps_the_enum_constraints`. The working tree does not compile, so nobody has run these against the in-flight state; run them before trusting any claim about flattening.
9. **e2e in `operator-kind-e2e.sh`, with anti-vacuity guards** — all four, because any one alone would pass on a hub whose file API had been added to the consumer Service: (i) `/status` answers 200 at the API Service name; (ii) it does **not** answer at the consumer Service ClusterIP; (iii) `/files` answers **401, not 404**, proving the routes are mounted and the token is enforced; (iv) `status.apiEndpoint` matches the rendered Service name exactly.
10. **Deletion legs**: deleting the share removes the API Service **before** the termination grace elapses; a conflict loser's API Service is gone before its pod is; `fileApi.enabled: false` removes it.
11. **`monitoring.port: 2049` is refused at admission**, and a stored CR carrying it is refused at render.
12. **`advertiseUrl` with an embedded newline is refused** (stage 2).

**Test 8 of the merged design is withdrawn as unimplementable.** It asked `two-doors-kind.sh` to assert 401 at `{base}-api` — but that script installs `flint-lite-chart` directly with no operator, no CRD and no FlintShare, and this design deliberately leaves that chart untouched, so the object does not exist on that code path. Written defensively it would pass vacuously by skipping, which is precisely the "passes by not looking" class this repo's drill methodology refuses. The assertion lives in leg 9 above. `two-doors-kind.sh` keeps its existing assertions unchanged and gains a comment saying that the API Service deliberately does not exist on the chart path — which is the concrete statement of the third sanctioned divergence.

### Docs, in this commit

1. `render.rs:16-26` — the third sanctioned divergence, with its reason.
2. `render.rs:575-578` — still literally true (not on the *consumer* Service); amend to name the sibling Service and to say the lifecycle controller still polls by pod IP **on purpose**.
3. `crd.rs` `MonitoringSpec` doc — amend "ClusterIP-only" to "headless, share-scoped, uid-named, and never on the consumer Service", and say the type is a **constant, not a default**.
4. `reconcile.rs:2216-2219` — why the poll deliberately does not use the new Service (it filters terminating pods; a Service would load-balance the poll across a rolling pair with no way to know which hub answered).
5. `mds/status.rs:291-295`, `fileapi/mod.rs:51-54` — the posture is unchanged; reaching it from outside is a cluster-admin routing decision, never a CRD knob.
6. `docs/flint-lite-operator.md:153` — a parallel "Reaching the file API from another cluster", mirroring the NFS section's table and bullets, stating plainly that the operator will not create a routable Service and that the NodePort trap has no API analogue because there is no type knob, **plus a fourth bullet the NFS door does not need: what else you just published on that port**.
7. `docs/flint-lite-operator.md:614` "The file API" — 120 lines that never name a host. Add addressing, the "read `phase` and `hubPhase` first" contract, the revocation rule from §4.6, and the front-door requirements.
8. **`docs/flint-lite-operator.md:667-671` and `file-api-fleet-auth.md` §2/§9 are factually wrong at HEAD** and must be corrected here (§2).
9. A deployment requirement: **a namespace holding FlintShares is a control-plane namespace; pod-create in it is equivalent to read on every file-API token in it** (§4.3).
10. `flint-lite-chart/README.md`'s Values table documents neither `monitoring` nor `networkPolicy` — fix while there, even though the chart is untouched.

---

## 8. Rejected alternatives

**A second port on the consumer Service.** `render::service()` emits exactly one `ServicePort` and all four golden cases — including `full`, which is LoadBalancer *with* the file API enabled — record exactly that. Adding a port breaks the golden test in the one case that proves the invariant, and it would put the file API behind whatever that Service faces, which is the thing four separate comments exist to prevent.

**`type: LoadBalancer` or `NodePort` on the API Service.** §4.1. The knob cannot be defended by any default this codebase can ship, so it is not offered. NodePort would have covered the one topology headless does not (two clusters with no shared routing and no gateway capability); that gap is accepted deliberately in §9.

**A `spec.apiService{}` block.** A whole new CRD node, a new `has()` hop in every guard, schemars anyOf-flattening risk, and new doc surface, to host what is now zero stage-1 fields. Worse, a new `apiService.enabled` field would be prunable by a downgrade, so "block absent ⇒ delete" would make a rollback delete the fleet's API Services. Gating on `monitoring.enabled` + `fileApi.enabled` makes that unrepresentable.

**`spec.service.apiAdvertiseAddress`.** Everything else under `spec.service` means *the consumer Service*, so a reader would reasonably expect `spec.service.annotations` to land on the API Service too.

**`host:port` for the endpoint, mirroring `status.address`.** One shape applied consistently is a simpler API than two, and that objection is real. It loses to this: the scheme is the only security-relevant fact about an HTTP door, and a cross-boundary field that cannot say `https` cannot distinguish "I put a TLS terminator in front" from "I exposed a rewrite-capable credential in cleartext". Two grammars is the price.

**Reusing `address_of`.** It reads `spec.ports.first().port` positionally, returns `None` for a LoadBalancer with no ingress, and would add a second live Service GET on the hot path. It would import four absent-windows the API door must not have, and its ladder-transition blanking makes a field unpollable.

**`publishNotReadyAddresses: true`.** §3.2 — withdrawn on three measured grounds.

**`{base}-api`.** §3.2 — withdrawn on four grounds, one of them a measured 422 that permanently wedges existing shares.

**Two ClusterIPs per share.** §3.2 — breaks the fleet plan's stated envelope and the failure is a cross-tenant DoS that lands on the NFS door.

**A CEL rule refusing CR names ending in `-api`.** Not expressible: the CRD's validations attach to the `spec` node, so `self` is the spec and `metadata.name` is not visible.

**A fleet-reflector scan for name collisions.** A per-reconcile O(fleet) rate term, against a plan of record whose every blocker is a rate term. A single owner-verified GET-by-name is O(1).

**Hashing the token Secret into `checksum/creds`.** Already considered and rejected in `file-api-fleet-auth.md` §9: it makes an HTTP credential change cost an NFS availability event on that share. Recorded so it is not re-proposed.

**Operator-rewrites-the-token-Secret as a revocation primitive.** §4.6 — it requires giving the operator a capability fleet-auth §5 explicitly denies it.

**Design 1's split listener as an optional follow-up.** It is not optional; it is stage 2's precondition. Under a headless, non-routable Service the unauthenticated `/status` is published exactly as far as it already was, so the split buys almost nothing in stage 1. The moment an admin builds a front door, it is the only mechanism that keeps `/status` off it.

---

## 9. What this does NOT solve

- **It does not make the file API reachable from another cluster. It makes it ADDRESSABLE.** Routing the name across a boundary is still an admin act. Anyone reading this as "cross-cluster file API, done" has misread it.
- **It does not cover two clusters with no shared routing and no gateway capability.** NodePort would have. That gap is the accepted price of making the dangerous configuration unrepresentable. **If the real deployment target turns out to be exactly that topology, this design does not solve the stated problem and the NodePort trade must be reopened deliberately, not by scope drift.**
- **Cross-cluster prefix arbitration is blind, and this feature's motivating topology is the blind spot.** `conflict::admit` reads one reflector. The fleet plan's B8, its "What this does NOT deliver" item 1, and `tests/regression/overlap-two-cluster-kind.sh` leg 3 ("two clusters, NESTED prefixes: both hubs reach Ready and BOTH hold a live epoch at once") all say so, and the hub-side fence (S12) is unbuilt. In that shape both operators return Admitted, both shares publish an endpoint, and **both CRs report `Conflict=False`** — a guarantee that was never made. Two consequences: the `Conflict` condition's meaning must be qualified in the schema to "unique among shares this operator can see", and **S12 is a named prerequisite for stage 2**, because stage 2 is the cross-cluster half.
- **No TLS.** The hub has none and `HealthConfig` is three fields. The published endpoint is `http://` unless an admin's front door terminates TLS — and the operator can tell `https://` in a string from `http://` and **nothing more**.
- **The unauthenticated `/status` and `/health` are not split in stage 1.** A Service is L4 and cannot split one socket. Stage 2's `fileApi.port` is the mechanism; until it lands, "deny `/status` at the front door" is documentation-enforced, which is why stage 1 ships no advertise knob.
- **The pre-auth phase oracle.** `routes_gated` is `gate.and(raw_routes(...))` (`fileapi/mod.rs:465`), so a pre-`Serving` hub names its phase to an unauthenticated stranger before the auth filter runs. Untouched; no test pins the ordering either way. Moving the gate behind auth is free and is filed.
- **The credential.** One token, no expiry, no audience, no per-caller identity, no rate limit, no lockout, no concurrency cap, no per-source accounting, `constant_time_eq`'s early length return, and `strip_prefix("Bearer ")` exact-case exact-space (a normalising proxy produces a 401). Fleet-auth §8 remains the endpoint of this work.
- **`advertiseUrl` is neither arbitrated nor verified** (§4.5, §10 Q1).
- **A token the operator did not project.** The operator sets only `RUST_LOG`, `POD_NAME`, `AWS_REGION` — but it `envFrom`s the whole `credentialsSecretRef` Secret when the share is tiered, so a tenant who puts a `FLINT_FILE_API_TOKEN` key there gets a working file API with a credential invisible to the operator. That makes the `TokenUnresolved` check very slightly over-strict in that obscure case, and it means a share can serve the file API with a credential the operator cannot see.
- **Whether the routes are mounted is not observable from `/status`.** `StatusDoc` has no such field (checked every field). The operator's Secret check (§3.3) is a proxy, not an observation. Adding `fileApi: { routesMounted: bool }` to `StatusDoc` is filed and is worth doing regardless of this design.
- **Waking.** `chert.us/requested-at` remains the entire protocol. No HTTP call — not even a 503'd one — may become a wake trigger, because file-API calls already count as activity (including a 304) and a wake-on-call would let a polling client resurrect every parked share. Poll `/status`, which deliberately does not count as activity — guidance that now has to reach callers who have never read this repo's docs.
- **A 503 for a share with no pod.** The gate runs inside the hub process; in the down phases the failure is at DNS. The answer is `phase` and `hubPhase`, not a synthesised response.
- **Interpreting a 503 fully.** Three unrelated causes — pre-`Serving` phase, hydration (`NFS4ERR_DELAY`), the write gate (`NFS4ERR_GRACE`) — share the status code and differ only in the body. `hubPhase` separates the first from the other two; it does not separate the second from the third.
- **Egress amplification.** `maxDownloadBytes` bounds the response and hub memory below the 8 MiB stream threshold. It does **not** bound the egress: the first `Range` of a cold object still hydrates the whole object out of S3.
- **ETag invalidation across a rebuild.** `render_etag(fileid, change)` is server-local; a hibernate recreates the PVC and the fileids, so every cached ETag fails **closed** with 412 — the right direction, but a remote client sees a stable URL, a phase returning to Ready, and a wall of 412s with no explanation. `status.serverId` is the signal and it is not echoed on any file-API response (the only headers set are `retry-after` and `etag`). Document that a `serverId` change invalidates every cached ETag; echoing it as a response header is filed.
- **`status.address` is unchanged.** Published in `Pending` before any pod, published in the down phases with nothing behind it, blanked on every ladder-transition pass and on all six reprovision arms. Nothing in the suite asserts what it says while a share is down, so that behaviour is unspecified rather than decided — and shipping a second endpoint field with better properties makes the older one look like a bug rather than a choice. Fixing it is a consumer-visible change to a field mounted in other people's clusters and deserves its own change with its own e2e leg.
- **The chart defects.** The operator chart's hardcoded NP port (mitigated, not fixed) and the lite chart's NFS rule keying `.Values.service.port` where the pod's 2049 is required both survive.
- **The `-config`/`-data` name-derivation class.** Only Services are guarded, and only because they are the one kind where two shares' formulas can now collide.
- **The API Service has zero consumers inside the operator.** Its correctness is exercised by unit tests and one e2e leg, written by the same author, and by nothing in the hot path.

---

## 10. Open questions for the author

1. **Does stage 2 wait on fleet-auth §8, or does it ship with a fleet-wide duplicate-`advertiseUrl` check?** §8 retires the whole unarbitrated-identity class (a token minted for one project is rejected by every other hub) and it is already the named endgame. The duplicate check is cheaper to build but adds an O(fleet) per-reconcile term to an operator whose every scaling blocker is a rate term. **My recommendation: §8.** Shipping stage 2 without either publishes an unarbitrated endpoint identity in a system whose entire premise is that endpoint identities are arbitrated.
2. **Is a headless Service an acceptable `backendRef` for the actual front door?** ASSUMED, not measured. If the answer is no, the choice is a per-share ClusterIP (breaking the fleet envelope) or telling admins to create their own Service per share they expose (paying an IP only where it is used). **Verify against the real front door before writing code.**
3. **Does `status.hubPhase` earn its place, given it is only present when the operator already polls?** It is the compensating control for dropping `publishNotReadyAddresses` and it is the only field that reaches a caller when the pod's node is gone. But `needs_hub_poll` returns true for idle-configured shares and not necessarily otherwise, so its absence is ambiguous unless every fleet share sets `spec.idle` — which is the current shape but not a guarantee.
4. **Should `spec.monitoring.port` be pinned to 8080 outright rather than merely constrained?** M4 refuses 2049 and bounds the range, but the operator chart's NetworkPolicy still hardcodes 8080, so any non-default value silently breaks the policy *and* the operator's own poll. Pinning would make the chart correct by construction at the cost of rejecting more stored CRs.
5. **Is `Reprovisioning` the right place to withhold?** Withholding protects `ReprovisionVerifying` writers from a discard; it also blanks the endpoint forever on a healthy share stuck in `ReprovisionDeferred` under the shipped `rpoClean false forever` condition. The alternative is publishing and relying on `hubPhase`. **My recommendation: withhold**, because the discard is data loss and the Deferred case is a bug to fix rather than a state to accommodate.
6. **Should the conflict-fence path also delete the consumer Service?** This design deletes only the API Service, so a fenced loser's NFS door stays advertised and routable for the 120s grace during which its shutdown publishes land in the winner's subtree. That is a pre-existing behaviour and a consumer-visible change, but the argument for closing it is the same one this design accepts for the HTTP door.
7. **Does `status.address` get the same treatment — withheld in the down phases, in `Reprovisioning`, and with a `ServiceMissing` check?** Two endpoint fields with opposite liveness rules on one CR is a worse API than either rule applied consistently. Deliberately not bundled, because it changes a field already mounted in other people's clusters.