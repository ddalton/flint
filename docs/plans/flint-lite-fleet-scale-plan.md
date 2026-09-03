# Taking flint-lite to 3000 shares / 300 live hubs

> **Status: plan of record, no code yet.** Produced 2026-08-20 by a
> 51-agent ultracode pass — five parallel readers over the fleet-critical
> subsystems, three independent designs from deliberately opposed priors,
> one adjudicating synthesis, then four adversarial review lenses whose
> every finding went to an independent refuter. **14 findings survived
> refutation (1 critical, 10 major, 3 minor) and are folded in below**;
> §Review record says what each one changed.
>
> **rev 2 (2026-08-20):** multi-volume hubs are now EXPLICITLY out of
> scope (§Explicitly out of scope); **Track B** adds the per-hub cost
> terms rev 1 declared hub-side and skipped; and S6's single-machine rig
> is split in two so the control plane is measured AT 300 live rather
> than extrapolated from 16. **Track A has been adversarially reviewed;
> Track B and the rig split have NOT.**

## The target, and why this document exists

The operator plan named **3000 FlintShare CRs with ~300 live hubs** as
the design target. **Nothing has ever asserted it.** Every drill to date
has run 2–4 shares. That is three orders of magnitude of unmeasured
ground, and the terms that dominate at 3000 are invisible at 4 by
construction.

**The finding that matters most: none of the blockers is a topology
problem.** Every one is a per-reconcile rate term inside a single
process. Sharding the controller, collapsing Services, or adding a
registry process buys new consensus surfaces and new split-brain modes
while leaving every measured term intact. So the plan collapses the rate
terms in place, proves it with a rig, then bounds the fan-out events.

## Explicitly out of scope: multi-volume hubs

**Nothing in this plan requires or assumes multi-volume hubs, and they
are not a prerequisite for any step.** Stated because the obvious
structural answer to "N entities each heartbeating" is a per-process
lease covering N resources (etcd lease IDs, ZooKeeper sessions), which
would take epoch-renewal cost from O(volumes) to O(hubs). It is
deliberately not taken, for three reasons:

1. **The economics do not justify it.** At 300 live the epoch heartbeat
   is ~$389/month against roughly $3,000/month of hub compute — ~13%,
   and ~4% after H2 below. A design carrying **8/8 critical-confirmed
   review findings and a formally refuted lease model**
   (`FlintTierSession` made depose-first mandatory) cannot be justified
   by a rounding error.
2. **It converts per-project isolation into shared fate.** One lease
   covering N volumes means one lease failure fences **N projects
   instead of one**. At 1:1 those are the same sentence; at 1:N it is a
   different product. Per-project isolation is the thing flint-lite
   sells.
3. **The independence is why the epoch lease is trustworthy.** Each hub
   runs a self-contained CAS loop that coordinates with nothing. That is
   what let the chaos campaign, the formal models and two real-cluster
   drills reason about it at all.

Multi-volume should be decided on **density and per-project capacity
economics** — see `docs/plans/multi-volume-hub-design.md` — and the
heartbeat saving must **not** be counted among its benefits.

## S0 — what is already good enough (do not rebuild these)

Written first because two of these were live premises in the older fleet
notes and are now **stale**; aiming work at them would be pure waste.

- **Parked shares already requeue at `REQUEUE_SETTLED`**
  (`reconcile.rs:86-105`). The 15s-forever bug is fixed.
- **The hub `/status` pod LIST is already label-selected** off the
  Deployment's own selector (`reconcile.rs:1339-1352`).
- **The three `.owns()` watches are ALREADY metadata-only.** kube 3.0's
  `owns_with` routes through `metadata_watcher`
  (`kube-runtime-3.0.0/src/controller/mod.rs:960-964`), so the
  ConfigMap's embedded `mds.yaml` is *not* in the operator's cache. The
  review corrected the design pass on this; ~21.7 MB of assumed relist
  traffic was never there.
