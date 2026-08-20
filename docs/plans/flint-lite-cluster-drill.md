# flint-lite: the real-cluster drill

**Status: designed, not yet run. Nothing in the idle-lifecycle wave has ever
executed on real infrastructure.**

This plan covers the flint-lite hub (standalone NFSv4.1 + S3 cold tier), its
HTTP/REST surface, the FlintShare operator and its idle ladder, and
cross-cluster mounts — on real clusters against real S3.

Design method: fifteen agents read the actual source, proposed legs per
dimension, and then attacked their own proposals looking for legs that would
pass even if the product were broken. **Twenty-four of forty-one proposed legs
could have passed without looking.** Those were rewritten or cut; what survives
is below.

The reading found four defects before a single cluster was created. They are
fixed (`cab0791`) and each has become a leg here, because a fix that has only
ever been checked by its author is not yet evidence:

| # | Defect | Consequence |
|---|---|---|
| 1 | Both charts' NetworkPolicies rendered `from: []` | An empty `from` matches **all** sources. Enabling the policy without a client list published 2049 and the read-write file API to the whole cluster, while reading as protection. |
| 2 | Operator RBAC granted core `""` events | kube-runtime publishes `events.k8s.io/v1`. **Every** event — `AdoptionBlocked`, `ReclaimRefused`, `IdleSuspended`, `Woken` — was refused 403, silently, because publishing is best-effort by design. |
| 3 | Upload temp name keyed on the pid | One hub serves every request for a share, so two concurrent PUTs to one path shared a temp file, interleaved their bytes, and both renamed it over the target — reporting 201 to both callers. |
| 4 | `suspendWithSessions` documented inverted | The CRD said "set it" to refuse suspending under a live mount; the code arms the guard on `false`. Anyone following the docs got a share that suspends under their mount. |

---

## 1. What this drill is for

Three questions, in priority order:

1. **Does anything lose data?** Unavailability is a measurement. Data loss is a
   stop-the-line failure.
2. **Do the contracts hold on real infrastructure** — the REST status codes, the
   ladder's decisions, the epoch fence, `status.address` — when the network, the
   disk, the object store and the node all have their real properties?
3. **What do the numbers actually say?** Drain time at real S3 rates, wake
   latency, bytes across a cluster boundary, memory per request. Several
   published figures in this repo are estimates that have never been measured on
   this path.

### What it is not

Not a benchmark. Not a soak. It does not test the pNFS multi-DS profile, the
block layout, or the CSI driver — those have their own rigs. It does not test
upgrade or migration: flint-lite has no users yet, so version skew is out of
scope by explicit decision.

---

## 2. Before you start

### What trove can and cannot give us (checked, 2026-08-19)

Trove is the cluster tooling (`~/github/trove`). Its AWS path was read
for this plan; the findings change what is runnable and when.

**Satisfied by trove, no work needed:**

- **The CNI enforces NetworkPolicy.** Cilium 1.16.5 is the only CNI on
  AWS clusters, in stock enforcement mode, with WireGuard pod encryption
  on by default. So legs A2, D5 and E4 are live rather than `VOID` —
  record "Cilium 1.16.5" with the result.
- **Single-AZ pinning is enforced by refusal**, not by warning: a
  cluster that cannot be pinned to one AZ fails to create unless
  `allowCrossAz` is passed. That is the drill's cost rule, already
  implemented upstream.
- **Spot including the control plane** is the drive script's default,
  with a reclaim poller that cordons and drains.
- **An RWO StorageClass that works immediately**: `flint-nfs`, applied
  post-install, needs no disk-init. `flint-spdk` is the performance
  class and is gated on disk-init — leg D8 wants that one specifically.

**Phase C: unblocked by a trove patch (`f40588d`), NOT yet validated.**

The obstacle was never the network — it was kubectl. Trove's Mac-side
WireGuard device is a SINGLE shared utun, and `set_device_key` no-ops
when a device already exists, so a second concurrent cluster inherits
the first's Mac identity while its own nodes expect a different key.
B's nodes reject the Mac and kubectl for B never connects.

Node bootstrap never needed WireGuard at all — `admin.conf` and the join
command are read over **SSM** — so the dependency was only the operator's
own kubectl. `TROVE_AWS_PUBLIC_KUBECTL=1` now: adds the instance's
public IPv4 (from IMDS) to the API server's cert SANs, opens 6443 in the
cluster SG to this Mac's `/32`, and points the kubeconfig at the public
IP. No shared-utun coupling, so two clusters coexist.

**This patch compiles and unit-tests, but has never provisioned
anything. The first create is its test** — if cluster A does not come up
clean on it, fall back to the substitute rig below rather than debugging
trove on the drill's budget.

Know what the flag costs: it puts an API server on the internet —
`/32`-restricted, but on the internet — and the `rolesanywhere` identity
has no `ec2:RevokeSecurityGroupIngress`, so the rule cannot be narrowed
back and lives until the SG is deleted with the cluster. If this Mac's
egress IP changes mid-run, kubectl stops working.

**Still true regardless of the patch**, and it shapes Phase C:

- Both clusters get an identical hardcoded pod CIDR (`10.244.0.0/16`)
  and the kubeadm default service CIDR, and pod traffic is VXLAN, so pod
  networks never enter the VPC route table. **Cross-cluster pod-to-pod
  routing is impossible.** It is also not needed: the drill mounts
  through a NodePort on cluster A's node IP, which is unique per
  instance in the shared VPC.
- **No AWS cloud-controller-manager**, so `type: LoadBalancer` stays
  `Pending` forever. NodePort is the only shape, and
  `advertiseAddress` is set to `<A-node-private-ip>:<nodePort>`.
- The SG admits only its own group plus the Mac, so cluster B's nodes
  need a hand-added ingress rule on A's SG.

**Fallback if the patch misbehaves:** one cluster plus one or two plain
EC2 instances in the same VPC and AZ running a kernel NFS client,
reaching the hub the same way. That is still a genuinely foreign client
— own kernel, own NFS client identity, no shared API server — and it
covers every Phase C leg except C6, which needs a real kubelet. Say so
in the report: it would prove the mount path and the address contract,
not two Kubernetes control planes.

**Cut the partition with a Cilium policy, not an SG revoke** — no
`ec2:RevokeSecurityGroupIngress` means a widened SG cannot be narrowed
back. A NetworkPolicy on the hub denying the consumer's CIDR is
in-cluster, needs no AWS permission, and Cilium enforces it.

**Manual work trove will not do:**

- **S3 is entirely on the runner.** Trove has no S3 code, no IRSA, and
  the node profile carries SSM only. The `rolesanywhere` policy has zero
  `s3:` actions, so the bucket must be created out of band and the hub
  needs a **separately minted, long-lived, bucket-scoped IAM user key**
  — a rolesanywhere session key expires in about an hour and would wedge
  the hub mid-drill. `tests/cloud/lite-tier-l4.sh` already does exactly
  this; reuse it.
- **Installing flint-lite.** Trove knows only the CSI chart; the lite
  and operator charts are `helm install` by hand from OCI.
- **disk-init**, which is a standing gate before `flint-spdk` can bind.
- **Teardown.** There is no TTL, no budget and no auto-teardown — a
  forgotten cluster bills until someone deletes it.

**Two traps that silently produce wrong results:** `FLINT_CHART_VERSION`
is captured at backend startup, so restart the backend after the pin
bump and verify with `helm list -A`; and a component install failure
does **not** fail the create, so a cluster can report Ready with no
StorageClasses at all. Check `kubectl get sc` before trusting a green
create.

### Blocking prerequisites

- [x] **Publish.** Done: **v1.29.0** — `flint-driver`, `flint-pnfs` and
      `flint-lite-operator` at `1.29.0` (amd64+arm64), charts
      `flint-csi-driver` 1.29.0, `flint-lite` 0.4.0, `flint-lite-operator`
      0.2.0. Trove is pinned to it. **Restart the trove backend** or it
      deploys the previous chart.
- [ ] **Decide the routing shape** (see §4). On trove this is settled by
      elimination: NodePort plus a hand-added SG rule is the only shape
      available, because there is no CCM and the pod CIDRs collide.
- [ ] **A bucket in the same region as the clusters.** In-region S3↔EC2 is free
      both ways; out-of-region is billed per GiB and several legs move gigabytes.
- [x] **Confirm the CNI enforces NetworkPolicy.** Cilium 1.16.5, stock
      enforcement mode. Checked in trove's source; still record it with the
      results.

### Go / no-go gates

Stop and fix before continuing:

- **G1** — the cluster bring-up runbook completes and disk-init has run on every
  node. (Disk-init has been the standing gate on this rig more than once.)
- **G2** — leg A1 passes. It is pure calibration: it proves the instruments can
  tell a refusal from an absence. Every REST leg is void without it.
- **G3** — `kubectl get --raw /readyz` is `ok` immediately before any destructive
  leg. A dead API server makes never-observed oracles pass silently; that has
  happened on this rig.

---

## 3. Rules of engagement

**Every leg states a claim that could be false.** "Check that X works" is not a
leg. If you cannot say what observation would make it fail, do not run it.

**Every leg carries an anti-vacuity guard, and the guard must be independent of
the oracle.** A guard that checks the same thing as the oracle is decoration.
This project's recorded failures, all of which the guards below are shaped
against:

- A dead API server made never-observed oracles pass silently.
- `grep -q` under `set -o pipefail` gave SIGPIPE false negatives. **Use
  `grep -c` and compare a number.**
- A drill piped through `tail` reported `tail`'s exit status, not the drill's.
- An assertion on an absent value passes exactly as an assertion on a correct
  one. **Pair every absence check with a presence check at the same instant.**

**Cost is bytes, not hours.** Cross-AZ transfer is billed both ways and has
historically cost more than the instances on this rig. Pin hub and consumers to
one AZ unless the leg is specifically about crossing. Every leg below carries a
byte budget.

**Destructive legs run last, and only after their data is checksummed.** "It
looked fine" is not a restore proof.

**Spot reclaim is expected.** The fleet is pure-spot including the control
plane. A reclaim mid-leg voids that leg; it does not fail it. Record and re-run.

---

## 4. Topology and the routing decision

One hub serves one bucket prefix — that is enforced by the epoch lease, not by
convention, so "a hub per cluster" is not available. The hub lives in cluster A;
cluster B mounts across the boundary.

