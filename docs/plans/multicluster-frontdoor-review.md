# Verdict, reasoning, and costs — multi-cluster front door for flint-lite

## 1. The verdict

**Yes, amend the plan — but Phase C is what's wrong, not the operator's placement.** Phase C as written ("look up FlintShare by deterministic name → apply if absent → touch `requested-at` → wait Ready → read `status.address`", priced at ~1.5d) is a contract written as if there is one API server; shipped against N, it hands the control plane `create` on flintshares in every workload cluster, N per-project file-API bearer tokens, and a `status.address` string (`reconcile.rs:315-334`) that means nothing outside the cluster. **Recommended architecture: `flint-gate` — the operator stays per-cluster, unchanged, and grows a second warp listener in the same process publishing one versioned HTTP contract (`ensure` / `keepalive` / `status` / inventory / file-API reverse proxy) per cluster.** That was the only one of four architectures to survive adversarial refutation, and it survived because it prices its costs instead of hiding them. **Your unease is one-third justified**: the RBAC footprint criticism is fair (and the fix is to *shrink* the grant, not move it); the fleet-view gap is real (and is not a placement problem); the "N upgrade surfaces" complaint is the weakest — no option on the table reduces N, because the hub image is per-cluster regardless (`values.yaml:27`).

---

## 2. What the code actually forces

### Forced by the code (not negotiable without changing behavior)

| Constraint | Evidence |
|---|---|
| Exactly **one** of eleven reconcile-loop dependencies needs cluster-network reach: `poll_hub` → `hubstatus::poll` dialing `http://<podIP>:<port>/status`, 3s timeout. It is the *whole* of suspend conjuncts C5a–C5d and the *whole* of hibernate verification. | `reconcile.rs:1100-1132` → `hubstatus.rs:216-232`; `hubstatus.rs:147-167`, `:174-186`; `reconcile.rs:791`, `:963` |
| `spec.bucket` and `spec.keyPrefix` are CEL-immutable. A wrong prefix is not editable — only replaceable by a new share plus a byte copy. | `crd.rs:71-77` |
| Cross-cluster uniqueness is **unenforced and unenforceable in-cluster**. `admit()` reads one reflector over one API server. The epoch cell keys on the *exact* prefix and its remedy is takeover after `6 × 10s`, never refusal. Nested-but-unequal prefixes (`tenant-a/` vs `tenant-a/sub/`) produce different epoch keys and never contend at all; flock is per-PVC so it never engages. | `conflict.rs:1-8`, `:11-20`, `:91-95`; `reconcile.rs:416-424`; `tier/epoch.rs:50-53`, `:74-76`; `state_backend/mod.rs:88-109` |
| `status.address` has **no override**. Everything except `LoadBalancer` — NodePort included — returns `<name>.<ns>.svc.cluster.local:<port>`. | `reconcile.rs:315-334` |
| The Service carries exactly one port, `nfs`/2049. 8080 (status **and** the read-write file API on one listener) is a containerPort only, with no CRD knob to expose it. There is **zero** NetworkPolicy anywhere in either chart. | `render.rs:401-410`, `:493-509`; `status.rs:287-299`; `grep -rn NetworkPolicy` → 0 hits |
| `IdleSuspended` and `Hibernated` fall through to `REQUEUE_PROGRESS` = 15s. Only `Ready \| Suspended` get 300s. Verified: `Phase::Ready \| Phase::Suspended => REQUEUE_SETTLED, _ => REQUEUE_PROGRESS` (`reconcile.rs:729-731`). Every parked project reconciles 4×/min forever, SSA-applying four objects each pass. | `reconcile.rs:58-64`, `:729-731`, `:597-647` |
| `hibernatable()` reads **only** `rpo_clean`. `epoch.held` is parsed (`hubstatus.rs:121`) and consumed by nothing but a test fixture — verified by grep. The consequence of a wrong answer is `claims.delete`. | `hubstatus.rs:174-186`; `reconcile.rs:1085` |
| The render creates **no ServiceAccount** and sets no `serviceAccountName` — verified, zero hits. Hub pods run as namespace `default`. Any future pod-scoped Role binds to `default` and grants every pod in the namespace. | `render.rs` (grep) |
| `reqwest` is `default-features = false, features = ["json","rustls-tls"]`. **`stream` is off**; `bytes_stream` has zero hits repo-wide. A streaming proxy is a Cargo change that must then survive the multi-arch musl zigbuild path. | `Cargo.toml:82` |
| The `install_crypto_provider` guard enumerates only `src/main.rs` and `src/bin/flint_lite_operator.rs` — verified. Any new binary or kube client elsewhere reproduces the shipped 1.26.0/1.27.0 panic silently. **v1.27.1 is still owed.** | `src/lib.rs:110-140` |
| `start_grpc_server()` is unconditional, even in standalone (which refuses `dataServers`), binds 0.0.0.0:50051 with `DeleteVolume`, and warns it is UNAUTHENTICATED when `FLINT_PNFS_CONTROL_TOKEN` is unset — which it is, everywhere. | `server.rs:762`, `:119-134`, `:1382-1396` |

