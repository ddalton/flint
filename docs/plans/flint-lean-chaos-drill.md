---
title: flint-lean chaos drill — the kind-runnable half of Phase 6
status: RESULTS (2026-08-25) — 12 legs green on kind against MinIO
type: drill-record
created: 2026-08-25
governs:
  - lean/e2e/run-chaos.sh (the drill)
  - lean/e2e/chaos.yaml (the rig)
plan-of-record: docs/plans/flint-lean-plan.md
formal-model: lean/formal/LeanSubtree.tla
---

# flint-lean chaos drill

`run.sh` and `run-chart.sh` prove the happy path: injection, the
checkout gate, a publish, a refusal. This drill proves what happens
when the happy path is **interrupted**.

It exists because the two things that most need evidence are the two
things neither the unit battery nor the formal model can supply:

- the model **abstracts** the scan as atomic and the takeover
  observation as one action, so the two-consecutive-scans deletion rule
  and the real 6-quiet-poll claim protocol are unrepresentable there;
- the unit battery drives a memory store in one process, so a process
  killed between two PUTs, a pod that loses its emptyDir, and a
  SIGSTOPped straggler thawing after it has been deposed are all
  outside its reach.

Everything here runs on kind against MinIO. What it deliberately cannot
prove is listed at the end.

## Anti-vacuity is the design

The lite drill's finding was that **24 of 41 proposed legs would have
passed if the thing they tested were broken**. The rule adopted here is
stricter than "assert the outcome": every leg must also observe its own
**precondition**, and a leg that cannot is a FAILED leg, not a green one.

Concretely — a crash leg that did not land inside the barrier has
tested nothing, so it fails:

```sh
[ "$landed" -gt 0 ] || { bad "kill landed before the first upload — leg vacuous"; return 1; }
objexists "$P/files/f$(pad $N).txt" && { bad "the last upload completed before the kill — leg vacuous"; return 1; }
objexists "$P/.flint/lean/manifest"  && { bad "the manifest CAS completed — the kill missed the barrier"; return 1; }
```

This mattered immediately. The first full run scored 4/10, and **four
of the six failures were the guards refusing to let a leg pass without
its precondition** — the barrier was finishing in under a second, so
the kill was landing after it, not inside it. A drill without those
guards would have reported 10/10 green while never once interrupting a
barrier.

The fix was to stop racing a `sleep` and make the timing observable
instead. Upload order is a `BTreeSet` walked at fan-out 1, i.e.
lexicographic; zero-padded filenames make that numeric order too, so
"is the barrier mid-flight?" becomes two cheap HEADs — an early key
present, the last key absent — and the drill kills the barrier only
once it can *see* it running.

## The rig

Deliberately **operator-free**. The webhook-injected `run` loop
publishes on its own clock, which is exactly what a crash-timing drill
cannot have. The sync containers run `sleep infinity` and the script
execs `flint-sync checkout|barrier` into them, so every barrier
boundary is under the drill's control.

Two rows of the restart matrix have to be distinguishable, so the rig
provides both gestures:

| gesture | what dies | what survives | which path it exercises |
| --- | --- | --- | --- |
| `kill -9 <flint-sync>` | the process | the emptyDir | self-recognition (persisted incarnation id) |
| `delete pod --force` | the pod | nothing | takeover (6 quiet polls + rotation) |
| `touch /work/DIE` | the container | the emptyDir | container restart over a live tree |

That third one has a trap worth recording: **`kill 1` inside a
container is silently a no-op.** The kernel refuses default-action
signals to PID 1 of a namespace from inside that namespace, so the
obvious gesture does nothing and a leg built on it would "pass" without
ever restarting anything. The rig uses a liveness probe on a sentinel
file instead (`test ! -f /work/DIE`), with startup clearing the
sentinel, and asserts `restartCount` actually incremented.

Legs never share a subtree (`tenants/c1` … `tenants/c11`): a crash leg
leaves orphan objects and a held lease behind *by design*, and a shared
prefix would make one leg's debris the next leg's starting state. The
drill resets both halves of the state — fresh emptyDirs and an emptied
subtree — so it is re-runnable.

