# flint-lite, multi-volume — many workspaces, one server, one port

Status: **DESIGN, NO CODE** (2026-09-09).

This is a capability added to **flint-lite**, not a new product. That is
a reversal, recorded deliberately: the first cut of this document was
written as a separate product ("Flint The hub") on the constraint that
lite must not be modified. The owner then established that **lite has no
deployed installs and no external users**, which removes the entire
basis for that separation. §2 states what the reversal buys, what it
costs, and the one obligation that survives it.

**Review record (2026-09-09).** A 14-agent adversarial pass ran against
this document — six dimension reviewers (surface, process globals,
buckets, unload, lease, boundary) raising 28 findings, the top 8 by
severity then sent to independent verifiers instructed to default to
REFUTED. **2 survived as PARTIAL, 6 were refuted, and the refutations
are as informative as the survivors:** most of them died because the
reviewers were reading the *previous* cut of this file at its old path
and attacked claims the recut had already removed (an "obligation 3"
about `ALTER TABLE` migration, an unconditional "correct as written" on
the inode row). The two survivors were both citation defects in §2 and
are folded in below, marked **Correction**. The `boundary` dimension
assumed the abandoned separate-product framing and its findings are
discounted accordingly.

`docs/plans/multi-volume-hub-design.md` remains the topology and lease
argument this builds on. Its ultracode review record (2026-08-18, 8/8
findings adversarially confirmed, F1–F8) is still authoritative and is
referenced by finding number throughout. Two constraints arrived after
it and drive this document: **the driver is the port surface**, and **a
separate S3 bucket per workspace is the common case** — the latter
inverts an assumption that document made throughout, and §4 is where it
lands.

A note on words. "Hub" is used throughout for the lite server process,
which is what it already means across `lite_operator/hubstatus.rs`,
`HubPhase`, `spec.hubNamespaces`, `flint-hub-gateway` and
`flint-lite-chart/templates/hub.yaml`. Under the separate-product
framing that word was unavailable and needed replacing; folding back
into lite makes it correct again. A **workspace** is one of the N
volumes a hub serves.

## 1. Why this exists

**The endpoint problem, and it is a multi-cluster problem.** The stated
common case is **agents on many Kubernetes clusters mounting the same
hub** (`tests/regression/many-clusters-one-hub.sh`, drilled 2026-08-22
at v1.35.1: **7 defects**). That is what creates this whole section:
when every consumer is in the hub's own
cluster, a ClusterIP per share is nearly free and there is no port
problem to solve. It appears the moment the consumers are somewhere
else.

A consumer outside the hub's cluster reaching N shares needs N
*reachable endpoints*. An L4 load balancer routes on
`(VIP, protocol, port)` and NFS carries no in-band routing key — no SNI,
no Host header — so one IP forces one port per share. 100 shares is 100
ports, or 100 LB IPs, or a tunnel.

A single NFSv4 server exporting N subtrees under one pseudo-root
collapses that to **one address, one port, N paths**, because the
demultiplexing happens where a routing key actually exists: in the NFS
namespace.

**Why "just use a tunnel" is not the answer, stated once so it is not
re-litigated.** A tunnel buys one *port*; it does not buy one
*endpoint*. Behind it there are still N server addresses, so everything
that scales with endpoint count still scales: N Services, N distinct
`spec.nfs.server` values across N PVs on every consuming cluster, N
names or addresses to allocate and track, N entries in every route table
and ACL. This design makes the endpoint **O(1)** and leaves only `path`
varying — a different and larger property than a firewall-rule count.
Three more reasons, in the order they tend to decide it:

- **It voids the zero-footprint consumer.** §3's promise is that stock
  kernel clients mount subtrees with no client-side feature at all; the
  prior doc sells consumers as "zero footprint, zero credentials"
  (`multi-volume-hub-design.md:65`). A tunnel puts a client on every
  consuming node, to be installed, authorized, renewed and monitored —
  and consumer-side footprint is the expensive kind, because it is the
  side you do not control.
- **It is a hop in a metadata-storm path.** The workload below is
  thousands of `stat`/`readdir`/`open` per second. A userspace overlay
  on every round trip is the worst available place to spend latency.
- **It is frequently not permitted.** "One standard NFS port to one
  address" clears a network review that "an outbound WireGuard overlay
  from the cluster" does not.

**And the multi-cluster case is where the topology earns more than a
port.** Every writer on every cluster reaches one owner, so cross-cluster
consumers get *genuine* POSIX semantics — enforced byte-range locks,
close-to-open coherence, atomic rename (prior doc §2) — rather than the
barrier-to-barrier satellite contract that a hub-per-cluster arrangement
gives. One endpoint and one writer authority are the same fact seen twice.

**The workload.** Fleets of agents across clusters, running ordinary
tools (git, build systems, sqlite, grep) against workspace files whose
durable home is S3, through a genuine POSIX interface. Three facts drive
the design, carried forward unchanged from the prior doc §1:

- **Tool workloads are metadata storms.** `git status`, builds and test
  runs are thousands of `stat`/`readdir`/`open` per second over small
  files — served sub-millisecond from a local namespace, unusable over
  an S3-passthrough mount.
- **Agents run arbitrary tools, so the POSIX surface must be genuine.**
  sqlite transactions, fcntl locks, atomic rename.
- **Writes are naturally partitioned.** Each agent writes its own
  workspace; sharing is read-mostly plus published results.