### Forced by the design (a decision, reversible at a stated price)

- **8080 off every Service** is a posture stated identically in five places (`hubstatus.rs:10-17`, `status.rs:255-268`, `fileapi/mod.rs:16-21`, `render.rs:493-496`, `crd.rs:286-290`) — never publish a whole-volume read-write API on a possibly-LoadBalancer Service. **The target topology has already spent the *other* half of that argument**: the control plane must reach that exact port for browse/edit, so "nothing outside the cluster can reach it" is already false as a product requirement. That is why the honest options all converge on a *proxy*, not a Service port.
- **`/status` is deliberately not activity; file-API calls deliberately are** (`fileapi/mod.rs:32-37`; `hubfs.rs:48`). A UI that auto-refreshes listings pins every project awake forever. Any front door must respect this.
- **`rpoClean` gates hibernate only, never suspend** (`hubstatus.rs:283-299`) — deliberately dropped from B2's plan text.
- **CRD re-apply on *equal* schema version is deliberate repair**, not a defeated guard: it restores a hand-stripped property, asserted by shipped e2e leg 6 (`bootstrap.rs:72-81`; `operator-kind-e2e.sh:359-380`). The per-cluster-operator option's amendment to "fix" this was **refuted**.
- **`spec.endpoint` mutability is deliberate and documented** ("Mutable on purpose — an endpoint can legitimately move — but it participates in share uniqueness"). It has *no* CEL rules at all. That amendment was also **refuted**.

### Merely current habit

- The **unselected cluster-wide Secret watch** (`bin:183-195`) caching every Secret in the cluster in operator memory — a stated trade against an unstable kube feature flag, not a requirement. A label selector fixes it.
- **`replicas: 1` / `strategy: Recreate`** (`deployment.yaml:20-22`) — correct for *reconcilers* (two stale caches disagreeing), and irrelevant to a lease-independent read/proxy path.
- **Pod-IP polling specifically.** The pod IP is the *only* route today; nothing forces it to be. It can be relayed.
- **`kubectl` as the front-door interface** (`docs/flint-lite-operator.md:306-320`) — a doc recipe, not a shipped contract. Phase C artifacts are entirely absent: no `frontdoor` ClusterRole, no `flint.io/project-id` (zero hits), no deterministic name enforcement.

---

## 3. On the per-cluster operator specifically

**The placement is right. The argument usually given for it is wrong, and I want to be precise about that**, because you'll otherwise defend it with a claim that doesn't hold.

The "forcing function" — *a central operator would need routable pod CIDRs across the fleet* — **was refuted, correctly.** Any design that proxies the file API from inside the cluster (which the topology requires) can relay `GET /status` identically: it's a 3s unauthenticated JSON read. The pod-IP poll forces a per-cluster **component**, not a per-cluster **reconciler**. The per-cluster-operator option built the component, kept the whole reconciler local anyway, paid for both, and never evaluated the thin-relay-plus-central-controller alternative it needed to beat.

**What actually justifies local placement, in order of strength:**

