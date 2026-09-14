# flint-lean — gated mode assessed, and concurrent writers without starvation: design

Date: 2026-09-13. Status: **§9 (gated removal) and §4 (the per-barrier
lease) BOTH IMPLEMENTED the same day; §10 records what was built and
where it departs from §4.**
Against `37ff97d0` (v1.51.0). Companion to
`flint-lean-per-user-access-design.md` (same day), whose §4.7 records
the one-writer constraint this document relaxes in granularity, not in
kind.

## 0. The two questions, verbatim

> What is gated mode and is it needed? Also it is fine to have one
> writer per workspace, but if two read-write users are concurrently
> working on the workspace, one should not face starvation.

### 0.1 The answers, short

1. **Gated mode** splits durability from visibility: an upload lane
   stages every changed file each floor tick as a new *object version*
   without moving the manifest, and a citation lane advances the
   manifest only at a coherent point the agent declared (or a cap
   forced). Readers that resolve through the manifest see only those
   points. It is opt-in, never the default, and it is the most
   expensive mechanism in the syncer for the least visible benefit
   (§1). **Not needed:** the one thing it gives a reader can be had with
   a second pointer under the default mode at a fraction of the cost,
   and without the durability regression gated carries (§2). Freeze it;
   do not extend it; retire it when no CR names it.
2. **Starvation** exists because the lease is held for the pod's life
   and a live holder is never deposed; the second writer waits before
   its checkout and never becomes Ready (§3). **The fix keeps "one
   writer at a time" and changes its granularity: the lease is held for
   one barrier, not for one life.** Every writer checks out without a
   lease, works, and claims only for the seconds its boundary takes,
   through a FIFO ticket so W writers wait at most W barriers (§4). The
   protocol already tolerates interleaved publishers at the manifest
   and the object level (§3.2); only the lease's duration forbade them.

## 1. What gated mode is

`spec.boundaryMode: cadence | hybrid | gated` (boundary-verbs plan
§2.4, D6; `lean_operator/crd.rs` doc-comment).

- **cadence / hybrid (default):** a boundary is one *fused* barrier —
  consume the inbox, scan, upload, CAS the manifest — every `floorSecs`
  (60) and, in hybrid, on every `publish` touch. Uploaded, cited and
  visible are one event. RPO after pod replacement is the last tick,
  automatically.
- **gated (opt-in):** two lanes. The *upload lane* runs every tick and
  PUTs each changed file in place, creating a new uncited version (the
  bucket must be versioned; a conformance probe refuses gated
  otherwise). The *citation lane* moves the manifest only at a coherent
  point: a `publish` touch, quiescence (`quiesceBoundSecs`, 30), the lag
  cap (`visibilityLagBoundSecs`, REQUIRED), the staged-backlog caps, or
  the preStop drain. Manifests are stamped `pinned_reads`, so checkout,
  sync and the gateway resolve the *cited version* of each key, never
  the newest. An exact per-citation version reaper plus a 30-day
  noncurrent-version lifecycle rule collect what a citation superseded.

**Who uses it.** Nobody selects it in production; it is a per-CR
opt-in (`boundaryMode: gated`) and every shipped default is `hybrid`.
The consumers that *know* about it only tolerate it: the
`flint-lean-gateway` crate resolves a `pinned_reads` manifest by
version id and carries two 410 errors (`dangling-citation`,
`uncited-bytes`) that can only arise under gated
(`lean/gateway/src/workspace.rs:148`, `:229-238`, `:699-765`); the
operator validates a gated spec, runs the versioning probe and
provisions the noncurrent-retention rule (`lean_operator/boundary.rs:66-291`);
forge never sets it (its "gated" hits are unrelated progress gating).
It is exercised by `lean/e2e/run-boundary.sh`, `run-verbs.sh` and
`boundary-workspaces.yaml`, and documented in the agent-fleets guide,
the chart NOTES and the CRD. It was my design answer to the plan's
problem 1, "torn published views" (§1 of the boundary-verbs plan), under
the user's hard constraint of no regression under defaults — which is
why it is opt-in, and why the question "is it needed" is fair.

**What it buys**, in the plan's own words: "coherent views for
manifest-resolving readers" — a downstream pod or a UI never sees half
a refactor — while bytes reach the bucket within a floor.

**What it costs**, all stated in the CRD doc-comment and the plan's
residual list:

1. automatic-recovery RPO regresses to the last *boundary*: after a pod
   replacement the staged-but-uncited bytes are recovered only by
   `flint-sync recover-staged`, an operator action, on a fleet where
   replacement is routine;
2. uncited generations are the CURRENT version of the real `files/`
   keys, so every reader that does not resolve through the manifest —
   `aws s3 cp`, an import, DR, a human — sees mid-change bytes; the
   coherence promise is explicitly scoped to flint's own readers;
