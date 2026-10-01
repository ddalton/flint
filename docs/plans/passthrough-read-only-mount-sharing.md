# Sharing one `mount-s3` mount between pods — read-only passthrough

Status: **BUILT 2026-09-29, opt-in (`spec.sharing.readOnly`), NOT RUN on
a cluster — see §10 for what was built, where it departs from the plan
below, and the two answers the plan asked for (§8).** Originally
research, no code, produced 2026-09-22. Written so the first line of
code can be judged against it. Citations are `path:line` as read
on this date. Claims I opened and read myself are unmarked; claims taken
from the recon pass and NOT independently re-read are marked **[recon]**
and must be re-verified before they are built on. Nothing here is
implemented.

Provenance: came out of the question *"since flint passthrough is a FUSE
driver, can we do better — support ROX and improve performance?"*, after
the F70/F71 ROX work established that flint has no shared cache and no
ROX, and that AWS's own driver has both.

---

## The answer, in one paragraph

**The bind mechanics are the easy 10% and already exist; the obstacle is
the credential, and it is the driver's stated anti-spoof property, not an
oversight.** `bind_mount` has no notion of "already bound", and the
read-only path deliberately gives every target its own independent `ro`
mount, so one FUSE `src` can feed N targets today at the kernel level.
What is strictly 1:1 is the *bookkeeping*: `VolumeState` records one
target, one src, one worker, and `src` is derived from the pod-unique
ephemeral volume id, so there is one mount per pod **by construction**.
Sharing means a second state record with a member set and a refcount —
mechanical, well-understood work. The crux is elsewhere: a passthrough
credential is minted from one specific tenant pod's ServiceAccount token
and the broker refuses any exchange whose registration does not match
that pod's UID **[recon: `broker.rs:378-382`]**, which is the design's
named T2 mitigation ("the one binding a pod cannot self-mint"). Relaxing
it needs a replacement invariant, not an extension. **The one genuinely
encouraging finding** is that for a READ-ONLY grant on the `sts` backend
the credential's *authority* is already a pure function of the CR's
bucket and prefix, with no per-pod component **[recon:
`read_session_policy`, `broker.rs:226-253`]** — which yields a security
argument narrow enough to defend, and which is why this note is scoped to
read-only on `sts` and nothing else.

---

## 1. Why bother — what sharing actually buys

Established while answering the ROX question (verified):

- There is **no data cache today at all**. `--cache` appears nowhere in
  the source, while `flint-s3-csi-chart/values.yaml:274-276` provisions a
  1Gi `scratch` emptyDir and documents it as *"mount-s3's cache, at
  /tmp"*. That comment was false. (Now addressed separately: `spec.cache`
  is wired, opt-in, with a required ceiling.)
- **One `mount-s3` process per POD.** Kubelet mints a pod-unique volume
  id for every inline ephemeral volume, `src` is derived from it
  (`state.rs:136-138` via `volume_dir`), and one worker pod is created
  per volume.

So for N pods reading one dataset on one node:

| | today | shared |
|---|---|---|
| mount-s3 processes | N | 1 |
| connection pools / prefetch budgets | N | 1 |
| block cache | N separate (or none) | 1 |
| kernel page cache for the same file | N superblocks → N copies | 1 superblock → 1 copy |

The last row is the FUSE-specific win and the reason this is worth doing
at all: **bind mounts of one FUSE mount share the superblock and
therefore the page cache**, while separate daemons cannot. Per-pod mounts
throw that away for every pod after the first.

**ASSUMPTION — must be verified before code:** that Mountpoint permits
kernel page caching rather than mounting `direct_io`. If it does not, the
last row collapses and the case rests on the first three (still real, but
smaller). Verify against the pinned build (`Dockerfile.passthrough`,
`MOUNT_S3_VERSION=1.24.0`).

---

## 2. What already works in our favour

**The bind primitive fans out.** `bind_mount(src, target, read_only)`
has no "already bound" check, and the read-only path binds `src` → a
stage, remounts the *stage* `MS_RDONLY`, binds stage → target, then
detaches the stage — precisely so each target is born `ro` on its own
rather than inheriting flags from a propagated copy
**[recon: `fuse.rs:150-178`]**. That is exactly the property N read-only
consumers need.

