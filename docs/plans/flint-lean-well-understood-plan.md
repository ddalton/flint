# lean multi-writer: from "well tested" to "well understood"

2026-09-15. Plan of record for four workstreams:
- checking the model against the code;
- wider model worlds;
- closing the model's open items;
- a finding rate that can be measured.

Each workstream has an exit criterion that a reader can check. None of them is "we ran more tests".

## Where we start

What is solid today:
- The two-writer barrier-lease model has found six real defects.
- The syncer carries a protocol event trace on every step.
- The contention drill (2026-09-14) committed 6-writer traces: about 1,100 events per writer, 15 runs.

What is not solid:

- **Model and code disagree in places nobody has enumerated.** Findings 12 and 13 both came from an action that is atomic in the model and two store calls in the code, or from an etag that is a content hash in the code and a unique mint in the model. Known gaps not yet modelled:
  1. The writer-local foreign queue (with tombstones). The model re-queues into the shared inbox and never propagates a peer's delete.
  2. The pull-only boundary. In the code it takes no claim, no window and no CAS; in the model it claims.
  3. The install-nothing commit (`merged == theirs`: no CAS). The model advances `manSeq` on every install.
- **The only sentinel × lease world is still unexhausted.** It violates `Inv_AckBoundaryCoherent` at depth 19 (a false alarm in the harmless direction). With that invariant removed, the 2026-09-15 box run (p2) is exhausting it.
- **Finding 10 is open** (a writer lost between upload and commit leaves the trees and the bucket diverged). It is pinned by an `#[ignore]` test.
- **No world has crashes with the sentinel under the barrier lease, and none has three writers.** The p2 world alone is about 5 billion states (16 h on an i4i.4xlarge).

## Status (2026-09-15, end of day one)

- **W0 done.** p2 exhausted with no violation (4,416,800,243 distinct
  states, depth 44, 13 h 59 min). TLC's collision estimate is 1.0–2.7
  expected collisions, so one skipped branch is not ruled out; the successor
  world re-runs it smaller and with a different fingerprint seed. The box was torn down and the zero set verified. Evidence is in
  `lean/formal/results/2026-09-15-outranked-box/`.
- **W2 done.**
  - `StrictView` plus `PathSym` in 66 cfgs.
  - `view-census.py` with five failing controls.
  - Measured 0.42× on LeanSentinelHolds and 0.30× on LeanSentinelRestart.
  - A census of all 100 gate runs, old against new, confirms the verdicts
    (the numbers go in the commit).
- **W1 steps 1–3 done in the model (tranche 7).**
  - **Its first run found a shipped data-loss defect.** A queued tombstone
    removed a UI write the same consume had just adopted. It is
    reproduced in the syncer, fixed, and pinned by a test.
  - The ack refinement has its known-bad run (`LeanBarrierLeaseQueueDropped`).
  - The unguarded fast path is NOT known-bad on the code's shape. Whether
    those guards are redundant is open.
  - **Step 4 (finding 10) done.**
    - In the model: `Inv_QuiescentConverged` is violated as shipped; the
      `TrackOrphan` fix holds with every invariant.
    - In code: `untracked.rs` sweeps for untracked uploads; the pinned test
      is un-ignored, and three mutations each fail a test.
    - Measured first: a new writer's checkout already healed it; live
      writers did not.
