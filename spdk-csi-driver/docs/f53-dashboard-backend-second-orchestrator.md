# F53 — the dashboard backend is a second controller process, and F50's fix left it running

**Status:** **FIXED and LIVE-VALIDATED** 2026-07-28 (§4; A/B on runaj in
§3: pre-fix the *dashboard* performed a live raid admission, post-fix the
*controller* does and the dashboard is silent). Found on runaj **before any drill ran**, by the
evidence capture that [F50](f50-hotrejoin-window-concurrency.md) §4 added as
its harness lesson: *"capture logs from every pod running the driver image,
not just the nominal controller."* The first time that lesson was actually
applied, it found the next instance of the same bug.

**Severity:** identical to F50 — a numReplicas=2 volume can cycle
`standby → stale → standby` indefinitely and never restore redundancy,
because two processes run admission windows that scrub each other's state.
Data stays safe; redundancy does not come back without intervention.

**Reach: worse than F50.** The `spdk-controller-operator` pod was a *trove*
artifact (`--set spdkOperator.enabled=true`); a plain `helm install` never
had it. The dashboard is enabled by default in the chart's own values, so
**every** flint install with the dashboard on has been running two
orchestrator processes.

## 1. What was found

On a freshly provisioned runaj (trove project 54, chart 1.20.0), every pod
running the `flint-driver` image, with its resolved `CSI_MODE`:

```
flint-csi-controller-…      CSI_MODE=controller
flint-csi-node-… (×5)       CSI_MODE=node
spdk-controller-operator-…  CSI_MODE=<UNSET -> defaults to all>     ← F50
spdk-dashboard-…/dashboard-backend  CSI_MODE=controller             ← F53
```

Both non-controller processes logged, at startup:

```
🔁 [CUTOVER]    Reassembly cutover orchestrator started  cooldown_secs=900 max_lag=1
♻️ [HOT_REJOIN] Hot-rejoin orchestrator started          max_lag=1 retry_backoff_secs=300
```

The dashboard's `CSI_MODE=controller` is not an accident or a stale
override — it is hard-coded in the chart
(`templates/dashboard.yaml`), because the backend reads the
controller-side query surface. But the orchestrator block was gated on
exactly that string, so asking for the query surface also started the
cluster-wide singletons.

**After** `helm upgrade` to the local 1.21.0 chart (F50's fix: the operator
Deployment is gone), the dashboard backend was still there, still on the
driver image, still running both orchestrators — now on `1.21.0-rc3`.

## 2. Two corrections to the F50 write-up

1. **F50 §4 says the second process ran "epoch scheduler OFF, catch-up OFF,
   cutover OFF — and hot-rejoin ON", i.e. hot-rejoin only. Wrong: cutover's
   compiled default is ENABLED.** Its startup line is right there in the
   captured log next to hot-rejoin's. Epoch and catch-up are genuinely off
   (they log "disabled (set … to activate)").
2. Because cutover owns **RWX replacement admission** — the NFS server
   bounce — a stray process could interfere with the cutover path too, not
   just hot-rejoin. F50 §5 attributes "the runai 3.6e contamination" to a
   mid-drill helm roll running old+new controllers side by side. That
   remains a real hazard (hence `strategy: Recreate`), but it is no longer
   the only candidate for what perturbed 3.6e: the operator pod and the
   dashboard backend were both running cutover the whole time.

Neither correction changes F50's fixes — they are all still necessary. They
change the claim that those fixes were *sufficient*.

## 3. Live on runaj: the dashboard performed the admission