**Workspace lifecycle runs at session rate.** This is why a workspace is
a registry row and not a Kubernetes object. `docs/flint-hub-gateway.md`
records the envelope: ~3,000 ClusterIP Services is about 73% of a
GKE-default /20, and once the allocator is dry a brand-new share's
*consumer* Service cannot be created either — one tenant's workspace
count stops every other tenant from mounting anything. Registry rows
have no such ceiling.

## 2. Folding into lite instead of forking from it

**The decision.** `FlintShare` becomes *one server hosting N ≥ 1
workspaces*. At N = 1 with no admin-API calls it is today's product,
behaving identically. There is no second CRD, no second operator, no
second chart, no second image, and no second release.

**What the no-users window buys.** Every one of these was a real
objection to touching lite, and every one of them was conditioned on
deployed state that does not exist:

| Objection | Why it dissolves |
|---|---|
| Removing a CRD field PRUNES it — a one-way door | No `FlintShare` objects exist to prune from |
| Existing PVs break when the export root moves to a subtree | No live mounts to migrate |
| The idle ladder silently changes meaning for existing shares | No existing shares |
| Failure domain changes for users who did not ask | No users |
| **Schema migration on a live PVC** — the scariest single item | **No PVCs.** The schema is simply authored with `volume_id` in it |
| A defect in the new path ships to lite's users | No users |

**What it does not buy, and this is the whole of what survives.**
`src/tier/`'s test suite is the control for the integrity path
*regardless of who uses lite*. Those 158 tests encode F14, the fold-cap
defect, BUG 7, the epoch fence and the pre-ack capture guarantee, earned
across runcd→runcn. Their value has nothing to do with user count and
everything to do with being the only thing between a scope-filter change
and silent data loss. So:

1. **Zero test edits in `src/tier/` and `src/state_backend/`.** A test
   rewritten to accommodate the new shape proves only that the new shape
   reproduces. Tests elsewhere — the operator's 132, the kind suite —
   are free to change, because their subject is deliberately changing.
2. **A positive control through the load-bearing path.** Mutate the
   volume-scoping predicate — force the scope filter always-true, then
   always-false — and the tier tests must fail under both, and fail
   *differently*. If either mutation passes, the scoping is not
   load-bearing and the test is vacuous. Both arms are required because
   the two have different signatures: always-true over-returns (visible
   as foreign rows), always-false under-returns (visible as nothing at
   all, which is also what a clean tier looks like). Only the second
   matches the real failure mode.

**What folding in costs.** Roughly 50 in-repo files carry lite's current
shape and will need updating: ~32 scripts and manifests referencing
`flint-lite` or `FlintShare`, 18 Rust files referencing `FlintShare`,
and the kind and drill suite (`tests/regression/lite-kind-e2e.sh`,
`lite-kind-tier-e2e.sh`, `operator-kind-e2e.sh`, `hub-lifecycle-drill.sh`,
`many-clusters-one-hub.sh`, `fleet-scale.sh`). That is test
infrastructure, not a user migration — bounded, in-repo, and the same
person's to fix.

**What forking would have cost, for the record.** A separate product
needs a separate operator, and `lite_operator/` is 10,461 lines with 132
tests: `reconcile.rs` 4,020, `render.rs` 1,800, `crd.rs` 1,318,
`conflict.rs` 1,091, `idle.rs` 838, `hubstatus.rs` 778 — Deployment,
PVC and Service rendering, phase computation, conflict detection,
persistence, the idle ladder. Nearly all of it would have been written
twice. That, not the release surface, is the argument that settled this.

**And no crate extraction.** The move that looks like hygiene — lift
`tier/` and `state_backend/` into a new crate — relocates 16,319 and
11,723 lines of the most drill-hardened code in the repository to buy a
property it already has. `src/lib.rs` is `pub mod` throughout and 13
binaries already build out of this one crate. Under the folded shape
there is not even a second consumer to justify it. Ruled out.

### The measured surface

Two structural facts, verified in the code, do most of the work already:

- **The two S3-facing engines are already per-prefix.** `FlushConfig`
  carries `key_prefix` (`tier/flush.rs:61`) and so does `ImportConfig`
  (`tier/import.rs:46`); `FlushOrchestrator` (`tier/flush.rs:238`) is an
  instantiable struct, not a global. N workspaces is N orchestrators.
  That shape exists — it was built for one instance, never restricted to
  one.

  **Correction, and it is not cosmetic.** An earlier draft of this
  bullet named `HydrateConfig` and `SpaceConfig` alongside those two.
  Both are false: `grep -c key_prefix tier/hydrate.rs` is **0**, and
  `SpaceConfig` (`tier/space.rs:50-62`) carries `root` — a filesystem
  scope, not an S3 key prefix. `SpaceConfig` is *correctly* singular:
  one PVC, one statvfs, one reserve, one ballast beside one `state.db`
  (§6), and one `space::configure` at the hub export root keeps every
  `/volumes/<n>/…` path inside `starts_with(root)`, so no admission gate
  takes its fail-open arm. **`HydrateConfig` is the one that bites.**
  Production does not reach hydration as an instance at all: `install()`
  writes the built `Arc<Hydrator>` into a process-global,
  last-write-wins slot (`static INSTALLED` + `OnceLock<RwLock<Option<…>>>`,
  `tier/hydrate.rs:160-195`) and the entry point
  `hydrate::request(dev, ino, path, trigger)` (`:269`) resolves it from
  that global with no volume argument, over five call sites in
  `nfs/v4/` (`ioops.rs:2211,2268,2661`, `fileops.rs:558`,
  `perfops.rs:104`). The `Hydrator` owns one `store` with the bucket
  baked in (`pnfs/mds/server.rs:1005`), installed once at `:1348`.
  Calling `install()` N times yields **one** hydrator, the last. This is
  the same work §4 and §9 already book as "per-volume client
  acquisition — the single largest addition"; what was wrong was
  implying the engine shape made it free. The instance half already
  exists (`request_on`, `tier/hydrate.rs:277`) and every caller already
  passes `path`, which §6's mount-subtree map resolves — so the repair
  is "delete the global, route by subtree", not new machinery.