1. **Blast-radius partitioning is free today and expensive to recover.** One operator holds cluster-wide `secrets: get,list,watch` (`rbac.yaml:75-77`) with every Secret resident in memory, `customresourcedefinitions: create,patch` (`:53-55`), and `persistentvolumeclaims: …,delete` (`:64-66`). Today that union is partitioned N ways *by construction*. Centralizing un-partitions it into one pod, and converts N bound, audience-scoped, auto-rotated ServiceAccount tokens into N exportable long-lived credentials in one place. That is a strict security regression, and it also quietly trades away the multicluster-mount architecture's load-bearing "consumers hold no credentials" property in a new location — a property with no recorded decision covering it.
2. **Arbitration is a per-API-server cache by construction.** `ctx.fleet` is one reflector (`reconcile.rs:83`, `bin:139-148`). A central operator needs N Controllers *plus a cross-store arbiter that does not exist*, and the central-operator option's own fail-closed amendment disables cross-cluster arbitration exactly during the partition/restart it was sold against. Its headline payoff is detection, not fencing — and enforcement still requires reaching the losing cluster's API server to merge-patch `replicas: 0` (`reconcile.rs:429-440`).
3. **Suspend survives a control-plane partition only if the decider is local.** A central decider leaves the whole fleet running and billing during a WAN outage. This asymmetry is real and was the hub-self-governs option's best insight, and it applies to the operator too.
4. **kube 3.0.0 is pinned without `oidc`/`oauth` features** (`Cargo.toml:64`), and the operator image carries no exec-plugin binaries and runs read-only non-root. EKS/GKE-shaped kubeconfigs would require a crate-feature change *and* an image change — unpriced in the central option, and enough on its own to make "just mount N kubeconfigs" a fiction.