**Every argv input is CR-derived.** `mounter_args_for` builds the whole
`mount-s3` command line from `spec` — bucket, prefix, endpoint,
path-style, region, mount options, and now cache (verified,
`passthrough/mounter.rs`). So "same CR" gives "same argv" for free. The
only per-pod argv inputs are `--uid`, `--gid` and `--read-only`.

**Read-only authority is already CR-scoped.** On the `sts` backend a
`Read` grant attaches a session policy of `s3:GetObject` on
`<bucket>/<prefix>/*` plus a prefix-conditioned `s3:ListBucket`, derived
from the CR **[recon: `broker.rs:226-253`]**. A session policy only ever
intersects. **This is the load-bearing fact for the whole proposal.**

---

## 3. What is strictly one-per-pod today

`VolumeState` holds one `target_path`, one `src`, one `worker_name`, one
`nonce`, one `tenant` **[recon: `state.rs:36-61`]**. There is no list, no
count, no cross-volume index. The 1:1 assumption surfaces at publish,
republish, rebind, `cleanup`/`fail`, `adopt_existing` and — most
destructively — at unpublish.

**Unpublish is the worst of them, and I verified this one myself** while
filing [F72](../../spdk-csi-driver/docs/f72-passthrough-teardown-kills-the-mounter-before-quiescing-it.md):
`node.rs:1398-1403` unmounts the target, then the ro stage, then `st.src`
itself, then releases and deletes the worker — unconditionally. **The
first pod to unpublish would tear the mount down for all N.** Every one
of those steps needs a last-member gate.

Note the interaction with F72: that finding already calls for reordering
this teardown. **Do F72's reorder first.** Building a refcount on top of
an ordering that is itself wrong means doing it twice.

---

## 4. The key

For two pods to share, all of these must be equal:

`(node, namespace, CR name, effective uid, effective gid, readOnly=true, credential backend)`

`uid`/`gid` are **not** cosmetic and cannot be omitted: the uid goes into
the daemon's argv, into the `mount(2)` data string for the kernel's
`allow_other` owner check, **and** is the uid the worker process runs as
**[recon: `fuse.rs:56-61`, `node.rs:1494-1499`]**. There is literally one
owner per daemon. A pod that sets `chert.us/uid` therefore opts itself
out of the shared class — which is correct, not a limitation.

---

## 5. The crux, stated so it can be argued with

Today the credential is pod-bound at three points **[recon]**: the
exchange presents *that pod's* kubelet-minted token
(`node.rs:415-418`), the registration is keyed by volume id
(`broker.rs:654`), and `decide` requires `reg.pod_uid == id.pod_uid`
(`broker.rs:378-382`).

Sharing needs a registration that is **not** volume-keyed and **not**
pod-uid-bound. That is a new grant shape with a new invariant. The
replacement must answer:

1. **What is checked instead of the pod UID?** Proposal: every current
   member's token still passes `TokenReview` AND is still a `consumers`
   entry, re-validated on a schedule — because today the only revocation
   channel is the registering pod's token ceasing to review.
2. **Whose token refreshes the key?** Republish rewrites `creds.json`
   using the republishing pod's token **[recon: `node.rs:616-660`]**.
   With N members the credential's principal would silently change every
   ~90s. Needs an explicit *sponsor* with a defined handoff when the
   sponsor leaves.
3. **One registration, not N sharing a nonce.** The exchange finds a
   registration by scanning for a matching nonce **[recon:
   `broker.rs:579`]**; N registrations sharing one nonce would match
   nondeterministically. The design must collapse to a single record.

**The defensible security argument, and the whole reason for the
read-only/`sts` scope:**

> For a read-only grant on the `sts` backend, two pods on the same CR
> receive credentials of identical authority, because that authority is a
> pure function of the CR's bucket and prefix. Sharing the credential
> therefore grants neither pod anything it could not already mint for
> itself.

That sentence does **not** hold for `rest` (the door receives
`podUid`/`serviceAccount` and scopes itself) or for `static` (a single
shared key, with `read_enforcement()` reporting merely *cooperative*)
**[recon: `broker.rs:482-496`, `:523-535`]**. **Sharing must be refused
outright for those backends**, and for any read-write grant.

