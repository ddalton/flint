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
so it is in the class key like everything else.

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
