# Rig A — the control plane at 3000 shares / 300 live

**Cluster `runbv`** (all-spot, 1 CP + 4 workers, `i4i.xlarge`, us-west-1),
2026-08-20, ~1.5h, **≈$0.90**. Torn down; all 5 instances terminated.

The design target had been named for months and asserted by nothing.
This is the first time it has been stood up.

## The headline: the OOMKill was real, and the fix holds

| | **Before** (published 1.32.0) | **After** (concurrency cap + indexed arbiter + de-tick) |
|---|---|---|
| operator pods | **both `OOMKilled`, exit 137** | `Running` |
| restarts | **8 each**, still climbing at 26 min | **0** |
| operator memory | killed at the **256Mi** limit | **49–53 MiB** |
| live hubs reached | **131 of 300** after 26 min | **300 of 300** |
| fleet converged | **never** | **1389s** |

Nodes were healthy throughout, so this was the container limit, not node
pressure. The prediction — unbounded controller concurrency
(`Config::default()` is `concurrency: 0`) × a whole-fleet snapshot per
reconcile against a 256Mi limit — was derived from reading code and
arithmetic. **It reproduces exactly.** The CrashLoop then re-enters the
same herd, which is why the fleet never converged rather than converging
slowly.

## Steady state, measured on a settled fleet

3000 shares, 300 live, 401s window, read from the **apiserver's own**
`apiserver_request_total` rather than from anything the operator says
about itself:

```
flintshares APPLY      11.83/s      <- status writes with NOTHING changing
flintshares TOTAL      11.85/s
child objects          87.15/s      (deploy/svc/cm/pvc)
whole apiserver       145.87/s
operator             98m / 53Mi  (leader), 22m / 49Mi (standby), 0 restarts
per share             0.237 req/min, 0.237 writes/min
```

**The unconditional-apply term is real and now has a number.** 11.83
server-side applies per second on `flintshares` alone, plus 87/s of
child-object traffic, on a fleet where nothing is changing. That is S7's
target (the render-hash apply gate) and it is no longer an estimate.

## Fleet footprint, as built

```
flintshares 3000   deployments 3013   services 3009
configmaps  3029   persistentvolumeclaims 3032        ~15,000 objects
```

Matching the plan's ~15,000 prediction. Two things worth pinning:

- **Suspended shares keep their PVC** — 3032 PVCs for 3000 shares, while
  only 300 were live. At real storage prices that is the dominant fleet
  cost, and it is invisible from the pod count.
- **300 live pods spread 74/78/71/77** across four workers, comfortably
  under kubelet's 110/node ceiling — which is exactly the ceiling that
  made a single-machine rig unable to exceed ~16–60 live and forced the
  two-rig split.

## What the rig's own guards caught

The guards exist because a load test that stands up 3000 CRs and reports
"fine" is the easiest false pass available. Three fired during
development, each on a real defect **in the rig**:

1. **`/status` is never polled for a share with no `spec.idle`.**
   `poll_hub` runs from the idle evaluation, so the first run measured a
   fleet the operator was not polling at all — the poll term, one of the
   things the rig exists to measure, was silently absent. Guard A3
   (HubReachable count) caught it.
2. **`apiserver_request_total` carries no client or user-agent label.**
   The first collector filtered on one and matched nothing, reporting
   `0.00/s` for a working fleet. Attribution is by *resource* instead.
   Guard A4 caught it.
3. **A 90s window is shorter than the 300s settled requeue**, so it
   landed in the quiet gap and reported ~0 for a healthy fleet. The
   window is now floored above the requeue period, and A4's threshold
   scales with fleet size and window rather than being a flat number.

A fourth was found in the reporting: **`APPLY` is a write verb**
(server-side apply) and was being counted as a read, which is why an
early run showed `writes 0.00/s` next to `APPLY 19.51/s`.

## What this does NOT say

- **Nothing about the data plane.** The 300 live hubs are
  `flint-hub-stub`: no `state.db`, no tier, no S3, no real PVC I/O. Per-
  hub constants — barrier-walk CPU, epoch PUT rate, real RSS, attach
  time — are Rig B's job, on 10–30 real hubs.
- **The arbitration win is inferred here, not isolated.** The indexed
  arbiter is measured directly by
  `measure_admit_against_the_index_at_fleet_scale` (18,516x on a full
  sweep at N=3000); on the cluster it is one of three changes in the
  same image, and without the S1 metrics endpoint the terms cannot be
  separated. That is an argument for landing S1 before the next run.