---

## 6. Risks that do not exist today

1. **Blast radius 1 → N.** The design's §8 claims FUSE death is survivable
   precisely because it affects "nobody else". Sharing gives that back: a
   worker OOM, eviction or drain race strands N pods on `ENOTCONN`, and
   recovery is "recreate the pod" — now N pods.
2. **Correlated, delayed credential failure.** If the sponsor pod leaves
   and its registration is dropped, surviving pods keep working until the
   current key expires, then all fail together — a ~15-minute fuse, with
   the warning events going to the *sponsor's* pod, i.e. not to any pod
   that will actually break.
3. **The preStop guarantee inverts.** The `released` marker disarms the
   hook immediately **[recon: `flint-s3-worker/src/main.rs:142-149`]**,
   so the first member's unpublish disarms it for the survivors — the
   exact failure the hook exists to prevent. Needs "last member released"
   semantics, which is a cross-binary contract change.
4. **Audit granularity collapses.** Every issued line currently carries
   `(ns, sa, pod-uid, cr, access, expiry)` **[recon: `broker.rs:606-617`]**
   and the design calls this a crown-jewel property. Shared, N tenants
   become one sponsor identity. **This is likely the hardest thing to
   defend in a customer review, and it should be decided before code, not
   after.**
5. **Noisy neighbour.** One `--max-cache-size`, one memory limit, one
   prefetch budget for N tenants. §7 of the CSI design rejected a
   per-node mounter partly for this. Sharing is a bounded move toward that
   rejected shape — bounded because it is one daemon per CR-class, not per
   node, and that distinction is the honest defence.
6. **A concurrency bug on the stage.** The ro stage is a single fixed path
   per source **[recon: `fuse.rs:317-321`]**, and the per-volume lock is
   keyed on the pod-unique volume id, so it would NOT serialize two pods
   sharing a mount. Needs a per-target stage or a shared-key lock.

---

## 7. Scope, and the order to build it

**In scope:** read-only, `sts` backend, same CR, same namespace, same
uid/gid, same node.
**Explicitly out:** read-write; the `rest` and `static` backends; sharing
across CRs; sharing across namespaces.

Order:

0. **F72's teardown reorder first** — a refcount built on the current
   ordering would be rework.
1. Per-target ro stage, or a shared-key lock (§6.6). Small, and it is a
   real race regardless of sharing.
2. The shared-mount record as a **second** state file
   (`<plugin>/shared/<key-hash>/state.json`) holding the member set, with
   per-pod `VolumeState` gaining a `shared_key` pointer. Keeping
   `VolumeState` per-pod preserves its existing invariants.
3. Last-member gates on `unpublish`, `cleanup`/`fail` and
   `adopt_existing`. Note `cleanup` currently unmounts `src` on a FAILED
   publish — under sharing, a failed *join* must never tear down the
   mount it failed to join. This is the same class of hazard the file
   already guards against for lean trees.
4. Worker naming and the `chert.us/volume-id` annotation keyed on the
   shared key — `ensure` refuses to adopt a worker whose annotation
   differs **[recon: `worker.rs:291-307`]**, so member #2 hits that
   refusal by design otherwise.
5. preStop "last member released" semantics.
6. The broker grant shape (§5) — last, because it is the only part
   needing a new security argument, and it should be reviewed on its own.

---

## 8. The three things to verify before any code

1. **Mountpoint's page-cache behaviour** (§1 ASSUMPTION). If it mounts
   `direct_io`, the headline benefit shrinks and the proposal should be
   re-costed.
2. **Re-read every [recon] citation.** They come from a single reading
   pass and have not been independently confirmed. Two of today's three
   staged scripts contained defects that looked fine from outside; a
   citation is not a verification.
3. **Decide the audit question first** (§6.4). If collapsing per-tenant
   attribution is unacceptable to the customer, the whole proposal dies
   there and the remaining performance answer is the cache plus
   `--metadata-ttl`, which are already available per-pod and need none of
   this.

---

## 9. What this does NOT need