- **W4 phase 2 done (2026-09-15).** A live storm leg replayed against the
  model: `churn/p23.txt` of round 3's churn leg — six writers, 3,258 model
  steps — is accepted to the end, and three mutations of it are rejected.
  It found FOUR places where the model was not the code: the handoff names
  the waiters read at the CLAIM (and with that modelled, `NoStarvation` is
  violated — the ticket's own purpose), a commit section can end without
  installing on a store error (which the event trace does not record at
  all), a published delete clears the baseline only for objects the GC
  collected, and a projection cannot check whole-leg counts. One path
  (`churn/p47.txt`) is still rejected: open.
- **W4 phase 1 done.**
  - Five syncer scenarios are traced and accepted; five mutations and five
    controls are rejected.
  - On its first run it rejected three traces at three real
    model-versus-code gaps, now modelled (`CommitLoadsCurrent`,
    `Upload412Preserves`, `DeclaredConfirmsAbsence`).
  - It runs in `formal-lean` CI.
- **W3 started.**
  - `Writers` is a constant.
  - `LeanBarrierLeaseImplThreeWriters` HOLDS exhaustively: 5,086,371
    states, a laptop, 12 minutes.
  - A box started at 19:27Z and was torn down after 14 minutes at the user's
    request: no more cloud spend. The partial runs had no violations.
  - The one-path worlds now run on the laptop, in sequence:
    - one-path sentinel;
    - unguarded fast path;
    - sentinel with crash and restart.
  - The two-path sentinel world does not fit a laptop. It is 27.7M states
    at depth 18, with 11.4M queued, after 14 minutes on 8 cores.
  - Results on the laptop: the one-path sentinel world HOLDS (36.2M states)
    and the unguarded fast path HOLDS (37.1M states: the guards are
    redundant for the ack's coherence).
  - The crash world violates `Inv_HITLTracked` in 19 states: a UI write
    adopted, deleted by the agent, and outranked. The code reaches the same
    state and then converges (S3-wins adoption at checkout, or the delete
    publishing). So this is a model-versus-code gap, pinned by two unit
    tests (README, "Crash1").
- **Live drill (2026-09-15, running):**
  - 4 × i4i.large spot: host legs H6/H5/H1/H2/H3, fixed and control, plus
    storm legs S0–S5 across 3 nodes;
  - results go to `lean/e2e/writers-live/results/2026-09-15-drill/`.
- **W5 ledger drafted:** `lean/FINDINGS.md`.
  - 141 product defects, 21 rig defects and 15 model-only rows, each with a
    cited source and a census against the CHANGELOG.
  - Defects fixed per release: v1.52.0 14, v1.53.0 5, v1.54.0 3, and 1
    unreleased (the queued-tombstone loss).
  - Found by: review/audit 59, code reading 29 (28 of them unverified),
    live drill 19, model 16, host leg 11, test 7.
  - Nine rows are still OPEN.

## W4 phase 2: the contention drill's traces (next)

The 2026-09-14 contention runs are 6 writers, about 300 paths, and about
6,500 events each, in disjoint mode, with no UI writes or kills. The
2026-09-13 deployed legs add UI writes (A2), same-bytes churn (A3) and
kills (A4), and they ran the binaries BEFORE findings 12 and 13 were fixed.
That makes them the known-bad traces: A3 should be accepted with
`VerifyUploadedCitations = FALSE` and rejected with it TRUE.

Four things phase 1 did not need:
1. `Writers` of length 6 (done: the constant).
2. The agent's writes inferred rather than logged. An upload's etag names
   the generation the tree held at the scan, so an `AgentWrite` goes just
   before that writer's scan. A deletion is inferred from the GC event one
   barrier later, because the two-scan rule means the absence began before
   the first scan.
3. Concurrency. An upload event is emitted after the whole upload wave, so
   its PUT may precede another writer's commit that the trace shows first.
   Each Upload becomes a silent step allowed anywhere between its writer's
   scan and its event.
4. Scale: state vectors of 300 paths × 6 writers. Each step is still
   determined by the trace; if TLC is too slow, project onto the paths one
   agent owns (sound in disjoint mode only for per-path steps; the counts
   become per-path).

## W0: finish the run in flight (today)

1. p2 (two paths, `AckHonest`, minus `Inv_AckBoundaryCoherent`) runs to exhaustion. Then p1.
2. Pull the logs into `lean/formal/results/2026-09-15-outranked-box/`.
3. Tear down and verify nothing is left.
4. Commit the pending model work: the `AckHonest` mirror of 231cff00, three new cfgs, and the 100-run gate.

**Exit:** the README's known-red paragraph states p2's result from the log, and the tree is clean under `lean/formal`.

## W2: ghost-state reduction (enabler for W3; first, because it is mechanical)

**Measured on 2026-09-15**, with no action changed:
- A TLC `VIEW` that drops the probe-only `gh` counters, `sc.pendReRun` and `sc.stageCarried` takes `LeanSentinelHolds` from 1,208,901 to 1,018,269 distinct states (0.84×), and `LeanBarrierLeaseAdoptVerified` from 641,858 to 320,184 (0.50×).
- `SYMMETRY Permutations(Paths)` on top takes `LeanSentinelHolds` to 512,322 (0.42×). All runs stayed green.

Deliverables:
1. `StrictView` and `PathSym` in the module.
2. `gen-cfgs.sh` emits the following:
   - `VIEW StrictView` on every cfg that checks no probe;
   - `SYMMETRY PathSym` where `FreePaths` is `{}` or `Paths`;
   - neither, on liveness cfgs (symmetry is unsound for liveness).
3. **A census check in the gate.** `check.sh` must fail if any field dropped from the view is read outside its own update or a probe definition. A view is sound only while that holds, and a future edit that reads a dropped counter in a guard would silently make it unsound. The check needs a positive control: a scratch copy that reads one dropped field in a guard must fail it.
4. **Verdict preservation.**
   - All 100 runs give the same verdicts.
   - Every must-fail run still names its invariant.
   - Every strict run's new distinct count is recorded next to the old one.

**Exit:**
- the gate is green;
- the census check has a failing positive control;
- the reduction table is in the README.

## W1: model the implementation (the open items)

In order. Each step preserves the earlier worlds where it can and states it where it cannot.

1. **The writer-local queue**, under `BarrierLease` only.
   - What it does: `sc[s].fq`, a set of `<<p, g>>` where `g = 0` is a tombstone.
   - What writes it: `CASInstall`'s foreign entries and foreign deletes go to `fq`, not the shared inbox.
   - How `Consume` drains it: queue first, then the inbox. For tombstones it follows `barrier.rs` `consume_counted`: absent → settle, clean → remove, dirty → keep with a record.
   - Crash: the queue dies with the pod; a restart keeps it.
   - Code refs: `state.rs` `queue_foreign`, `barrier.rs` `consume_counted`, and step 7's queue-before-base order.
2. **The two no-install routes.**
   - *Pull-only:* nothing uploaded, deleted, consumed, removed or observed, and the merge adds nothing. Queue theirs, set `instBase := manifest`, no claim, no window, no seq.
   - *Install-nothing* inside the commit section: `inst = manifest` → no CAS and no seq bump; GC still runs; the release follows.
   - The ack's `installed` stays FALSE on both routes (the code's `note_boundary` is skipped), so `Inv_BoundaryNamesItsClock` does not fire on an ack that installed no boundary.
3. **`Inv_AckBoundaryCoherent`, one-directional.**
   - The r1 counterexample is an ok ack naming a document AHEAD of the writer's tree by a peer's change waiting in its queue.
   - Exempt exactly the paths where `<<p, AckedDoc(s)[p]>> \in sc[s].fq`: the doc is ahead by what is queued, and nothing else.
   - *Known-bad re-runs:*
     - `LeanSentinelFastPathUnguarded` must still violate.
     - A new mutation, `QueueForeign = FALSE` (the merge base advances past a peer's change without queueing it), must violate. That mutation is the direct fingerprint of the harm the exemption must not excuse.
4. **Finding 10, model first.** A writer lost for good between its upload and its commit leaves an object no citation and no inbox entry tracks.
   - *Candidate fix* (recommended over re-publishing the cited version, which needs a live copy and loses the newer edit): a writer whose checkout finds a cited key holding an untracked etag older than a grace appends it to the shared inbox. It is then integrated exactly like a UI write: adopted when clean, surfaced when dirty, and cited by the next commit.
   - *Model action:* `OrphanTrack(p)`, enabled when `objects[p]` is neither cited nor in the inbox and its uploader is quiet (the same quiet abstraction the deposal uses).
   - *Property:* a convergence check at quiescence. Run with and without the arm; without it, the check must fail.
   - Then the code: the `#[ignore]` test goes green, and a mutation (no append) turns it red.

**Exit:**
- `LeanBarrierLeaseSentinel` (with the refined invariant) exhausts on a box with no violation, and the gate no longer carries a known red;
- CHANGELOG "Known limitations" loses its formal-model and finding-10 entries.

## W4: trace validation (the model checked against the code)

The approach follows Cirstea, Kuppe, Merz et al., *Validating Traces of Distributed Programs Against TLA+ Specifications* (2024). A trace is a behaviour the model must be able to produce. TLC searches for it, and failing to find one is a model-versus-code gap with a line number in the trace.

1. **`TraceLean.tla`** extends the module with a trace cursor. Each event (or group of events) advances the cursor only through the model action it maps to, with the event's values bound.
   - Sub-steps the model treats as atomic (a consume is one event per entry; a scan followed by its uploads) are grouped.
   - Unobserved actions (agent writes, UI writes, touches) are taken just in time: an upload naming etag e for path p requires the tree to hold gen(e) at the scan, so the writer is set to it then.
   - Generations come from etags. Equal etags get one generation, which is exactly the property finding 13 needed.
2. **`lean/formal/trace/ndjson2tla.py`** turns syncer trace lines (plus the harness's op log where one exists) into a TLA+ sequence module. The event-to-action mapping table lives in one place and is quoted in the README.
3. **Two trace sources.**
   - The syncer's own multi-writer tests: a `Sink::Memory` trace, and the test harness logs its agent and UI ops as synthetic events. They are deterministic and small.
   - The committed contention-drill traces (6 writers). This needs `Syncers` as a constant, which W3 needs too.
4. **Teeth, before any green result counts.**
   - *Positive control:* a test trace that crosses a pull-only boundary must be REJECTED by the pre-W1 model at that event, since that gap is known.
   - *Trace mutations:* swap two dependent events, change one etag, or drop a `queue` event. Each must be rejected.
5. `lean/formal/trace-check.sh` runs every committed trace, and a trace that stops matching fails it.

**Exit:**
- every multi-writer syncer test trace and one full contention run are accepted by the post-W1 model;
- the positive control and three trace mutations are rejected;
- every rejection found along the way is either a model fix or a code finding, recorded.

## W3: wider worlds (needs W2 and W1)

1. **Sentinel × lease × crash + restart**: the p2 world plus `MaxCrashes = 1`, `MaxRestarts = 1`, with W2's view and symmetry.
2. **A third writer**: `Syncers` becomes a constant.
   - The start order (`B` no earlier than `A`) generalizes to a chain.
   - `SYMMETRY` over writers is used where no writer is special (`AllowStall = FALSE`).
   - Budgets start small (one path, `MaxBarriers = 2`) and grow only while a laptop depth-bounded run stays under an hour.
3. Box runs follow the 2026-09-14 recipe (i4i.4xlarge one-time spot, BAQueue + THP, on-box cap, logs to S3). **Each box is provisioned only after asking**, with plan and cost. At about $0.29/h, a 16-hour run costs about $5.

**Exit:** both worlds exhausted with no violation, or a finding.

## W5: a finding rate you can read

1. **`lean/FINDINGS.md`**, one ledger with one row per finding since multi-writer lean began. Columns:
   - id, date found;
   - found by: model, live drill, host leg, test, code reading or review;
   - class: data loss, contract, convergence, availability, or model-only;
   - fix commit and the release that shipped it;
   - the regression pin (test or cfg).
2. **The rate per release and per campaign**, computed from the ledger, not asserted.
3. **A cadence.** Every lean release runs:
   - the contention drill;
   - W4's trace check over its traces;
   - the gate.

   Each new row is dated at the release it was found in. "Well understood" is claimed only after N consecutive releases add no data-loss or contract row, with N stated in the ledger, not here.

## Order and parallelism

- W0 and W5 run now: W5 is document work and touches no model or code.
- W2 is next, then W1 (steps 1–3 in the model), then W4's harness.
  - W4's positive control wants the PRE-W1 model, so it is captured before W1 lands.
- W1 step 4's code fix follows its model run.
- W3 waits on W1 and W2, and on approval for each box.

The Mac has 8 GB of RAM, so there is one cargo build or one large TLC run at a time. Local TLC runs use `-Xmx2g`.