- **The fence is already an injected callback.**
  `spawn_heartbeat(…, on_deposed: Box<dyn FnOnce() + Send>)`
  (`tier/epoch.rs:473`). `exit(70)` is not in the tier at all — it is a
  closure the caller passes, at `pnfs/mds/server.rs:1131`. So F5's
  "process-scoped fencing is fatal for N" is fixed by **passing a
  different closure**: `exit(70)` at N = 1, quarantine at N > 1. Zero
  change to `epoch.rs`.

What remains, of the 18 `tier_*` methods on `StateBackend`
(`state_backend/mod.rs:1007-1151`), across its two implementations
(`sqlite.rs:763`, `memory.rs:66`):

| Verb class | Count | Change |
|---|---|---|
| Keyed by `(dev, ino)` | 6 | **None** — *if* the inode is genuinely unique across workspaces on the shared disk, the same fact that made per-volume databases impossible [F4]. This is the most load-bearing claim in the section and §10 carries it as a risk until measured. `tier_clear_dirty`, `tier_repoint_dirty`, `tier_clear_dirty_if_seq`, `tier_delete_generation`, `tier_delete_evicted`, `tier_set_hydrating`. |
| **Tombstone lane — keyed by the S3 key string, NOT by inode** | 3 | **Needs the scope, and the inode argument does not reach it.** `tier_delete_tombstone(&self, key: &str)` (`state_backend/mod.rs:1074`) carries no inode; the table's identity is `key TEXT PRIMARY KEY` (`sqlite.rs:2769`) and the memory double is `DashMap<String, TierTombstone>`. `tier_apply_rename` (`:1089`) and `tier_apply_remove` (`:1101`) each `INSERT OR REPLACE INTO tier_tombstone` (`sqlite.rs:1076`, `:1143`) — so they sit in the same lane despite taking an inode. Identity becomes `PRIMARY KEY (volume_id, key)`. |
| List / enumerate | 4 | Need a scope: `tier_list_dirty`, `tier_list_generations`, `tier_list_evicted`, `tier_list_tombstones`. Trait decl + 2 impls each. |
| Write / insert | 5 | Row gains `volume_id`. |

**Why the tombstone row is called out separately.** An earlier draft of
this table put those three in the "None" class on the strength of the
shared-disk inode argument, which does not protect them: a tombstone's
only identity is `key_prefix + relative_path` (`tier/flush.rs:330`), and
under §4's bucket-per-workspace two workspaces holding the same relative
path can produce the same key. The consequence would have been the
section's own named worst case arriving through a verb the section
excluded — `INSERT OR REPLACE` silently overwriting a foreign row, and
`consume_tombstones` (`tier/flush.rs:720-780`) HEAD/DELETE-ing against
the wrong bucket past its "foreign interference; deleting anyway" arm
(`:758-763`). It stays a labelling and change-budget correction rather
than a live defect only because §6 and §8 already require that **every**
tier verb become volume-scoped, and scoping `tier_list_tombstones` alone
severs the enumeration step all three collisions need. The lesson is the
one this repo keeps relearning: a carve-out justified by one identity
argument has to be checked verb by verb against the identity each verb
actually uses.

**68 production call sites, 9 files** — `flush.rs` 23, `import.rs` 18,
`evict.rs` 11, `hydrate.rs` 7, `state_backend/mod.rs` 2, `rpo.rs` 2,
`durable.rs` 2, `identity.rs` 2, `reporter.rs` 1 — plus a `volume_id`
field on the two S3-facing engine configs and the hydrator's store
resolution above. These are signature changes, so the compiler finds
every site.

**This figure replaces an earlier "40 in 8 files", which was an
undercount of about 40%.** The census script excluded test code by
brace-matching `mod tests {`, and the matcher counted braces inside
string literals — so it lost the module boundary in the two largest
files and read production sites past it as test sites. Re-measured with
a string- and comment-aware scanner: 68 production, 123 test, and
`identity.rs` appears as a ninth production file the first pass missed
entirely. Nothing about the *shape* of the change moves; the size does,
and step 1 in §9 should be sized against 68.

Safety net: **123 test call sites of those same verbs**, 163 tests in
`tier/`, 84 in `state_backend/`, ~2,465 test attributes crate-wide, the
composition suite, and the drill record.

**The risk is not in the 40 sites.** It is in three places, and the
build order (§9) is cut around them: the pre-ack capture guarantee must
stay ONE batched transaction with `volume_id` inside it (the L2 A3
property, and F4's whole reason for one database); the scoped delete
lane is new code running against a live listener; and **a wrong scope
fails silently** — a list verb returning too few rows does not error,
the file simply never flushes, which is indistinguishable from "nothing
was dirty". An error must not return a legal value, and here the legal
value is the common case.

## 3. The workspace model, and the port answer

A workspace is a **row in the hub's registry**: *(bucket, prefix,
local subtree, volume cell, class, role ∈ {owner, satellite, frozen})*.

**Namespace.** One NFSv4 pseudo-root; each workspace is an export
beneath it at `/volumes/<name>`. This machinery is not new and is not
speculative: `nfs/v4/pseudo.rs` implements the pseudo-filesystem and
`pnfs/config.rs:26` already takes `exports: Vec<ExportConfig>`. The
pNFS MDS exercises multiple exports today; lite renders exactly one
(`lite_operator/render.rs:355`). The hub renders N.