- **The Secret watch is label-selected** (`LABEL_CREDENTIALS`).
- **Operator HA ships**: 2 replicas, kube-Lease leader election, PDB.
- **The adoption fence's whole-namespace pod LIST (`reconcile.rs:563`)
  is DELIBERATE** and must stay. It is the only guard against two
  writers on one `state.db`, and it needs a namespace-wide view.

## The blockers, quantified

Baseline: 3000 CRs, 300 live, 2700 parked, 300s cadence.

| # | Blocker | Measured / derived |
|---|---|---|
| B1 | ✅ RETIRED by S5 (`e0c26d1`, v1.33.0) — `conflict::admit` now has **no production call site at all** (the only calls left are past `mod tests`, `conflict.rs:522`); `AdmitTable` replaced it. Was: **O(rank²) per reconcile**, re-deriving a constant | 13.0 ms median share, 51.1 ms newest, at N=3000. **0.17 core steady; 3.5 cores at the legal clamp floor.** Chart requests **50m**. Full-fleet sweep N³/6 ≈ 4.5e9 calls ≈ **46–52 CPU-seconds** |
| B2 | **Self-triggering reconcile loop.** Two condition *messages* embed a ticking seconds counter, so status changes on most reconciles, which fires the operator's own FlintShare watch | amplification **1/(1−min(1,d))**. 1.11x at d=0.1s — invisible in a 4-share drill. **Unbounded at d≥1s**, and d rises with fleet size. Positive feedback |
| B3 | **Unbounded controller concurrency** (`Config::default().concurrency == 0`) × a ~750 KB whole-fleet snapshot per in-flight reconcile | **2.2–3.3 GB transient** against a **256Mi** limit. Cold start at 3000 CRs is a deterministic OOMKill |
| B4 | **Unconditional writes**: 4 SSA applies + 1 status apply per reconcile, no diff gate | **50 writes/s with nothing changing**; 1000 writes/s at the clamp floor |
| B5 | **Failure and boot ADD load**: `Pending`/`Starting` requeue flat 15s forever, no cap; `error_policy` returns a flat 30s under a doc comment claiming exponential | a broken fleet sustains **200 rec/s, 1000 writes/s, forever** |
| B6 | **The hibernate rung can invert**: `HibernateVerifying` renders `replicas: 1` and a failed verify requeues at 30s with no deadline | **2700 pods pinned up permanently** |
| B7 | **Zero instrumentation.** No metrics, no health endpoint, no probes | every number above is derived, none observed |
| B8 | **Nothing reads the bucket.** `epoch_key` keys on the exact prefix, so ancestor/descendant prefixes **never contend** | two hubs writing overlapping bytes, no fence, no error |

## The plan

Ordering bet: **instrument first**; then land the bundle that is correct
on inspection and needs no rig; then build the rig; then let measurement
aim everything with a real correctness surface.

### S1 — Give the operator a pulse *(days, no deps)*
`src/lite_operator/metrics.rs`, warp listener (already a dependency; the
pattern exists verbatim in `src/pnfs/mds/status.rs`). Export
`reconcile_duration_seconds` — **this is `d`, B2's amplification
denominator** — plus reconciles/s, writes/s, `reconciles_in_flight`,
`admit_duration_seconds`, store size. Add `/healthz` and chart probes.
**Without this, S2–S13 are unfalsifiable.** Today the only symptom of an
OOMKill loop is the leader Lease going stale.

### S2 — The bundle that is correct on inspection *(days, dep S1)*
Six independent edits, none needing a rig: cap concurrency at 32 and
debounce 250 ms (both stable on pinned kube 3.0.0); jitter every requeue
±25%; standby's non-leader requeue 10s → jittered 120s; select the
`reclaim_hibernated_disk` pod LIST (`:1282`) and move it after the cheap
already-reclaimed check; hoist the per-poll `reqwest::Client`; drop the
redundant Service GET (`:800-804`).

