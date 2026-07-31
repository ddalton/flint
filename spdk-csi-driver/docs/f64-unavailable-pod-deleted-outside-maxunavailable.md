# F64 — a NotReady csi-node pod is deleted OUTSIDE the maxUnavailable budget

**Status:** open, live in v1.21.0 as shipped. Not fixable in flint code.
**Mitigation:** an operator precondition (below). No code change closes it.
**Found:** 2026-07-30, live on runaq (k8s v1.34.10) and independently in TLC.

---

## The mechanism

Kubernetes `pkg/controller/daemon/update.go`, `rollingUpdate()`:

```go
oldPodsToDelete := append(allowedReplacementPods,
                          candidatePodsToDelete[:remainingUnavailable]...)
```

An old-revision pod that fails `IsPodAvailable` goes into `allowedReplacementPods`
and — this is the whole finding — **does not increment `numUnavailable`, and is
never clipped by the budget**. Only `candidatePodsToDelete`, the *available* old
pods, is clipped.

Upstream's rationale is "an unavailable old pod is already broken, so replacing it
cannot make things worse." For flint that is exactly backwards: a NotReady
csi-node pod is one holding **live quiesced ublk mounts**. Deleting it kills
`spdk-tgt` under them.

The unavailable pod is therefore **added on top of** the budgeted deletion, not
substituted for it. One unavailable pod plus the one the budget allows = **two
`spdk-tgt` processes killed in a single sync**, with `maxUnavailable: 1`.

## Why this needs no readinessProbe to trigger

The shipped csi-node DaemonSet declares **no `readinessProbe` on any container**
(verified live against chart 1.21.0: `readinessProbe: NONE` on `flint-csi-driver`,
`node-driver-registrar`, `liveness-probe`). That does **not** make its pods
permanently Ready. A container with no readiness probe is Ready **iff it is
running** — so a container that is restarting, OOM-killed, crash-looping, or still
pulling makes the pod NotReady. Any of those, concurrent with a template change,
is enough.

## Evidence

**Live (runaq, k8s v1.34.10, 4 nodes, `maxUnavailable: 1`):** with two csi-node
pods made NotReady and the DaemonSet template bumped, **both were deleted in the
same second** — two `spdk-tgt` sidecars, one budget. On a synthetic 4-pod
DaemonSet the same bump with all four NotReady deleted **all four**, against a
control run where all-Ready rolled strictly one node at a time.

**Model (`formal/FlintReplication.tla`, kube-DS tranche, commit `3601a8b`):**
`Inv_DsBudgetNeverBroken` is violated in four states on the *socket* arm — i.e.
the shipped configuration, with no flint probe involved:

```
1  Init                 all pods up
2  ExtProbeRed(l1)      one container NotReady, non-data-path cause
3  TemplateBump         a routine helm upgrade
4  DsRollingUpdate      l1 AND l2 both deleted; budgetBroken = TRUE
```

## Which triggers reach it

Only those routed through `rollingUpdate()` — i.e. anything that mutates the pod
template:

* `helm upgrade` (including a no-op re-apply that changes an image tag or env)
* `kubectl rollout restart ds/flint-csi-node`
* GitOps reconciliation (Argo/Flux) syncing template drift
* any `kubectl patch`/`set image` on the DaemonSet

Failure-driven paths (spot reclaim, eviction, OOM kill, node replacement) go
through `manage()` → `podsShouldBeOnNode` → `syncNodes(createDiff)`, which has
**zero availability accounting** and so cannot exhibit this. They have their own
exposure, which is F62's, not this one.

## Operator precondition — the actual mitigation

**Before any operation that mutates the csi-node DaemonSet template, confirm every
csi-node pod is Ready:**

```sh
kubectl get pods -n flint-system -l app=flint-csi-node \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.conditions[?(@.type=="Ready")].status}{"\n"}{end}'
```

Every row must read `True`. If any pod is NotReady — restarting, OOM-killed,
crash-looping, image-pulling — **wait for it to recover before upgrading.** One
NotReady pod converts a paced roll into a two-tgt kill.

This is a precondition, not a fix. Flint cannot patch the DaemonSet controller's
arithmetic, and no flint-side guard can decline a deletion the controller has
already decided on — the roller stands down entirely when `on_delete` is false
(`maint_roll.rs:248-254`), which is the default.

## What does NOT close it

* **A readinessProbe.** It changes which pods are NotReady, not what the
  controller does with them. A *volume-scoped* probe makes it dramatically worse
  — every peer reddens when one node rolls, and the whole fleet is deleted in one
  sync. This is why the shipped predicate must be self-scoped and latching; see
  `Inv_ProbeNeverReddensLive` in the kube-DS tranche, which fails the gate for any
  cross-node or non-latching predicate.
* **`minReadySeconds`.** It makes it worse. Measured: with `minReadySeconds: 30`,
  four pods that were `Ready=True` but had merely flipped Ready recently
  (`ready=4 available=0`) were **all four deleted**. It enlarges the unavailable
  set rather than shrinking it. Keep it at 0.
* **A PodDisruptionBudget.** The DS controller issues a plain `DELETE`; PDBs bind
  only the Eviction subresource. `kubectl drain` skips DaemonSet pods entirely.
* **`updateStrategy: OnDelete`** does close it, by removing the controller from
  the loop — but only where the maintenance roller is running to drive the roll,
  and that flag is off by default.

## Related

`maintenance-drain-csi-node-roll.md` (the roller, which governs OnDelete only),
`f62-local-half-outage-and-blind-barrier.md` (the destroyer with no inverse),
`formal/FlintReplication.tla` kube-DS tranche (`3601a8b`).