**This is the port answer.** One Service, one port, N paths:

```yaml
# 100 workspaces, 100 PVs, one endpoint, one port
spec:
  nfs:
    server: hub.flint.svc.cluster.local     # or one LB address
    path: /volumes/ws-agent42                 # the only field that varies
  mountOptions: ["nfsvers=4.2", "hard", "proto=tcp"]
```

Kernel clients mount subtrees natively; no client-side feature is
required and no non-standard port appears anywhere. For consumers
outside the cluster this reduces one `LoadBalancer` Service on 2049 —
one firewall rule, one LB, regardless of workspace count. The tunnel
and shared-IP arrangements that a per-share fleet needs become
optional rather than structural.

**A topology invariant this document inherits and must contradict.** The
prior doc's §3 reads: "One hub (or several) per cluster. Consumers mount
their **local** hub over in-cluster NFS… **No hub ever addresses another
hub**", with S3 as the complete inter-cluster channel
(`multi-volume-hub-design.md:65-73`). That is a *hub-to-hub*
non-communication rule, and it survives untouched — nothing here makes
one hub address another. But its first sentence assumes **local**
consumer mounts, and §1's common case is the opposite: agents on other
clusters mounting *this* hub directly. Both topologies are supported and
they are not the same product:

| | Consumers mount | Cross-cluster sharing | Endpoints |
|---|---|---|---|
| Hub-per-cluster + satellites | their local hub | read-mostly, barrier-to-barrier | 1 per cluster, in-cluster |
| **One hub, many clusters** | a remote hub | **full POSIX, one writer authority** | **1 total, external** |

The satellite row is the scaling answer and stays the recommendation for
read-mostly fan-out. The second row is the row that needs an external
endpoint, and it is the one the fleet actually asked for — so the
per-workspace access-control gap it opens (§10) is step-1 scope, not a
later hardening item.

**Control plane.** An admin API on its own port with Secret-token
auth, never on the Service that carries NFS: `POST /volumes`
(create / fork / claim), `/barrier`, `/release`, `DELETE /volumes/{n}`,
`GET /volumes`. Claim and release are step-1 scope, not a later
polish item — the session lifecycle in §7 is unusable without them
[F5].

## 4. One bucket per workspace — the common case, and what it costs

**Owner's constraint (2026-09-09): a separate bucket per workspace is
the COMMON case, not an option.** The prior document assumed the
opposite throughout — a volume there is a *(bucket prefix, …)*, a
prefix inside the hub's single bucket, and its §6 pins session cells,
volume cells and manifest authority to "exactly ONE home bucket". That
assumption is now inverted, and inverting it moves three things.

### What bucket-per-workspace makes better

Worth saying first, because it is not merely tolerable — on three axes
it is the stronger design:

- **Isolation is a bucket boundary**, the strongest one S3 offers. A
  policy, a KMS key, a replication rule and an access log per
  workspace, with no prefix-condition IAM to get subtly wrong.
- **Lifecycle rules stop competing.** F3's noncurrent-tail work needs a
  lifecycle rule per workspace class, and a bucket caps at 1,000
  lifecycle rules — a ceiling prefix-per-workspace hits at scale.
  Bucket-per-workspace never approaches it.
- **The volume cell colocates with its data.** Which is not just tidy:
  the cell, the `.flint/volume` identity object, the manifest and the
  data objects share one bucket, so a workspace's entire durable state
  is one enumerable unit. A per-workspace DR sweep becomes a
  single-bucket sweep.

### Cell placement — and why the lease survives it

The prior "one home bucket" rule was doing two jobs, and they separate:

- **The session cell** (`.flint-hubs/<hub-id>`) must live in **one
  control bucket**, named in the CR. It is per-hub, it is what depose
  CASes, and it has to be findable without reference to any workspace.
- **Volume cells live in their own workspace's bucket.** This is safe
  for the exact reason depose-first exists: **the protocol never
  performs a cross-object CAS.** Depose CASes the session cell, *then*
  claims volume cells as separate conditional writes. Nothing in the
  correctness argument requires the two objects to share a bucket, and
  `FlintTierSession`'s properties rest on per-object CAS plus token
  rotation, not on colocation.

What genuinely does not survive is cross-*region* CAS. Replication is
still not a coordination primitive; a workspace's bucket is its
authority.

### F7 breaks, and it needs a roster

**"The bucket alone is restorable" (F7) does not hold across N
buckets.** The DR path in §6 is a claim-time sweep that rebuilds the
registry from `.flint/volume` objects — which works when there is one
bucket to list, and is impossible when the buckets are the thing you
have lost. You cannot enumerate buckets you do not know exist, and
`registry.db` is deliberately a cache.

**Decision: the control bucket carries a durable workspace roster.**
Every create and delete writes it; it holds (workspace name, bucket,
prefix, class, created-by) and nothing that changes at session rate.
DR becomes: read the roster from the control bucket, then sweep each
listed bucket for its `.flint/volume`. The restorability property is
recovered, and its unit is now *the control bucket plus the roster*
rather than any single bucket. **Say that out loud in the runbook** —
losing the control bucket is now a distinct and more serious event than
losing a workspace bucket, which the prior design had no equivalent of.

Rejected alternatives: `ListBuckets` plus a naming convention (works
until someone else's bucket matches the pattern, and it grants a
fleet-wide read the hub does not otherwise need); the CR as authority
(Kubernetes is not a durable store for data-plane truth, and F7 exists
precisely because v1 made that mistake with `registry.db`).

