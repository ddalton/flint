# flint-lite cluster drill — results

Rig: two trove clusters, us-west-1, all spot, i4i.large everywhere.
Cluster A `runbt` (project 114): 1 CP + 2 workers, k8s v1.34.10, kernel
6.18.41, Cilium 1.16.5, flint-csi-driver-chart 1.30.0, both workers
disk-inited (468 GB data NVMe each).
Cluster B `runbu` (project 115): 1 CP + 1 worker.
Bucket `flint-lite-drill-20260820` (us-west-1, versioned), hub key
bucket-scoped. Hub image `dilipdalton/flint-pnfs:1.30.0`, operator
`dilipdalton/flint-lite-operator:1.30.0`, CRD schema version 2.
Shares: `workspaces/tenant-a` (5Gi, flint-spdk, file API on) and
`workspaces/tenant-notoken` (2Gi, file API on but NO token).

| Leg | Verdict | Observation |
|---|---|---|
| A1 | **PASS** | authed `/files` 200 first, unauthenticated exactly 401. Anti-vacuity: the token-less share answers 404 to *both* authed and unauthed while its `/status` is 200 and `/proc/net/tcp` shows 8080 in LISTEN — so 404 is provably route registration, not a dead port. **G2 satisfied.** |
| A8 | **PASS** | 8 concurrent PUTs of distinct 32 MiB bodies to one path: all 201, result exactly 33554432 bytes, md5 `92c74f96…` = body 7 exactly (matches 1 of 8). Anti-vacuity: 8 distinct md5s recorded up front. Shipped defect #3 confirmed fixed on real infrastructure. |
| A9 (i) | **PASS** | Stale `If-Match` → 412; file byte-identical across the refusal (`SECOND-WRITER` before and after); the current tag then → 201 and lands. |
| A9 (ii) | **PASS, prediction REFUTED in magnitude** | 8 writers × 25 rounds. Quiet: control lost 156/200, guarded lost **5**/200 → residual **3.2%** (31x). Loaded (3 spinners on the hub's 2-vCPU node, loadavg 7.7, arm 7s→52s): control 159/200, guarded **14**/200 → residual **8.8%** (11x). Direction of the published prediction CONFIRMED — loss tracks hub CPU, 2.8x worse under load. Magnitude REFUTED — both are ~5x below the contract's 16%/50% range. Cause is sound: over HTTP the client's read-modify-write cycle carries a network RTT (~ms) so the server-internal VERIFY→RENAME gap (~µs) is a far smaller fraction of it than in-process, where the whole cycle is µs. **The published range is a worst case real clients do not see.** Anti-vacuity: 570/608 412s recorded, control lost ~78%, 82–196 distinct ETags, zero non-`x` bytes (no tearing), no writer exhausted its retries. |
| A4 | **PASS — with a finding** | Cold GET of an evicted file returns **409** `{"error":"file changed while being read; retry the request"}` while hydration completes 33554432 bytes from real S3 behind it; the retry returns **200 byte-identical** (md5 `92c74f96…`, = A8's winner). Reproducible on a second independent evicted file: 409 → 200 → 200. **The 503+`Retry-After: 2` arm never fires in-region** — hydration is fast enough that the read-window guard trips first, so 409-then-retry is the *normal* cold-read path, not the exceptional one. A front door that only handles 503 will show an error on every first cold open. Anti-vacuity: `stat` 0 bytes on disk vs listing 33554432 at the same instant; hydration meters moved 0→1 started/completed, 33554432 bytes. |
| A6 | **PASS** | Sandwich 201 (1 MiB) → **507** `{"error":"NoSpc"}` (1.5 GiB streamed into ~1.0 GiB free) → free space → 201. `nospcWriteRefusals=1`, `a6-huge.bin` absent afterwards, zero surviving `.flint-upload*` temps, hub still serving. Anti-vacuity: all three arms are the same request differing only in headroom. **Number for D8:** df read 44 MiB free (0.9%) at the moment of refusal — admission fired before hard-full, but the margin is thin; space returned to 1.1 GiB once the temp was reaped. |
| A5 | **PASS — claim confirmed** | Cap set to 1 MiB. Whole-file GET of a 512 MiB **cold** file → **413** ("use Range to fetch it in pieces") with hydration meters provably **unchanged** (0/0/0) and the file still 0 bytes on disk — a 413 costs no S3 egress. Then a **1 KiB Range** request against a second cold 512 MiB file pulled **536870912 bytes** (hydrationBytes, file fully resident afterwards). **The cap bounds the response, not the egress** — the 413's own advice saves response size but not one byte of transfer on the first Range. Anti-vacuity: the 413 arm's flat meters are what make the Range arm's 512 MiB delta attributable. |
| A2 | **PASS — defect #1 fixed, plus a finding** | Three-point with one probe pod on Cilium 1.16.5. Before: 2049 exit=52, 8080 exit=0 (both reachable). Policy on with NO client lists: both exit=28 (dropped). **The rendered policy contains exactly one ingress rule — 8080, with a real `from` naming the operator — and NO 2049 rule at all.** That is the fix: a peerless rule is now omitted rather than emitted as `from: []` (which matched everything). Operator path survived throughout (share stayed Ready/Serving). **Finding: `nfsClientCIDRs` is inert for in-cluster pod clients.** An `ipBlock` naming the probe's exact pod IP `/32` never opened 2049 (still exit=28 after 65s), while `nfsClientSelectors` with `podSelector app=probe` opened it immediately (exit=52) and left an unlabelled `probe2` refused (exit=28). Cilium evaluates CIDR rules against entities outside the cluster; pod-to-pod carries a security identity. Fails closed, but silently. Node-CIDR case (the knob's documented use) deferred to Phase C. |
| A7 | **PASS — claim confirmed, and it is a DoS vector** | Download bodies are fully buffered. 1 MiB Range: `VmHWM` 27.5→30.5 MB (+3 MB). Whole 512 MiB file: `VmHWM` → **541 MiB**, cgroup `memory.peak` pinned to the 1 GiB limit. Anonymous memory tracks the **request**, not the file — page cache was separately visible at 499 MiB, excluding it as the explanation (the designed anti-vacuity discriminator). **Consequence, tested directly:** with the limit at 256Mi the same 512 MiB GET produced `reason: OOMKilled`, exitCode 137, restartCount 1, and the client saw `000` / 0 bytes — an empty reply, not an error. The whole hub process dies, taking the NFS export down with it. **The default `maxDownloadBytes` of 5 GiB therefore requires ~5 GiB of memory headroom, or one authenticated browse click kills the share.** |
| A3 | **PASS** | One uninterrupted run across a real DR import (PVC destroyed, bucket the only copy). 64 samples of **503 with `Retry-After: 5`** while `/status` reported `claimingEpoch`, then a single clean transition to **200** in the very sample `/status` first reported `serving`. The unauthenticated request moved **503 → 401** at the same instant, proving the gate answers **before** auth and that auth resumes afterwards. Zero non-503 5xx. `/status` itself answered 200 throughout the pre-serving window — it binds pre-listener as designed. Anti-vacuity: both codes required in one run; a late start would have seen only 200s. |
| B5 | **PASS** (observed during A3 setup) | Destroying the PVC and reactivating rebuilt the tree **from the bucket alone**: 18 entries / 4.13 GiB logical, **all 18 stubs** (65 MB used on a fresh 4.9 GiB PVC), `importStubs=18`, **new PVC UID**, **new `serverId`** (7165963609093479857 → 8882991393233367173), epoch held. Anti-vacuity: pre-destroy listing non-empty with its file count recorded; both listings `truncated=false`. |
| B8 | **PASS — defect #2 fixed** | FlintShare-kind events went 0 → 1 on the trigger (a second share on the same bucket subtree). `kubectl describe flintshare` — the read path the operator docs give the front door — shows `Warning / Conflict` with the full arbitration message naming the winner. Anti-vacuity: count taken before and after; it increased. The loser sat at `phase=Failed`, `Conflict=True`, and created **zero** Deployments. Before the fix every event was refused 403 silently. |
| B9 | **PASS** | With a token for `flint-frontdoor` only: list flintshares 200, get share 200, read the operator Lease 200, patch annotations 200; **delete share 403, get Secret 403, list pods 403, list PVCs 403**. Anti-vacuity: allowed and denied calls in the same session, so a bad kubeconfig cannot explain the denials. |
| B1 | **PASS** | Three arms. Liveness first: untouched, the share reached `IdleSuspended` at t+139s with `replicas=0`, PVC still Bound (same UID), state carried in `flint.io/idle-state`/`idle-since` annotations, `IdleSuspended` event emitted. Arm 2 — 170s of `/status` polling only (**11 × HTTP 200**, so the poller provably reached the hub): still suspended. Arm 3 — 170s of authenticated `/files` calls (**6 × 200**): stayed `Ready`/`replicas=1`. Same duration, same rig, only the endpoint differs. `/status` is not activity; the file API is. |
| B2 | **PASS — contract clarified** | Grace renders as 120s. With 768 MiB dirty (`dirtyFiles=3` verified immediately before SIGTERM), the hub exits in **~1s**, not 120: `SIGTERM — draining and flushing before exit` → `manifest barrier seq 55 — 24 entries, 2 beyond RPO` → `WARN shutdown with work still unpublished (2 unpublished, 2 beyond RPO) — epoch left HELD so no successor claims a bucket that is behind this PVC`. **The hub deliberately does not spend the grace window flushing**; it writes the barrier and leaves the epoch held. The feared SIGKILL-mid-flush costing a full lease does **not** occur: the successor shares the `serverId`, claims instantly (`epochTakeovers=0`), and republishes the backlog (`dirty-6.bin` reached the bucket ~10s later). Nothing was lost. |
| B3 / D5 | **PASS** | Baseline arm established first: `rpo.clean=true`, `manifestCurrent=true`, `beyondRpo=0` observed on a healthy hub. Break: with data-prefix `PutObject` revoked under a *running* hub, the hub reported `rpo.clean=false, dirtyFiles=3, beyondRpo=3, publishFailures=6` — the predicate genuinely went false. With a 120s hibernate ladder armed, the **PVC survived every one of 20 samples over 500s**. The operator named its reason precisely: `HubReachable=False (PollFailed)` and `IdleEligible=False (Held)` carrying the connection error. **An unreachable hub is an unknown hub, never an idle one — and the operator never deletes a PVC it could not ask about.** |
| D4 (startup arm) | **PASS** | With `PutObject` revoked, the hub **refuses to start**: `tier bootstrap: versioning: Enabled` → `WARN cannot read lifecycle configuration (403) — the MPU abort rule is UNVERIFIED` → `ERROR tier: bucket posture refused: cannot create a probe MPU (s3:PutObject)`, exit 1, CrashLoopBackOff. It fails fast rather than serving a share it cannot publish from. |
| C1 | **PASS** | `advertiseAddress: 172.31.0.236:31049` reported verbatim in `status.address`. A kubelet-driven NFS PV on cluster B (`mountOptions: port=31049`) mounted it and the consumer pod reached Running. The operator-derived name `tenant-a.workspaces.svc.cluster.local` fails from B with **NXDOMAIN** specifically. Anti-vacuity: the SG probe emitted a literal RC line **both** times — `BEFORE-SG RC=28` (blocked), `AFTER-SG RC=52` (connected). |
| C2 | **PASS** | `/proc/mounts` on B shows `vers=4.1,nconnect=4,port=31049`; the hub counts **4 ESTABLISHED** on 2049. Anti-vacuity: a second mount from a **different node** (cluster B's CP) with `nconnect=1` yielded exactly 1, hub total **5**. Trunking is real across the boundary, not silently refused. |
| C3 | **PASS — number measured** | Boundary bytes measured at the hub's NIC. Two pods on the **same** node reading one 64 MiB file: **0.0 MiB** each (shared page cache). A pod on a **different** node: **64.1 MiB** — one full copy. Anti-vacuity: three identical md5s (`1a6717c6…`), MemAvailable ~12 GB on both nodes (≫3× file). **Boundary cost scales with nodes, not agents.** |
| C5 | **PASS** | Tiny write first, then 64 MiB written from cluster B at 122.6 MB/s, read back through cluster A's HTTP file API **byte-identical** (`1a6717c6f845fec0461ad547a49807cf` both sides); the tiny file's content round-trips too. No PMTU/large-write hang on this path. |
| C7 | **PASS — the everyday agent path** | A hard cross-cluster mount survived a full suspend→wake mid-read. Read started from cluster B's CP node; hub admin-suspended at t+18s. The client **blocked for 120s** across 12 samples at `replicas=0` — no error, no partial output. On wake it **resumed without remount** and completed ~40s after `phase=Ready`, producing `1c78c6004214dab22b4036cf29d1af9e`, byte-identical to ground truth fetched independently through cluster A's file API. Anti-vacuity: an uninterrupted read of the same-sized file in the same session took 62s and completed normally. NodePort is what makes this work — the advertised address is a node IP, so it survives the pod IP changing under it. |
| E7 | **PASS** | Anti-vacuity: authenticated `/files` 200 first. A symlink to `/etc/passwd` and one to `../../state/state.db` (the credential-theft vector) both return **409 "path is a symbolic link; read its target from the listing"** — never dereferenced. Traversal *through* a symlinked directory component is refused too (503 `Resource`; a 409 would be clearer, but it is a refusal, not a traversal). The listing carries the raw target as data (`linkTarget: "../../state/state.db"`, `size` = target length). **Zero `escape*` objects reached the bucket**; the manifest names both as symlinks. No symlink carries its target's bytes into S3. |
| C6 | **PASS — wedge confirmed, and bounded** | Anti-vacuity: mounts asserted present on B's node before the suspend. Suspending with kubelet-driven cross-cluster mounts held **does wedge consumer teardown**: pods went to `Error` but would not terminate for 288s, the node kept **2 pinned nfs4 mounts** under `/var/lib/kubelet/pods/…/kubernetes.io~nfs/`, and exactly one process sat in D state — `172.31.0.236-manager`, the kernel NFS state manager for that server. **Waking the hub cleared all three pods in ~31s.** The recorded scar now holds for a kubelet-managed mount, not just a Lima kernel — and this is the empirical case for `suspendWithSessions: false` as the default. |
| §M0 mitigation | **VALIDATED** | With a live cross-cluster mount and `suspendWithSessions: false` on a **60s** ladder, the share stayed `Ready`/`replicas=1` for **350s** (≈6× the threshold), the operator reporting `IdleEligible=False` — **"a client still holds a lease"**. Paired with C6 (where the default `true` wedged three consumer pods for 288s and pinned two node mounts), this is direct evidence that `suspendWithSessions: false` should be the **default**, not an option — exactly as the plan's §M0 argued. |
| B7 | **PASS** | Fast arm: clean suspend → wake **Ready in 13s**, same `serverId`, `epochTakeovers=0` — pod start plus instant self-recognition. Slow arm (same bucket, same session): epoch left held + PVC destroyed so the successor has a **new** `serverId` → **Ready in 79s**, `epochTakeovers=1`. Two arms, same instrument, opposite results, oracle read as a counter not a log grep. |

## SHIPPED BUG FOUND — the clean epoch release never lands

**Deterministic, 3/3 reproductions plus the original.** On every clean
shutdown with `rpoClean: true`, the hub logs:

```
🛑 SIGTERM — draining and flushing before exit
🛑 WARN shutdown flushed cleanly but the epoch release did not land: LostCas
```

and the cell keeps `"released": false`.

**Cause.** `server.rs` shutdown order is `guard.fence()` (the publish
barrier, mandated by the plan's [R-4] for straggler safety) and then
`heartbeat.release(...)`. But the heartbeat's shutdown arm in
`tier/epoch.rs` opens with `if guard.is_fenced() { ReleaseOutcome::LostCas }`,
reading the fence as "already deposed". The clean shutdown sets that exact
flag one line earlier, so the guard is **always** fenced when the release
runs and `store.epoch_release()` is **never called**. Proof: the inner
branch's own warning (`"release lost the CAS — deposed during shutdown"`)
never appears in any log — only the outer one does.

`is_fenced()` carries two meanings — "deposed by a rival" and "we closed
the barrier ourselves on the way out" — and only the first should suppress
the release.

**Measured cost.** Self-recognition hides it whenever the `serverId` is
unchanged (B7 fast arm, 13s). It does not hide it when the identity
changes — which is exactly the hibernate-wake path, since hibernate
destroys the PVC and therefore the `serverId`. B7's slow arm measured
**79s vs 13s**: every hibernated project's first wake pays the full
6×10s lease that a landed release exists to avoid.

**Why nothing caught it.** The kind e2e asserts the ladder's Kubernetes
side, never the bucket's `released` flag; the formal model checks the
protocol, not this predicate collision; and the fast path masks it in
every same-identity restart.
| E1 | **PASS — all three arms** | Corpus of 6 × 32 MiB with md5s recorded up front. **Retain:** deleting the share left the PVC Bound with the same uid and no Deployment; a new share re-adopting the claim served all six files **byte-identical** (6× OK). **Delete, own claim:** PVC `tenant-e3-data` gone (`NotFound`). **Delete, ADOPTED claim (the C10 fix):** PVC survived, still Bound, same uid, with `Warning/ReclaimRefused — PVC was NOT deleted despite reclaim: Delete — the claim is adopted`. Anti-vacuity: one deliberately planted corruption made the same comparison report exactly **1 FAILED**. |
| E8 | **PASS — strong form** | Anti-vacuity: inventory taken first and non-empty (8 objects, keys+sizes recorded). The share was then deleted **with `reclaim: Delete`, destroying its PVC**. Inventory after: **identical**, all 8 objects byte-for-byte. Deletion touches Kubernetes objects only; the bucket — which for a hibernated share is the only copy — is untouched. |
| E6 | **PASS — both parts, hazard demonstrated** | **(i) In scope:** a share on `nested/sub/` against an existing `nested/` was refused with `phase=Failed`, `Conflict=True`, a message naming the winner, and **0 Deployments, 0 PVCs** — refused before any hub started. **(ii) Across scopes** (a second operator installed on cluster B): B accepted `nested/sub/` with `conflict=False`, both hubs reached Ready simultaneously, and the bucket held **two independent epoch cells** (`nested/.flint/epoch` holder `hub-a738c6…`, `nested/sub/.flint/epoch` holder `hub-2266f2…`) — neither can fence the other. A wrote `/sub/overlap.txt` and B wrote `/overlap.txt`; both map to S3 key `nested/sub/overlap.txt`. The survivor is **exactly** B's bytes (anti-vacuity: exact match, not merely different from A's), while **hub A still serves `AAAA-written-by-cluster-A`** — silent divergence between what a hub serves and what its bucket holds. A hibernate/wake on A would return B's data. This is why prefix uniqueness must live in a control-plane DB (§M1-a); nothing in this repo can enforce it. |
| E5 | **PASS — both halves** | Two clusters, one prefix (`nested/`). While A held the lease, B sat at `phase=Starting` for 320s and **its 2049 was refused (`rc=7`) at every sample** — the loser never binds. B's log: *"hub-a738c6… is ALIVE at epoch 2 (token advanced) — this hub waits; two hubs on one prefix is a misconfiguration."* Anti-vacuity at one instant: **A:2049 rc=52 (answering), B:2049 rc=7 (refused)**. With A removed, B stayed refused 63s then took over at **t+79s** (≈6×10s lease + pod start), oracle read as a **counter**: `epochTakeovers=1`, cell holder `hub-a738c6…`@2 → `hub-adcf0f…`@3. **Independently corroborates the release bug** — A shut down cleanly and B still paid the full lease. |
| D9 | **VOID — instrument failed** | Force-killing the hub (`--force --grace-period=0`) to produce an unclean death skipped the CSI `NodeUnstageVolume`, and the flint CSI driver then refused every remount with `FailedPrecondition: staging path … is not mounted — restage required (F29)`. The successor never started, so nothing could be learned about self-recognition after an unclean death. **This is a CSI-driver defect, not flint-lite**, and the drill declares the CSI driver out of scope — but it makes `--force` unusable as an unclean-death instrument on a `flint-spdk` volume. D9's claim is partly covered anyway: B7's fast arm showed same-`serverId` re-claim in 13s with `epochTakeovers=0`. |

## Legs NOT run, and why

Stated so the gaps are chosen rather than discovered later.

| Leg | Why not |
|---|---|
| **B4** (hibernate ordering: PVC only after the bucket says released) | Partly observed — the `HibernateStarted → HibernateVerified → DiskReclaimed` sequence was captured with the bucket-side inventory intact (see B3/E8). The timestamp-ordering oracle against the epoch object's `LastModified` was not run, and is now **partly meaningless anyway**: the release never lands (see the shipped bug), so `released: true` is not observable on this build. **Re-run after that fix.** |
| **B6** (`wake-intent: warm`) | Not run. Needs a cold/warm arm pair with hydration DELAY counting; deprioritised behind the data-safety legs. |
| **C4 / D7** (lease decay under partition) | Not run. Needs a sustained Cilium-policy partition plus lease-expiry timing; the highest-value remaining leg, because it decides whether a partitioned agent fleet pins its share awake forever. |
| **D1** (node loss bounded by RPO) | Not run. |
| **D2** (120s grace at real S3 rates) | Superseded in part by B2, which showed the grace window is **not spent** — the hub exits in ~1s and leaves the epoch held. A throughput-under-grace number would now be measuring something the code does not do. |
| **D3** (wake racing drain; the flock) | Not run. Blocked by the same F29 wedge that voided D9 — producing the race needs an unclean kill. |
| **D6** (leader node loss) | Not run. |
| **D8** (disk-full on a real CSI volume) | Partly covered by A6, which produced a genuine `507` from the tier's space admission with `nospcWriteRefusals=1` and recorded the reserve margin (44 MiB free at refusal). The dedicated ENOSPC-vs-filesystem arm was not run. |
| **E2 / E3** (hibernate round trip, refuse-then-complete) | Substantially covered: the full hibernate rung ran twice (B3/B5) with `HibernateVerified`, a new PVC UID, and a complete tree rebuilt from the bucket as stubs; and hibernate provably deferred while the RPO predicate was false. The byte-identical corpus comparison specifically across a hibernate was not run (it was run across a Retain detach in E1). |
| **E4** (operator never deletes a PVC it could not ask about) | **Covered by B3/D5** — 20 samples over 500s with the PVC intact and `HubReachable=False (PollFailed)`. |
| **Fleet budget** (3000 CRs / 300 live) | Out of scope on a 2-worker rig, as the plan already stated. |

## Summary

**28 legs run. 26 PASS, 1 PASS-with-refuted-prediction (A9), 1 VOID (D9,
instrument failed). No leg failed its oracle. Nothing lost data.**

Phase A 9/9 · Phase B 7/9 · Phase C 6/7 · Phase D 2 arms · Phase E 4/8,
plus the §M0 mitigation validated.

### Act on these

1. **The clean epoch release never lands** (shipped bug, deterministic,
   3/3 + 2 independent corroborations). `guard.is_fenced()` means both
   "deposed" and "we closed the barrier ourselves", and the clean
   shutdown sets it one line before calling `release()`, so
   `store.epoch_release()` is never called. Cost: every identity-changing
   wake pays the full 6×10s lease — measured 79s vs 13s (B7), and again
   as E5's 79s takeover after a *clean* shutdown. Fix: distinguish
   self-fencing from deposition.
2. **The download path fully buffers** (A7). `VmHWM` 30 MB → 541 MiB on a
   512 MiB request; at a 256Mi limit the same GET produced `OOMKilled`
   and an empty reply. The 5 GiB default cap needs ~5 GiB of headroom or
   one authenticated browse click kills the share and takes the NFS
   export with it. The module doc already describes the streaming design
   the code does not implement. A hybrid — buffer to a small threshold,
   stream beyond with error-on-short — keeps the clean pre-byte 409 for
   small files. Interim mitigation with no code change: lower
   `maxDownloadBytes`, since the cap is checked against the *range*, not
   the file.
3. **`suspendWithSessions: false` should be the default** (C6 + §M0).
   With the default, suspending under cross-cluster mounts left three
   consumer pods unable to terminate for 288s and two pinned node
   mounts; with it, the share correctly refused to suspend for 350s on a
   60s ladder.
4. **A cold read's first answer is 409, not 503** (A4). In-region
   hydration outruns the read-window guard, so 409-then-retry is the
   normal cold-open path. A front door handling only 503 errors on every
   first cold open.
5. **`nfsClientCIDRs` is inert for in-cluster pod clients on Cilium**
   (A2). An `ipBlock` naming a pod's exact `/32` never opened; a
   `podSelector` did. Fails closed, silently. Document it, or reject pod
   CIDRs at admission.
6. **A5: the cap bounds the response, not the egress.** A 1 KiB Range
   against a cold 512 MiB file pulled all 512 MiB. The 413's advice to
   use Range saves response size and no transfer.
7. **A9's published loss range is ~5x too pessimistic for real clients.**
   Direction confirmed (loss tracks hub CPU), magnitude refuted: 3.2%
   residual idle and 8.8% loaded, against a contract quoting 16%/50%.
8. **F29 in the CSI driver** — `--force` deleting a pod on a `flint-spdk`
   volume wedges the staging path permanently (`restage required`), and a
   normal pod delete does not clear it. Voided D9 and blocked D3.

### Rig lessons

- On `i4i.large` the node root is **8 GB**; container ephemeral storage
  lives there. A probe writing multi-GiB temp files trips kubelet
  eviction on every pod on the node. Give probes a real PVC.
- `curl --data-binary @file` buffers the whole body in RAM; use `-T`.
- 8080 is deliberately not on the Service, so there is **no stable
  address to poll a hub across a restart** — resolve the pod IP each
  iteration or the poller measures a dead IP (this voided one A3 run).
- Both clusters landed in the same AZ and subnet, so boundary transfer
  was free. Do not assume that; verify it.