The oracle reads the bucket **directly** through `mc`, never through
the sidecar's own code, so one bug cannot hide another.

## The legs

| # | Claim | Precondition guard |
| --- | --- | --- |
| C1 | A crashed barrier leaves the bucket coherent (uncited orphans, never a dangling manifest), and the retry adopts its own crashed PUTs instead of parking them as foreign | some uploads landed, the last did not, no manifest |
| C2 | Pod loss ⇒ loss is exactly the RPO; the successor's checkout reproduces precisely the published set; rotation bumps seq without changing entries | burst partially uploaded; successor observed *waiting* on the lease |
| C3 | A deposed straggler's manifest CAS never lands (`Inv_NoStragglerInstall`) | straggler frozen mid-upload; successor really rotated |
| C4 | A container restart never re-materializes (`Inv_NoResurrection`); a delete needs **two** consecutive absent scans | `restartCount` incremented; barrier 1 must *not* delete |
| C5 | HITL write vs dirty local file: both versions stay recoverable — the bytes, not just the reference (`Inv_HITLDurable`) | the two versions must actually differ |
| C6 | Per-request epoch validation refuses a stale epoch on every sidecar-facing verb | the same verbs at the *current* epoch must succeed |
| C7 | An open barrier window refuses HITL writes with 409 + `Retry-After`, and releases them after | the same write must succeed once cleared |
| C8 | The gateway is control plane only — agents keep publishing through an outage | HITL write must succeed before and after, fail during |
| C9 | The state-dir occupancy lock refuses a second sidecar over one tree | the lock must *release* afterwards |
| C10 | The GC never reaches outside `<prefix>/files/`, including a **string-prefix** neighbour (`tenants/c10` vs `tenants/c10-sibling`) | the GC must have actually deleted something that run |
| C11 | A HITL upload survives later barriers — including a GC — with the sync verb never invoked | each later barrier must have done real work |
| C12 | Proxy unreachable ⇒ publish fails, checkout **wedges**, and the agent-start marker is never written over an empty tree | both must succeed again once the proxy answers |

C11 is the model's load-bearing finding made physical. `LeanDirectMerge`
was *refuted*: merge alone preserves a foreign entry for exactly one
barrier, because Finish absorbs it into the merge base and a later
local delete then destroys the citation. The leg therefore runs four
barriers with an unrelated delete in the middle and demands the human's
file is still cited at the end.

## Findings

### 1. The P5 data-plane residual, measured

C3 freezes a straggler mid-barrier, lets a successor depose and rotate,
then thaws it. The control plane holds exactly as designed: the
straggler self-fences before its CAS
(`fenced: cell at epoch 4 holder … (we are epoch 3)`), the manifest
stays at the successor's seq, and zero straggler paths are cited.

But the drill also counts objects before and after the thaw, and the
data path is a different story: **the deposed straggler landed 7,591
further data PUTs after rotation** (409 → 8,000). Those are uncited
orphans, so no reader sees them and no invariant is violated — but they
are real writes by a writer that had already lost the lease, and only
proxy-side epoch enforcement can stop them. This is the residual the
proxy conversation needs to cover, now with a number attached rather
than a hypothesis.

### 2. The gateway and the proxy are two failure domains, not one

Plan §2.2 states the failure mode as a single line: "**gateway/proxy
down** ⇒ publishes pause AND checkouts/restarts wedge AND sync is
unavailable AND HITL UI writes fail loudly", and Phase 3 turns that
into an acceptance criterion — an "outage drill asserting ALL FOUR
effects". C8 and C12 drill the two halves separately, and they behave
nothing alike:

| outage | publishes | checkout | sync | HITL writes |
| --- | --- | --- | --- | --- |
| **gateway** down (C8) | continue | works | works | fail loudly |
| **proxy** unreachable (C12) | fail | wedges | dead | fail |

All four stated effects belong to the **proxy**. The gateway costs only
the fourth. That is the better arrangement — a control-plane outage
cannot cost an agent its work — but the plan's single sentence, and the
Phase 3 criterion built on it, would be unsatisfiable if you drilled
the gateway alone and concluded the system was broken. Worth splitting
in the plan.