ROX. It is worth saying plainly, because the two were raised together: a
PV is one way to key a shared mount, but nothing above requires one. The
key in §4 is computable from the CR, the namespace and the pod's own
attributes, all of which the inline-ephemeral path already has. Adding
`Persistent` to `volumeLifecycleModes` (and relaxing the hard refusal at
`attrs.rs:171-173`, which I verified) would give ROX the *form* — a
PVC that existing tooling understands — but passthrough's read-only
enforcement is already stronger than ROX's, since ROX on the block/NFS
driver turned out to be client-side only ([F70](../../spdk-csi-driver/docs/f70-rox-export-is-not-enforced-server-side.md)).
Build sharing for the performance; add PV support only if a customer
needs the PVC shape.

---

## 10. What was built (2026-09-29), and where it departs from the plan

Code: `s3csi/state.rs` (`SharedMount`, `share_key`, `VolumeState.shared`),
`s3csi/node.rs` (`publish_shared`, `sharing_decision`, the class lock,
`cleanup_shared_member`, `adopt_shared`, `teardown_mounter`),
`s3csi/worker.rs` (`ANN_SHARED_KEY`, `wait_exited`),
`passthrough/spec.rs` (`SharingSpec`), the CRD, `CHANGELOG.md`. Kind-rig
legs S23 (F72) and S24 (sharing) in `s3csi/e2e/run-s3csi.sh`, NOT RUN.

**§8.1, answered: Mountpoint does not mount `direct_io`, but it never
keeps the page cache either.** At v1.24.0 `fs.rs:400` replies to `open`
with `FOPEN_DIRECT_IO` only when the application asked `O_DIRECT`, and
never `FOPEN_KEEP_CACHE`; without that flag the kernel invalidates the
file's cached pages at every open. So the last row of §1's table is
"one copy per OPEN, shared by concurrent readers" — the durable per-node
win is the block cache (`spec.cache`) and one connection pool. Still
worth it for the flagship case; smaller than the table said.

**§5, dissolved rather than solved.** No sponsor, no handoff, no new
grant shape, no broker change. Every member registers and exchanges on
its own, exactly as today (its own nonce, its own token, its own
registration keyed by its volume id), and writes ITS OWN key into the
shared worker's comm dir — the freshest key wins the file, and since
every member's authority is the same function of the CR, whose key is
in the file is immaterial to access control. The door token
(`auth.token`) is written by the creator only; joiners skip it, because
the mounter is reading it. The broker's audit lines are therefore still
one registration and one exchange per pod (§6.4's concern is narrowed
to CloudTrail attribution of the S3 requests themselves, which
alternates between members' sessions — the admin who set the knob
accepts that).