3. uncited is invisible to imports, DR checkouts, GitOps re-applies and
   cross-cluster moves;
4. the versioning conformance surface (versioning enabled,
   `x-amz-version-id` on PUT, version-scoped GET/HEAD/DELETE,
   `ListObjectVersions`) — a proxy that strips one is refused;
5. the lag bound is mandatory; switching modes needs pod recreation;
6. code and defects: `gated.rs` is 1,666 lines beside the fused
   barrier's 1,909 — nearly a second syncer with its own tombstone
   withholding, park kind, stage journal and reaper. The model found a
   live defect in it before any mutation ran ("the gated citation
   deleted an acked user write", plan §10.1c), and the 2026-09-12
   review found two HIGH (gated-1, gated-2) and six lower findings in
   it against zero HIGH in the fused path's citation step.

## 2. Is it needed?

The need is real for some consumers: a reader that must not see a
half-done logical change. The question is whether gated is the cheapest
way to serve it. It is not.

**The cheaper equivalent — a `declared` pointer.** In hybrid mode every
fused barrier is already a coherent *snapshot* of the tree; what the
reader cannot tell is which snapshots the agent *declared* coherent.
The manifest object already carries the boundary source stamp (D6,
"bucket-visible source stamp"), so the information exists; it is just
not addressable. Add one pointer beside `current`:

```
<prefix>/.flint/lean/current    → the newest boundary (as today)
<prefix>/.flint/lean/declared   → the newest boundary whose source is a publish touch or a drain
```

written by the same lease holder in the same barrier, immediately
after the `current` CAS, only when the source is `sentinel` or `drain`.
Readers choose: `checkout`/`sync`/the gateway's `snapshot` take a
`declared` flag (a CR default `readers.follow: current | declared` for
pods that never learn the flag). A reader following `declared` reads
the cited etag by `If-Match`, and, when the key has since moved on,
by version id — D7's `version_id` in `LeanEntry` already exists for
exactly this — so versioning is required **only for readers who ask
for a declared view**, never for the writer.

| property | gated | hybrid + `declared` |
|---|---|---|
| readers see only declared points | yes (all of flint's readers, always) | yes (readers who follow `declared`) |
| bytes durable within a floor | yes, as uncited versions | yes, as cited boundaries |
| RPO on pod replacement | last **boundary**; operator recovers the rest | last **tick**, automatic |
| non-manifest readers | mid-change bytes (current version) | mid-change bytes at `current`; `declared` names a coherent one |
| versioning required | for the workspace, always | for `declared` readers only |
| mode switch | pod recreation | none — one mode |
| second writer possible | no (two staging lanes on one key) | yes (§4) |
| code | a second syncer | ~150 lines: one pointer write, one reader flag |
| crash between the two pointer writes | — | `declared` lags one boundary; benign; next declaration repairs |

A `declared` pointer is a *second pointer* but not a second manifest
writer: the same holder writes both in the same barrier, so the
one-writer invariants (`Inv_NoStragglerInstall`, `Inv_BoundaryAtomic`)
are unchanged; the model needs one tranche to say so.

**Recommendation.** Do not build on gated: no reader mode, no per-user
work, no multi-writer support in it. Keep it shipped and frozen for any
CR that names it today. If coherent views are wanted, build `declared`
(one release). Retire gated in a major bump once no CR names it — with
the migration being "set `boundaryMode: hybrid`, `readers.follow:
declared`, recreate the pod", and the CRD doc-comment already tells a
gated user the recreation step.

## 3. Why the second writer starves today

### 3.1 The mechanism

- `run` = claim → checkout → barrier loop (`flint_sync.rs:29`, `:441`).
  The claim comes **before** checkout, so a waiting writer's pod is not
  Ready.
- A holder renews its token every `min(floorSecs, 30)` s
  (`flint_sync.rs:491`). A claimant supersedes a foreign holder only
  after `QUIET_POLLS = 6` observations, 10 s apart, in which the token
  did not advance (`lease.rs:24`, `:119`; `verbs.rs:97`). A live holder
  always advances. **A live holder is never deposed; the waiter's bound
  is the holder's lifetime.**
- The code already names this: "What claiming actually bought was a
  DEADLOCK dressed as mutual exclusion … a checkout that met a running
  publisher waited forever" (`verbs.rs:123-133`), which is why
  `checkout`, `status` and `ctl` take no lease since 2026-09-11 (four
  readers ran 2.1x faster lease-free and 0.41x with it). `run` was left
  as it was.

### 3.2 What the protocol already tolerates

The lease's duration is the only thing that forbids two publishers;
the data path does not:

- **The manifest CAS is a three-way merge** (`barrier.rs:1090-1160`,
  `manifest::merge` `:969-983`): it starts from THEIRS "so foreign
  entries survive by construction", applies my upserts, applies my
  deletes only where theirs is unchanged since my base, retries up to
  four races, and returns the foreign entries "a consume must integrate
  next" (`report.foreign_queued`). Two writers on disjoint paths merge
  cleanly; on the same path the later upsert wins in the manifest.
- **An upload that meets a newer foreign version** preserves it and
  supersedes (`upload-412-preserved`, review atomicity-1), or parks and
  reports `partial` on a second 412. Nothing is silently lost.
- **Foreign changes reach a writer's tree** at its next consume, onto
  paths it has not modified; a modified path wins and the foreign copy
  is preserved (`consume-dirty`). This is the "previous incarnation"
  rule the contract already states, and it is exactly the two-writer
  rule.
- **The epoch is a publish fence, not a session**: `verify_not_deposed`
  runs before the CAS; the gateway's window and `LeanEntry.epoch` are
  per barrier already.

So: one writer *at a time* is a property of the barrier, and the code
keeps it per barrier. Holding the fence between barriers protects
nothing and costs the second writer everything.

## 4. The design: the lease is held for a barrier

### 4.1 The loop

```
run:  verify_claim (project-id precondition, a read)
      checkout            (lease-free — the verb already is)
      capabilities
      loop:
        wait for floor tick | publish touch | sync touch | SIGTERM
        consume inbox, scan, UPLOAD the dirty set   (no lease: every PUT is
                                                     guarded by If-Match on
                                                     the base etag — S3's
                                                     own optimistic lock)
        claim(ticket)      ─┐
        merge + manifest CAS, deletes, baseline    │  the lease is held here
        release            ─┘                       and only here: milliseconds
```

**Why no finer lock is needed (user's question, 2026-09-13).** S3's
conditional writes are the lock-free primitive. Every upload already
carries `If-Match` on the object's base etag, so two writers' uploads
overlap fully and a collision on one key is decided by the store (412 ⇒
preserve the foreign version, supersede knowingly, or park). The only
thing that must serialise is the manifest install, and the pointer CAS
with its three-way merge already does that in one small request. So the
lease shrinks to the commit section — merge, CAS, deletes, baseline —
and write throughput scales with the number of writers up to the NIC and
S3's per-prefix rate, exactly as reads already do.

Between boundaries nobody holds the cell. `sync` never needs it. The
preStop drain claims for its final boundary as today. The renew
discipline inside a barrier is unchanged (renew-before-barrier, 30 s
heartbeat, D12).

### 4.2 Fairness: a FIFO ticket

Contention among two or three writers with random retry is
probabilistically fair; the user asked for a guarantee. The cell gains
a queue:

```json
{ "holder_id": "…", "epoch": 41, "token": "…", "released": false,
  "waiters": ["lean-b7…", "lean-03…"], "handoff": null }
```

- `claim`: if the cell is free (fresh or released with no `handoff`
  naming another) acquire; else append my `holder_id` to `waiters` by
  CAS (once) and poll.
- `release`: write `released: true, handoff: waiters[0]`, popping it.
- A claimant acquires a released cell only if `handoff` names it, or
  `handoff` has aged past two polls (the named waiter died — the
  existing quiet-polls idea applied to a handoff).
- Bound: with W live writers a waiter acquires within W barriers plus
  W polls. Starvation is impossible while the holder ahead makes
  progress; a holder that stops making progress is deposed by the
  unchanged 60 s quiet-polls rule.

The queue lives in the same cell the epoch does, so it is one CAS'd
object and no new fence. `flint-store`'s `EpochLease` gains two
`#[serde(default)]` fields; an old syncer ignores them (it never
queues and never hands off, which degrades to today's behaviour, never
to a second holder).

### 4.3 What changes for the operator and the agent

- **Epoch per barrier.** The epoch increments at every boundary
  instead of every pod. Acquiring a *released* cell is a clean handoff
  and does not rotate the manifest (`lease.rs:119-131`), so the cost is
  two small CAS per boundary. `rotate_for_takeover` still runs only for
  an unreleased foreign takeover.
- **Status.** `observed_*` on the CR comes from the heartbeat echo of
  whoever ran the last barrier. Add `observedWriters` (distinct
  holders in the last hour) so two writers are visible as two.
- **The contract** (`AGENTS.md`) gains one paragraph: *other writers may
  share this workspace; a path you have not modified may change under
  you at a boundary (`report.consumed` names it); if two writers edit
  one file, the later boundary's version is current and the earlier is
  preserved in the bucket with an `upload-412-preserved` record — edit
  disjoint files, and `sync` before you start on a path that
  `remote.seq` says has news.*
- **Pod replacement.** Today a still-alive old pod keeps the lease and
  the new pod waits; after this both publish, and the old pod's late
  work lands rather than being lost. The straggler case (deposed after
  60 s quiet) is unchanged.
- **Gated** is not supported under per-barrier leases: its upload lane
  runs outside any barrier and two lanes on one key would stage two
  uncited versions for one citation. A gated CR keeps the life lease
  (one more reason for §2's recommendation).
- **Reader mode** (per-user design §4.4) never claims and is unaffected.

### 4.4 What is deliberately not solved

- **No content merge.** Two agents editing the same file concurrently
  get last-boundary-wins per file with the other version preserved and
  a record on both sides. There is no line-level merge and there will
  not be one in the syncer; that is what branches and a merge executor
  are for (`flint-lean-branching-design.md` §5.3).
- **Latency floor.** A writer whose touch arrives while another's
  barrier is running waits that barrier out (seconds; up to ~50 s for
  a 4 GiB publish at the default part parallelism). The ack is honest
  about it (`boundary: sentinel-deferred` already exists for a held
  touch).

### 4.5 Rollout

`FLINT_SYNC_LEASE=life | barrier`, default `life`, flipped to
`barrier` in the release after the model tranche and the drill are
green. The CR needs no field; the env is the plugin's to set.

## 5. The model tranche

`LeanSubtree.tla` today has one holder per incarnation. The tranche:

- holders alternate per barrier; the ticket is modelled as a sequence;
- re-check `Inv_NoStragglerInstall`, `Inv_NoDeposedPut`,
  `Inv_HITLDurable`, `Inv_NoForeignLost`, `Inv_BoundaryAtomic`,
  `Inv_AckImpliesCited` with two live writers and one gateway writer;
- **a new liveness property**: every requested boundary completes
  within W barriers (`WF` on the holder's barrier action, `SF` on
  handoff) — the starvation freedom the user asked for, as a checked
  property rather than a sentence;
- a required-reachable probe that two writers *do* interleave (the
  house rule: never prove a guarantee by the attack's absence).

Deferred item barrier-7 from the review (a crash between the CAS and
the window clear needs a second manifest writer to be observable)
becomes reachable in this tranche — two writers are exactly that — so
it is folded in.

## 6. Falsifiers — each with its control

| # | claim | test | control |
|---|---|---|---|
| L1 | two writer pods on one workspace are both Ready in checkout time | apply both; both `2/2 Running` | today's binary: the second is `ContainerCreating` until the first ends |
| L2 | both publish every floor | 10 minutes; each ack `ok` within 2× floor | today's binary: the second never acks |
| L3 | disjoint edits cross | A edits `a/`, B edits `b/`; each sees the other's within two floors, `report.consumed` names them | no `sync`, no boundary: unchanged |
| L4 | same-path edit is preserved, not lost | both edit `x`; one is current, the other's bytes are at the `preserved_key` its record names; the earlier writer's tree carries the later version after its consume | — (the assertion is the record) |
| L5 | the ticket is load-bearing | three writers in a hot publish loop, 30 minutes; max wait ≤ 3 barriers | delete the handoff check: a starved writer appears (max wait unbounded) |
| L6 | a dead holder mid-barrier is deposed | SIGSTOP a holder inside its barrier; the waiter holds within 70 s; the resumed straggler's PUTs are uncited (chaos B12, re-run) | SIGCONT before 60 s: no deposal, the waiter takes the next handoff |
| L7 | a reader never claims | a read-only pod beside two writers; cell never names it | a writer: it does |
| L8 | epoch-per-barrier costs two requests | request count per no-change boundary = today + 2 | life lease: today |
| L9 | `declared` (§2) names only sentinel/drain boundaries | cadence ticks move `current` only | a `publish` touch moves both |

## 7. Cost

| item | size |
|---|---|
| run-loop restructure (claim inside the barrier), env flag | ~150 lines |
| ticket in `EpochLease` + `claim`/`release` | ~120 lines, flint-store minor |
| `observedWriters`, contract paragraph, guide | ~60 lines |
| model tranche (§5) | the real cost: days, and it gates the default flip |
| drill L1–L8 on a cluster | a script; ask before provisioning |
| `declared` pointer (§2), if wanted | ~150 lines + one model run |

## 8. Decisions

- D1 **One writer at a time stays; its unit becomes the barrier.**
- D2 **FIFO tickets, not backoff:** starvation freedom is a checked
  liveness property, not a probability.
- D3 **Gated is frozen:** not extended, not made multi-writer, retired
  when unused; `declared` is the replacement for coherent views.
- D4 **No content merge in the syncer**, ever; branches are the answer
  to concurrent same-file work.
- D5 **Default flips only after the model tranche and L1–L8.**

## 9. One mode: what goes, what stays, what regresses

The user (2026-09-13): versioning is not a first-class concept in lean,
so drop gated; and do we need modes at all?

### 9.1 There is only one mode in the code already

`boundaryMode: cadence | hybrid | gated` is three names for two
behaviours. Every mode test in the syncer is `is_gated()`
(`gated.rs:1376,1599,1631`, `sentinel.rs:877,1295,1406`,
`flint_sync.rs:251,616`); `BoundaryMode::Cadence` occurs only in
`parse`/`as_str` (`lib.rs:168-178`). A consumed publish touch forces a
fused barrier in cadence exactly as in hybrid. The CRD doc-comment's
"`cadence` — exactly pre-boundary behavior; the escape hatch"
(`crd.rs:193`) describes a branch that does not exist; the plan's own
review had already recorded it ("the code has no cadence branch at
all", §7 U33/U44). The knob that really turns the verbs off is
`sentinels: off`, which stays.

So collapsing to one mode loses **nothing** between cadence and
hybrid, because nothing was there, and loses **gated**, which nobody
runs (§1, "Who uses it").

### 9.2 What gated's removal deletes

| where | what | size |
|---|---|---|
| syncer | `gated.rs` (upload lane, citation lane, stage journal, withheld tombstones, reaper, `version_index`, conformance probe), the three `is_gated` arms in `sentinel.rs`, the lag-bound refusal, the `recover-staged` verb | ~1,700 lines + the gated tests |
| CRD | `boundaryMode`, `visibilityLagBoundSecs`, `quiesceBoundSecs`, `stagedBacklogCapObjects`, `stagedBacklogCapBytes`, `noncurrentRetentionDays` | 6 fields |
| operator | versioning probe, lifecycle-rule provisioning, `VersionRetentionProvisioned`, the gated term of the derived preStop grace (`boundary.rs`, `reconcile.rs`) | ~130 lines |
| gateway crate | the two 410s only a gated manifest can raise (`dangling-citation`, `uncited-bytes`) | ~40 lines |
| model | 10 of the 94 configs (`LeanGated*`, `LeanProbeGated*`, `LeanProbeScopedGated`), the staged/pinned state in `LeanSubtree.tla` | a faster gate |
| e2e, docs | gated legs in `run-boundary.sh`/`run-verbs.sh`, `boundary-workspaces.yaml`, the guide row, the chart NOTES, the `AGENTS.md` gated section | — |

### 9.3 What stays even so

- **The pinned reader rule** in checkout, sync and the gateway, and
  `LeanEntry.version_id`. Manifests already written by gated syncers
  are stamped `pinned_reads` with version ids; a reader that forgets
  the rule 412s on every path the staging lane touched and adopts
  uncited bytes (`checkout.rs:565-580`, D13). It is a permanent
  legacy-reader case, ~60 lines, and it costs nothing on a manifest
  without the stamp.
- **flint-store's version surface** (`list_versions`, `get_version`,
  `delete_version`, the memory store's version chains): the reader
  rule uses it, it is small, and the branching design assumes it.
- **`sentinels: auto | off | force`**: the one real posture knob.
- **`boundary_mode` in `capabilities.json` and `gauges.json`**: report
  the constant `"hybrid"` for one release so an agent reading it sees
  nothing missing; then drop it with a protocol note.

### 9.4 Migration, fail-closed — NOT NEEDED (user: lean is not deployed)

The two-release path below is recorded for the record; the removal
shipped as one cut, including the CRD fields and the reader rule.

Removing a CRD field prunes it from stored objects, so a CR that says
`boundaryMode: gated` would silently become hybrid and its uncited
staged versions would become invisible — the "manufactured orphans"
the CRD doc-comment warns about. Two releases:

1. **N:** the writer side is deleted; `validate_spec` (operator AND
   plugin, at admission and at publish — a refusal ships with its
   callers) refuses `gated` with "drain the workspace on the previous
   release, then remove the field"; `cadence` and `hybrid` are accepted
   as aliases and the doc-comment says so. The reader rule stays.
2. **N+1:** `boundaryMode` and the five fields are removed and pruned.

### 9.5 What regresses, honestly

- **Nothing under defaults**: the default was hybrid and hybrid is the
  one mode.
- **Readers-see-only-declared-points** is gone with gated. If anyone
  asks for it, the `declared` pointer (§2) supplies it without a lane.
- **Non-destructive stragglers** were a property of the versioned
  bucket, not of the mode; on a versioned bucket a late PUT still makes
  a version. What goes is the reaper that collected them, which is
  what the bucket's own lifecycle rule is for.
- **The branching design** (`flint-lean-branching-design.md` §3.4)
  requires versioning, reuses gated's conformance probe and its
  `version_index` for the one-time backfill, and pins every branch
  pointer. With versioning demoted from first class, branching must be
  re-based before it is built: either a copy-based fork (a full branch
  needs no pinning after its fork) or versioning as a precondition for
  branching workspaces only. This is the one design consequence of the
  decision, and it is recorded here so it is not rediscovered at build
  time.
- **Two shipped fixes from the review** (gated-1, gated-2) are deleted
  with the code they fixed. Their tests go with them.

### 9.6 What gets simpler at once

The reader mode (per-user design §4.4) needs no lane case; the
per-barrier lease (§4) loses its "gated unsupported" caveat; the
contract drops a section and a mode sentence; the protocol review's
gated cluster (two HIGH, six lower, and the model-found live defect)
becomes a class of defect that cannot recur.

### 9.7 Decision

- D6 **One mode.** `boundaryMode` is retired on the two-release path
  of §9.4; `sentinels` is the only posture knob.
- D7 **Versioning is not a lean precondition.** The reader rule and
  the store surface remain as legacy support; the branching design is
  re-based before any branching code.

## 10. What was built (2026-09-13, the same day)

§4 was built as one mode — there is no `FLINT_SYNC_LEASE=life|barrier`
flag (§4.5) and no life-long lease left to fall back to; lean is not
deployed, and the user's standing decision is one mode. Departures from
§4, each deliberate:

- **Uploads outside the lease (§4.1 as refined).** The barrier consumes,
  scans and uploads with the cell at rest, stamped with the last epoch
  this incarnation held; the manifest entries carry the commit epoch.
  The window opens in the commit section, under the lease, not at the
  scan: the uploads race a HITL write the way two writers race each
  other (If-Match decides; `LeanNoWindowHolds` is the proof safety never
  depended on the window). With a SECOND writer, If-Match alone did not decide: a
  UI write conditional on an uncited upload's etag was lost (§10.1, the
  seventh row), and the gateway now refuses to overwrite what the
  workspace does not track.
- **Liveness left the cell.** With the cell released between barriers,
  neither the operator nor the gateway can read liveness from it, so
  each writer PUTs `<prefix>/.flint/lean/writers/<holder_id>` every ≤30 s
  (the same cost the renewal was) and deletes it on a clean drain. The
  handoff KEEPS the echo, so `SyncerObserved` names the last barrier's
  binary; `observedWriters` counts heartbeats fresher than five minutes.
- **A fence is a retry.** §4.3 kept "the straggler case unchanged";
  under the per-barrier lease the only straggler is a holder stalled
  inside its commit section, and being deposed there costs it that
  barrier and nothing else. So `refused-fenced`, the `fenced` marker,
  `gauges.state` and the fenced exit are gone; the pending stands and the
  next tick claims again. The 2026-09-12 review's lease-1/lease-6/gated-3
  fixes (settle-on-fence, refuse-while-waiting) were deleted with the
  state they settled.
- **The claim wait is bounded** (150 s) so a holder that never releases
  is a failed barrier retried at the next floor, never a hang; the
  quiet-poll judgement counts only observations ≥ 10 s apart, so the loop
  can poll every second without judging a live holder dead in six.
- **The deposal round is unordered.** Any waiter may depose a dead
  holder (the CAS picks one); FIFO resumes at the next handoff.
- **Model tranche** (§5) and the cluster drill (§6): see the commit
  record. `lean/e2e/run-writers.sh` is the drill, written beside the
  code and not yet run; W5 opens the mid-commit window with the
  drill-only `FLINT_SYNC_DRILL_HOLD_COMMIT_SECS`, because a commit
  section is milliseconds and no drill hits it by timing.

Cost as built: `lease.rs` rewritten (~600 lines), `barrier.rs` commit
section (~100), `sentinel.rs` −250 (the fence-settling paths), flint-store
+250 (queue, handoff, enqueue in both stores), gateway/operator +60, 11
tests deleted and 11 added (174 in the syncer battery).

### 10.1 What a second live writer broke, and the fixes (same day)

The model tranche (§5) was run against this build, and the tests written
to check its findings against the code found more. §3.2's "the protocol
already tolerates two writers" was wrong in seven places — none
reachable under the life lease, because the second writer did not exist;
the seventh was found by the full formal gate once the first six were
fixed. Each is a
test in `lean/syncer/src/tests.rs` that failed on its load-bearing
assertion before the fix and fails again when the fix is disabled by
exact string (the file restored by string, checksum verified):

| Defect | Test | Fix |
|---|---|---|
| The GC HEADs, recognizes the etag, and DELETEs unconditionally; the other writer's lease-free upload of that path lands between and is deleted, then cited (model: `LeanBarrierLeaseGCUnconditional`) | `a_peer_upload_between_the_gc_head_and_its_delete_is_not_deleted` | `ObjectStore::delete_if_match` (default refuses), S3 `If-Match` on DeleteObject; the GC deletes `If-Match` the recognized etag, re-HEADs on 412; `probe_conditional_delete`, run by `flint-sync probe-conditional` — no result recorded on S3 or Ozone yet |
| An upload that finds its bytes already at the key cites that OBSERVED etag with no lease; the other writer's commit uncites the path and its GC removes the object before the adopter's CAS (model: `LeanBarrierLeaseAdoptBlind`). A citation repair has the same shape | `an_adopted_upload_deleted_by_the_peer_before_the_claim_is_not_cited` | observed citations are re-read INSIDE the commit section and withheld when gone (`adopt-withheld`, `partial`, the path stays dirty). Race-free only because GC runs under the lease — so the lease is load-bearing here, and GC must not leave it |
| `sync` advanced its merge base to the manifest for a path an older inbox entry hid (model: `LeanBarrierLeaseSyncOverlayStale`) | `a_sync_does_not_advance_its_base_past_a_change_the_inbox_hid` | step 5 keeps the base for overlay-hidden paths; model control `LeanBarrierLeaseSyncOverlayHolds` (`SyncKeepsHiddenBase`) holds |
| A writer's merge queued the other writer's changes as `merge-preserved` entries in the SHARED inbox; the other writer's consume found its own bytes there and dropped them, and the first writer never converged | `a_peers_change_reaches_the_writer_whose_merge_queued_it_even_if_the_peer_consumes_first` | a writer-local queue (`state::ForeignChange`, `foreign-queue.json`), saved before the baseline; nothing merge-preserved in the shared inbox |
| A peer's DELETE never reached the other tree | `a_peers_delete_reaches_the_other_writers_tree` | the merge records foreign deletions as tombstones in that queue; consume removes a clean copy, keeps a modified one (`consume-foreign-delete-vs-dirty`) |
| Two idle writers traded empty generations and fence claims every tick (seq 5 → 13 in 8 idle barriers) | `two_idle_writers_do_not_trade_empty_generations` | a barrier whose merge adds nothing to theirs installs nothing; theirs becomes its merge base |
| A UI write through the gateway lands on a path while another writer's upload of it is uncited, If-Match that upload's etag (the window opens at the claim, so nothing holds the gateway off); a second writer consumes and cites the UI write and drops its entry; the uploader's commit re-cites its own generation over it — the acked UI write is preserved nowhere (model: `Inv_HITLDurable` in `LeanBarrierLeaseSentinel`, pinned as `LeanBarrierLeaseHitlOverUncited`) | `a_ui_write_over_an_uncited_upload_is_never_silently_lost`; gateway `a_blind_write_over_an_uncited_upload_is_refused_until_it_is_cited` | the gateway overwrites only a TRACKED version (the manifest's citation or an inbox entry's etag), else the retryable `concurrent-write` 409 before any precondition; an untracked object past 600 s or with no live writer heartbeat is fair game (`inbox::hitl_may_overwrite`, used by `put_file` and `promote_draft`); model arm `HitlOverwritesTrackedOnly` |

**Still open:** the 412 arm's supersede of the other writer's UPLOADED
BUT NOT YET CITED object leaves that writer's CAS citing a generation the
key no longer holds, until the superseding writer's own commit re-cites
the path (no bytes lost — the preserved copy exists). A commit-section
re-read does not close it: the supersede is lease-free. The model hides
it (its 412 arm parks, and `Inv_NoDangling` checks existence, not the
generation).

**Found on real S3 by the drill (finding 11, FIXED):** S3 answers an
If-Match PUT on a key that no longer exists with 404 NoSuchKey; Ozone
2.2.1 and the in-memory double answer 412. The "vanished base is a create"
rule lived behind the 412, so on S3 a writer whose edited path a peer's GC
had collected failed every barrier from then on — host leg H2's fixed arm
showed it right after the F2 re-read correctly withheld the citation. The
404 now takes the same rule (whole PUT and compose); the double answers
S3's 404 by default, and the vanished-base test runs against both answers.
The drill's host legs on S3, with single-mutation controls built from the
drill commit: H1 fixed PASS / control FAIL with F1's signature; H3 fixed
PASS / control FAIL with F3's; H2 control FAIL with F2's, fixed FAIL on
finding 11; H5 reproduced finding 10 (`lean/e2e/writers-live/results/2026-09-13/`).

**Ozone does not enforce If-Match on DELETE.** `probe-conditional` on
Ozone 2.2.1's s3g failed its DELETE leg, and the AWS CLI confirmed it with
a control: the same `delete-object --if-match <wrong etag>` is 412 on S3
and 204-and-deleted on Ozone. Fix 1 above (the conditional GC delete) is
therefore VOID on Ozone 2.2.x: a writer's GC can delete a peer's upload
there. This is Ozone's release scope, not a bug: the conditional-request
umbrella HDDS-13117 shipped conditional PutObject, GetObject, HeadObject,
CopyObject and CompleteMultipartUpload in 2.2, and conditional DeleteObject
is HDDS-14907, fix version 2.3.0 (resolved 2026-06-27; 2.2.1 of 2026-08-27
is the latest release). Until 2.3.0, more than one writer on an Ozone
workspace is unsafe; one writer is unaffected (its GC runs under its own
lease and no peer uploads). Re-run `flint-sync probe-conditional` on 2.3.0
when it ships. Not yet enforced in code.

**Still open, found after the fixes (finding 10):** a writer lost for good
between an upload and its commit — its pod replaced, its node gone — leaves
bytes at the key that no manifest cites and no inbox entry tracks. Nothing
acked is lost (its agent never got an ack), but the manifest keeps citing a
generation the key no longer holds, a fresh checkout reads the orphan
(S3-wins), and a live writer that never edits the path again keeps the
cited version: the trees and the bucket disagree until some writer rewrites
the path. A container restart does not produce it (the state directory
survives and the retry adopts its own upload by `flush_uuid`), nor does a
deleted worker pod (the plugin relaunches it over the same tree). Pinned as
`a_writer_killed_after_its_upload_does_not_leave_the_trees_diverged`
(`#[ignore]`, fails today). Candidates: each writer's heartbeat names its
in-flight upload paths and a live writer reconciles a dead writer's list; or
a live writer re-publishes over an untracked object older than a grace,
preserving it as a conflict copy. Not built.

**Found in the deployed storm (finding 12, data loss, FIXED locally):** the
gateway's `put_file` PUTs the object, then appends the inbox entry that
tracks it. Leg A2 (six writers editing the same files, plus a UI through
the gateway) lost two acked writes in five minutes: each time a barrier's
window opened between the gateway's admission check and its append, the
append refused (409 `barrier-window-open`, "not acked"), but the PUT had
already replaced a version the workspace TRACKED — one writer's acked,
cited upload; the UI's own earlier acked write, still in the inbox — and
nothing preserved it. The syncers' consume then found the key moved and
dropped the queued install as `superseded`, and the commit-section re-read
withheld the adopters' citation. F8 made the gateway overwrite only a
TRACKED version; that is safe only if the replacement ends up tracked, and
the refusal broke exactly that. Fix: after the PUT, `Workspace::track`
waits for the window to close and appends (bounded by the window deadline,
where `admits_hitl` admits a dead barrier's writes), in a spawned task so a
disconnecting client cannot cancel it half way; tests in
`lean/gateway/tests/verbs.rs` with a hook store that opens the window the
moment the PUT lands. The model could not see it: `HitlWrite` is one
atomic step. Open with it: (a) a request may now wait up to 180 s behind a
dead barrier — a short wait and then 202 "durable, tracking pending" is the
recommended shape; (b) every store write that DESTROYS bytes — gateway PUT
then append, draft promote, upload then commit (finding 10), GC HEAD then
DELETE, consume adopt then baseline — needs the question "what keeps those
bytes if the process dies, is refused, or is cancelled right after"; (c)
the model should split each such action into its two store calls, and move
merged foreign changes into a per-writer queue as the code has since
F5/F6 (the model still re-queues them in the shared inbox).

**Found in the deployed storm (finding 13, data loss, FIXED locally):** an
S3 whole-PUT ETag is the MD5 of the bytes, so it is not a version identity,
and two claims in this design rested on it being one: "uploads are
S3-guarded and need no lock" (§4) and finding 1's GC fence (delete If-Match
the etag the collector integrated). Leg A3 (churn: writes, deletes,
renames, same-content rewrites, plus a UI) broke both on `churn/p14.txt`.
Writer A moved the file away and committed the delete (seq 155). Writers
B and C had already rewritten it with IDENTICAL bytes (a new inode) and
re-uploaded it lease-free: the If-Match succeeded and the etag did not
change. A's GC HEADed the etag it integrated, found it, and deleted B's
object. B's commit (seq 156) merged its modify over A's delete and cited
the hole: a fresh checkout refuses the workspace ("manifest cites … but the
object is gone"). Finding 1's regression test could not see it because its
peer edit is DIFFERENT bytes; the in-memory store's etag was already a
content hash, but no test wrote identical bytes. Fix: the commit section
re-reads EVERY citation it adds, not only adopted and repaired ones, with
the HEADs fanned out; whatever is gone or replaced is withheld
(`upload-withheld`, a `partial` ack, the path left dirty) and the next
barrier PUTs it again. Cost: one HEAD per uploaded path, inside the fence.
Regression test: `a_peer_upload_of_identical_bytes_before_the_gc_delete_is_not_deleted`
(fails "seq 3 cites x.txt but the object is gone" with the re-read limited
to observed citations). Open with it: the model gives every write a
distinct version, so equal bytes must share an etag there (a `Put` whose
content equals the current object's leaves its etag unchanged) and
`LeanBarrierLease` must be re-gated exhaustively; Ozone's ETag is an MD5 as
well, so the fix is not S3-specific.

**Not yet modelled:** the writer-local queue and the empty-install rule
— convergence properties the safety invariants cannot see. The module's
`foreignQ` still joins the shared inbox at install.

Verification with all of it: syncer 180/180, flint-store 33 (46 with
`s3`), gateway 45, forge 175, operator 16, plugin 64. The assessment of
which named protocols these rules come from, and why GC may not move
out of the lease, is `flint-lean-consensus-protocol-assessment.md`.