**What per-cluster placement genuinely costs, unsoftened:** N helm releases with two recorded scars that N multiplies (`--reuse-values` uses the *old* chart's computed values; helm never upgrades `crds/`, which is why the operator SSAs its own CRD at startup). N places to look when something breaks. And **N does not shrink under any option here** — the surviving one says so in its own scorecard.

**What the alternatives actually buy, and where to get it cheaper:**

- *Fleet view.* Not a placement problem. `GET /v1/projects` off the reflector is a memory read, zero API calls; the control plane fans in across clusters into one table with a per-cluster `staleAsOf`. This is strictly better than `kubectl get flintshares -A`: it carries project ids (nothing in tree does), and an unreachable cluster shows as stale rather than as a timeout. It is also what your own design of record already concluded — `docs/plans/multi-volume-hub-design.md` §5 [F7] rules the *bucket* the registry of record, not any API server.
- *Cross-cluster uniqueness.* Obtainable **without moving anything**: allocate prefixes centrally, one per project, never nested, never reused, with a UNIQUE constraint on normalized `(endpoint, bucket, prefix)` in the control-plane DB. That is the only mechanism that holds across clusters until the multi-volume design's claim-gen volume cells exist (§5 [F1], no code). A central operator's union store would be advisory on top of it, not a substitute for it.
- *Fewer upgrade surfaces.* Not on offer. What the gate buys instead is **decoupling**: the gate keeps serving `/v1` while the operator behind it rolls, so the control plane is insulated from CRD schema and operator version. A kubeconfig-based front door cannot be.

**On the three options that died:** the per-cluster shim and the gate are the *same idea*; the shim was refuted for selling a security improvement its own risk section retracted and for listing as "unchanged" the RBAC it silently had to expand (`rbac.yaml:46-48` grants no `create`). The central operator died on a circular credential argument (the control plane still needs the per-share file-API token, which the design returns as a `token-ref` and stops) plus an unspecified route (`hubstatus::poll` hardcodes `http://` and sends zero headers; nothing renders the per-share HTTPRoute path-routing requires — zero `HTTPRoute` hits repo-wide). The self-scuttling hub died on an unrepairable wake/delete race — a `replicas: 1` PATCH landing inside the hub's 120s grace window (`render.rs:89`, verified) creates a Pending pod against a Terminating PVC, permanently, with nothing left in the cluster to repair it — plus the fact that demoting FlintShare from a CRD silently deletes all seven CEL admission rules (`crd.rs:67-99`), including the one that stops `hibernateAfterSecs` from deleting a sole-copy PVC.

---

## 4. The amendments, ranked

### NOW — one-way doors, decide before more code

**A1. Prefix allocation policy + UNIQUE constraint (control-plane DB, outside this repo).** One prefix per project, derived from project id, never nested, never reused; UNIQUE on normalized `(endpoint, bucket, prefix)`; `project_id → cluster_id` is the placement registry of record. *Why:* `crd.rs:71-77` makes bucket/keyPrefix CEL-immutable, so a wrong prefix is a byte migration, not an edit. The failure it prevents is the one `conflict.rs:11-20` names in its own words — after one missed lease window (`epoch.rs:74-76`) the other hub imports the prefix and serves those bytes at *its* address, under *its* name. Nested prefixes never contend at all. **This is the single cheapest-now / most-expensive-later item on the list.**

**A2. Decide, in writing, whether the control plane ever holds Kubernetes credentials.** Recommendation: **never** — gate token only, with a documented break-glass exception. *Why:* once N front-door replicas hold `create` on flintshares in N clusters, taking it back is a product rewrite plus N credential revocations plus an audit of everything the unscoped credentials could reach. Give this the same weight as "consumers hold no storage credentials" or the first operational emergency dissolves it permanently. **Delete Phase C's `frontdoor` ClusterRole bullet** (plan.md:409-417); add `create` (and `delete` behind a distinct token scope) to the operator's own ClusterRole at `rbac.yaml:46-48` instead.

**A3. Move the `requested-at` stamp in-cluster, and bound the clamp.** The gate stamps with the workload cluster's clock. Separately, add a ceiling to `idle.rs:130-142` (currently `.max(0)` with no upper bound, pinned as intended at `:427-435`): reject or log-and-clamp anything more than one `suspendAfterSecs` in the future. *Why:* C4 is today a cross-clock subtraction; one fast front-door clock pins a project awake for the length of the skew, unlogged and indistinguishable from demand. Every front door will be written against whatever Phase C documents.

**A4. `epoch.held` conjunct in `hibernatable()`.** Three lines. Refuse unless `self.epoch.as_ref().is_some_and(|e| e.held)`. *Why:* the field is parsed (`hubstatus.rs:121`) and consumed by nothing but a test — verified. `hibernatable()` reads `rpo_clean` alone and its consequence is `claims.delete` (`reconcile.rs:1085`). Self-recognition is gated on `is_single_occupant()` (`epoch.rs:176-195`), so a second live process on the same PVC genuinely does *not* hold the epoch — this is a real pod discriminator, and **a latent bug in the shipped pod-IP path today**, not just a prerequisite for routed polling. This is the best standalone catch in the whole review set; it came from the option that otherwise died.

**A5. Extend the crypto-provider guard to a `[[bin]]` walk.** Replace the hardcoded two-entry list in `src/lib.rs:110-140` with a walk over the crate's `[[bin]]` sources. *Why:* the test's own doc says "what shipped broken was a binary that never called it," and it enumerates two files. The gate lands in an existing binary (which already calls it correctly at `bin:104`), but the next one won't. v1.27.1 is still owed for exactly this.

**A6. Gate 50051 off in standalone, or refuse to bind without `FLINT_PNFS_CONTROL_TOKEN`.** `server.rs:762`. *Why:* a shipped, unauthenticated `DeleteVolume` surface on a hub that has no DS fleet by construction, with no NetworkPolicy anywhere. Topology-independent; it should not survive Phase C under any architecture, and it gets categorically worse the moment a cluster becomes a product endpoint.

**A7. Default-deny NetworkPolicies** in both charts: 2049 from in-cluster consumers, 8080 *only* from the operator/gate pod selector, 50051 denied. *Why:* adding default-deny before workloads exist is a template file; adding it after is an outage hunt for every unnoticed dependency on the flat pod network.

**A8. Re-price Phase C from ~1.5d to ~11–13d.** *Why:* 1.5d prices a document and a ClusterRole. Leaving it there guarantees the gate is cut to a recipe under schedule pressure — which is exactly the outcome that puts N kubeconfigs in the control plane. The gate's own estimate (6–9d) omits: the `reqwest` `stream` feature plus musl-zigbuild verification, the CRD status field `stalled` actually needs, TLS/cert issuance for the product's first cross-cluster listener, and Secret/namespace pre-provisioning.

**A9. One comment, three files** (`crd.rs:286-290`, `render.rs:401-408`, chart): name the gate as the *only* sanctioned external path for the file API and state the Service stays single-port permanently. *Why:* five places justify keeping 8080 off the Service and none names an alternative, so the first person who needs remote browse will helpfully add the port. Once one customer has a LoadBalancer'd whole-volume read-write API, you cannot walk it back.

**A10. Retarget the uncommitted B4 legs before committing.** Leg 7 (`/status` + file API, 401, symlink 409) is topology-neutral — commit as is. Do **not** commit anything that pins `flint.io/requested-at` as *the front-door contract*; pin it as *the operator's ladder input*, which is what it is. *Why:* the working tree is `+243/-8` and uncommitted; retargeting is free today and is a test rewrite plus a contract change tomorrow.

**A11. `Phase::IdleSuspended | Phase::Hibernated => REQUEUE_SETTLED`.** One match arm at `reconcile.rs:729-731`. *Why:* verified — parked shares requeue at 15s forever, each pass SSA-applying ConfigMap/PVC/Service/Deployment. At the plan's own 3000-CR/300-live target that is ~180 SSA-write reconciles/minute of pure waste, and B1's stated design was "~60s with jitter."

### PHASE-C — build with the gate

**B1. Build `src/lite_operator/gate/`** in the existing binary: `/v1/projects` (GET/PUT), `/v1/projects/{pid}` (GET/DELETE), `/ensure`, `/keepalive`, `/files/*` streaming proxy. Reuse `warp` (`Cargo.toml:92`), the auth filter lifted from `fileapi/mod.rs:345-390`, `hubstatus::poll`, and the reflector store. **Serve on every replica, not lease-gated** — standbys keep watches warm (`bin:239-241`) and gate writes are idempotent.

**B2. Extract `hub_pod_ip()` from `poll_hub`** (`reconcile.rs:1116-1129`), including the `deletion_timestamp.is_none()` exclusion. The proxy must resolve *exactly* the pod the poller does; duplicating the predicate guarantees drift, and browsing a draining hub is as wrong as polling one.

**B3. Chart: `replicas: 2` + RollingUpdate + PDB + reflector-synced readiness**, conditional on `leaderElection: true` (already default). *Why:* this is the design's central regression — with one replica, an operator restart makes every project in the cluster unopenable from the UI while every hub is healthy. The `Recreate`/single-replica reasoning at `deployment.yaml:4-7` is about *reconcilers* and does not apply to a lease-independent gate.

**B4. Scoped gate tokens from the first token issued** (token → allowed namespaces + allowed verbs), TLS terminated at the edge, and an audit log of every proxied mutation. Retrofitting scopes means re-issuing every control-plane credential.

**B5. Add `serverId` + `podName` + an operator-liveness field to `StatusDoc`/`FlintShareStatus`.** *Why:* `StatusDoc` becomes a public contract the moment a front door parses it. **And note a refutation that matters:** the gate's `state: "stalled"` cannot be derived from anything shipped. `observedGeneration` never lags after an annotation-only merge patch (metadata changes don't bump `metadata.generation`), and `last_transition_time` is explicitly "only bumped when status actually changes" (`crd.rs:568-570`) — so ">60s since a condition moved" is a false positive on every healthy settled share. Without a real heartbeat field, the *only* named mitigation for "operator wedged, gate healthy" is inert and the UI spins on `waking` forever. This bumps `SCHEMA_VERSION` (`bootstrap.rs:39-45`) — safe, because `decide` returns `RefuseNewer`.