**What sharing cannot do, stated on the CRD field:** revoke one member
before its pod exits. A refusal on one member's refresh removes the
shared key (today's revocation), the next still-allowed member's
republish re-mints it within ~90 s, and the refused member — still bound
to the same superblock, from inside its own mount namespace — reads
again. Every member could have minted that key itself, so nothing is
gained by the lingering member; but the isolation §4.6 promises per pod
is per CLASS here.

**Scope, as built:** read-only members (`access.is_read() ||
spec.readOnly`), same node/namespace/CR/uid/gid/credential mode/argv
(`share_key`; the argv is in the key so a CR edit is a new class for new
members), on `ambient` or on `broker` with a backend the plugin reads
from `/v1/status` as `sts` or `static` (both CR-scoped: a session policy
on the prefix, or the one read key). `rest` (per-pod scoping), an
unreadable broker, a read-write consumer: a mounter per pod, reason
logged. `identity.mode static` with the knob: refused by `validate`.
`webIdentity` is already refused on passthrough.

**Departures from §7's order:** step 0 (F72) done first, in the same
change. Step 1 (per-target ro stage) not needed: the class lock
serialises every bind of a class, so the one stage per source is never
contended. Step 5 (preStop "last member released") needed no worker
change: only the last member's unpublish writes the marker. Step 6
(broker grant shape) not needed, per §5 above. A dead shared mounter
(worker gone or source not answering) is REPLACED under the same class
by the next member's publish; its current members are stranded exactly
as a per-pod dead mounter strands its tenant, and their `MounterDead`
event already said so.

**The cache comes with it (added the same day, at the user's ask).** A
sharing CR that names no `spec.cache` gets Mountpoint's block cache by
default: three quarters of `workers.scratchSize`
(`MountSpec::with_default_cache`, 768 MiB at the chart's 1Gi), applied
at publish and logged; `cache: { enabled: false }` opts out and a named
`maxSizeMib` is kept. Three quarters, not all, because the scratch
emptyDir's `sizeLimit` evicts the worker when overrun, and under sharing
that strands every member. The default is part of the mounter's argv,
so it is in the class key like everything else. **WITHDRAWN 2026-09-30
(§11 step 1): measured on EC2 the same day, the default's PLACEMENT — the
scratch emptyDir on the node's root disk — made it a 5× regression for
any working set larger than the cache. A sharing CR that names no `cache`
now runs without one; `with_default_cache` and `default_shared_cache_mib`
are gone, and the placed default of §11 step 2 will be a different
function (`workers.cacheSizeMib` on a named device), not a fraction of
the scratch.**

**How to ask for one.** On the CR: `readOnly: true` (or the pod's SA in
`consumers.readOnlyServiceAccounts`) and `sharing: { readOnly: true }`,
with `identity.mode` broker (on an sts or static backend) or ambient.
Pods mount it as before (`chert.us/mount: <cr>`); those on one node with
the same effective uid/gid share the worker, which carries
`chert.us/shared-mount` naming the class. Helm applies `crds/` on
install only: after an upgrade, `kubectl apply -f
flint-passthrough-chart/crds/flintpassthroughmounts.yaml`.

**Not built:** ROX/PV shape (§9); per-member revocation; sharing across
namespaces or CRs.

**Follow-ups from the 2026-09-30 design review (built the same day).**
A shared mounter's `--memory-target` is two thirds of its memory limit,
and that limit is its own: `workers.sharedResources` (falls back to
`workers.resources`). An unreadable broker at publish is a retry
(`Sharing::Retry`), not a silent mounter of the pod's own; the backends
that cannot share raise `SharingUnavailable` on the tenant. A refused
refresh on a member leaves the class's `creds.json` alone
(`revocation_removes_the_key`): removing it cut the other members' reads
for up to a republish period and revoked nothing, since the refused pod
is bound to the same superblock. A shared create whose worker was kept at
the publish deadline, never launched, is resumed by the next publish
rather than replaced (`worker::alive`).

## 11. The cache is bounded by the disk under it — placement (follow-up, 2026-09-30)

**What was measured** (`s3csi/e2e/results/2026-09-30-ec2-campaign-3/`,
M1 and M2: trove `s3a`, i4i.large, real S3 through a VPC gateway
endpoint, the worker's `scratch` emptyDir on the node's 8 GiB gp3 root —
3000 IOPS, **125 MiB/s** — with the 435 GB NVMe instance store
unmounted, which is how trove hands the node over). Four parallel reads
of a set, cold then warm, three reps; the cache arm is this document's
default, 768 MiB.

| set | no cache, cold → warm | cache 768 MiB, cold → warm |
|---|---|---|
| 512 MiB (fits the cache) | 1.1 s → 0.95 s | 1.2–3.3 s → **0.38 s** |
| 6 GiB (8× the cache) | 10 s → 9.5 s | **48 s → 49 s** |

Uncached, one mounter reads S3 at ~600 MB/s on this node. The set that
fits warms 2.5× faster. The set that does not is **five times slower
with the cache than without, cold and warm alike**: Mountpoint writes
every fetched block into the cache directory on the read path, the
directory sits on a disk that takes 125 MiB/s, and 6 GiB at 125 MiB/s is
48 s — the measured number to the second. A warm read of a set larger
than the cache misses everything (the block cache evicts by recency, and
a scan larger than it evicts all of itself) and pays the disk a second
time. M2 says the same from the other side: four concurrent readers of
6 GiB through one shared mounter took the same 97 s at a 1Gi limit and
at 4Gi — memory was never the ceiling; the disk was. The 09-12 door
drill ran on the box's NVMe with a 60 GB cache and could not see any of
this.

**Why it is a placement problem, not a cache problem.** The cache is
worth what its hit rate pays minus what its misses cost, and the miss
cost is one block write to whatever device holds the directory. On host
NVMe that write is free against S3's ~600 MB/s; on an EBS root it is the
throughput cap. Kubernetes puts every emptyDir on the kubelet's root
filesystem, and on EC2 that is a gp3 root volume by default — trove's
nodes and EKS's managed node groups alike, 125 MiB/s unless the operator
paid for more. Instance-store nodes (i4i, m6id, c6id …) carry NVMe that
nothing mounts unless the bootstrap does. So on the platforms this driver
targets, **the default cache sits on the slowest disk on the machine**,
and it is a regression for every working set that does not fit in
768 MiB — which for a dataset "many pods read" (§1) is the common case.

`mounter.rs` says *never a hostPath* — AWS moved their cache off the
host for isolation, and a SHARED host directory would be a cross-tenant
read channel. That argument is against sharing a directory, not against
placing a private one: a per-worker subdirectory under an
operator-designated device, created 0700 for the worker's uid by the
plugin and removed with the worker, is exactly as isolated as the
emptyDir is today (root on the node reads both). Mountpoint offers no
write-behind or throttle for its cache, so nothing on the mounter side
can make an EBS-backed cache cheap; the fix is where the directory lives.

**What to build, in order.**

1. **Default off unless placed.** A sharing CR with no `spec.cache` gets
   the block cache only when the chart names a placement for it
   (`workers.cacheHostPath`, below) or the CR asks for one explicitly.
   On an emptyDir the default is off. This is one line in
   `with_default_cache`'s caller plus the values text, it removes the
   regression on every EBS-only node, and it costs the 2.5× warm win only
   for operators who neither placed the cache nor asked — which the
   values comment then tells them how to do. The class key is unchanged
   in shape (the argv still carries the cache flags when they apply).
   **DONE 2026-09-30** — removed rather than gated: with no placement
   knob yet the gate would be a constant, and the three-quarters-of-scratch
   sizing was the emptyDir's argument (§10), which step 2's placed cache
   does not share. `with_default_cache`, `default_shared_cache_mib` and
   their test are gone; the publish logs `sharing: no block cache` for a
   shared CR that named none; the CRD's `cache` and `sharing`
   descriptions, the values text and `MountSpec`'s field docs carry the
   measurement and the sizing rule (step 4's text landed with this);
   `mounter_args_for` has a test that a sharing CR without `cache` gets
   no cache flags and one with `cache` gets exactly its ceiling. S24
   asserts no `--cache` in the shared mounter's argv and no cache
   directory under its `/tmp` after the reads, and as its control
   recreates shared-c's class against the CR with a 256 MiB cache named
   and checks the flags and a populated cache directory (kind
   2026-10-01, with S30: 46 ok, 0 bad —
   `s3csi/e2e/results/2026-10-01-kind-s24-s30/`). The M2 fixtures name
   the 768 MiB cache they were measured with, so a re-run measures the
   same mount.
2. **A placement knob: `workers.cacheHostPath`.** When set (e.g.
   `/mnt/nvme/flint-s3-cache`), every worker that runs with a cache gets
   a `hostPath` volume at `<cacheHostPath>/<worker-name>` mounted at
   `/tmp` in place of the `scratch` emptyDir. The plugin creates the
   subdirectory before the create (0700, the worker's uid:gid — it
   already owns per-volume directories on the node) and removes it in
   `teardown_mounter` after the mounter's own exit (the F72 order), so a
   worker that dies leaves at most one directory the next publish of its
   class removes. `sizeLimit` does not apply to a hostPath, so the
   cache's own bound is `--max-cache-size` (`spec.cache.maxSizeMib`);
   the default when placed can be larger than three quarters of
   `scratchSize` — the eviction argument (§10) was about the emptyDir's
   `sizeLimit` killing the worker, which no longer applies — and
   `workers.cacheSizeMib` names it (suggest 4096). The device is the
   operator's: on trove nodes that means mounting `nvme1n1`, which is a
   trove change, not a chart one.
   **DONE 2026-09-30** — as written, with these particulars: the scratch
   volume is REPLACED (a hostPath named `scratch` at `/tmp`, type
   Directory), not added to, so the mounter's argv and the class key are
   untouched by placement; the plugin mounts the root itself as a
   type-Directory hostPath at the same path (a node without the device
   fails the plugin pod, `hostPath type check failed`, rather than
   kubelet creating the directory on the root disk) and refuses to start
   on a path that is not a directory; the chart refuses a relative path
   and a zero `cacheSizeMib` at render time; the workers' admission
   policy gains the root as its second and only other hostPath prefix
   (the API server refuses `..` in a hostPath, so a prefix check holds).
   `cacheDir` is on both records; `teardown_mounter` removes it after the
   pod, `cleanup` on a failed publish too; a directory a dead worker of
   the same name left is emptied before the next create
   (`prepare_cache_dir`), and `sweep_cache_root` at startup removes
   `s3w-*` entries no record names and nothing else. The placed default
   is `MountSpec::with_placed_default_cache(placed, cacheSizeMib)`, a
   different function from the removed three-quarters rule, logged as
   "block cache defaulted on the placed device". Kind leg S30 covers the
   mechanics — run 2026-10-01 on the box's kind cluster with images from
   `7b5a6da7`, 46 ok with S24, 0 bad
   (`s3csi/e2e/results/2026-10-01-kind-s24-s30/`); the speed is step 4's
   M1 on a node with the instance store mounted.
3. **Say where the cache is.** At publish, when a cache is on, the plugin
   logs the cache directory's device (`stat -f`/`statfs` of `/tmp` in
   the worker's spec is not visible to the plugin, but the emptyDir's
   host path under the kubelet root is, and so is its device) and emits
   `CacheOnRootDisk` on the tenant when that device is the node's root
   filesystem. A note, not a refusal: the operator who reads it knows
   what M1 says.
   **DONE 2026-10-01** — `note_cache_device` at the end of every
   successful publish (own mounter, shared creator and shared joiner;
   never a republish): `stat` of the cache directory — the placed
   `<root>/<worker>`, or the scratch emptyDir under the kubelet root
   (`worker::scratch_dir`) — against `stat` of the kubelet root, logged
   as `block cache device` with both `major:minor`; `CacheOnRootDisk`
   (Normal) on the tenant pod when they match, with the CR, the size,
   and the place: the emptyDir shape tells the operator to size for a set
   that fits or to place the cache, the placed shape says the knob points
   at the node's own disk and names the directory. "Root filesystem" is
   the kubelet root's device — what kubelet calls nodefs and where every
   emptyDir lives — which is what the plugin can see; the host's `/` is
   not mounted into it. S24 asserts the note on its emptyDir-cache control
   and its absence without a cache; S30 asserts by the devices (same disk
   on kind → the note names the directory; its own device → no note).
   Run on kind 2026-10-01: 52 ok, 0 bad with S24
   (`s3csi/e2e/results/2026-10-01-kind-s24-s30/legs-S24-S30-step3.log`).
4. **The sizing rule, in the values text and the CR's field doc**: enable
   a cache only for a working set that fits in it; a set larger than the
   cache pays every block to the disk twice and warms nothing. With the
   placement knob set that rule relaxes to "the device is at least as
   fast as the node's S3 path" — on NVMe, always.

**What not to do.** Raising `workers.scratchSize` on the root disk buys
a bigger cache on the same 125 MiB/s. `medium: Memory` for the scratch
moves the cache into the memory target the review just spent to keep
the mounter alive (§10, follow-ups). Express One Zone as a shared cache
tier is AWS's answer to the same problem at a different price and is
out of scope here.

**How to know it worked.** M1 again with the cache on a device faster
than S3: the 6 GiB cache arm must read within 10% of the no-cache arm
cold (the writes are free) and no slower warm; the 512 MiB arm keeps its
warm win. On the box that is `aws-measure.sh` against kind + RustFS with
the cache on the NVMe, which answers the mechanism; on EC2 it needs a
node with the instance store mounted. S24 gains one assertion: the
cache directory's device is the one the chart named. Until then the
truthful default is off.