> **[review, minor] The buys line was 10x wrong.** `apply()` awaits every
> call sequentially — no `join!`/`FuturesUnordered` anywhere in
> `src/lite_operator/` — so a reconcile has **at most one** request in
> flight. Correct figure is **peak in-flight 3000 → 32**, not
> 30,000 → 320. Burst *total* is ~30,000 requests spread over the drain.
>
> **[review, minor] The cap adds an unpriced wake queue.** At the
> concurrency limit kube-runtime calls `scheduler.hold()`, which moves
> expired messages into a `HashSet` dispatched by `iter().find()` —
> **hash order, not FIFO and not due-order**. Contingency must be in
> scope *before* the rig: either a second Controller carrying
> wake-bearing events, or a non-blocking hub poll on the ladder's Hold
> path.

### S3 — Stop the operator triggering itself *(hours, dep S1)*
Take the ticking counters out of the four condition messages
(`reconcile.rs:983-987`, `idle.rs:279-281`, `idle.rs:313-317`,
`hubstatus.rs:175-179`) and gate `write_status` on a real diff.
**Amplification → 1.0 unconditionally.**

> **[review, CRITICAL] As drafted, S3 defeated itself.** It said to
> "publish the precise numbers as numeric status fields" — which
> re-creates the identical defect one field over: a monotonically
> advancing integer in `status`, applied every reconcile, changing the
> object and **permanently disabling S3's own write gate**. There is no
> such field today (`crd.rs:570-611`).
> **Corrected:** publish idle as an S1 Prometheus gauge only. If a
> status field is genuinely wanted it must be an **absolute
> `lastActivityTime` (RFC3339)**, rewritten only when the derived
> instant moves — never an elapsed count.

### S4 — The admit equivalence property test *(days, no deps)*
`benches/fleet.rs` + property tests. Fleets at N ∈ {1,2,5,50,400,3000}
across four nesting densities, including **non-slash-terminated
prefixes** (`overlaps` is raw `starts_with`, so `tenant-a` deliberately
collides with `tenant-abc`), empty prefixes, ties, and a candidate
absent from the fleet. **A1**: indexed verdict ≡ `conflict::admit`,
including the named winner. **A2** guards against a benchmark that
silently stopped generating 3000 shares.

### S5 — Arbitration becomes a table built once per fleet change *(days, dep S4)* — ✅ LANDED e0c26d1 (v1.33.0)
Kills B1: ~1.6 ms whole-fleet build + ~12 ns per-reconcile lookup,
replacing 13–51 ms per reconcile.

> **[review, MAJOR ×2 — found independently by two lenses, and I
> verified it by hand] The antichain lemma is FALSE.** The design pass
> claimed to have verified that "any admitted descendant of p must be
> p's first successor, so the winner is unique and the message is
> byte-identical." It is not. **Two or more descendants of `p` can be
> admitted simultaneously** — sibling prefixes are not comparable to
> each other, so both survive. `admit` scans `admitted` in **age order**
> and returns the **oldest** overlapping share (`conflict.rs:151-163`);
> a `BTreeSet` first-successor returns the **lexicographically** first.
> Whenever those differ, the winner and the rejection message differ,
> and **A1 — this step's own gate — fails.**
>
> Worked counterexample: admitted `tenant-b/` (older) and `tenant-a/`
> (newer); candidate `tenant-`. `admit` names `tenant-b/`; first
> successor is `tenant-a/`.
>
> **Corrected:** the *admission decision* is still sound with a
> `BTreeSet` (any overlap ⇒ Rejected). Only the **winner** needs care:
> store age-rank per admitted prefix and take the **minimum `created`
> over the ancestor candidate plus the descendant RANGE**
> `set.range(p..).take_while(|s| s.starts_with(p))` — not the first
> successor. **RESTATED AND LANDED — `e0c26d1`, shipped v1.33.0.** The corrected
> lemma is carried in the code that implements it
> (`conflict.rs:358-383`), including this counterexample verbatim:
> the ancestor direction is exact (`range(..p).next_back()`), the
> descendant direction takes the MINIMUM AGE RANK over `[p, p+)`.
> Because `admitted` is built in age order the rank IS the index,
> so no timestamp re-comparison is needed. This gate no longer
> applies and must not be used to hold S9/S13.
>
> **[review, MAJOR] The table would also go stale.** The draft rebuilt
> it in the FlintShare `.watches()` mapper, which is fed by a
> **different watch connection** than the reflector Store
> (`flint_lite_operator.rs:147` vs `:221-236`). A stale table strands
> losers forever and can admit a second hub on a contended subtree.
> **Corrected:** make the *lookup* self-validating — hash
> `(uid, created, ShareKey)` over `ctx.fleet.state()` in `apply()` (µs
> against the 17 ms it replaces) and rebuild synchronously on mismatch.
> The mapper rebuild stays only as an optimisation.