Drill 2.9 (destroy the remote leg's lvstore in place) on rc3, operator
already pruned, dashboard still stock. **The drill PASSED** — leg `in_sync`
at 308s, raid 2/2, db PASS, zero `standby → stale` flips recorded.

It passed because the dashboard did the work:

```
01:11:06.9  CONTROLLER  [CATCHUP] Replica caught up to warm standby   node=runaj-aws-1
01:11:23.2  DASHBOARD   [HOT_REJOIN] Window committed                 node=runaj-aws-1 window_ms=239
01:11:52.3  CONTROLLER  [CLAIMS] volume claimed by another operation — skipping this tick
                                  wanted_op="hot-rejoin-reconcile" held_by="catch-up"
01:12:13.5  DASHBOARD   [HOT_REJOIN] Rejoin complete                  window_ms=239 localized=true
```

The real controller's hot-rejoin was refused by its own catch-up claim —
verbatim the observation [F50](f50-hotrejoin-window-concurrency.md) §1 is
built on — while **the dashboard backend opened a 239 ms quiesce window,
admitted the leg into the live raid, and localized the esnap chain.** The
monitoring UI's backend restored the volume's redundancy.

This is not a benign stray process. It is a second process performing raid
admission surgery on live volumes, and its window happened to land in the
17 s gap before catch-up's reconcile could scrub it. On runai the same
shape lost the race three times in a row and the volume cycled
`standby → stale → standby` indefinitely. **Nothing in the system decides
which of those two outcomes you get.**

Two honest consequences:

- **The livelock was NOT reproduced here**, so "the dashboard alone is
  sufficient to wedge a volume" remains an inference from the shared
  mechanism, not a reproduced failure. What *is* proven live is the
  precondition: the dashboard runs unserialized admission windows against
  volumes the controller is concurrently working on.
- **This run's PASS is not evidence that F50's fix works.** The admission
  came from the wrong process. The F50 grace and the claim discipline in
  the controller were never exercised on the path that mattered. Only a
  run where the *controller* performs the admission validates F50 — that
  is run B, on the F53-fixed build.

The zero flip count is explained by the same log: the whole window took
239 ms and completed between two of the drill's 10 s polls. A flip counter
cannot distinguish "one fast window that landed" from "no window at all",
which is why §5 asserts the cause directly.

### Run B — the fix, same cluster, same drill (rc4)

```
01:27:08.6  CONTROLLER  [CATCHUP] Replica caught up to warm standby   node=runaj-aws-1
01:27:54.7  CONTROLLER  [HOT_REJOIN] Window committed                 node=runaj-aws-1 window_ms=234
01:28:38.2  CONTROLLER  [HOT_REJOIN] Rejoin complete                  window_ms=234 localized=true
```

Dashboard admission activity in run B: **zero** lines matching
`[HOT_REJOIN]` or `[CUTOVER]`; its only orchestrator line is the decision
`[ORCHESTRATORS] DISABLED — FLINT_ORCHESTRATORS is set to disabled`.
`single_orchestrator` reports exactly one enabled process in the namespace.

Drill result: `in_sync` at 264s (vs 308s in run A), raid 2/2, db PASS, max
ledger stall 1s (vs 10s). **This is the first run in which the CSI
controller performed the admission on this vector**, and therefore the
first that validates F50's claim discipline rather than a bystander's luck.

Artifacts: `runA-dashboard-did-the-admission.log`, `runA-controller.log`,
`runB-controller.log`, `runB-dashboard.log`,
`orchestrator-decisions-rc4.txt` (all pods' decisions) in
`tests/chaos/artifacts/f50-two-controllers-runaj/`; drills in
`2-2.9-1785200826/` (A) and `2-2.9-1785201855/` (B).

Artifacts: `tests/chaos/artifacts/f50-two-controllers-runaj/`
(`runA-dashboard-did-the-admission.log`, `runA-controller.log`,
`pods-running-driver-image.txt`, `operator-pod.log`) and
`tests/chaos/artifacts/2-2.9-1785200826/` (the drill itself).

## 4. The fix

The root error is conceptual: **`CSI_MODE` says which gRPC services a
process serves. It was also being read as "may this process own
cluster-wide singletons."** Those are different questions with different
answers, and every instance of this bug family — the operator pod, the
dashboard backend — comes from conflating them.

So the second question is now asked separately, in a new module
`orchestrator_role.rs`:

- **`FLINT_ORCHESTRATORS`** is an explicit grant. The chart sets
  `enabled` on the controller Deployment and `disabled` on the dashboard
  Deployment. Nothing else sets it.
- **Unset** falls back to the historical rule — controller/all — *minus*
  any process that declares `ENABLE_DASHBOARD`. That keeps a hand-rolled
  or kind/dev single pod (`CSI_MODE=all`, no chart) self-healing exactly as
  before, while the one shipped second-controller stops.
- **An unparseable value falls through to the default rule rather than
  meaning "enabled"** — a typo must not hand a second process the
  orchestrators.
- The decision is logged once, with its reason, by every process. An
  operator can now answer "who is running the orchestrators here?" by
  grepping `[ORCHESTRATORS]` across the namespace instead of inferring it
  from which log lines are missing.

Gated by it: the epoch scheduler, catch-up, cutover, hot-rejoin, and the
NFS server-pod liveness reconciler (also a mutating background loop that
two processes must not both run). The gRPC `ControllerServer` and the
snapshot-CRD preflight stay on `CSI_MODE` — they are genuinely about which
services this process serves.

Tests: six in `orchestrator_role.rs`, including
`the_dashboard_backend_never_runs_orchestrators` (the exact shipped env:
`CSI_MODE=controller` + `ENABLE_DASHBOARD=true`, no grant) and
`unset_grant_keeps_the_historical_controller_rule` (the no-regression case).

**Still deferred, and now better justified: kube-Lease leader election.**
This fix is a narrowing, not a proof. A *fourth* process that sets
`CSI_MODE=controller` with no `FLINT_ORCHESTRATORS` still runs
orchestrators, and this family has now produced two instances in one day —
each found only by looking, never by a failing assertion. A lease is the
only construction that makes "exactly one" true by design rather than by
audit. `orchestrator_role.rs` is where it belongs when it lands.

## 5. Drill gate

Drill 2.9's F50 cycle gate (fails by name after >2 `standby → stale` flips)
covers the *symptom* for both findings. The *cause* needs its own
assertion, because the symptom only shows up on the one vector: the harness
should assert that **exactly one pod in the driver namespace logs
`[ORCHESTRATORS] ENABLED`**. That is a cheap, vector-independent invariant
which would have caught F53 — and F50 — at deploy time instead of at
drill time.

Related: [F50](f50-hotrejoin-window-concurrency.md) (same family, the
process this one was hiding behind),
[F48](f48-standby-rejoin-epoch-race.md) (the wedge F50 replaced),
[F43 claim arbitration](f43-rwx-replacement-admission.md) (the in-process
registry whose scope this is all about).