The mechanism: the shipped `flint-sync` writes the manifest, window and
inbox cells **directly** to the store
(`lean/sidecar/src/barrier.rs:257, 261, 384, 467`) and links no HTTP
client at all (`lean/sidecar/Cargo.toml` has no `reqwest`/`hyper`/
`ureq`). The gateway's sidecar-facing verbs are implemented and
correct, but the sidecar is not one of their callers.

This is a **known deferral, now measured** — not a surprise. The plan's
own status header already lists "routing sidecar barriers through the
gateway verbs by default" as open. What the drill adds is the
consequence: for as long as that deferral stands, the failure-mode
sentence and the Phase 3 acceptance criterion describe a system that
does not exist yet, and P5 covers a writer that is not the one doing
the publishing.

Which sharpens what P5 means. `lean/formal/README.md` records
`LeanEpochOnlyHolds` — per-request epoch validation alone fences the
straggler's manifest CAS. C6 proves the **gateway** really enforces
that, so the claim holds for gateway-mediated writers. It does not
currently describe `flint-sync`, whose CAS is fenced by **rotation plus
its own cooperative `verify_not_deposed`** (what C3 observed), and
whose data PUTs are fenced by nothing (finding 1). The enforcement
point for both has to be the **proxy**, not the gateway.

The good news is that nothing on the wire has to change for that: plan
§2.2 already has every sidecar PUT carrying its epoch in the
`GenerationStamps`, so a proxy can reject a stale-epoch write from the
metadata it already receives. That turns the proxy conformance gate
from one question into two — *does the proxy preserve our conditional
headers*, and *can it read our epoch stamp and refuse a stale one*.

### 3. Rig-level traps worth keeping

- `kill 1` in a container is a no-op (PID-1 namespace signal rules).
- **`pidof` matches zombies.** PID 1 here is `sleep infinity`, which
  never reaps, so a backgrounded `flint-sync` that has already exited
  and fenced correctly stays visible to `pidof` forever. Two wait loops
  built on it always burned their full 180 s timeout — the drill looked
  hung when it was merely wrong. Read `/proc/<pid>/stat` field 3 and
  treat `Z` as gone. Worth ~7 minutes a run.
- S3 ETags are quoted; `mc` reports them unquoted. Compare values, not
  quoting conventions.
- `kubectl rollout status` returns on Deployment availability, but the
  Service endpoint can be a beat behind — poll the door itself.
- YAML anchors do **not** cross `---` document boundaries; a
  multi-document manifest that shares an `env:` block via `*anchor`
  fails to apply.

## What this drill cannot prove

Everything above ran on kind against loopback MinIO. Three Phase 6 legs
are outside its reach and need a real cluster:

1. **Real spot NODE reclaim** — kind can kill a pod or a container, not
   reproduce reclamation notice and timing across a fleet.
2. **Burst wave at N ≥ 1000 with admission** — the output is the proxy
   replica-sizing formula, which needs real scheduling scale.
3. **Rates through the real proxy against real S3** — the loopback
   numbers here are floors, not proxy floors.

There is also a correctness gate that needs no cluster at all and
should come first: whether the deployment proxy preserves `If-Match`,
`If-None-Match: *`, ETag stability, and the `x-amz-meta-flint-*` stamps.
Every fencing mechanism in lean rides those headers, and a proxy that
strips or rewrites them converts guarded publishes into blind
overwrites without failing loudly.

## Running it

```sh
kind create cluster --name flint-lean-chaos
cd lean/sidecar && cargo zigbuild --release --features s3 --target aarch64-unknown-linux-musl
cd ../e2e && cp ../sidecar/target/aarch64-unknown-linux-musl/release/{flint-sync,flint-lean-gateway} .
docker build -t flint-sync:e2e -f Dockerfile.sidecar .
docker build -t flint-lean-gateway:e2e -f Dockerfile.gateway .
kind load docker-image flint-sync:e2e flint-lean-gateway:e2e --name flint-lean-chaos
kubectl --context kind-flint-lean-chaos apply -f minio.yaml -f chaos.yaml
./run-chaos.sh          # ~8 min; resets the rig itself, so it is re-runnable
```