### S6 — Two rigs, because one was answering two questions badly *(days, dep S1)*

Rev 1 built a single `fleet-scale-kind.sh` on **one machine** and
accepted ~16–60 live hubs. Two things bind there, and only one of them
is real: a single-node cluster cannot exceed **kubelet's default 110
pods/node** no matter how small the pods are, and a laptop Docker VM
holding 3000 CRs plus ~15,000 child objects pushes it to ~16. **Neither
is a property of flint.** Split the rig so each half answers one
question.

**Rig A — control plane, AT the target.** 3000 real CRs, **300 live
stub hubs**, on ~5 small spot nodes (enough to clear the 110/node
ceiling; stubs are a few MB, so 300 of them is ~3 GB and ~3 vCPU).
Estimated well under $0.50/hr. This measures what actually breaks:
operator CPU and RSS, apiserver read/write rates, watch bandwidth,
reconcile latency `d`, wake latency. **It hits 300 rather than
extrapolating 20x from 16**, which is the whole reason to split.

Keep everything rev 1 got right: the seeder pre-stamping parked shares
with the operator's own durable annotations; the stub that constructs
and serializes the **real `StatusDoc`** so drift is a compile error
rather than a silent switch onto the operator's `Err` branch; the
lie-on-command knobs; the collector; oracles O1–O11 run continuously;
and the teardown path built **before** the first deliberate-OOM run, or
3000 finalizer-bearing CRs make the rig unrecoverable.

**Rig B — data plane, deliberately small.** 10–30 **real** hubs against
real S3, measuring **per-hub constants**: barrier-walk CPU, epoch PUT
rate, RSS, PVC attach time, DR-import time vs manifest entry count.
Then extrapolate those constants to 300.

**Why this is more honest than one rig:** extrapolating a per-hub
constant from 30 real hubs is defensible; extrapolating everything from
16 is not. It also separates "does the operator survive 3000 CRs"
(Rig A, cheap, hits the number) from "what does one hub cost" (Rig B,
real, small) — two questions the single rig answered badly at once.

**Standing directive: ask before provisioning either rig.** Quote the
shape and the hourly cost first.

### S7 — Make parked shares actually cheap *(days, dep S6)*
Long jittered cadence + a render-hash apply gate on `is_down()` shares,
with a forced full apply every 10th pass so level-triggered drift repair
survives.

> **[review, MAJOR ×3 — three lenses independently] The headline
> arithmetic does not hold.** The draft raised the requeue only for
> `IdleState::Hibernated`. But `settled_requeue` has a separate arm for
> `IdleState::Suspended` (`reconcile.rs:96`) returning
> `bounded(hibernate_after_secs)`, clamped to **≤300s** — and
> `IdleSuspended` is **the rig's own composition A** and ~90% of the
> write load. So "10 → 2.5 rec/s" and "50 → 7 writes/s" are true for
> composition B only, and **0x for A**.
> **Corrected:** clamp on the time **remaining to the next rung**, not
> the raw threshold — `down_for` is already computed locally with no I/O
> (`idle.rs:272-274`), so return
> `clamp(hibernate_after_secs − down_for, PROGRESS, PARKED)`. Return
> `REQUEUE_PARKED` outright when `hibernate_after_secs` is `None` (no
> next rung ⇒ the timer buys nothing). Restate every number
> **per composition**.