- **Storage was `local-path`, not `flint-spdk`.** Deliberate: 300 real
  CSI attaches would have made storage the bottleneck and measured
  something other than the control plane. CSI attach rate at fleet scale
  remains unmeasured.

## Next, aimed by these numbers

1. **S7's apply gate** — the 11.83/s + 87.15/s of writes-with-nothing-
   changing is now a measured baseline to beat.
2. **S1 metrics** — three changes shipped in one image and could not be
   attributed on-cluster. Land it before the next run.
3. **Rig B** — per-hub constants, on real hubs.

---

# Rig run 2 — S7 and S9, cluster `runbw`

Same shape (1 CP + 4 workers, `i4i.xlarge`, all-spot), 2026-08-20, ~2h,
≈$1.20. Torn down.

## S7: a parked fleet now costs essentially nothing

3000 shares, **all parked**, measured after the seeding burst drained.
Parked-only is deliberate — it isolates exactly the term S7 targets, and
it converges in seconds because it creates no pods at all.

```
t+200s   flintshares 12.26/s    child 61.75/s     <- seed burst draining
t+400s   flintshares  0.01/s    child  0.19/s     <- settled

sample 1 flintshares  0.023/s   child  0.228/s
sample 2 flintshares  0.006/s   child  0.229/s
sample 3 flintshares  0.024/s   child  0.218/s
operator 1m CPU / 77 MiB, holding 3000 shares
```

Against run 1's converged baseline of **~99 writes/s** (11.83 flintshare
applies + 87.15 child), a settled parked fleet is now **~0.24/s**. The
comparison is not apples to apples — run 1 had 300 live shares and this
had none — but the parked contribution is what S7 targets, and it has
gone to approximately zero.

## S9: making hubs schedulable makes their cost visible

The first attempt at run 2 **failed to converge, and that was the
result**: 162 of 300 hub pods sat `Unschedulable — Insufficient cpu`.
The new default hub request (100m/128Mi) means **300 live hubs ask for
30 vCPU**, and the rig's four workers have 16 allocatable.

This is S9 working, not S9 breaking. Before it, hubs were BestEffort:
the scheduler saw a zero-cost pod, packed by pod count alone, and the
capacity was borrowed silently. The number is now visible and has to be
planned for. **The rig's stubs are not hubs**, so they now carry
explicit tiny requests — otherwise the run measures cluster capacity
rather than the control plane.

## A run that was VOID, and why it is recorded

The first 3000/300 attempt on this cluster produced `37.97` flintshare
writes/s and `271/s` child — WORSE than the baseline. It is not a valid
S7 measurement and is not quoted as one: the fleet never converged (131
Pending, 67 Starting), and a not-Ready share requeues every 15s, which
is the S8 term that has NOT been built. It was comparing a settled fleet
to a thrashing one.

The proximate cause was **kube-apiserver saturation** — 1.5 of 4 cores
on the control-plane node — under the churn of a delete-and-recreate
cycle. Worth recording on its own: at 3000 shares the KUBERNETES control
plane needs sizing, independently of anything flint does. Deployment
`Available` status ran ~20 minutes stale while the pods themselves were
`1/1 Running` and all 300 hubs were `HubReachable` — so the operator was
fine and the fleet was not.

## Archive and restore, measured

The full cycle, on a live 3000-share fleet:

| step | result |
|---|---|
| archive (`reclaim: Retain`, default) | Deployment, Service, ConfigMap GC'd; **PVC kept** |
| archive (`reclaim: Delete`) | **all five objects gone, PVC included**; bucket untouched |
| restore (recreate the CR) | **Ready in 30s**, every object back |

An archived project therefore costs **zero Kubernetes objects**. That
changes the fleet sizing premise: the 3000 in "3000 shares" is the
count of NON-ARCHIVED projects, not of all projects that ever existed.

## What is still not measured

- **S7 against a converged fleet WITH live shares.** The parked-only
  experiment isolates the term cleanly, but the mixed steady state at
  3000/300 has not been re-measured on the fixed operator.
- **S8** (real backoff) is not built, and run 2 showed exactly why it
  matters: a fleet that cannot converge pays 15s requeues forever and
  drives the apiserver harder than a healthy one.
