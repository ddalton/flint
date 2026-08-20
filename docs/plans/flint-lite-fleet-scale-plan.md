# Taking flint-lite to 3000 shares / 300 live hubs

> **Status: plan of record, no code yet.** Produced 2026-08-20 by a
> 51-agent ultracode pass — five parallel readers over the fleet-critical
> subsystems, three independent designs from deliberately opposed priors,
> one adjudicating synthesis, then four adversarial review lenses whose
> every finding went to an independent refuter. **14 findings survived
> refutation (1 critical, 10 major, 3 minor) and are folded in below**;
> §Review record says what each one changed.

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
| B1 | **`conflict::admit` is O(rank²) per reconcile**, re-deriving a constant | 13.0 ms median share, 51.1 ms newest, at N=3000. **0.17 core steady; 3.5 cores at the legal clamp floor.** Chart requests **50m**. Full-fleet sweep N³/6 ≈ 4.5e9 calls ≈ **46–52 CPU-seconds** |
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

### S5 — Arbitration becomes a table built once per fleet change *(days, dep S4)*
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
> successor. **Do not budget or land S5 until the lemma is restated.**
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

### S6 — The cluster rig *(days, dep S1)*
`tests/regression/fleet-scale-kind.sh`. 3000 **real** CRs across M
namespaces; parked shares pre-stamped with the operator's own durable
annotations so they cost zero pods from birth. A stub hub that cannot
drift replaces the full flint-pnfs pod, so one machine hosts 60+ live
shares. Compositions: **A** 300 Active + 2700 IdleSuspended; **B** 300 +
2700 Hibernated; **C** the legal clamp floor; **D** all Pending
(the broken fleet). Every oracle carries an anti-vacuity guard.

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
Kills B6. A deadline from `flint.io/idle-since`; past it, fall back to
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
4. **Validated at 3000 CRs with ~16–60 live hubs, not 300.** The rig
   trades pod fidelity for CR count because CR count is what breaks. Node
   packing, attach rates, the aggregate epoch PUT rate and the manifest
   barrier's export walk stay **extrapolations, flagged as such**.
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