**Check first whether the VPCs are peered with routable pod networking.** If they
are, `advertiseAddress` points straight at a Service or pod address and the
entire internal-L4-endpoint layer disappears. This is the cheapest shape and the
one to prefer.

| Shape | `advertiseAddress` value | Cost | Notes |
|---|---|---|---|
| Peered VPC, routable pods | pod or ClusterIP `host:port` | none | Preferred. Pod IP churns across suspend/wake — leg C1 measures whether that matters. |
| Internal L4 endpoint, port-per-project | `hub-gw:30042` | LB hours | The design of record for production. |
| NodePort | `<node-ip>:<nodeport>` | none | Works, but the node is a single point of failure and the address is not stable across node loss. |
| Per-project LoadBalancer | LB ingress | LB hours × projects | Quota wall. Persists per CR, not per live hub. |

`ClusterIP` alone is **not** an option across the boundary, and NodePort is the
quiet trap: without `advertiseAddress` the operator derives the `.svc` DNS name
for it, which a foreign client cannot resolve. Leg C1 asserts exactly this.

### Cluster shape and what it costs

**One instance type everywhere: `i4i.large`, all spot, control plane
included.** Measured in us-west-1 at **$0.0408–0.0441/hr** against
$0.1148–0.1175 for `i4i.xlarge` — a 2.7x saving for a drill that is
functional, not a benchmark. It stays SPDK-eligible (trove matches on
the `i4i` family, not the size), and it keeps a local NVMe, which
`m5.large` does not. Using a single type also improves AZ pinning: the
chooser only accepts an AZ quoting **every** requested type.

| Role | Count | Type | $/hr |
|---|---|---|---|
| Cluster A control plane | 1 | i4i.large spot | ~0.044 |
| Cluster A workers | 2 | i4i.large spot | ~0.088 |
| Cluster B control plane | 1 | i4i.large spot | ~0.044 |
| Cluster B worker | 1 | i4i.large spot | ~0.044 |
| **Total** | **5** | | **~$0.22/hr** |

Cluster B exists only to mount, so one worker is enough. If the
public-kubectl patch misbehaves, the same two instances become plain EC2
consumers instead and the cost is unchanged.

At ~16 h of drill wall clock that is **≈ $3.50 of compute**. Transfer is
≈ $0: S3 is in-region (free both ways) and everything is pinned to one
AZ. Budget ~$5 all in, and the real financial risk is not the rate — it
is that **trove has no TTL or auto-teardown**, so a forgotten cluster
bills indefinitely.

**What `i4i.large` costs in fidelity, stated up front.** The standing
rule on this rig is to drop to a small instance for functional drills
but *never for a number anyone will quote*. Two vCPU and 16 GiB affect
some legs and not others:

