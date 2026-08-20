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
| **C4 / D7** | **CONFIRMED — the documented residual is FALSE** | Under a proven partition, `nfs.activeLeases` stayed **1 for 770 seconds (12.8 min)** against a **90s** NFSv4 lease, and the share stayed `Ready`/`idle=Active` the whole time with `suspendAfterSecs=120` **and** `suspendWithSessions: false` armed. Then **one** file-API compound took it to **0 immediately** (`fileapi_rc=200` → `activeLeases=0`). That is the anti-vacuity the plan demanded: the same instrument that read "1, forever" read 0 the instant a compound drove the sweep, which also proves the lease had been expired the entire time — nothing was reaping it. **And the ladder then fired**: the share went `IdleSuspended` at **t+988s**, 208s after that one compound (its own `suspendAfterSecs` plus reconcile cadence), so the ladder was armed and working the entire 12.8 minutes and the stale lease was the *only* thing holding it. One stimulus, two coupled effects. **A partitioned agent fleet pins its share awake forever**, and the docs' "leases expire, so a long enough partition drops the count to zero on its own — document the window" is wrong: the window never closes. |
| **E3** | **PASS — strong form, and it earned its keep** | `HibernateStarted → HibernateDeferred — not reclaiming the disk: 8 file(s) beyond RPO`. The PVC was **not** deleted (same uid throughout). This was not a staged refusal: the share genuinely was not recoverable, because of the generation-key bug below. The predicate refused a real reclaim of a real disk holding the only complete copy of the POSIX metadata. **This is the single property that kept the drill from losing data.** |
| **E2** | **PARTIAL — blocked by the bug it exposed** | The hibernate never completed, so "PVC destroyed, then bytes return identical" is **not** demonstrated. What did hold: all 8 × 32 MiB files read back **byte-identical** across a suspend→wake (`identical=8 mismatched=0`), and the comparison is not vacuous — against a deliberately corrupted truth list the same harness reported exactly `identical=7 mismatched=1`. Wake took 203s, most of it the ladder re-deciding rather than the hub booting. |
| **D8** | **REFUTED — the hub does reach hard-full** | The claim was "the tier's space admission delivers ENOSPC while the filesystem still has ≥ the reserve free — the hub never reaches hard-full". It does reach hard-full. A **single 600 MiB PUT** onto a volume with **539 MiB free** (283 MiB headroom after a ~268 MiB reserve) returned **HTTP 201** and drove `df` to **0 bytes available, 100% used** — the entire reserve consumed, with `nospcWriteRefusals` still **0**. The admission never refused once. The incremental fill did produce a genuine `507` with **155 MiB free** inside the failing iteration (the plan's anti-vacuity, and the reserve did hold *there*) — but 155 MiB is already below the reserve, so even that refusal came late. **The hub self-healed** (publish + evict took it back to 84%, then 12% after cleanup) and nothing was lost. |
| **D6** | **PASS — the best anti-vacuity in the set** | One stimulus (`flint.io/requested-at`), three outcomes. **Leader failover:** deleting the lease holder moved the lease to the standby in **46s** (`leaseDurationSeconds: 15`), and the same stimulus then woke the share. **No operator at all** (`replicas=0`): the stimulus was stamped and the share sat at `IdleSuspended`/`replicas=0` for **245s — `woke=NO`**. **Operator restored:** the **already-stamped** annotation took effect and the share was `Ready` in **41s** — `woke=YES`, proving the wake is genuinely level-triggered rather than edge-triggered on the write. A typo'd key or wrong namespace would have failed all three halves and could not be mistaken for a pass. **Confirms §M6-7: level-triggered wake is real only while the operator reconciles — "fails safe, only a delete hangs" is wrong.** Two replicas plus a PDB is therefore load-bearing, not hygiene. |
| **D1** | **PASS — nothing certified was lost** | A fresh share reached `rpo(clean=True, beyondRpo=0, seq=3)` — first boot, so its generation rows were still keyed on the live device. Anti-vacuity first: **3 distinct epoch-cell ETags** in 3 samples, so the instrument reads the live cell and not a cache. Then total local-state loss: the hub `--force --grace-period=0`, the share suspended so kubelet could unstage, and the **PVC destroyed**. The bucket was the only remaining copy. It came back on a **new PVC uid** and all **6 × 48 MiB files returned byte-identical** at full size (`identical=6 lost=0`). Worth noting for the DR story: the operator **re-creates the PVC immediately** — a level-triggered empty claim — which is what lets the import rebuild into it. |
| **B6** | **PASS — the knob is wired end to end** | `wake-intent` reaches the hub's boot config with **opposite values from one instrument**: `warm` → `hydrateWarmAfterImport:true`, `cold` → `hydrateWarmAfterImport:false`, both caught in the ConfigMap 2s into the wake. It must be sampled *during* the wake — the operator consumes and clears the intent once the hub is up, then re-renders without the line, so a read taken after `phase=Ready` sees nothing and looks inert. **[C8] is resolved in this build**; the annotation is no longer the parsed-but-inert knob the plan flagged. **[R-8] does not fire:** with the idle ladder disarmed, consuming the intent left the hub **unrolled** and the Deployment's `checksum/config` **byte-identical** before and after — even though the ConfigMap did change. That is `rollout_checksum` stripping the boot-only line, working. **A first run reported `rolled=YES` in both arms and was wrong**: the share still carried `suspendAfterSecs: 60` from an earlier leg, so the ladder's own suspend moved the pod inside the watch window. Any measurement on a pod-identity timescale has to disarm the ladder first. |
| **D3 (census arm)** | **PASS** | The wake-during-drain race, produced without a force-kill: suspend, then resume **4 seconds later**, so the operator is asked to scale 0→1 while the previous pod is still terminating. Census over **90 samples**: `two_serving=0`, `exactly_one=86`, `none=4`. The oracle wanted zero samples with two pod IPs both accepting 2049 and ≥20 with exactly one — it got 0 and 86. The 4 `none` samples are the `Recreate` gap and are what prove the census was **alive across the handover** rather than watching a static pod; the pod IP did rotate (`10.244.1.125 → 10.244.1.7`). |
| D3 (flock arm) | **PASS** | A second `flint-pnfs-mds` started by hand inside the running hub pod, on the same `/data/state`, **never became a writer**. It logged `🔒 state dir /data/state is held by another hub process — waiting for it to finish draining`, waited the full 150s budget (`~grace + slack`), then exited non-zero with `refusing to start a second writer on this volume (two processes here share one server_id, which the epoch protocol cannot fence)`. Oracles read as **counts, not greps**: `refusals=1`, `bound2049=0` — it never reached the listener, never claimed an epoch. Anti-vacuity: a first run with `timeout 60` caught it mid-wait with `refusals=0`, proving the count tracks the refusal and not merely the process exiting. |
| **F29 recovery** | **FOUND — the wedge has a non-destructive exit** | The force-delete wedge that voided D9 is cleared by **dropping the volume's last referencing pod**, which lets kubelet issue the `NodeUnstageVolume` the force-delete skipped. `spec.lifecycle: Suspended` (replicas→0) then `Active` did it: both wedged shares were `Ready` **30s** later, csi-node logging `Volume … unstaged successfully` for both PVCs. **Why a plain `kubectl delete pod` can never work:** the ReplicaSet recreates a pod immediately, so the volume never loses its last reference and kubelet never unstages — the driver's F29 refusal (`main.rs:4968`) is correct in itself, but kubelet's volume-manager cache still says "staged" and so `NodeStageVolume` is never called again. Nothing self-heals and nothing surfaces the remedy. |

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

## SECOND FINDING — `activeClients` never decays without inbound traffic

**C4/D7, confirmed on real infrastructure.** The count that
`suspendWithSessions: false` acts on is a raw map length, and the only
production code that reaps it runs on the inbound path.

- `active_count()` is `self.leases.len()` (`nfs/v4/state/lease.rs`) — no
  expiry filter. An expired lease is still counted.
- The **only** production caller of `StateManager::cleanup_expired()` is
  `dispatch_compound_inner` (`nfs/v4/dispatcher.rs:305`), at the top of every
  COMPOUND. No compounds arrive from a partitioned client, so nothing reaps.
- The background sweep exists but is the wrong sweep:
  `start_lease_sweep` (`pnfs/mds/server.rs:1548`) runs
  `lease_sweep_pass(&layout_manager, …)`, which reaps **layout grant rows**,
  not lease records — and standalone (flint-lite) turns layouts **off**
  entirely, so in this posture it has nothing to sweep at all.

**Consequence.** With `suspendWithSessions: false` — which §M0 argues should
become the default, and which C6 independently supports — a partitioned agent
fleet holds its share `Ready` indefinitely. The ladder never fires, the hub
never suspends, and no rung of the idle ladder is reachable. The cost is a pod
and a PVC per abandoned project, forever.

**The instrument matters, and one attempt was vacuous.** Revoking the AWS
security-group rule does **not** partition an established flow: security groups
are stateful, so existing connections keep flowing and only new ones are
refused. That attempt reported `activeLeases=1` too — for entirely the wrong
reason — and was caught only because the leg required the mount to hang first.
The working cut is an `iptables -j DROP` in the client node's host netns for
`<hub-node>:<nodeport>`, verified by a write **and** a read both timing out
(`rc=124`, captured with `$?` directly rather than through a pipeline, whose
status would have been `tail`'s).

**Two candidate fixes, both small.** Either filter expired leases out of
`active_count()` (makes the gauge honest, leaves dead client state resident),
or have the periodic sweep also drive `StateManager::cleanup_expired()` (also
releases the dead client's locks, opens and delegations). The second is the
real fix; the first alone would still leak state.

## THIRD FINDING, AND THE MOST SERIOUS — a stale `dev` silently empties the manifest

**Found by accident while running E2, then reproduced deliberately on a second
share.** Generation rows are persisted keyed by `(dev, ino)`
(`tier_generation` columns `dev, ino, key, generation, …`), and `dev` — the
device number of the mounted volume — **is not stable across a restage**. When
it drifts, every row becomes unreachable and the consequences cascade.

**The evidence, on `tenant-a`:**

- the hub logs `tier flush: startup loaded 33 generation row(s)` — the rows are
  read back fine, this is not data loss — and then, one line later,
  `tier manifest: barrier seq 81 — 4 entries, 33 beyond RPO`;
- the live export is `dev=66312`; all 33 rows carry `dev=66311`;
- `a5-probe.bin` has **ino 131081 in both** the row and the live file. The
  inode is identical. Only the device number moved;
- `manifest::build` treats "no generation record" as `beyond_rpo` and **drops
  the entry** (`manifest.rs:220-233`), and `write_at_barrier` then publishes
  that manifest over the good one.

**What it did to the bucket.** `tenant-a/.flint/manifest` went from **7919
bytes, 37 entries, `beyond_rpo: 0`** (05:31) to **534 bytes, 4 entries — one
directory and three symlinks, zero entries carrying an S3 key — `beyond_rpo:
33`** (05:57). The 33 data objects are all still present and intact in S3; the
manifest simply no longer mentions them.

**Reproduced from a clean start.** `tenant-e2`, created fresh for this leg,
published 8 objects under epoch 1 at 05:45:04 with correct `flint-gen`/
`flint-epoch` metadata, restarted for hibernate-verify at 05:49:48, and wrote
`{"seq":4,"epoch":2,"beyond_rpo":8,"entries":[]}` — a **completely empty
manifest** — one second later.

**It never heals.** `dirtyFiles` is 0, so nothing will ever republish those
files. Rewriting one by hand proves the mechanism and the trap in one shot:
overwriting `a6-small-1.bin` moved `beyondRpo` 33 → 32 and left a **second**
row for that file (`(66311,131082)` stale, `(66312,131117)` live, generation
2), then it froze at 32. Recovering a share means rewriting every file
individually.

**Consequences, in order:**

1. **The manifest — documented as carrying everything, and the sole input to
   manifest-first cold import — becomes false.** Bytes are recoverable through
   the post-listener foreign-key sweep, but `mode`, `uid`, `gid`, `mtime` and
   symlink targets live **only** in the manifest, so a sweep-based recovery
   restores content and loses POSIX metadata.
2. **`rpoClean` is permanently false, so hibernate is blocked forever.** That
   is also what saved this drill: `HibernateDeferred` refused to delete a PVC
   holding the only complete copy. The safety property worked exactly as
   designed, against a defect nobody had predicted.
3. Stale rows accumulate rather than being reconciled.
4. It is **silent** — no error, no event, no failed publish. The only surfaces
   are `rpo.beyondRpo` in `/status` and `IdleEligible=False`.

**Why earlier legs passed.** The drift is not deterministic — it depends on
which device minor the CSI driver gets on restage. A control test on
`tenant-notoken` came back `dev=66310` **both** before and after a
suspend/resume, so a single restage often reuses the number. B3/B5 hibernated
successfully earlier in the drill for exactly that reason. This bug hides until
the minor happens to move.

**The fix.** `dev` earns nothing in that key: a hub owns exactly one export
root on one filesystem, so `ino` alone identifies a file, while `dev` adds a
value that is stable only by luck. Normalise it — remap stored rows to the
export root's live `st_dev` at load, or drop `dev` from the lookup — and
deduplicate rows that collide on `ino` by `updated_unix`.

## FOURTH FINDING — the write reserve is defeated by write speed, and the code knows

`admit_bytes` (`tier/space.rs:155`) is the A10 admission for every
byte-extending client mutation. It reads `s.headroom()` — a gauge the refresher
updates every **`REFRESH_SECS = 2`** — and deliberately does *not* force a
`statvfs` on the admit path ("Admits never pay this"); only the refusal branch
re-checks. There is **no accounting for bytes already admitted but not yet
landed**.

So a sustained write admits chunk after chunk against a gauge up to two seconds
stale. At NVMe speeds two seconds is hundreds of megabytes, which is larger
than the whole reserve. Measured: 600 MiB written onto 539 MiB free, `201
Created`, `df` avail **0**.

**The codebase already contains the fix, applied to the wrong path.**
`admit_warm` — the bulk-hydration admission, twenty lines below — is documented
as *"STRICTER than the demand admission on both axes: headroom must cover `len`
PLUS the fill's admitted-but-unfinished bytes (`pending` — N blind concurrent
admissions would otherwise overshoot by N × object size, and the fill must
never eat the demand/write reserve), AND the refresh is unconditional."* Every
word of that rationale applies to a bulk client upload. The warm fill is
protected from overshoot; the client write path that the reserve exists to
bound is not.

**Why it matters even though nothing broke here.** The reserve's job is to keep
the state database writable when the export fills. This drill drove the volume
to literally zero bytes and the hub stayed `Ready` — publish-and-evict clawed
the space back, and a 64 MiB `flint-ballast.bin` sits in the state dir for
exactly this. That is the mitigation working, not the guard working. A hub that
cannot write `state.db` is the failure this reserve was drawn to prevent, and
right now the only thing standing between a fast uploader and that state is how
quickly eviction runs.

**Fix:** give `admit_bytes` the same `pending` accounting `admit_warm` already
has — a running total of admitted-but-unlanded bytes, decremented as writes
land — or force the `statvfs` refresh once headroom falls below some multiple
of the reserve. The second is cheaper and bounds the error to one refresh
window's worth of writes near the boundary only.

## Legs NOT run, and why

Stated so the gaps are chosen rather than discovered later. Most of the
original list was cleared in the second session; what remains is here.

| Leg | Why not |
|---|---|
| **B4** (hibernate ordering: PVC only after the bucket says `released`) | Still not run, and still **not runnable on this build** — the clean release never lands, so `released: true` is not observable. The fix is now on `fix/epoch-clean-release`; re-run against a build carrying it. The *sequence* (`HibernateStarted → HibernateVerified/Deferred → DiskReclaimed`) has been observed three times. |
| **D2** (120s grace at real S3 rates) | Superseded by B2: the grace window is **not spent** — the hub exits in ~1s and deliberately leaves the epoch held. A throughput-under-grace number would measure something the code does not do. |
| **E2** (full hibernate round trip) | **Attempted, blocked by the generation-key bug** it exposed — hibernate correctly deferred, so the PVC was never destroyed. Bytes were verified byte-identical across a suspend/wake instead. Re-runnable once the fix ships. |
| **Fleet budget** (3000 CRs / 300 live) | Out of scope on a 2-worker rig, as the plan said from the start. |
| **True EC2 node termination** | D1 reproduced node loss as *total local-state loss* (force-kill + PVC destroy), which is what losing a node means for a node-local `flint-spdk` volume. It did **not** terminate an instance, so nothing here exercises kubelet rescheduling or trove's node replacement. |

## Summary

**36 legs across two sessions. 32 PASS, 1 pass-with-refuted-prediction (A9),
1 claim REFUTED (D8), 1 PARTIAL (E2), 1 VOID (D9). No leg failed its oracle.
Nothing lost data — twice because a safety predicate refused, not because
nothing went wrong.**

Session 1 ran 28 legs and found the epoch-release bug. Session 2 ran the
remainder — C4/D7, D3, D6, D8, D1, B6, E2, E3 — and found **three more shipped
bugs**, one of them the most serious of the campaign.

### Act on these, in order

1. **A stale `dev` silently empties the manifest.** Generation rows are keyed
   `(dev, ino)`; a restage can move `dev`; every row then loads but matches
   nothing, so the barrier publishes a manifest naming **no files**. Measured:
   7919 B/37 entries → 534 B/4 entries, and a fresh share wrote a literally
   empty `{"entries":[]}`. Never self-heals. **Fixed** on
   `fix/epoch-clean-release` (re-home to the live `st_dev`, prune rows whose
   inode is no longer live).
2. **The clean epoch release never lands.** `is_fenced()` conflated "deposed"
   with "we fenced ourselves on the way out", and the clean shutdown sets it one
   line before `release()`. Costs every identity-changing wake the full 6×10s
   lease — 79s vs 13s. **Fixed** (`fence_for_shutdown` / `fenced_by_deposition`).
3. **Expired leases are only reaped by inbound traffic.** A partitioned client
   pins its share awake **forever** — 770s at `activeLeases: 1` against a 90s
   lease, dropping to 0 the instant one compound arrived. The periodic sweep
   looked like it covered this and did not: it reaps layout grants, and
   standalone has no layouts. **Fixed** (the courtesy-release pass now runs on
   both the COMPOUND path and each sweep tick).
4. **The write reserve is defeated by write speed** (D8, **not fixed**). A
   600 MiB PUT onto 539 MiB free returned `201` and took the volume to 0 bytes.
   `admit_bytes` reads a 2s-stale gauge with no in-flight accounting;
   `admit_warm` twenty lines below already implements exactly that accounting.
5. **The download path fully buffers** (A7, not fixed). 512 MiB request →
   541 MiB `VmHWM`; at a 256Mi limit, `OOMKilled` and an empty reply. Interim
   mitigation needs no code: lower `maxDownloadBytes`, since the cap is checked
   against the *range*.
6. **`suspendWithSessions: false` should be the default** (C6 + §M0), now with
   the caveat that finding 3 was what made the option safe to rely on.
7. **Operator availability is load-bearing, not hygiene** (D6). With no
   operator, *no project in the cluster can wake* — 245s of a stamped
   `requested-at` doing nothing, then 41s to Ready once it returned. "Fails
   safe, only a delete hangs" is wrong.
8. **A cold read's first answer is 409, not 503** (A4) — and it cost this drill
   a false "6 files lost" (see below).
9. **`nfsClientCIDRs` is inert for in-cluster pod clients on Cilium** (A2).
10. **Removing `spec.idle` does not un-park a parked share.** With the policy
    deleted and `lifecycle: Active`, the share stayed `IdleSuspended` on a stale
    `flint.io/idle-state` annotation — `IdleEligible=False (Held) idle and
    unrequested`. Only `requested-at` brings it back. Desired state does not
    converge.

### What the safety properties actually bought

Two separate refusals prevented real loss, both against defects nobody had
predicted. `HibernateDeferred` would not reclaim a PVC while 8 files were beyond
RPO — that PVC held the only complete copy of the POSIX metadata. And the flock
refused a second writer on one `state.db` after waiting out its full budget.
Neither was staged; both were load-bearing on the day.

### Rig lessons, the expensive kind

- **AWS security groups are stateful** — revoking a rule does **not** cut an
  established flow. That produced a fully vacuous partition run whose numbers
  looked exactly like the real result. The working cut is `iptables -j DROP` in
  the client node's host netns.
- **Never md5 a response body without checking its HTTP status.** This bit the
  drill a third time and briefly read as "D1 lost all 6 files" — every file had
  the same md5, which was the 60-byte `{"error":"file changed while being
  read..."}` from A4's cold-read 409.
- **A pipeline's exit status is `tail`'s.** `timeout … | tail -1` reports 0 for
  a command that timed out. Redirect to a file and read `$?`.
- **Disarm the idle ladder before measuring anything on a pod-identity
  timescale.** A `suspendAfterSecs: 60` left over from an earlier leg made the
  ladder's own suspend look like an [R-8] config roll in both arms.
- The `flint.io/idle-state` annotation reads `Suspended` while `status.phase`
  reads `IdleSuspended`; a script matching the phase string against the
  annotation waits forever.
- **Every fix in this campaign was checked against its own absence** — each new
  regression test was re-run with the fix reverted and confirmed to fail. A
  green test proves nothing on its own.
