# F50 — the hot-rejoin admission window races a concurrent catch-up on the same volume, and never lands

**Status:** **FIX IMPLEMENTED** 2026-07-27 (§4–§5), but **NECESSARY, NOT
SUFFICIENT — see [F53](f53-dashboard-backend-second-orchestrator.md).** The
runaj evidence capture found a *third* controller process this fix does not
touch: the dashboard backend, which the chart gives `CSI_MODE=controller`.
Two factual corrections to §4 are recorded in F53 §2 (the second process
runs **cutover as well as** hot-rejoin — cutover's compiled default is ON,
not OFF as stated below). **Root cause CONFIRMED — §4: there were TWO
controller processes** *(at least — three, counting the dashboard)*. Found live
2026-07-27 on runai (drill 2.9, driver `1.21.0-rc2`). This is the finding
the [F48](f48-standby-rejoin-epoch-race.md) write-up flagged as its open
question — *"if an admission path can run without the volume claim, that is
its own finding."* It is — and the answer to F48's "unidentified admission
issuer at 18:38:52.8" is the same root cause: nothing runs *unclaimed*; it
runs claimed **in a different process**, where the claim means nothing.

**NOT a regression from the F47/F48/F49 wave.** Proven by ordering, §2.

**Severity:** a numReplicas=2 RWO volume stays at **1/2** indefinitely after a
leg rebuild. Materially better than the F48 wedge it replaces — the leg sits at
`standby` and catch-up keeps it chasing, data is safe, and the F48 zombie-sever
prevents the permanent pin — but redundancy is never restored without
intervention. The pending expand behind it also never completes (the
degraded-refusal belt correctly holds it).