**B6. Secret watch label selector** (`bin:183-195`) plus a required label on referenced Secrets. Free now; a breaking change to every existing FlintShare later. This is the *substantive* half of your RBAC concern, and it is fixable rather than relocatable.

**B7. Cache the polled `HubSnapshot` in-process**, keyed by share with an observation timestamp; gate `GET` serves from it, `?deep=true` forces a poll. Otherwise every UI refresh adds a round trip on top of the ladder's own.

**B8. Wire or delete `flint.io/wake-intent`** (`idle.rs:51`, getter `:119-121`, zero consumers). A parsed-but-inert protocol knob that a front door might reasonably start writing is worse than an absent one.

**B9. Gate e2e legs**, minimum: ensure-from-zero returns `waking` then `ready`; concurrent double-ensure yields one CR; overlapping-prefix declare returns 409 *synchronously* (not `phase: Failed` asynchronously); browse against `IdleSuspended` returns 503+`Retry-After` **and** wakes; Range/`Retry-After`/status pass through byte-identically; a namespace-A token gets 403 for a namespace-B project; the per-share hub token never appears in any gate response; **and a mid-stream upstream failure resets the connection rather than ending the body cleanly**. Plus a fleet-budget leg at 3000/300 measuring apiserver write rate, `pods.list` rate, and RSS — B1 named that target and no leg asserts it.