### Credentials — the real wall, and it is IAM, not code

Credentials are process-wide today. Lite delivers them by `envFrom` a
Secret (`lite_operator/crd.rs:177` → `render.rs:833`, injected at
`:1030`) — environment variables for the whole container. One process,
one identity. With bucket-per-workspace, that one identity must reach
every bucket in the registry, and **that runs into an IAM limit, not a
design preference**: an inline role policy caps at 10,240 characters
and a managed policy at 6,144, while a bucket needs two ARNs (the
bucket and its objects). At roughly 150 characters per workspace, one
policy holds on the order of 40–65 buckets. A ten-managed-policy
ceiling per role does not get you to 1,000.

So enumerating buckets in a policy does not scale, and the options are:

- **(a) Bucket naming convention + wildcard ARN** —
  `arn:aws:s3:::flint-ws-*`. One short policy, any number of
  workspaces. The hub can reach every bucket matching the pattern,
  so the isolation the bucket boundary bought back is partly given
  away at the identity layer. **Recommended for step 1**: it is the
  only option that is both O(1) in policy size and no new code.
- **(b) Per-workspace `AssumeRole` or a scoped-down session policy.**
  The hub holds one identity and derives a per-workspace credential
  bounded to that bucket. Real isolation, AWS-native, no stored
  secrets, and the credential cache and refresh are the new code. **The
  isolation upgrade, and where this should land.**
- **(c) Per-workspace stored Secrets.** The registry row carries a
  Secret reference. Required only when workspace buckets live in
  *other accounts*. Most code, most operational burden, and a failure
  mode with no analogue in the shipped tier: a workspace whose Secret
  is revoked mid-session.

The shipped tier resolves one client from the process environment, so
(b) and (c) both mean **the tier's client acquisition becomes
per-volume** — a change the §2 surface count does not include, and the
largest single item bucket-per-workspace adds to step 1.

### Two things to check before designing for 1,000

- **Account bucket quota.** This was historically 100 by default and
  1,000 by quota increase; AWS raised it substantially in 2024. Verify
  the actual current quota for the target account rather than trusting
  either number — at bucket-per-workspace it is a hard ceiling on
  fleet size, and it is per account, not per hub.
- **Bucket creation is slow and rate-limited**, where a prefix is free
  and instantaneous. §7's "ready in seconds" claim was written against
  prefix-per-workspace. Measure `CreateBucket` plus the first
  conditional write before promising session-rate workspace creation,
  and consider a pre-warmed bucket pool if it does not hold.

### A failure mode the model does not have

With one bucket, the store is up or down. With N buckets, **a single
workspace's bucket can be unavailable while the control bucket and
every other workspace is healthy** — permissions revoked, bucket
deleted, quota or throttling scoped to one bucket. `FlintTierSession`
abstracts the store as one namespace and cannot express this. The
consequence to specify: such a workspace must park loudly and *alone*,
never fence the session cell and never take the other N−1 with it.
This is the volume-scoped-quarantine invariant of §5 arriving through a
second door, and it is worth one extra model state rather than a
comment.

## 5. Ownership — the two-level lease

Reused from the prior design, which modelled it: `formal/FlintTierSession.tla`
(436 lines, 7 gate runs, gate 172). The essentials, unchanged:

- Per-workspace heartbeats do not scale — 1,000 volumes at one PUT/10s
  is ≈8.6M requests/day, ≈$1,300/month. One **session cell** per hub
  heartbeats; **volume cells** are per prefix and change only on
  claim/release. Idle workspaces cost zero requests.
- **Depose-first.** S3 CAS conditions one object, so nothing binds "the
  session is quiet" to "the volume cell is mine". The naive protocol
  leaves the loser's session cell alive and it publishes forever; the
  `NoDepose` mutation finds exactly that immortal-zombie lasso. Depose
  the session cell first, then claim volume cells.
- **Fencing is per-volume in mechanism** [F5]. The shipped fence is
  `exit(70)` and the shipped heartbeat reads NotFound as deposed —
  correct for one volume per process, fatal for N. `DELETE /volumes/X`
  purging X's cell must not kill the hub. Re-scoped invariant: **an
  unfenced VOLUME is never served.** A contested claim parks that
  workspace; it does not block the listener.