### S8 — Make failure shed load instead of adding it *(hours, dep S6)*
Implement the exponential backoff `error_policy`'s own doc comment
already claims (`reconcile.rs:1526-1532`): 30s → 900s, jittered, cleared
on success. Escalate the `_ => REQUEUE_PROGRESS` arm for Pending/Starting.
**Broken-fleet load 200 rec/s → 10 rec/s, a 20x reduction.**

### S9 — Size the deployment envelope, and refuse one that cannot hold it *(days, dep S2,S5,S6)*
Operator requests 500m/256Mi, limit 1Gi, **no CPU limit** (throttling
raises `d`, which is exactly what keeps B2 bounded). Add hub resource
defaults to `RenderDefaults` — which today has fields for image, probe
budget, grace period and port, and **none for resources**, so 300 live
hubs are BestEffort and invisible to the scheduler. `revisionHistoryLimit: 2`
caps fleet objects at ~30,600 instead of ~54,600. A preflight names the
dataplane requirement rather than pretending to fix it.

### S10 — Give the hibernate rung an abort path *(days, dep S6)*
Kills B6. A deadline from `chert.us/idle-since`; past it, fall back to
`Suspended` (disk kept, nothing deleted).

> **[review, MAJOR] The deadline could not fire on the branch that
> matters.** A failed `poll_hub` returns first (`reconcile.rs:1173-1186`)
> with state left at `HibernateVerifying` and replicas at 1 — it never
> reaches the deadline, which is the *most plausible fleet-wide cause*.
> **Corrected:** evaluate the deadline **before** the poll, and on
> expiry with no snapshot set `Active` + a `HibernateBlocked` condition.
> Add a **concurrency** cap on hibernate-verifies, not just S11's rate —
> rate bounds entry, not concurrent population.
>
> **[review, MAJOR] The size gate compared the wrong variable.**
> `spec.persistence.size` is the *working-set budget*, not the restore
> size, and the pre-listener DR import is dominated by **object count**
> (stub creation), not GiB at a fetch rate. **Corrected:** gate on the
> manifest's **entry count and total bytes** — both of which the hub
> already knows at the barrier — at **hibernate-begin**, when the hub is
> up and can be asked.

### S11 — A fleet pacer for the two fan-out events *(days, dep S2,S6)*
Token buckets for WAKE (~30/min) and ROLL (~10/min), chart values.
Bounds a 300-share image roll and a mass wake — and caps concurrent S3
DR imports, which is what turns a wake storm into an egress bill.

> **[review, MAJOR] The pacer created a treadmill and undid S3.**
> Requeuing a token-starved share at `REQUEUE_PROGRESS` (15s) across a
> 30-minute drain is **~18,000 reconciles and ~160,000 apiserver calls**,
> and a `Paced` condition "naming the queue position" is a ticking
> message — **exactly the self-trigger S3 just removed**.
> **Corrected:** requeue at a jittered interval derived from the refill
> rate (`60s / roll_rate_per_min`) — one wakeup per expected token — and
> make the `Paced` message **constant**.

### S12 — Let something read the bucket *(weeks, dep S6)*
Kills the reachable half of B8. Before `takeover_sweep` fences anything,
probe the ancestor chain (bounded by prefix depth, 1–3 GETs) and fold
descendant detection into the LIST `sweep_foreign` already performs —
**zero extra LISTs**. Puts the check in the hub, the one process every
deployment path shares, instead of only in a control plane that half the
dangerous cases never touch. Also fixes the shared-bucket lifecycle rule
(every share pushes the same hard-coded rule ID today).

### S13 — Re-measure gate *(days, dep S5,S7,S8,S9,S10,S11)*
Re-run every composition with one before/after row **per fix, keyed to
that fix's claimed number**. Any fix that did not move its number is
reverted or re-explained, not kept on faith.