**B10. Two gaps the gate proposal missed.** (i) `DELETE /v1/projects/{pid}` under `reclaim: Delete` deletes an **adopted** PVC regardless (`reconcile.rs:1169-1174` logs `adopted` and deletes anyway), in direct contrast to the hibernate path which refuses (`:1056-1058`). One HTTP token would then destroy a user-supplied PVC over a WAN. Fix the inconsistency or refuse adopted claims on the delete verb. (ii) The hub buffers whole download bodies *deliberately* so a 200 can never be followed by a short body (`fileapi/mod.rs:483-491`); a `bytes_stream` hop **re-opens that class at the gate**.

**B11. Single-flight keyed off the reflector, not process memory.** With `replicas: 2` mandatory (B3), in-process click suppression is wrong by construction — key it off the `flint.io/requested-at` value already in the store.

### LATER

- Additive `spec.service.advertiseAddress` overriding `address_of` (`reconcile.rs:315-334`). ~20 lines and purely additive today; a consumer migration once front doors and PV `nfs.server` fields parse the computed value. **Note this gap was already deferred once** as P1 of the multicluster-mount workstream. Deferring it twice is how it becomes permanent.
- Chunked/resumable upload and byte-range PATCH — move out of "Out of scope" into a named phase with a trigger. They were deferred on the assumption the browse client sits next to the hub; a WAN front door makes "large uploads that fail are retried whole" the routine case. Budget the bytes: browse egress is now billed twice (S3→hub→gate→CP), with `maxDownloadBytes` 5 GiB bounding a *second* transfer.
- B1 jitter/cadence beyond the A11 match arm.
- The multi-volume satellite role. The gate is the natural front for its §4 per-hub admin API — that design already assumes the front door speaks HTTP to hubs, not kubectl to N API servers.

### NEVER

- Adding 8080 to any share's Service, or a CRD knob that could.
- A `frontdoor` ClusterRole for a subject outside the cluster.
- Re-applying the two refuted amendments: "make the CRD bootstrap skip on equal version" (it is the drift-repair path, asserted by e2e leg 6) and "make `spec.endpoint` CEL-immutable" (mutability is documented and deliberate; it has no CEL rules to tighten).
- `spec.existingClaim` reachable through the gate — the adoption fence is the only guard against two writers on one `state.db` and it needs a whole-namespace pod list.

---

## 5. What NOT to change — decisions this topology validates

- **The idle ladder's decision structure.** `decide()` (`idle.rs:188-270`), the hub as the authority on quiet, `rpoClean` as a hibernate-only gate, the annotation carrier, and admin `lifecycle: Suspended` outranking a wake. The gate writes the same annotation the operator already reads. Only *whose clock stamped it* changes.
- **Verify-then-delete hibernate**, and the reason for it: the operator holds no bucket credentials, so it cannot read the epoch itself. That property survives every option and is why hibernation must ask the hub. Keep it; just make the question honest (A4).
- **The hub's total lifecycle passivity.** No kube client, no self-suspend. The self-governing-hub option was killed partly by what happens when you give the data plane a control-plane credential — and by the fact that it would run as the `default` ServiceAccount, since the render creates none.
- **`Recreate` + RWO + flock + the PVC deliberately never ownerRef'd** (`reconcile.rs:11-21`, `:615-616`) with the compensating PVC watch. Nothing in this topology touches single-occupancy.
- **Conflict arbitration and the adoption fence as-is.** They are correct within their stated scope; the gate's pre-check is *advisory* and the operator remains the authority (`reconcile.rs:429-462`).
- **CRD self-apply with the version guard**, including the equal-case repair. It exists precisely because helm never upgrades `crds/`, and it is the mechanism that makes fleet version skew safe. Fleet skew is an argument *for* it, not against it.
- **`/status` is not activity; file-API calls are.** Load-bearing, and the gate must preserve it — which is why an `X-Flint-Poll` header serving from a short-TTL cache is not optional.
- **`status.address` staying cluster-local for in-cluster agents.** It is correct for them. Label its scope in the gate response rather than changing what the field means.
- **Phase A entirely, and Phase B's shipped logic.** Nothing here reopens landed work. Phase A is the reason the hub already knows everything the decision needs.

---

## 6. The untested claims, ranked by damage if false