- **Throughput-derived numbers become provisional.** B2 and D2 measure
  drain time at real S3 rates; on 2 vCPU with burstable network they
  will be pessimistic. **Do not compare them to the ~13.3 s/GiB figure
  measured on larger hardware** — report them as a floor ("120 s of
  grace was/was not enough on this shape"), which is the pass/fail the
  legs actually need.
- **Unaffected**: A7 (memory per request byte), C3 (bytes across the
  boundary), E-phase checksums, and every status-code and ordering
  oracle. These are the majority.
- **Set an explicit memory limit on the hub container** before A7, so
  the leg measures the buffering behaviour instead of OOM-killing the
  node.
- **Size the drill PVCs small** (~5 GiB). D8 deliberately fills one, and
  a 468 GB volume would make that leg slow and pointless.

If a quotable throughput number is ever wanted, re-run B2/D2 alone on
`i4i.xlarge` — one leg, one hour, about $0.12.

---

## 5. The legs

Run order is deliberate: calibration, then non-destructive contract legs, then
the ones that break things. Total ≈ 11 h of wall clock across two clusters, most
of it waiting on ladder timers.

### Phase A — the REST surface

The file API is served on the monitoring port only, **never on the
consumer-facing Service**, so every leg here runs from inside the cluster
against the pod IP.

| Leg | Claim | Time | Bytes |
|---|---|---|---|
| **A1** | Calibration: 401 means 401, not "the routes never existed" | 8 m | <1 MiB |
| **A2** | NetworkPolicy on an enforcing CNI closes 8080 and now fails **closed** | 15 m | ~0 |
| **A3** | The phase gate during a real DR import: 503 + `Retry-After: 5`, before auth | 25 m | ~20 MiB |
| **A4** | Cold download from real S3: three outcomes, three distinct codes | 30 m | ~2.1 GiB |
| **A5** | The download cap bounds the response, not the S3 egress | 25 m | ~2 GiB |
| **A6** | A genuinely full PVC answers 507, leaves no temp, does not wedge | 18 m | ~2.5 GiB local |
| **A7** | Peak RSS grows ≈ linearly with request size | 25 m | ~1.4 GiB local |
| **A8** | Two concurrent PUTs to one path do not corrupt the file | 10 m | ~256 MiB |
| **A9** | `If-Match` refuses a stale write, and the published loss ratio survives a real network | 20 m | ~100 MiB |

**A1 — calibration.** *Claim: an unauthenticated call to `/files` returns 401
specifically.* This is the whole dimension's instrument. With no bearer token
configured the six file routes are **never mounted** and the same call returns
404 — so "refused" and "never existed" are different codes, and a leg that
accepts "non-2xx" cannot tell them apart.
*Oracle:* an authenticated call returns 2xx **first** (proving the API exists),
then the unauthenticated call returns exactly 401.
*Anti-vacuity:* a second share deployed with no token at all, as a live
counterexample, must return 404 for the identical request. Plus: port 8080 is
LISTENING per `/proc/net/tcp` — a check independent of HTTP entirely, since a
bind failure is logged and swallowed while NFS keeps running.

**A2 — NetworkPolicy.** *Claim: with the policy enabled and no client lists
configured, nothing reaches 2049 or 8080 except the operator.* This is defect #1
re-tested after the fix. Before it, the 2049 rule rendered an empty `from` and
admitted everyone.
*Oracle:* from a probe pod, 8080 refused and 2049 refused; from the operator's
pod, 8080 answers. Then set `nfsClientCIDRs` and assert 2049 opens for exactly
that CIDR.
*Anti-vacuity:* three-point before/during/after with the same probe pod, and the
hub's `HubReachable` condition must stay True throughout with a
`lastTransitionTime` after the policy was applied — proving the operator's own
path survived while the probe's did not. **Void if the CNI does not enforce.**

**A3 — the phase gate.** *Claim: during a real DR import every file route
answers 503 with `Retry-After: 5` and a body naming the phase — not 404, not
500, and not 401 even without a token.*
*Oracle:* one uninterrupted poller run must yield **both** a 503 (while
`/status` reports a pre-serving phase) and a later 404 (while it reports
`serving`), with zero 500s.
*Anti-vacuity:* that both-codes-in-one-run requirement is the guard — a late
start sees only 404s, a dead pod only connection errors, and either fails.

**A4 — cold download.** *Claim: a GET of an evicted file returns exactly one of
three distinguishable answers: 200 with a byte-identical body; 503 with
`Retry-After: 2`; or 409 if the size changed under the read.* Note `Retry-After`
is 2 for hydration and 5 for grace — the two are distinguishable and the drill
should confirm it.
*Anti-vacuity:* `stat -c %s` reports 0 bytes on disk while the listing reports
the logical gigabyte — un-fakeable proof of coldness, and not the thing the
oracle measures. The hydration meters must also move, so a 200 served from a
warm file fails.

**A5 — the cap bounds the response, not the egress.** *Claim: a 413 costs no S3
egress, but an in-cap Range request against a cold file pulls the whole object.*
The 413 message says "use Range to fetch it in pieces"; whether that actually
saves egress is the question.
*Anti-vacuity:* the 413 arm must show hydration meters provably **unchanged** —
that control is what makes the Range arm's ~1 GiB delta attributable rather than
background noise.

**A6 — a full PVC.** *Claim: 507, no surviving `.flint-upload.*` temp, and the
hub still serves afterwards.*
*Anti-vacuity:* the 201 → 507 → delete → 201 sandwich. Three identical requests
where the only variable is headroom: a hub that 507s everything fails, and so
does a hub that 507s nothing.

**A7 — memory per request.** *Claim: the download body is fully buffered before a
status code is chosen, so peak RSS grows ≈ linearly with the request — which
would make the 5 GiB default cap unservable under any sane memory limit.* The
module's own doc comment describes a streaming body; this leg decides which is
true.
*Oracle:* anonymous memory from `memory.stat`, cross-checked against `VmHWM`,
with page cache reported separately.
*Anti-vacuity:* a 1 MiB Range against the same warm file. A buffering
implementation allocates ~1 MiB while page cache still holds ~1 GiB — if
anonymous memory tracks the range and not the file, buffering is confirmed and
page cache is excluded as the explanation.

**A8 — concurrent uploads.** *Claim: N simultaneous PUTs to one path leave the
file byte-identical to exactly one of the bodies, never a mixture.* This is
defect #3 re-tested on real infrastructure.
*Oracle:* 8 concurrent PUTs of distinct 32 MiB bodies to one path; the final
md5 must equal one of the eight exactly.
*Anti-vacuity:* the eight bodies have distinct md5s recorded up front, and the
result must match one of them — "differs from body 1" would also be satisfied by
corruption.

**A9 — conditional writes, and the number we published about them.** *Claim, in
two parts:* (i) a write carrying a stale `If-Match` is refused with 412 and
changes nothing, while the holder of the current tag succeeds; (ii) the loss
ratio the front-door contract publishes — measured in-process at **32-66 lost of
200 with `If-Match` against 168-174 without**, across repeated runs — holds no
worse over a real network.

Part (ii) is the one worth the cluster time, because it is a **falsifiable
prediction, not a re-run**. The VERIFY→RENAME gap that loses updates is
server-internal, so a client's round trip does not widen it; a longer client
cycle instead raises the chance of a *412*, which is the guard working. Real
infrastructure should therefore lose **less** than the in-process figure, which
came from maximum contention against an in-process dispatcher with no network at
all. If it loses MORE, the model of where the window lives is wrong and the
number in `docs/flint-lite-operator.md` is wrong in the dangerous direction — a
front door would have been told the guard is stronger than it is.

Note the in-process spread is load dependent and the worst case came from the
LEAST loaded machine, so "the cluster is busier" is not a reason to expect a
better number. Run both arms back to back on the same share, not on different
days.

*Oracle:* two arms, same writers and rounds, run back to back against one share:
8 concurrent clients × 25 rounds of read-modify-write (append one byte),
unconditional against one path and `If-Match` + re-read-on-412 against another.
Record surviving bytes for each. Part (i) is asserted directly inside the second
arm: capture a tag, let another client write, then replay the captured tag and
require 412 **and** a file whose bytes are unchanged by the refusal.

*Anti-vacuity:* three guards, none of which is the loss oracle.
1. **The conditional arm must record a non-zero 412 count.** Writers that never
   collide "pass" with zero loss while proving nothing — this is the leg's
   vacuity mode, and it is the same one that made the in-process leg's first
   version worthless.
2. **The unconditional control must lose a substantial fraction.** If the
   control loses nothing, the storm did not race on this hardware and both arms
   are measuring an idle system.
3. **The tag must provably rotate:** at least three distinct `ETag` values
   observed across the run. A static tag means the instrument is reading a
   cache, not the file — the same guard the epoch-cell legs use.
Plus the A1 discipline: an authenticated 200 on `/files/content` **first**, so a
404 from an unmounted file API can never be counted as "no loss".

*Also worth capturing while the rig is up, since it has never been seen against
real S3:* on a tiered share the tag moves when a file is evicted and again when
it is hydrated, because both rewrite the local inode and the tag derives from
its change attribute. Documented as fails-closed (a spurious 412, never a lost
edit). Confirm it against a real bucket rather than a stub — and confirm that a
GET across the same boundary still returns the right bytes, because a validator
that drifts is an annoyance while a body that drifts is corruption.

### Phase B — the idle ladder and the front door

| Leg | Claim | Time | Bytes |
|---|---|---|---|
| **B1** | The two-signal AND is a real conjunction; `/status` never postpones, one file-API call does | 14 m | <1 MiB |
| **B2** | Is 120 s of grace enough to flush at real-S3 rates? | 28 m | ~2 GiB |
| **B3** | Hibernate defers loudly and names which conjunct failed | 20 m | <100 MiB |
| **B4** | Verify-then-delete is ordered: PVC goes only after the bucket says released | 12 m | ~0 |
| **B5** | Waking a hibernated share is a real DR import: fresh PVC, new `serverId` | 12 m | ~2 MiB |
| **B6** | `wake-intent: warm` changes the boot, measurably | 24 m | ~2 GiB |
| **B7** | A suspend→wake round trip costs a pod start, not an epoch lease | 7 m | ~0 |
| **B8** | Operator events actually land | 8 m | ~0 |
| **B9** | The front door, run **as** the front door | 15 m | ~0 |

**B1 — the two-signal AND.** *Claim: suspend requires both a stale front-door
heartbeat and a quiet hub clock; either alone holds the share awake; the
operator's own `/status` polling does not postpone; one authenticated file-API
call does.*
*Oracle:* run the liveness arm **first** — the share must actually reach
`IdleSuspended` with replicas 0 and the PVC present. If it does not, the whole
leg is inconclusive and the other arms are meaningless.
*Anti-vacuity:* the `/status` poller must prove it reached the hub by counting
HTTP 200s; zero 200s voids the arm. The browse arm's counter increment is what
makes the `/status` arm's "unchanged" a statement about `/status` rather than
about a poller that never ran.

**B2 — the grace budget.** *Claim: 120 s is enough to drain, flush, write the
manifest barrier and release the epoch for a realistic dirty set; and when it is
not, the hub is SIGKILLed and leaves the cell **held**, costing the next wake a
full lease wait.* This is the open risk from the L4 measurements (~13.3 s/GiB
publish against real S3) and nothing has ever measured it here.
*Oracle:* after `kubectl delete pod`, the cell reads `released: true`, the log
carries the clean-shutdown line, and the **successor's own** takeover counter is
0 — not "unchanged".
*Anti-vacuity:* assert `dirtyFiles >= 1` from a successful `/status` read
immediately before each SIGTERM, or the drain measured nothing. Plus the
ETag-rotation baseline, which proves the cell is being observed at all.

**B3 — hibernate defers loudly.** *Claim: the operator refuses to reclaim the
disk unless the hub reports `rpoClean` **and** `epoch.held`, names which
conjunct failed, and never deletes a PVC it could not verify.*
*Anti-vacuity:* a baseline arm first — with the bucket healthy, observe
`rpo.clean == true` and `manifestCurrent == true` on the same hub. Without it,
"manifestCurrent is false" carries no information.

**B4 — ordering.** *Claim: the operator records `Hibernated`, lets the pod fully
drain, and deletes the PVC only on a later reconcile — by which time the bucket
itself says `released: true`.*
*Anti-vacuity:* the bucket-side oracle is the point. The operator holds no bucket
credentials, so a Kubernetes-side bug cannot satisfy it.

**B5 — hibernate wake is a DR import.** *Claim: one annotation rebuilds the
volume from the bucket alone — new PVC, **new `serverId`**, complete tree, and
near-zero content bytes because regular files come back as stubs.*
*Anti-vacuity:* the pre-hibernate listing must be non-empty with its regular-file
count recorded, and both listings must report `truncated == false`. An inventory
diff over two empty listings is a pass that proves nothing.

**B6 — `wake-intent: warm`.** *Claim: it overrides `hydrateWarmAfterImport` for
exactly one boot, causes a real bulk restore, and measurably lowers
time-to-first-byte on a file that would otherwise be cold.*
*Anti-vacuity:* the cold arm must observe at least one hydration DELAY, and the
warm arm must show the warm-fill report restoring at least the file count with
zero still-cold — read **before** the timed read, so "no DELAY" is evidence of
warming rather than of a read that never happened.

**B7 — suspend→wake latency.** *Claim: waking a PVC-retained share is a pod start
plus an instant epoch claim via self-recognition, not a lease wait.*
*Anti-vacuity:* produce the slow arm on the same bucket in the same session —
kill uncleanly **and delete the PVC**, so the successor gets a new server id,
falls through to the foreign-holder path, and waits the full lease. Two arms,
same instrument, opposite results.

**B8 — events land.** *Claim: an operator event is actually readable via
`kubectl describe`.* Defect #2 re-tested. Every event was 403 before the fix, and
the operator's own documentation tells the front door to read them.
*Oracle:* trigger `ReclaimRefused` (adopt a claim, set `reclaim: Delete`, delete
the share) and read the event back.
*Anti-vacuity:* count events on the object before and after; the count must
increase. Absence of an event and absence of the object are otherwise identical.

**B9 — the front door as the front door.** *Claim: the `frontDoor` ServiceAccount
can run the whole ensure-live loop and nothing more.* Every other leg here runs
as cluster-admin, so the capability boundary is never actually exercised.
*Oracle:* with a token for that SA only: create by deterministic name → 201;
patch annotations → 200; read `status.address` → 200; read the operator's Lease
→ 200. And **delete the share → 403**; get a Secret → 403; list pods → 403.
*Anti-vacuity:* the allowed calls must succeed in the same session as the denied
ones — a bad kubeconfig denies everything and would otherwise "pass".

### Phase C — cross-cluster (two clusters, via the public-kubectl patch)

Two trove clusters, same VPC and AZ, both created with
`TROVE_AWS_PUBLIC_KUBECTL=1`. Cluster B mounts cluster A's hub through a
NodePort on A's node private IP, with `advertiseAddress` set to
`<A-node-private-ip>:<nodePort>` and a hand-added ingress rule on A's SG
admitting B's. Pod-to-pod routing across the boundary is impossible
(identical pod CIDRs, VXLAN) and is not used. If the patch misbehaves,
run these against the EC2 substitute rig instead and label the report
accordingly — see §2.

| Leg | Claim | Time | Bytes |
|---|---|---|---|
| **C1** | `advertiseAddress` is the only routable answer | 35 m | ~8 MiB |
| **C2** | `nconnect>=2` really establishes trunks across the boundary | 25 m | ~1.5 GiB |
| **C3** | Boundary bytes scale with **nodes**, not agents | 15 m | ~512 MiB |
| **C4** | Under partition, does the lease count actually decay? | 25 m | <5 MiB |
| **C5** | A large write across the boundary completes | 15 m | 64 MiB |
| **C6** | Suspending under a kubelet-driven cross-cluster consumer | 25 m | ~0 |
| **C7** | A hard mount survives suspend→wake and its blocked I/O resumes | 20 m | ~64 MiB |

**C1 — the routable address.** *Claim: a consumer in cluster B given only
`status.address` can mount; and the address the operator would have derived
without `advertiseAddress` is genuinely unusable from B.*
*Anti-vacuity:* the security-group probe must emit a literal `RC=` line **both**
times — a pod that never ran is a void leg, not a refusal. And the derived-name
mount must fail with NXDOMAIN specifically, paired with the advertised mount
succeeding in the same run.

**C2 — trunking.** *Claim: `nconnect=4` opens more than one established flow to
the hub.* The kernel refuses trunks silently, so this must be **counted**.
*Oracle:* with a read in flight, the hub shows ≥2 ESTABLISHED rows on 2049 from
cluster B, and `findmnt` confirms `nconnect=4` is on the mount.
*Anti-vacuity:* an `nconnect=1` mount from a second node, on its own PV, must
yield exactly 1. Read both `/proc/net/tcp` and `tcp6`.

**C3 — read amplification.** *Claim: two agents on the same node cost the
boundary one copy; a third on a different node costs a second.* This is the
number the cross-cluster cost model rests on and it has never been measured.
*Anti-vacuity:* three identical md5s, plus `MemAvailable` on the node exceeding
3× the file size at both ends of the same-node re-read — otherwise "the page
cache did not hold it" is indistinguishable from "the page cache does not work
per node", which is the whole claim.

**C4 — lease decay under partition.** *Claim: the documented residual — "leases
expire, so a long enough partition drops the count to zero" — is **false** when
the partitioned client is the only client, because the only reaper runs on
inbound traffic.* If true, a partitioned agent fleet pins its share awake
forever. That inverts the mitigation advice now in the docs.
*Anti-vacuity:* prove the cut on the flow under test — a hung `stat` on the
client **and** the peer's ESTABLISHED rows gone from the hub. "A new connection
is refused" is not evidence about an existing one.

**C5 — large writes.** *Claim: a 64 MiB write across the boundary completes.*
Small reads can work while large writes hang when the path is double-encapsulated
and PMTU discovery is broken — the "browsing works, saving is broken" shape at
the network layer rather than the grace-window layer.
*Anti-vacuity:* a tiny write first, an in-cluster 64 MiB control, and read-back
verification on loopback. On hang, observe D-state processes on the node
out-of-band rather than inferring from a killed `kubectl`.

**C6 — suspend under a foreign consumer.** *Claim: suspending a share whose
consumers are in another cluster does or does not wedge them.* The recorded scar
says unmount before arming the ladder — but that was learned on a Lima kernel,
not a kubelet-managed mount.
*Oracle:* defined by the **node**, not the API: the consumer pod object gone,
no mount in `findmnt`, the kubelet volume directory gone, and no `umount` process
in D across five samples.
*Anti-vacuity:* assert the mount is present in the node's table **before** the
suspend, so its absence afterwards is a state change rather than a state.

**C7 — wake from behind the boundary.** *Claim: a hard mount from cluster B
survives a full suspend→wake cycle and its blocked I/O resumes without remount.*
The adversary rated this the single most important cross-cluster leg, and no
proposal covered it: it is the everyday path for an agent fleet.
*Oracle:* start a large read from B, suspend mid-read, confirm the client blocks
(not errors), wake, and confirm the read completes with a matching md5.
*Anti-vacuity:* an identical read that is **not** interrupted, in the same
session, establishing the baseline duration and md5.

### Phase D — failure injection

| Leg | Claim | Time | Bytes |
|---|---|---|---|
| **D1** | Node loss: loss bounded exactly by the published RPO | 45 m | ~500 MiB |
| **D2** | 120 s grace at real rates: does the released mark ever lie? | 40 m | ≤12 GiB |
| **D3** | Wake racing drain: the flock is the fence | 35 m | ~4 GiB |
| **D4** | S3 revoked mid-flight: self-fence, exit 70, client never errors | 30 m | ~200 MiB |
| **D5** | Operator cannot poll: share pins awake, PVC never deleted | 25 m | <10 MiB |
| **D6** | Losing the leader's node: nothing wakes; the standby takes over | 35 m | ~0 |
| **D7** | Partition: a partitioned client is invisible and pins the share | 60 m | ≤100 MiB |
| **D8** | Disk-full on a real CSI volume: ENOSPC before the filesystem | 40 m | ~8 GiB |
| **D9** | Unclean death re-claims by self-recognition, no lease wait | 25 m | ~2 GiB |

**D1 — node loss.** *Claim: terminating the hub's node loses nothing that
`/status` reported as `rpoClean: true`.*
*Anti-vacuity:* three distinct epoch-cell ETags across the run. Every renewal is
a conditional PUT carrying a fresh salt, so the ETag provably rotates — a static
ETag means the instrument is reading a cache, not the cell.

**D3 — wake racing drain.** *Claim: a second hub process on the same PVC blocks
on the flock and never serves.* The ReplicaSet does not count a terminating pod
as active, so `replicas 0→1` creates the second pod immediately. This is the one
split-brain the epoch explicitly cannot fence, because both processes share a
server id.
*Oracle:* a census with **zero** samples showing two pod IPs both accepting 2049,
and ≥20 samples where exactly one did — proving the census was alive during the
window.
*Anti-vacuity:* run a second `flint-pnfs-mds` in the pod by hand and require
`grep -c 'refusing to start a second writer'` to equal 1, capturing the exit
status so it survives the pipeline.

**D5 — the operator cannot poll.** *Claim: blocking operator→hub:8080 stops an
armed share from suspending, reported as `HubReachable=False/PollFailed`, and
**never** deletes its PVC.* An unreachable hub is an unknown hub, never an idle
one.
*Anti-vacuity:* two bracketing suspends of the same share with the same knobs,
before and after the injection — a genuine positive control. Plus the condition's
`lastTransitionTime` must have moved, since conditions deliberately preserve
their timestamp when the status is unchanged.

**D6 — leader node loss.** *Claim: with no leader holding the lease, nothing
wakes however the annotation is stamped; and when the leader dies with its node,
the standby takes over within the lease.* This is the reason the second replica
exists.
*Anti-vacuity:* structural, and the best in the set — **one** stimulus (annotate
`requested-at`) must produce **opposite** outcomes in the two halves. A typo'd
key or a wrong namespace fails both halves and cannot be mistaken for a pass.

**D7 — partition.** Same claim as C4 from the operator's side. Keep the payload
under 100 MiB; do not run a throughput leg over this path.
*Anti-vacuity:* the same instrument that read "1 lease, forever" must read 0 the
moment a compound drives the sweep. Without that, a null result means nothing.

**D8 — disk-full on a real CSI volume.** *Claim: the tier's space admission
delivers ENOSPC to the client while the filesystem still has ≥ the reserve free
— the hub never reaches hard-full.*
*Anti-vacuity:* `df` captured **inside** the failing iteration must show
meaningful free space. "The write failed with ENOSPC" is equally consistent with
a hard-full filesystem, which is the exact failure the reserve exists to prevent.

### Phase E — data safety (destructive; run last)

| Leg | Claim | Time | Bytes |
|---|---|---|---|
| **E1** | Reclaim Retain / Delete / adopted, decided by **bytes** | 30 m | ~192 MiB local |
| **E2** | Hibernate destroys the PVC only after a verified flush; bytes return identical | 40 m | ~256 MiB |
| **E3** | Hibernate refuses while the RPO predicate says no, completes when it says yes | 30 m | ~16 MiB |
| **E4** | The operator never deletes a PVC it could not ask about | 25 m | <10 MiB |
| **E5** | Two hubs on one prefix: the loser never serves | 35 m | ~64 MiB |
| **E6** | The nested-prefix hazard, demonstrated | 40 m | ~14 MiB |
| **E7** | Symlink confinement with the tier on | 15 m | ~1 MiB |
| **E8** | Deleting a hibernated share does not touch the bucket | 10 m | ~0 |

**E1 — reclaim, by bytes.** *Claim: Retain leaves data byte-identical after a
full detach and re-adopt; Delete removes the operator's own claim; and an
**adopted** claim survives `reclaim: Delete`.* The third is defect-adjacent — it
was fixed this wave and has only been checked in kind.
*Anti-vacuity:* a deliberately planted md5 mismatch must be caught by the same
comparison, with `grep -c FAILED` equal to 1. Pair every absence assertion with a
presence assertion at the same instant.

**E2 — hibernate round trip.** *Claim: the full rung ends with a corpus
byte-identical to what went in, restored from the bucket alone.* This is the one
operation where the operator deletes the only local copy.
*Oracle:* order proven by timestamps both sides provide — the epoch object's
`LastModified` when `released` first reads true, against the PVC's own deletion
timestamp.
*Anti-vacuity:* the new PVC's UID must differ from the old one; a hibernate that
silently kept the volume would otherwise pass.

**E5 — two hubs, one prefix.** *Claim: while A holds the lease, B never binds
2049; and when A dies uncleanly B takes over after the full lease.*
*Anti-vacuity:* the same probe must succeed against A while failing against B,
and the takeover must be read as a **counter**, not a log grep.

**E6 — the nested-prefix hazard.** *Claim, in two parts:* (i) inside the
operator's watch scope, a share on `p/sub/` against an existing share on `p/` is
refused **before** any hub starts; (ii) outside it — two clusters, or the epoch
layer alone — `p/` and `p/sub/` mint **different** epoch objects, never contend,
and silently overwrite each other's bytes.

Part (ii) is a **demonstration of a known unguarded hazard**, not a bug hunt. It
is here because the mitigation lives in a control-plane database outside this
repo, and a constraint that costs someone a migration deserves evidence rather
than an assertion. Run it on a throwaway prefix.
*Anti-vacuity:* the surviving object must match hub B's hash **exactly**, not
merely differ from A's — "differs from A" is also satisfied by corruption.

**E7 — symlink confinement.** *Claim: no traversal through a symlinked directory
component, and no symlink ever carries its target's bytes into the bucket.* The
export root and the state database are siblings on one PVC — the geometry that
made the shipped symlink hole a credential-theft hole.
*Anti-vacuity:* an authenticated 200 on `/files` before any refusal is asserted.
With no token the routes are not registered and warp 404s, which would read as a
refusal.

**E8 — deletion does not touch the bucket.** *Claim: deleting a hibernated share
removes Kubernetes objects and leaves every S3 object intact.* The code comment
says the bucket is never touched; nothing tests it, and for a hibernated share
the bucket is the only copy.
*Oracle:* full object inventory under the prefix before and after; identical.
*Anti-vacuity:* the inventory must be non-empty and its count recorded first.

---

## 6. What this drill does not cover

Stated so the gaps are chosen rather than discovered:

- **The 3000-CR / 300-live fleet budget.** B1's design named that target and no
  leg asserts it. Doing it properly needs a fleet-sized cluster; doing it on a
  laptop-scale cluster produces a number that means nothing. The cheap partial
  mitigation — the pod-list selector on `poll_hub` — has already landed.
- **Sustained client-identity collision.** One `EXCHANGE_ID` exchange is proven
  (two same-hostname clients do not share OPEN state; the server logs case 5).
  A crash-looping same-named pod evicting its twin forever is not.
- **Multi-volume hubs and the satellite role.** Design-only, no code.
- **Upgrade and version skew.** Out of scope by decision — no users yet.
- **Anything on the pNFS multi-DS or block-layout profiles.**

---

## 7. Reporting

One row per leg: `PASS` / `FAIL` / `VOID` (instrument failed, e.g. spot reclaim
or a non-enforcing CNI) / `INCONCLUSIVE` (the leg ran but its precondition arm
did not hold). **`VOID` and `INCONCLUSIVE` are not `PASS`** — the whole point of
the anti-vacuity guards is to make that distinction possible, so record it.

Capture for every leg: the oracle's raw observation, the anti-vacuity guard's
observation, wall-clock duration, and bytes moved. Numbers worth carrying
forward regardless of pass/fail: drain time at real S3 rates (B2, D2), wake
latency decomposition (B7), boundary bytes per node (C3), and peak RSS per
request byte (A7).