> **[review, MINOR] Merge criterion (1) was unsatisfiable.** "Every
> oracle passes in BOTH runs" contradicts five oracles that *define
> their control as the before-behaviour* — O7's ≤1.1x is precisely what
> must fail before S3. **Corrected:** split by run. BEFORE asserts only
> anti-vacuity controls; AFTER asserts the primary oracles. Label each
> oracle baseline-capture or merge-gate.
>
> **[review, MAJOR] The invalidation rule voided the runs the blockers
> depend on.** "Any run where apiserver flow-control rejections move is
> VOID" cannot coexist with O8/O9, which **require** a saturating cold
> start and a broken fleet. **Corrected:** saturation voids
> *steady-state rate* measurements only; for burst measurements
> saturation **is the result** and is recorded as an outcome.

## Track B — per-hub cost (hub-side, no distributed-logic changes)

Rev 1 was a control-plane plan and said so: "the barrier-walk CPU is a
hub-side term I do not touch." But **fleet scalability is the product of
the control plane surviving 3000 CRs and one hub not being wasteful 300
times over**, so the per-hub terms belong here. All four are local, none
alters the epoch lease's independence, and none needs multi-volume.

Ordered by ratio of win to risk.

### H1 — Reap the epoch key's noncurrent versions *(hours)*
Every CAS renewal mints a version. At the current cadence that is
**8,640 versions/day/live hub on one key** — ~3.15M/year/share — and
nothing reaps them. `s3.rs:418-427` only **logs** a recommendation to
enable `NoncurrentVersionExpiration`; the sole rule the tier actually
creates is `flint-tier-abort-mpu` (`s3.rs:764`).

**The trap:** S3 lifecycle filters match **prefix or tag, not suffix**,
and the epoch cell is `<keyPrefix>.flint/epoch` — a suffix. So a prefix
filter cannot target it without also catching data objects, whose
versions the tier's delete-marker recovery **depends on**. Correct fix:
**tag the epoch object on PUT** and use a tag-filtered rule. Inherits
S12's shared-bucket rule-ID collision problem — fix both together.

### H2 — Re-tune the lease, and write the trade down *(hours)*
`epochHeartbeatSecs` and `epochLeaseMisses` are already CRD/config
knobs; the defaults are `10s x 6` (`epoch.rs:70-77`,
`config.rs:290-295`) = 60s TTL, tolerant of 5 consecutive failed PUTs,
renewing 6x more often than the TTL requires.

**Lease exclusion trades three things: renewal cost, takeover latency,
and transient-failure tolerance. You get two.** `10 x 6` maximizes the
last two, which is exactly the setting that costs the most PUTs.
`30s x 4` gives 120s TTL, tolerates 3 failures, and is **3x cheaper**.

**What makes the longer TTL acceptable here:** since 1.31.0 a CLEAN
shutdown releases the cell, so a normal suspend/wake pays nothing. The
TTL is only paid on **unclean** death. Change the chart default, publish
the trade, and let a latency-sensitive deployment tune back down.

### H3 — Stop rebuilding a manifest nobody reads *(days — the risky one)*
`write_at_barrier` (`manifest.rs:389-403`) receives an **already-`Built`**
manifest and only then compares digests, so the digest check skips the
**S3 PUT, not the walk**. Every live hub walks its entire export, clones
the generation registry and re-serializes every entry **every 10s**,
whether or not anything changed — then usually throws it away.

Order-of-magnitude (assumed 10k files/share, 50 us/file, so treat as a
magnitude not a measurement): ~5% of a core per **idle** hub, ~15 cores
fleet-wide at 300 live. Comparable to the entire heartbeat bill, and
local to one function.

**The trap that makes this the risky item:** `beyond_rpo` is computed
FROM the built manifest, and it is **time-dependent, not
change-dependent** — a file ages past the RPO window with nobody
writing anything. A naive "nothing changed, skip the build" gate would
freeze `beyond_rpo` at a stale value, and **`beyond_rpo == 0` is a
conjunct of `rpoClean`, which is the hibernate gate that authorizes
deleting a PVC.** Getting this wrong is a data-safety bug, not a
performance regression.