**Vector:** drill 2.9 (destroy the remote leg's lvstore in place). Presumably
any vector that drives catch-up and hot-rejoin at the same volume
concurrently.

## 1. What happens

Catch-up rebuilds the leg and parks it at warm standby. Hot-rejoin then opens
its quiesce window to admit it — but **catch-up is still active on the same
volume**, and the two collide. Every window unwinds; catch-up re-parks the leg
at standby; the cycle repeats indefinitely:

```
standby → (hot-rejoin intent) stale → window unwinds → standby → …
```

Three consecutive attempts on runai, each failing differently:

| attempt | E_f create | outcome |
|---|---|---|
| 22:03:59 | **succeeds** | two `nvmf_delete_subsystem` land on the E_f NQN at 22:04:00.019/.020, then `add_ns` at 22:04:00.191 fails **-32602** — the subsystem was deleted out from under the in-flight window |
| 22:04:59 | succeeds | `add_ns` succeeds; unwinds later on `ns swap (old ns still visible on consumer): bdev nvme_…_0n1 did not become absent on runai-aws-3 within the AER budget` |
| 22:08:59 | fails **-32603** (residue from the above) | adopted by the F48 fix-3 probe, proceeds past the create as designed |

The concurrency is visible directly in the source node's RPC trace: catch-up's
`bdev_lvol_start_shallow_copy` / `bdev_lvol_check_shallow_copy` calls interleave
with the window's E_f `nvmf_*` calls on the same node in the same seconds. And
the controller logged, 48ms *after* the window's E_f create had already gone
out on the wire:

```
22:03:59.836  (aws-2) nvmf_create_subsystem  …:hotrejoin:<pv>      ← window is live
22:03:59.884  [CLAIMS] volume claimed by another operation — skipping this tick
              wanted_op="hot-rejoin-reconcile" held_by="catch-up"
```

So the claim registry was doing its job for `hot-rejoin-reconcile` while some
path was *already* running window mechanics against a volume catch-up held.
Two `HotRejoinScrubbed` events ("Scrubbed the stranded artifacts of an
uncommitted hot rejoin") land in the same window — the scrub is the most
likely issuer of the two deletes, and the scrub is dispatched by
`reconcile_marked`, which **catch-up itself dispatches** under its own claim.

That is the shape of the bug: the per-volume claim makes *orchestrators*
mutually exclusive, but catch-up's marked-dispatch performs hot-rejoin
maintenance (resume/adopt/**scrub**) under the *catch-up* claim, so it can
destroy E_f state belonging to a window that the hot-rejoin orchestrator is
running. Mutual exclusion between claim holders is not mutual exclusion
between the *operations* that touch E_f.

## 2. Why this is not a regression from the F47/F48/F49 wave

The wave's only change on this path is fix 3 in `hot_rejoin::prestage`: when
`nvmf_create_subsystem` returns an error the textual matcher doesn't
recognise, probe `nvmf_get_subsystems` and adopt an existing subsystem instead
of failing. **That code runs only when the create fails.**

- Attempt 1 — the one that produced the `-32602` — had a **successful**
  create. The new code never executed, and the failure still occurred.
- Attempt 3 is the only one where the adopt path ran, and it behaved as
  designed (continued past a residual subsystem to `add_host`).
- Pre-wave, runah run A hit `-32603` at the create and unwound *there*, which
  masked these later failures. The wave removed the earlier failure and
  exposed the next one in the chain — the same "each fix exposes the next bug"
  progression as F44 → F45 → F46 on runae.

Confirmed live in the same run: the F48 zombie-sever fired
(`ReplicaHeadZombieConsumerSevered … pinned by 1 dead controller
connection(s) backing no raid slot; severed`), and claim contention stayed at
`held_secs=0/4` with **zero** reservations recorded — F48 fixes 1 and 2 both
working, run A's ~4-minute starvation gone.

## 3. Answers to §1's puzzles (from the code + bundle, 2026-07-27)

- **The window DOES hold `OP_HOT_REJOIN` for its whole duration** — the
  orchestrator's spawned task owns the claim guard until it ends
  (`hot_rejoin.rs`, the Rejoin arm). Claim discipline within one process is
  sound. That is precisely what made the observations impossible to explain
  in-process:
- The controller's own hot-rejoin orchestrator was **refused** at
  22:03:59.884 (`held_by="catch-up"`) and again at 22:04:59.884 — yet
  windows prestaged at 22:03:59.836 and 22:04:59.969 anyway.
- Attempts 1 and 2 emitted `HotRejoinUnwound` events but the controller log
  has **no** `Rejoin failed (unwound) — backing off` line for them (attempt
  3 has one, 22:09:00.010). Their spawn-arm logs went to a log nobody
  captured.
- A fourth window ran at ~22:11:59, **inside attempt 3's 300 s back-off**.
  Back-off state is per-process.

One process cannot produce that trace. Two can, exactly.

## 4. Root cause — CONFIRMED: a second controller process with its own claim registry

**The `spdk-controller-operator` pod is a full second controller.** The
chain, every link verified:

1. Trove's SPDK-mode installer passes `--set spdkOperator.enabled=true`
   (`trove/backend/.../flint_csi.rs`) — every trove cluster deploys the
   "operator" pod. The chart's own default has been `false` since the
   audit-L4 review (2026-07-04).
2. The operator's intended module is **dead code** (`controller_operator.rs`;
   its `[[bin]]` is commented out of Cargo.toml). The pod runs the image
   entrypoint: the standard `csi-driver` binary.
3. The pod sets no `CSI_MODE`, and `main.rs` defaults it to **`"all"`** —
   which includes the controller role and therefore every
   `mode == "controller" || mode == "all"` orchestrator block.
4. In the operator pod, chart-driven env is absent, so orchestrators run at
   compiled defaults: epoch scheduler OFF, catch-up OFF — and **hot-rejoin
   ON** (`HotRejoinTriggerConfig::default().enabled = true` since
   `076985d`, v1.19.0).
   **CORRECTION (runaj, 2026-07-28): cutover is compiled-default ON too**
   — the captured startup log shows `[CUTOVER] Reassembly cutover
   orchestrator started` right next to hot-rejoin's line. So the pod is a
   second **cutover + hot-rejoin** controller, not hot-rejoin-only, and
   since cutover owns RWX replacement admission it could perturb the
   cutover path as well. See [F53](f53-dashboard-backend-second-orchestrator.md) §2.
5. The F43 claim registry is **in-process** (`volume_claims::global()`).
   The operator's registry is empty; its `try_claim(OP_HOT_REJOIN)` always
   succeeds. Nothing serializes its windows against the real controller's
   catch-up, scrubs, or anything else.

The failure mechanics then follow §1 exactly: the operator's window writes
intent (standby→stale + marker) and prestages E_f; the real controller's
catch-up dispatch decodes stale+marker with no head in the raid — **which is
also exactly what a live pre-flip window looks like** — and scrubs the E_f
export out from under the in-flight window (attempt 1's `-32602` at
`add_ns`, 200 ms after the create), plus a defensive unquiesce against the
window's own lease. Every window dies at whichever step the concurrent
scrub's deletes landed before; catch-up re-parks the leg; the operator's
next eligible tick opens the next doomed window. `standby → stale → standby`
forever.

Why it appeared only now: the pod ran on every trove cluster all along, but
was **inert until v1.19.0 flipped hot-rejoin default-ON** (the audit's
2026-07-04 "verified unreachable" was true *at the time* — no orchestrator
was default-enabled then, and its SPDK-socket calls fail). Drill 2.9 (the
first vector that parks a standby under an attached consumer, triggering
hot-rejoin) first ran 2026-07-27 on runah — and its "F48 run-A wedge"
(including the unidentified 18:38:52.8 admission) was this same second
process. Honest limit: per-attempt attribution of attempt 2's issuer
(operator back-off should have blocked it; a CAS-conflict-prolonged unwind
spanning the tick, or an unwind panic skipping the back-off insert, both
fit) cannot be settled without the operator pod's log, which no drill
captured — harness lesson: **capture logs from every pod running the
driver image, not just the nominal controller.**

## 5. The fix (implemented 2026-07-27)

Layered — eliminate the second process, serialize the windows that can
still produce one, and make the destructive decode tolerant of the shape:

1. **Chart: the vestigial `spdk-controller-operator` Deployment is
   REMOVED.** Not default-off — gone. An installer still passing
   `spdkOperator.enabled=true` (trove does) now sets an unused value.
   Revival requires a dedicated binary and identity-legible mints (audit
   L4's precondition) — at which point re-adding a template is the easy
   part.
2. **Chart: the controller Deployment upgrades with `strategy: Recreate`.**
   A rolling upgrade briefly runs old+new controllers side by side — the
   same two-registry shape as the operator pod, on every roll (the runai
   3.6e contamination). With in-process claims as the correctness backbone,
   controller instances must never overlap.
3. **Marker grace (`FLINT_HOT_REJOIN_RECONCILE_GRACE_SECS`, default 300):**
   the intent write now stamps `hot_rejoin_at` alongside the marker
   (refreshed at the flip), and `reconcile_marked` leaves any marker
   younger than the grace completely alone — no scrub, no defensive
   unquiesce, no marker clear. A young stale+marker is indistinguishable
   from a live window by record state, so the reconciler stops pretending
   otherwise. Old markers (crashed windows) reconcile as before, ≤5 min
   later — their artifacts are inert and the data-plane quiesce lease
   self-expires in `lease_ms` regardless. Records from older builds carry
   no timestamp and reconcile immediately (the pre-F50 behavior).
4. **Visibility:** `main.rs` warns loudly when `CSI_MODE` is unset and
   defaults to `"all"` — the exact silent step that armed the operator pod.

**Deliberately deferred:** kube Lease-based leader election for the
orchestrator block — the complete answer to "two controller processes",
and the right follow-up, but a bigger change than a validated release wave
should absorb.

> **The claim that items (1)–(3) "close every known two-process window
> today" did not survive contact with the next cluster.** They closed every
> window *I had looked for*. The runaj capture found the dashboard backend
> running the same shape, and F53 replaces the CSI_MODE inference with an
> explicit `FLINT_ORCHESTRATORS` grant. Read (1)–(4) below together with
> [F53](f53-dashboard-backend-second-orchestrator.md) §4 — that is the
> actual fix set.

Tests: `reconcile_leaves_a_young_marker_alone_until_the_grace_lapses`
(hot_rejoin — the runai kill shot replayed against a just-stamped marker:
E_f export, head, snapshot, quiesce, marker, events all untouched; then the
same state aged past the grace scrubs normally) and
`hot_rejoin_intent_stamps_marker_time_and_clears_it_with_the_marker`
(replica_sync — stamp/age/clear lifecycle, legacy-record tolerance). Full
suite: **886 green**. `helm template --set spdkOperator.enabled=true`
renders zero operator manifests.

## 6. Drill gate — DONE

Drill 2.9's in_sync poll now tracks `standby → stale` flips (each one = a
window opened on the parked leg) and **fails by name as F50 after the third
flip without in_sync**, instead of timing out generically — a silent 15-min
timeout is what let this hide behind F48. The flip count rides the PASS
line too, so a healthy run records how many windows it took.

Related: [F48](f48-standby-rejoin-epoch-race.md) (the wedge this replaces; its
fixes are proven working in the same run), [F43 claim arbitration]
(f43-rwx-replacement-admission.md), C6 in `catchup.rs`.