**Owed before any step-1 code — and this is a hard gate, not
bookkeeping.** The publish stamp must be **claim generation**, and the
model must prove it. Session generation is per-hub and *not*
monotonic across owners (a fresh hub deposing a long-lived one claims
at sgen 1 against the loser's 7), so stamping it feeds
`stamps.epoch <= ours ⇒ ForeignHand ⇒ local-wins re-publish` at
`successor_check` (`tier/flush.rs:1683`, whose own doc comment names the
hazard — "an unfenced zombie's local-wins re-publish would land OVER the
live successor's object") and resurrects BUG 7 **as silent data loss**.
*(The prior document cites `flush.rs:1150`; that line has since drifted.
The mechanism is unchanged.)*
Silent, cross-hub, timing-dependent: the bug class a test cannot
reach and a counterexample can. Extend `FlintTierSession` with
claim-gen-stamped publishes and port the `NoStampCheck` mutation so the
wrong binding reproduces the BUG 7 counterexample in the gate [F1].

**Also owed, smaller:** the per-volume quarantine states are not in the
model — its evidence semantics are hub-scoped throughout. Model
`DELETE` of a contested cell, NotFound-on-cell ≠ hub death, and the
re-scoped invariant. This is where a hub either serves an unfenced
subtree or wedges all N over one disputed claim.

**Not to be modelled:** the registry, the admin API, the namespace, and
satellite refresh's update/delete lanes. Those are directly testable,
and this repository's own record is that the abstraction was the bug
three separate times. A model at the wrong layer buys confidence that
has not been earned.

## 6. State

**One `state.db`; tier rows carry `volume_id`; every tier verb becomes
volume-scoped** [F4]. This is the prior design's decision and it
survives the new framing intact. The reasoning is worth restating
because it is the opposite of the obvious answer: the shipped tier
speaks to ONE `StateBackend` carrying NFS state (clients, sessions,
stateids, locks) *and* every durable tier verb, threaded as a single
`Arc` through flush / evict / hydrate / import / reporter, and **the
pre-ack capture guarantee is one batched transaction to that backend**.
One sqlite per workspace would silently reverse that guarantee. Capture
marks are `(dev, ino)` and the disk is shared, so the inode cannot
select a database either.

Costs accepted: dropping a workspace is a scoped delete lane (tier rows
plus an NFS-state purge for the subtree), not a file unlink; capture's
path-less lanes resolve `volume_id` from the mount-subtree map at mark
time.

Per-workspace databases stay recorded as the fallback **if** the single
WAL measurably serializes capture at fleet scale — with the pre-ack
guarantee re-derived over N commits before any such move. Measure
before ~1,000 live workspaces.

**Registry durability: the roster plus the buckets are sufficient**
[F7, restated for §4]. `registry.db` is a cache, never the authority.
Every create and claim mirrors the row into `<prefix>/.flint/volume`
*in the workspace's own bucket*; the bucket and prefix are stamped into
the workspace's own durable tier rows; hub identity comes from
configuration, never from a PVC-persisted `server_id` (after PVC loss
the sweep must still be able to say "owner == me"); the identity object
is written at workspace *creation*, before first mount, so a
never-yet-flushed workspace has a binding from minute zero.

**What changed with bucket-per-workspace:** F7's original property was
"the bucket alone is restorable", and one bucket could be listed to
find every `.flint/volume`. N buckets cannot — you cannot enumerate
buckets you have lost the names of. So DR is now two-phase: read the
**roster** from the control bucket, then sweep each listed bucket. The
unit of restorability is the control bucket plus the roster, and the
runbook must say so (§4, §10).

## 7. Lifecycle, and the scale-down mechanism this owes

flint-lite's idle ladder is per-share because the share *is* the
Deployment: `Active` → `IdleSuspended` (`replicas: 0`, PVC kept —
`lite_operator/reconcile.rs:863`) → `Hibernated` (PVC deleted, data in
S3 — `crd.rs:106`), gated by `HubStatus::suspendable()`
(`hubstatus.rs:246`).

**The hub cannot inherit that ladder, and pretending otherwise is the
biggest risk in this design.**

- **Suspend becomes an AND over N workspaces.** The pod scales to zero
  only when every workspace is idle. If a workspace is idle with
  probability p, that is pⁿ — at p = 0.9 and N = 100, about 3 × 10⁻⁵.
  In practice the hub never suspends.
- **Hibernate is structurally impossible.** It deletes the PVC, and one
  PVC holds the hot tier, the namespace and the `state.db` for every
  workspace. There is no hibernating one workspace's share of a PVC.
- **And eviction reclaims the wrong resource.** The tier's eviction is
  an in-place truncate that *deliberately* keeps the inode alive
  (`tier/evict.rs`, C6 — every cached fd depends on it). It reclaims
  bytes, never namespace. An idle workspace keeps its full local
  namespace resident forever. The prior design concedes "metadata per
  hub (full local namespace)" as a scaling statement but never draws
  the idle consequence, and the hardening drills already established
  that a 1 GiB workspace's ceiling is **inodes**, not bytes.

So hub needs a fourth mechanism that appears nowhere in the prior
document's §5, §8 or §9:

**Volume unload.** Drop an idle workspace's namespace from local disk;
keep the registry row, the bucket binding and the volume cell; rebuild
the subtree from the manifest on next touch. It is hibernate at
workspace granularity, and it is what makes the economics hold at
1,000 workspaces.

It is also the hardest new coherence problem here, because unloading
under held fds, locks or delegations is exactly the ESTALE corner the
prior §9 flags. It is a **lima-rig question, not a model question** —
hold a lock across an unload/reload and watch what the client does.

With unload in hand, the hub-level ladder returns as pure addition:
suspend the pod when every workspace is unloaded, which is now a
reachable state rather than a coincidence.

**Session lifecycle, end to end:**

1. `POST /volumes {name: ws-agent42, from: base-repo@latest}` → ready
   in seconds; identity object written at create.
2. Pod mounts `hub:/volumes/ws-agent42`. Full POSIX.
3. Tools run: metadata local, cold reads hydrate in parallel, writes
   flush on the class floor, turn barriers are durable resumable
   checkpoints.
4. Session end: `release` (drain + final barrier + release token) or
   `DELETE` (version-enumerating purge).
5. Resume anywhere: any hub claims instantly off a clean release;
   only touched files move.

## 8. Reused versus new — honestly

The prior review's most useful finding was that v1 claimed reuse where
none existed [F4][F5][F8]. Restating the boundary plainly, because
build estimates depend on it:

**Genuinely reused, unchanged:** the NFSv4 stack and its POSIX surface;
the pseudo-filesystem and multi-export namespace; the tier's capture /
flush / evict / hydrate machinery; the epoch fence's three legs; the
F17b/c fd-anchoring layer (which is what defeats the ESTALE-storm
objection to §7's unload and to satellite refresh).

**Reused but re-scoped — modification of shipped code:** every tier
verb gains a volume scope; the fence moves from process-scoped
`exit(70)` to per-volume quarantine; the heartbeat stops reading
NotFound as death.

**New code, no reuse:** the registry and its bucket mirroring; the
admin API; the session cell, depose and clean-release drain — note
that the shipped `epoch_release` **has no caller** and `claim()` has no
contested return arm, so the release/drain/teardown orchestration is
new; volume unload (§7); the DR sweep across many prefixes.

**New code that looks like reuse and is not** [F8]: satellite refresh.
The shipped `import_refresh` is local-wins in every lane and has **no
delete lane** — reused as-is, a satellite would freeze every already
imported file forever and never remove one the owner deleted. It needs
a manifest poll, an update lane (etag change ⇒ per-file rename-over,
held fds keep old bytes until close), a delete lane (manifest omission
⇒ tombstone-respecting unlink), and versionId-pinned hydration.

## 9. Build order

0. **The model gate** (§5): claim-gen-stamped publishes plus the ported
   `NoStampCheck` mutation; the per-volume quarantine states. Before
   any step-1 code.
1. **Multi-volume core, merged**: registry + admin API (all five verbs
   including claim and release); volume-scoped tier rows and the scoped
   delete lane; per-volume guards and quarantine with the listener
   decoupled from any contested claim; session cell + depose + clean-
   release drain; claim-gen stamps in all three fence legs; registry
   mirroring to `.flint/volume`, config-stable hub identity, and the
   DR sweep and runbook.
   **Bucket-per-workspace adds three items to this step** (§4), and the
   §2 surface count does not include them: the **workspace roster** in
   the control bucket (without it F7's restorability is gone, not
   degraded); **per-volume client acquisition** in the tier, which today
   resolves one client from the process environment — the single largest
   addition; and the **per-workspace store-unavailable** arm, which must
   park one workspace without fencing the session or the other N−1.
   Sequence the roster first: it is what makes every later step's DR
   story true.
2. **`FlintShare` surface**: the CRD gains the workspace fields, the
   operator renders the multi-export config, and `render.rs`/`reconcile.rs`
   stop assuming share == volume. Free to change shape (§2), so this is
   ordinary work rather than a migration. Deliberately after 1 — the
   server has to serve N before the operator that configures it means
   anything.
3. **The two-hub handoff drill** on a live cluster: clean, crash,
   zombie. This is the review boundary; nothing past here starts until
   it is green with controls.
4. **Volume unload** (§7), with the lima lock-across-unload rig.
5. Parallel small-object hydration: pipeline the queue, raise
   `hydrateConcurrency` well past 4, prefetch siblings on
   readdir-then-open. No correctness surface changes.
6. Satellite role as a coherence protocol (§8) — update and delete
   lanes, versionId-pinned hydration.
7. Fork-from-barrier: O(metadata) zero-copy fork of a workspace from a
   barrier, versionId end-to-end, retention bound and refusal, priced
   noncurrent tail.
8. Range-serve for large artifacts + the `FlintTierMarker` extension.

## 10. Open risks

- **One hub is one failure domain for N workspaces.** A hundred single-workspace hubs fail independently; one N-workspace hub does not. On the pure-spot
  clusters this fleet runs on, a single reclaim takes out every
  workspace it holds. Mitigations exist — K hubs per cluster with
  workspace→hub mapping, and a clean claim path that makes restart
  fast — but the blast radius is real, it is new, and it is the
  strongest argument a reviewer will make against this topology.
  **Measure restart-to-serving with N loaded workspaces before
  promising anything.**
- **One endpoint means per-workspace access control has no
  network-layer discriminator — and the drill proved the discriminator
  was already gone.** This is the cost of §1's benefit and it is the
  most serious new item in this list. With N separate shares there were
  N Services, each with its own NetworkPolicy; behind one endpoint every
  consumer that can open 2049 reaches the pseudo-root and can `LOOKUP`
  its way into **every** `/volumes/<name>` beneath it. The obvious
  fallback — source CIDR — does not exist for the case that matters:
  `many-clusters-one-hub.sh` measured **1486/1486 remote connections
  arriving from the hub's own gateway address** (`10.244.0.1`) because
  `externalTrafficPolicy` is unset and kube-proxy SNATs, so the
  `nfsClientCIDRs` allowlist both charts document as *"Cross-cluster
  consumers are always CIDRs"* (`flint-lite-chart/values.yaml:155-156`)
  admits either nobody or everybody. Multi-volume does not cause that
  defect, but it converts it from "one share is over-exposed" into "one
  reachable port exposes N tenants". **Authorization has to move into
  the NFS layer or above it** — per-export `sec=krb5` principals, a
  per-workspace export ACL keyed on something the wire actually carries,
  or the admin API minting per-workspace credentials — and until it does,
  a multi-tenant hub on an external endpoint is a single trust domain.
  Say that in the runbook and price the answer in step 1.
- **N clusters × N workspaces concentrate every NFS client-identity
  collision into one `ClientManager`.** `co_ownerid` is
  `Linux NFSv4.<minor> <nodename>` and nothing else — no address, no
  cluster — unless `nfs.nfs4_unique_id` is set per node, and the drill
  wire-captured **byte-identical owners from two different kind
  clusters**. RFC 8881 §18.35.5 requires the server to honour that, so
  flint can only avoid losing state over it, never refuse it. Under
  hub-per-share a collision cost one share; under one hub serving M
  clusters it lands in the single `ClientManager` that all N workspaces
  share. The drill's defects 1 and 3 (a permanent lock leak through the
  case-5 cascade; `owner_to_id.remove` unconditional, so one cluster's
  clean unmount deletes another live cluster's registration) are share-
  scoped today and become hub-scoped here. **Their fixes are a
  prerequisite for the many-clusters row of §3's table, not a parallel
  workstream.**
- **The control bucket's AVAILABILITY is a hub-wide liveness
  dependency, and that is the direction §4 did not analyse.** §4 works
  through the benign asymmetry — one workspace's bucket unreachable
  while the control bucket is healthy — and specifies that such a
  workspace parks alone. The converse is the one that hurts. The shipped
  heartbeat has **two** doors to `on_deposed()`, not one: a real
  deposition on 412/NotFound (`tier/epoch.rs:550-559`), and
  `consecutive_failures >= cfg.lease_misses` (`:589-597`), which fires
  on **any** error — a 5xx, a timeout, or a `StoreError::Auth` 401/403
  from a rotated key or a policy change. The code says so deliberately:
  *"A refusal and a failure are different events with the same shape.
  Both end in a fence — correctly, since a hub that cannot renew cannot
  prove it is still the holder."* At the shipped `10s × 6`
  (`pnfs/config.rs:304-326`) that is a **60-second control-bucket blip
  fencing all N workspaces while every workspace bucket is healthy and
  nobody deposed anything.**

  This inverts §4's own headline. With one bucket, the outage that
  fences you is the same outage that makes the data unreachable, so the
  fence costs nothing you had. Split the session cell out and one
  unreachable object takes down N healthy workspaces. And it cannot be
  relaxed away: a depose needs **only** the control bucket, so a hub
  that cannot reach it genuinely cannot prove it still owns anything —
  the coupling is structural, not an implementation choice.

  **The model cannot see it.** `BeatFail(h)` is guarded on
  `~(sess[h].gen = myGen[h] /\ ~sess[h].deposed)`
  (`formal/FlintTierSession.tla:211-214`) — there is no
  failure-without-deposition action anywhere in the module, and `sess`,
  `vcell` and `nextTok` are one store with one availability
  (`:110-113`). So the "one extra model state" §4 asks for covers the
  wrong half, and the step-0 gate as currently specified will not find
  this.

  **What the design owes.** (1) Distinguish a renew *failure*
  (5xx/timeout/Auth) from a *depose* (412/NotFound) at the session cell,
  and state what a hub does in the failure case — continuing to serve
  reads while stopping publishes is safe, because no successor can have
  claimed a volume without also writing that volume's own bucket,
  whereas parking N is a self-inflicted outage. (2) Model it as a
  distinct `BeatUnreachable` action so the invariants are checked
  against it. (3) Either state the control bucket's availability as an
  explicit SLO dependency here, or keep a fallback lease cell in each
  workspace's own bucket so a control-bucket outage **degrades** instead
  of fencing. Option (3) is the one that restores the property §4
  claimed — "a workspace's bucket is its authority" — and it should be
  priced in step 1 rather than discovered in a drill.
- **Losing the control bucket is a new, worse class of event** than
  losing a workspace bucket (§4). It holds the session cell and the
  roster; without the roster the workspace buckets are unenumerable
  even though every byte in them survives. Versioning, MFA-delete and
  a replicated copy of the roster belong in the runbook, and the DR
  drill must include "control bucket gone, workspace buckets intact".
- **Account bucket quota is a hard ceiling on fleet size** under
  bucket-per-workspace, and it is per account rather than per hub.
  Verify the target account's actual current quota before designing for
  1,000 workspaces.
- **`CreateBucket` is slow and rate-limited** where a prefix was free.
  §7's "ready in seconds" was written against prefix-per-workspace;
  measure bucket creation plus the first conditional write before
  promising session-rate creation, and consider a pre-warmed pool.
- **Wildcard-ARN credentials give back some of the isolation the bucket
  boundary bought** (§4 option a). It is the step-1 recommendation on
  scaling grounds, not on security grounds; option (b) is where this
  should end up, and the gap should be stated to whoever signs off on
  tenancy.
- **The idle floor is unmeasured** and it decides whether §7's unload
  is an optimisation or a step-1 blocker. Inodes, `state.db` rows and
  RSS per loaded-but-idle workspace. A single-volume hub can proxy this
  today by loading N workspaces into its one export — a cheap
  experiment that should run before step 1, not after.
- **Tenancy is undecided** (§4). Option (a) makes a hub single-tenant
  and most of this risk evaporates; (b) and (c) do not.
- Single-WAL capture serialization at fleet scale [F4] — the trigger
  for per-workspace databases; measure before ~1k live workspaces.
- Residual ESTALE corners: special stateids, lease-recovery CLAIM_FH,
  and LOCK held across a refresh rename-over or an unload [F8].
- `registry.db` is a **cache** of bucket and config truth — the DR
  sweep is the recovery path and must be drilled, not assumed.
- The admin API is a new attack surface. Token auth plus NetworkPolicy;
  it must never share a listener with NFS, and it holds no credentials
  beyond the hub's own (see §4 before that stays true).
- Sessions-end-before-delete is enforced harness/CSI-side. NFSv4 has no
  MOUNT protocol, so the hub can make delete-under-mount *safe*
  (ESTALE) but not *polite* [F8].

## 11. Deliberately out of scope

- Cross-cluster concurrent writers to one workspace — refused by
  design. The owning hub is the consistency point.
- Automatic claim-on-first-write.
- Any change to flint-lite's product surface (§2).
- Multi-region CAS. Cross-region replication can make satellite data
  reads region-local, but session cells, volume cells and manifest
  authority live in exactly one home bucket (§4).