So the gate must be: skip the build only when the dirty set, capture
queue and tombstones are all empty **AND** force a full rebuild every
Nth tick so `beyond_rpo` cannot go stale. **Land H3 behind Rig B
measurement and its own review** — it is the one Track B item that
touches a data-safety conjunct.

### H4 — Jitter the tier's periodic loops *(hours)*
Every periodic tier loop is a bare `interval` seeded at process start,
with no jitter anywhere. 300 hubs woken by a correlated event
phase-lock their heartbeats and barriers onto the same second, which is
the fleet-scale version of a thundering herd on the bucket. Jitter each
period +/-25%. Trivial, independently valuable, and it makes every Rig B
number less bimodal.

### Suggested order across both tracks
**H1, H2, H4 are near-free and independent — do them first**, alongside
S1 (metrics). Then S2/S3 (the correct-on-inspection bundle and the
self-trigger fix). Then the rigs. **H3 lands last in Track B**, behind
measurement and review, because of the `beyond_rpo` coupling.

## What this does NOT deliver

1. **Cross-cluster / no-operator prefix uniqueness is not closed.** S12
   catches the dangerous broad- and empty-prefix cases from the hub, but
   not a descendant appearing after the outer hub started. Closing it
   needs a bucket-side CAS'd claim registry or an external control plane.
2. **3000 ClusterIP Services remain, by design.** Per-share ClusterIP is
   what makes suspend→wake invisible to a mounted client — NFSv4 carries
   no host header and the Linux client pins the mount address at mount
   time. S9 turns this into a stated envelope (3000 cluster IPs is 73% of
   a GKE-default /20 service CIDR), not a code change.
3. **The CSI controller is still `replicas: 1`** with no sidecar leader
   election. S11 bounds the rate into it; it does not make it HA.
   **Largest untouched availability term.**
4. **The CONTROL plane is validated at 300 live (Rig A); the DATA plane
   is not.** Rig A's 300 live hubs are **stubs** — no `state.db`, no
   tier, no S3, no real PVC I/O. Rig B measures per-hub constants on
   10–30 real hubs and those are extrapolated to 300. Node packing with
   real hubs, EBS/NVMe attach storms at scale, and kube-proxy at 3000
   Services remain **extrapolations, flagged as such**.
5. **`managedFields` memory cannot be reduced here** — k8s-openapi models
   `fieldsV1` as untyped `serde_json::Value` and the Store retains it
   (17.4 KB/share, 52.4 MB at 3000). Size for it.
6. **D8 is deliberately excluded**, and this was checked rather than
   assumed: `admit_bytes` is on the client write path only, while
   hydrate — the path the ladder makes routine — uses `admit_warm`,
   which already does in-flight accounting. D8 fires identically at N=1
   and N=3000. **3000 shares is 3000 instances of one per-volume bug,
   not a scaling term.** Real, shipped, own branch.

## Review record

Four adversarial lenses produced findings; each went to an independent
refuter instructed to default to *refuted* under uncertainty. **14
survived: 1 critical, 10 major, 3 minor.** Every one is folded in above.

The three that changed the plan most:

1. **S3 defeated itself** (critical) — the fix for the self-trigger
   re-created it one field over and disabled its own write gate.
2. **S5's central lemma was false** (major, found by two lenses
   independently, then hand-verified against `conflict.rs:151-163`). The
   design pass explicitly claimed to have verified it. It would have
   shipped an arbiter that names a different winner, failing the step's
   own equivalence gate.
3. **S7's headline arithmetic did not hold** (major, found by three
   lenses) — the 6x parked-share win applied to `Hibernated` only, while
   the rig's primary composition is `IdleSuspended`.

**Pattern worth carrying forward: the errors clustered in the steps that
claimed the largest wins, and two of them were self-contradictions
between one step and another in the same plan.** A design that reviews
its own arithmetic finds neither.