1. **Nothing in this wave has ever run in a real cluster.** The ladder's only cluster-shaped coverage is two *uncommitted* kind legs; the operator doc still says "no cluster coverage yet." Every property below inherits this.
2. **"`rpoClean: true` means it is safe to delete the PVC."** Never tested against a pod that is not the epoch holder. The hibernate-verify path *deliberately restarts the hub* (`reconcile.rs:963`, replicas render 1 while `HibernateVerifying`) with `strategy: Recreate` — that is a window. Consequence of a false positive: deletion of the only warm copy of a project's disk, with recovery reduced to a DR import.
3. **"One share per bucket subtree."** Holds inside one reflector; asserted by convention across clusters and enforced by nothing. The failure is not waste — `conflict.rs:11-20` calls it cross-tenant data exposure, and nested prefixes never contend at all.
4. **The B1 fleet budget (3000 CRs / 300 live).** No leg asserts it, and the verified 15s parked requeue makes the real number materially worse than designed. `poll_hub`'s unselected whole-namespace `pods.list` per poll is the term most likely to dominate.
5. **"120s termination grace is enough to flush and release the epoch cleanly."** L4 measured ~13.3s/GiB publish against real S3. A hub with a large dirty set will be SIGKILLed mid-flush, leaving the epoch cell **deliberately HELD** (`server.rs:1307-1314`), so the next wake pays the full 6×10s lease. Under this topology that latency is user-visible on every cold open of a busy project, and nothing measures it.
6. **The proxy preserving hydration semantics.** 503+`Retry-After` from `hydrate_wait_secs`, 503 from the pre-`Serving` phase gate (`fileapi/mod.rs:223-244`), `Range`/`Content-Range`/`Accept-Ranges` — all must pass byte-identically or callers retry wrong or silently accept partial files. No test can exist until the gate does.
7. **The wake path's availability.** Level-triggered wake is real, but it is *only* real while the operator reconciles. `render.rs:582-584` derives replicas from the idle annotation, which only the operator writes — an `IdleSuspended` share with a dead operator never scales back up, and a `Hibernated` one has no PVC to come back to. "Fails safe, only a delete hangs" is wrong. Co-locating the gate in that process is actually *better* than the kubectl recipe (it fails closed rather than PATCHing into a void), but the wedged-lease case needs B5.
8. **The future-clamp being harmless.** Pinned as intended at `idle.rs:427-435`, unbounded in fact. One bad clock is an unbounded, unlogged cost event indistinguishable from real demand.
9. **The partition story.** Under a CP↔cluster partition, a *browse-only* project loses both C4 and C5 on the same timeline and suspends under an actively-clicking user at ≈`last_touch + suspendAfterSecs`. Agents doing real in-cluster I/O are safe (`idle.rs:371-381`). A long partition proceeds to `claims.delete` on operator-clock arithmetic alone. **No option on the table fixes this**, because the protocol has no liveness signal for the front door and no "unknown" state for the heartbeat channel — front-door silence is indistinguishable from front-door disinterest. Say so in the docs rather than pretending otherwise.

---

## 7. Open questions for you

1. **Does the control-plane DB own prefix allocation, with a UNIQUE constraint on normalized `(endpoint, bucket, prefix)` and no nesting ever?** This is the one-way door. Say yes and everything else is cheap; say no or defer, and the first thousand projects make it a byte migration that no code in the tree can help with.
2. **Is "the control plane holds no Kubernetes credential" an invariant with the same weight as "consumers hold no storage credentials," or a preference?** If it's an invariant, Phase C's ClusterRole bullet dies now and the gate is mandatory. If it's a preference, the gate's whole security argument is optional and you should know that before it's built.
3. **Who creates and rotates the per-share file-API token Secret and the S3 credentials Secret?** Nothing in-tree does — the operator only *projects* an existing Secret (`render.rs:541-566`), holds `secrets: get,list,watch` with no `create`, and has no `namespaces` rule at all. So `PUT /v1/projects/{pid}` cannot stand up a browsable project from zero without *someone* holding Kubernetes credentials. That someone is either the gate (expand its RBAC) or a platform provisioning step (and then question 2 has a caveat).
4. **Can a project ever be active in more than one workload cluster — migration, failover, or read-only fan-out?** If no, the prefix constraint suffices and this is settled. If yes, the multi-volume design's claim-gen stamp and satellite refresh become load-bearing, and both are unbuilt and (for claim-gen) unmodeled — step 0b is still owed.