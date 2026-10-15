# LeanP1: the boundary verbs' answer, the restart and deposal — the modelling plan for SAFETY.md rows 15, 16 and 17 (2026-10-14)

`lean/SAFETY.md` §5 leaves three things of the shipped shape outside
`lean/formal/LeanP1.tla` and its proof: the sentinel's ack (row 15), the
restart's promise about an unpublished delete (row 16), and deposal under a
stall (row 17). This plan says what to add to the model for each, which
claims to state over it, how the gate will try to break them, and what the
proof then costs. The three are planned together because rows 15 and 17
both add state, and every added variable re-opens all 19,235 obligations:
one edit, one re-proof.

**Status: APPROVED 2026-10-14 by the owner, with D1-D9 as proposed (§9).**
Step 0 (the code fixes) DONE 2026-10-14: `FINDINGS.md` L-133 (D7, the record
id kept across folds) and L-134 (D4, the drain's ack in one write), each
pinned by a test that fails on the old code; the syncer suite passes (284).

**Progress (2026-10-15).** Steps 1-2 DONE: the model additions and the gate
(`lean/formal/results/2026-10-14-rows15-17-gate-ec2/SUMMARY.txt`). Every old
world reproduces its count and verdict; AckHolds1 HOLDS on the final model
(2,916,984,743 distinct, depth 43); every mutant and probe fires. The gate
found two more code defects, L-135 (a barrier did not carry the gateway
request it read) and L-136 (the carrier kept a re-uploaded path as a delete),
both fixed with tests that fail on the old code. AK1 was restated as the gate
showed claim errors: the declaration excludes bytes the document already
holds; "dropped is within the declaration" is gone; a conflict preserves the
declared bytes unless its version is one of the writer's own uploads swept
before its CAS (the Writer ghost `lost`: the code copies every other
conflict's bytes to `conflicts/`, and the copy outlives a reap). A
restatement that excused a peer's later delete (DeletedLater) was tried and
withdrawn: the AckReadsOwnReport control stopped firing. AckWithheldHolds and
AckHolds2 are not decided by the gate and are left to the proof (owner),
DeposeAck to depth 22. Step 3 (trace events) not started. Step 4 (the proof)
STARTED: `lean/formal/results/2026-10-15-tlaps-rows15-17/STATUS.txt` --
make-anc.py v4 (v3 silently dropped edits inside the spans it rewrites; v4
pins them), the tie on five worlds exact, M0's first run.

**Revision (2026-10-14, second draft).** The first draft covered row 15
only. Its code references were checked line by line against the tree at
`aef254c6` and hold; nine of its claims and gate entries did not, and are
corrected here: AK3 named the latest ack instead of every ack; AK1 read the
code's carrier, which is what its own mutations disable; the declaration
was recorded at the fold, which the code does not do; the honour was not
keyed to the pending's id; parking is not in `LeanP1`; the U22 mutation
needs a touch without a nonce; the drain's ack is written twice with two
clocks; the regression check named seven worlds only EC2 ran; and a
coalesced touch loses H10's protection (§9, D7).

---

## 1. What is and is not missing

**Row 15, the ack.** The inbox cell's two requests are NOT the gap.
`boundary_request` only schedules a barrier, and `sync_request` is carried
to the agent as news and never performed (D14). Both are watermarked in a
local file and mutate nothing shared (`note_verb_requests`,
`lean/syncer/src/barrier.rs:427-457`); the gateway's request and the UDS
door both call `request_boundary`, the same fold the file touch takes
(`barrier.rs:449`, `bin/flint_sync.rs:756`). A barrier in `LeanP1.tla` can
already start at any moment, so a requested barrier is a behaviour the
model has, and every proved claim covers it. The gap is the ANSWER: the ack
file the syncer writes for the agent (`.flint/publish.ack`; `sentinel.rs`).
It is an observation, and S1 rows 1-5 are promises about it:

| S1 row | promise | old enforcer (pre-step-5 model, `LeanSubtree.tla`) |
|---|---|---|
| 1 | an `ok` ack means the boundary is installed at the seq it names, and that seq carries the declaration; a declared path it does not carry makes the ack `partial` and is named | `Inv_AckImpliesCited == ~gh.ackEarly` (:4379) |
| 2 | the boundary it names cites everything this workspace integrated | `Inv_AckBoundaryCoherent == ~gh.ackIncoherent` (:4386) |
| 3 | a fenced writer never answers `ok` | `Inv_NoFencedOkAck == ~gh.fencedOkAck` (:4412) |
| 4 | a consumed publish request is always answered | `Inv_NoNonceOrphan == owed \subseteq pendN \cup ackN` (:4394), `ackN` accumulating |
| 5 | the ack and the bucket name the same clock | `Inv_BoundaryNamesItsClock == ~gh.srcMismatch` (:4406) |

**Row 16, the restart.** S4 row 1, "a restart never republishes a delete
the agent made", is `LeanSubtree.tla`'s `Prop_NoResurrection` (:4357): no
step that is a restart creates a local file. `LeanP1.tla` has `Restart`
(:570), which keeps the tree, but states no claim about it, so the
rematerialise defect (`checkout.rs:1-14`: a re-checkout over a live tree
resurrects the agent's unpublished deletes) has no model that would
rediscover it on the shipped shape. No new state is needed: the claim is an
action property over what the model already has.

**Row 17, deposal.** S5 rows 3-4, "a deposed writer's manifest CAS never
lands" and "a deposed writer's data PUT never lands", are
`Inv_NoStragglerInstall` and `Inv_NoDeposedPut` of the old models.
`LeanP1.tla` has one `holder` and a `Claim` that waits for `"none"`
(:445); nobody is ever deposed, so `Inv_OneHolder` (:903) holds by
construction, and the L-132 defect fixed on 2026-10-14 (a holder deposed
after its CAS went on deleting under its successor) has no model either.
`PROTOCOL.md` says so ("What this page does not cover").

S1 row 3 needs both the ack and deposal, which is the other reason the
three rows go together (§4, S1 row 3).

## 2. The code being modelled

Each line below is something the model must do, with where the code does it.

### 2.1 The ack (row 15)

- **The touch.** The agent touches `.flint/publish`, with a nonce or bare;
  the gateway's `boundary_request` and the UDS door call
  `request_boundary`, always with a nonce, which folds into the SAME
  pending record (`sentinel.rs:635-642`).
- **The consume (fold).** A touch is renamed out of the agent's reach and
  folded into the pending record `.flint-sync/publish.pending.json`
  (`consume_sentinel`, `:479-537`; `fold_into_pending`, `:541-631`): its
  nonce is appended if new, the record gets a fresh `id`, and the record
  keeps at most `MAX_NONCES` = 32 nonces, dropping the oldest (`:58`,
  `:588`). The record holds NO path set: what a touch declares is "the
  tree as it stands when it is consumed" (the `id` comment, `:150-154`),
  and the paths come from the carrying barrier's scan. A later boundary
  strictly contains an earlier one (`AGENTS.md:111`): at-least semantics.
- **Due or deferred.** A pending is honoured on the poll tick when the
  min-interval and the hourly work budget allow (`sentinel_due`,
  `:427-439`); otherwise the floor tick honours it later, forced, stamped
  `sentinel-deferred` (`floor_tick`, `:1024`).
- **The carrying barrier.** ANY barrier that begins while a publish
  declaration stands carries it, whoever called, the cadence barrier
  included (H10): it confirms first absences (every absent dirty path is
  deleted, not withheld to the next scan; `barrier.rs:665-667`), stamps
  the manifest `sentinel-deferred` when it was cadence, and journals itself
  with its CAS (`:600-616`). The honour's own barrier is the same code
  under the `sentinel` stamp (`declared_barrier_as`, `:589`; the only
  other production stamp is `cadence`, `:575`).
- **The carrier** (`IntentJournal::carrier`, `state.rs:156-190`), matched
  on the pending's `id`: at each carrying CAS, the dirty paths it published
  (`paths`, `deletes`), and the declared paths it did not carry: parked or
  withheld uploads, and deletes the merged document still cites
  (`dropped`; `barrier.rs:1107-1125`). At the consume, at a pull-only's
  end and after an install, the carried uploads the document in hand does
  not cite (`uncited`: a peer deleted them since; `note_carrier_uncited`,
  `:2091-2107`, called at `:186`, `:856`, `:1338`).
- **The honour and the ack** (`honor_publish`, `sentinel.rs:742-837`): a
  reader answers `refused-read-only` before any barrier (`:750-765`);
  otherwise the honour RUNS a declared barrier (`:777-779`), then
  `dropped` = that barrier's parked ∪ deferred ∪ the carrier's `dropped` ∪
  `uncited` for THIS pending id (`:793-815`); status `ok` iff `dropped` is
  empty, else `partial`; the ack names the barrier's seq (the current seq
  on a no-change barrier, `barrier.rs:857`), the baseline's etag, the
  coalesced nonces, and the clock. The ack is written atomically and
  traced (`write_ack`, `:653-660`), then the pending is retired
  (`honor_pending_as`, `:707-739`).
- **The restart rule** (`ack_matches`, `:665-697`): a pending whose ack
  already answers it is retired without re-running. The test is: the ack
  was minted after the consume (`completed_unix >= consumed_at`, `:681`),
  its sentinel mtime is not older than the consume's (`:684`), and it names
  every nonce of the pending; for a BARE touch the mtime equality is the
  whole test (`:692-694`). The mtime is still part of the decision; what
  U22 removed was deciding by it ALONE. The pending, the ack and the
  carrier are files on disk and survive a restart (`settle_pending_at_startup`,
  `:913-928`); the pod's replacement loses them with the tree.
- **The bucket's clock** (`boundary_source` on the manifest,
  `manifest.rs:71, 161`): the stamp the CAS writes, which an operator
  reads. The ack's clock is written from `forced` alone (`:823`) and
  ignores the stamp the CAS was given; the drain therefore writes its ack
  twice, `sentinel-deferred` first and `drain` second (`:1152-1159`), and
  between the two writes the ack and the bucket disagree (§9, D4).

**Not modelled, on purpose:** nonce payloads and notes (a nonce is an
identity); torn bodies (honoured as a bare touch with a conflict record);
the budget's arithmetic (one nondeterministic due-or-deferred choice per
honour); `refused-scope` (a sync-verb status) and the `sync` verb's ack
(§9, D1); the stale `recovered` clock in the code's comment (`:174`), which
nothing writes; and the mtime rule of a bare touch (§9, D8).

### 2.2 The restart (row 16)

A restart keeps the tree and the baseline and drops the barrier in flight
(`LeanP1.tla:567-585`). The code's rule is the restart matrix
(`checkout.rs:1-14`): with the marker present, NEVER re-materialise; a
re-checkout over a live tree is refused (`:781-800`), because local-wins
protects only present paths and would resurrect the agent's unpublished
deletes. The unpublished delete survives as an absent dirty path and is
published by the next scan (the two-scan guard, `Scan`, `:377`).

### 2.3 Deposal (row 17)

- **The cell.** One object per workspace: holder, epoch, token, released,
  handoff, waiters. `Claim` is one read plus at most one CAS
  (`lease.rs:166-260`): a held cell whose token stood still across
  `QUIET_POLLS` = 6 spaced observations, `QUIET_SPACING_SECS` = 10 apart
  (`:61-67`; about a minute), is judged dead and DEPOSED (`:247-248`), and
  the takeover ROTATES the manifest: a content-identical CAS, seq+1, so a
  straggler's CAS computed on the old document fails on its etag
  (`takeover_rotation_fences_the_straggler`, `tests.rs:588`).
- **The holder stays alive by renewing.** Inside the commit section the
  holder renews once its cell has stood still for half the threshold, by
  its own monotonic clock (`renew_if_due`, `:505-539`; L-132): before the
  fence read of each CAS attempt (`barrier.rs:1018-1019`), before the
  generation and chunk sweeps (`:1356-1385`), and between delete batches
  (`untracked.rs::delete_many_renewing`). A renew that 412s on a cell no
  longer ours is `Fenced` (`:426-494`); any other failure is retried.
- **The fence before the CAS.** `verify_not_deposed` reads the cell after
  the current manifest is loaded and before the CAS (`barrier.rs:118-132`,
  `:1020`; H2): deposed before the load, the read sees it; deposed after,
  the rotation has moved the etag the CAS expects. A fenced barrier is
  abandoned: its manifest never installed, its uploads stand and are
  adopted by the next barrier (`:1404-1424`; the pending stands too, so
  the honour re-runs).
- **A deposal after the CAS stops every deletion after it** (L-132,
  `may_delete`, `:1356-1388`): what is left is a leak the next holder's
  sweeps take. Step 7 (the baseline follows the install) still runs: the
  install landed. The release at the end is a CAS on our token and ends
  quietly when the cell is no longer ours (`lease.rs:577`).
- **A live holder is never deposed** at the shipped spacing unless its
  renew fails or it stalls past a minute. The model does not have clocks,
  so it deposes a holder at any point of its section (§9, D9): every
  safety claim is then proved for the stall the code guards against and
  for stalls it does not.

Tests that pin the code: `a_holder_deposed_mid_commit_abandons_the_barrier`,
`a_holder_deposed_during_its_commit_heads_does_not_install_onto_its_successors_document`,
`a_holder_deposed_after_its_cas_deletes_nothing_under_its_successor`,
`a_restart_between_a_deposal_and_its_rotation_still_rotates`.

## 3. The model additions

Design rule: **zero cost to every existing world.** All new state is
written only when a touch or a deposal exists, so with `MaxTouches = 0` and
`MaxDeposals = 0` the reachable states are exactly today's, and every gate
world the box can run must reproduce its 2026-10-14 distinct count to the
state (§5). That is the regression check for the change itself.

**Where the state goes.** All of it goes into the `Writer` record as new
fields; there are NO new variables (as built, 2026-10-14). Every
`w' = [w EXCEPT ...]` keeps the new fields, `TypeOK`'s text does not
change (the record type does), and no action's `UNCHANGED` list changes,
so `vars` and the proof's frame lemmas are as they were. The bounds are
per writer (`nt`, `nd`), and the bucket's clock stamp is recorded by the
writer whose CAS wrote it (`stamps`): each seq is one CAS's.

**Constants.** `MaxTouches` (requests per writer, agent and gateway
together; 0 in every existing world), `MaxNonces` (the code's 32, set low
in a world that should reach the cap; §9, D2), `MaxDeposals` (0 in every
existing world), and two rule constants, shipped TRUE: `DeletesUnderLease`
(a writer deletes only while it holds the lease; FALSE is the tree before
L-132) and `InstallFenced` (an install needs the lease; FALSE is the tree
before H2). The cfgs are generated (`gen-leanp1.sh`), so the new constants
are one edit.

### 3.1 The ack (row 15)

New `Writer` fields (as built; `0` and `"none"` stand for "no record"):

```
nt     : Nat                                         \* requests so far
pend   : [id, fold : Nat, nonces : SUBSET Nat]       \* publish.pending.json
ack    : [st : {"none","ok","partial","refused-read-only"}, nonces : SUBSET Nat,
          seq : Nat, src : Clocks + "none", dropped : SUBSET Paths, inst : BOOLEAN]
car    : [id : Nat, paths, deletes, dropped, uncited : SUBSET Paths]   \* the carrier
cId, cFold : Nat; clk; iseq : Nat; instd : BOOLEAN  \* the barrier in flight
bound  : [id, fold, doc, seq, wd (its withheld uploads), inst, clk]    \* the last carrying barrier, ended
\* ghosts: no guard reads them
cN, aN, capped : SUBSET Nat                         \* consumed / acked / dropped by the cap
dset : SUBSET Paths; decl : [Paths -> Opt(Handles)]; dMint : [Paths -> Nat]
moved : SUBSET Paths; stamps : SUBSET (Nat \X Clocks)
deposed : BOOLEAN; nd : Nat                          \* row 17
```

`Clocks == {"sentinel", "sentinel-deferred", "drain"}` (after D4; the
cadence stamp never reaches an ack, `barrier.rs:615`). A nonce is the
request's number, `nt'`, so nonces are unique per writer and the oldest is
the least.

**Actions** (all in `WriterStep`):

| action | what it does | code |
|---|---|---|
| `Touch(s)` | `nt < MaxTouches`; nonce `nt'`, folded at once: added to `pend.nonces` (a new record, with id `nt'`, if none; a fold keeps the id, L-133), `pend.fold := nt'`; `cN` grows; past `MaxNonces` the oldest drops into `capped`; the ghosts record the declaration | `consume_sentinel`, `request_boundary`, `fold_into_pending` |
| `Honor(s)` | at idle, a writer, the record not already answered, and `bound` is a barrier that began after the latest fold (`bound.id = pend.id`, `bound.fold = pend.fold`). `dropped` = `bound.wd ∪` (for this record's carrier) `car.dropped ∪ car.uncited`; `ack` := [ok iff dropped = {}, pend.nonces, bound.seq, bound.clk, dropped, bound.inst]; `aN` grows. The pending stays until `RetireByAck` | `honor_pending_as`, `honor_publish` |
| `HonorReader(s)` | a reader with a pending: `ack` := refused-read-only | `honor_publish`, first arm |
| `RetireByAck(s)` | at idle, the ack names every nonce of the pending: `pend` := none. This is the retire after the ack and the restart rule at once: a restart between the ack and the retire leaves exactly this state | `ack_matches` (the nonce arm; the bare-touch arm is D8), `retire_pending` |

The touch and the fold are one step: nothing observable lies between the
rename into staging and the record write except a crash, which
`recover_consume_staging` makes equivalent to the fold having happened.

**Changes to existing actions** (each a conjunct, not a rewrite):
- `Consume(s)` (the barrier's start, where the code reads the pending,
  `barrier.rs:608`): `cId`, `cFold` := the pending's id and fold, and the
  barrier's clock is chosen.
- `Scan(s)`: while the barrier carries, `dels` = every absent dirty path
  (the declared form) instead of any subset.
- `Install(s)`: while the barrier carries, update the carrier as
  `barrier.rs:1107-1125` does (same id: extend; else: a new record for
  this id), and record the stamp `<<seq', clk>>`; a barrier that adds
  nothing makes no CAS and no entry (the code's adds-nothing path).
- `Finish(s)`, `Skip(s)`, `PullOnly(s)`: recompute `car.uncited` from the
  document the barrier ended at (`W.inst` after an install, else `doc`;
  `:2091-2107`); if the barrier carried, set `bound` := [its id and fold,
  that document, its seq, its withheld uploads, whether it installed, its
  clock]; the barrier's fields reset.
- `Restart(s)`: keeps `pend`, `ack`, `car`, `bound` and the ghosts (all on
  disk); the barrier's fields reset with the barrier in flight.
- `Fence(s)` (§3.3) before the CAS: as `Restart` for the barrier's fields.
- `Edit(s, p)`, `Delete(s, p)`: while a record stands, `p` joins `moved`.
- `Next`'s writer disjunct gains the new actions. No `UNCHANGED` list
  changes.

### 3.2 The restart (row 16)

Nothing. The claim and its mutations read `Restart`, `Consume`, `Sync`,
`RescopeSecond` and `Edit` as they are.

### 3.3 Deposal (row 17)

New `Writer` field `deposed : BOOLEAN` (set by a takeover, cleared when the
straggler learns of it). New actions:

```
Depose(t) ==                      \* t's claim finds a holder it judges dead
  /\ On(t) /\ t \notin Readers /\ w[t].pc = "scanned"
  /\ w[t].uploads \subseteq w[t].upDone /\ w[t].uploads \cup w[t].deletes # {}
  /\ holder \in Writers /\ holder # t /\ deposals < MaxDeposals
  /\ holder' = t /\ deposals' = deposals + 1
  /\ seq' = seq + 1                                 \* the takeover rotation: the document unchanged
  /\ w' = [w EXCEPT ![t].pc = "claimed", ![holder].deposed = TRUE]
  /\ UNCHANGED <<doc, live, ...>>

Fence(s) ==                       \* the straggler's next cell read is Fenced
  /\ On(s) /\ w[s].deposed /\ w[s].pc \in {"claimed", "cased"}
  /\ w' = [w EXCEPT ![s].deposed = FALSE,
                    \* before its CAS: the barrier is abandoned, the uploads stand
                    ![s].pc = IF @ = "claimed" THEN "idle" ELSE @, ... (the scan's fields reset as Restart resets them),
                    \* after it: the deletes stop; step 7 still follows
                    ![s].collected = IF w[s].pc = "cased" THEN TRUE ELSE @,
                    ![s].retire = IF w[s].pc = "cased" THEN {} ELSE @]
```

The rotation bumps `seq` with the document unchanged, so `RetUpdate`
retires nothing, a reader's `memo` and a writer's `derived` fall behind
(the no-change barrier's early exit is defeated, as the code's comment
says, `lease.rs:255-262`), and `TookUpdate` sees nothing new.

**Changes to existing actions:**
- `Collect(s)`: `DeletesUnderLease => holder = s` (the renew before the
  deletes, L-132). `Sweep` (`SweepUnderLease`) and `Reap` already require
  `holder = s`.
- `Install(s)`: `holder = s` becomes `InstallFenced => holder = s` (the
  verify before the CAS, H2). `Verify(s)` keeps `holder = s`.
- `Finish(s)`: `holder' = IF holder = s THEN "none" ELSE holder` (a
  straggler's release is a no-op on a cell that is no longer its),
  `deposed' = FALSE`.
- `Restart(s)`: `deposed' = FALSE`.
- `Inv_OneHolder` is restated (§4): with `MaxDeposals = 0` the new form is
  the old one.

## 4. The claims

Stated over state and steps, read from ghosts no mutation touches, so the
proof can use them and the mutations cannot satisfy them by construction.

**Row 15.**

- **AK1 — an `ok` ack carries its declaration** (S1 rows 1 and 2). The
  declaration is what the code says it is: the tree as it stands when the
  request is consumed. The ghosts record it at the fold (`dset`: the
  paths dirty then; `decl`: their versions; `dMint`: the mint counter),
  merged across the folds of one record (D7), with `moved` the declared
  paths the agent wrote or deleted since. (A first draft took the
  declaration from the carrying scan; the first run refuted it in 9
  steps: an edit made after the touch counted as declared.) A declared
  path is BROKEN in the ack's document when the agent has not moved on,
  the document does not hold the declared bytes, what it holds is not a
  later version (derived from the declared one, or minted after the
  request; a declared delete is answered by any later file), and no
  live conflict record preserves the declared bytes (R7: a peer's later
  boundary may win, loudly). On the step that writes an ack: every
  broken path is in `ack.dropped`, and `ack.dropped` names only declared
  paths. `ok` names none, so an `ok` ack has no broken path. A peer's
  delete is a break: nothing at the path is not a later version.
  Row 2 ("cites everything this workspace integrated") is part of AK1:
  under P1-lite the scan publishes every path that differs from the
  baseline, so a separate claim would restate it (§9, D5). What AK1 says
  about a touch that coalesced with an earlier fold is D7.
- **AK3 — no consumed nonce is orphaned** (S1 row 4). An invariant:
  `cN \ capped \subseteq pend.nonces \cup aN` (every nonce ever
  consumed is still pending or was named by SOME ack; the old model's
  `ackN` accumulated the same way). The cap is visible, not hidden:
  `ProbeCapDropped == \A s : capped = {}` must fire in `AckCap` (§9, D2).
- **AK4 — one clock** (S1 row 5). An invariant: an ack whose barrier
  installed (`~no_change`) names the clock that barrier's CAS stamped at
  the seq it names (`<<ack.seq, ack.src>> \in stamps`). Stated after D4's code fix; on
  today's code the model would have to allow the drain's second write.
- **L2 — a request is answered** (optional). Under a spec fair to writer
  `s`'s own steps, a consumed nonce is eventually in `aN`. This needs
  more fairness than L1 (a writer whose CAS loses to peers forever, or is
  deposed at every section, never answers), so it may need an assumption
  that peers eventually quiesce (§9, D6).

**Row 16.**

- **NR1 — only the agent re-creates what it deleted** (S4 row 1,
  `Prop_NoResurrection`). An action property:
  `[][\A s \in Writers, p \in Paths : w[s].local[p] = Nil /\ w[s].baseline[p] # Nil /\ w'[s].local[p] # Nil => w'[s].local[p] = <<p, nextGen>> /\ nextGen' = nextGen + 1]_vars`:
  a path the tree deleted and has not published comes back only by an
  `Edit`, which mints. It covers more than the old property's restart: a
  consume takes only clean paths (`Owed`, `:321`), convergence of an absent
  path needs the document absent too (`:332`), a sync and a reader's pull
  take `Owed` (`:593`, `:729`), the widen fetches only where the baseline
  cites nothing (`:681`), and an upload rewrites a present path only
  (`:410-411`). A narrow that drops a deleted path from the baseline and a
  widen that fetches it back is outside the premise, by the scope's design
  ("what the scope declined stays declined").

**Row 17.**

- **`Inv_OneHolder` restated:** `\A s : w[s].pc \in {"claimed", "cased"} /\ ~w[s].deposed => holder = s`,
  and `holder = s => w[s].pc \in {"claimed", "cased"} /\ ~w[s].deposed`.
  With `MaxDeposals = 0`, `deposed` is never TRUE and this is today's
  claim; M1's theorem changes with it.
- **DP1 — an install is the holder's** (S5 row 3, `Inv_NoStragglerInstall`):
  `[][\A s \in Writers : w[s].pc = "claimed" /\ w'[s].pc = "cased" => holder = s]_vars`
  (only `Install` makes that transition). In the model it follows from
  `Install`'s guard; the gate's evidence that the guard matters is
  `InstallFenced = FALSE` under deposal, and the code's is H2's test.
- **DP2 — a delete is the holder's** (L-132): every step that shrinks
  `live` is taken by the writer that holds the lease:
  `[][live' # live /\ live' \subseteq live /\ w' # w => \E s \in Writers : holder = s /\ w'[s] # w[s]]_vars`
  for the collector (the only deleter that moves `w`), with `Sweep` and
  `Reap` guarded in place. S5 row 4 (`Inv_NoDeposedPut`) is a different
  promise on the shipped shape: a straggler's PUT lands at a fresh handle
  nobody cites, and `Inv_CitationsLive` and R4a already cover what a
  commit may cite; it is retired by the handles design, and SAFETY will
  say so.
- **S1 row 3 — a fenced writer never answers `ok`** is a corollary of AK1
  and DP1, not a fifth claim: an `ok` ack's seq is a document a barrier
  that read this fold ended at, and an install is the holder's. A holder
  deposed AFTER its CAS answers `ok` honestly (the install landed; the
  code returns `Ok` from that section, `barrier.rs:1356-1399`). D3 is
  resolved by this plan.

## 5. The gate

Expectations in `WORLDS-LeanP1.tsv` before any run, as always.

**The regression check.** With `MaxTouches = MaxDeposals = 0` in every
existing cfg, the 62 phase-1 worlds re-run on the box and must reproduce
the distinct counts in
`results/2026-10-14-leanp1-gate-ec2/out/p1/*.out` exactly. The seven
phase-2 worlds (P3Age at 1,080,412,937 distinct among them) ran only on
EC2 and are NOT re-run unless the owner approves a spot box; the argument
for them is the same model under larger bounds, and it is an argument, not
a run.

**Worlds** (new constants on, small bounds):

| world | shape | expect |
|---|---|---|
| `AckHolds1` | one writer, one path, two touches, the UI, MaxBarriers 3, MaxRestarts 1 | AK1, AK3, AK4 HOLD |
| `AckHolds2` | two writers, two paths, a removal, two touches | HOLD |
| `AckCap` | one writer, `MaxNonces = 2`, three touches | HOLDS; `ProbeCapDropped` fires |
| `ResHolds` | one writer, two paths, a UI delete and a rename, MaxRestarts 2, MaxBarriers 3 | NR1 HOLDS |
| `DeposeHolds` | two writers, one path, MaxDeposals 1, the retire age | `Inv_OneHolder`, DP1, DP2 and every existing claim HOLD |
| `DeposeAck` | `DeposeHolds` with one touch | AK1, AK3 HOLD (the fenced honour re-runs) |

NR1 is also listed as a PROPERTY in the existing `Holds` worlds: an action
property adds no state.

**Mutations**, each a defect the code has had or a rule the contract
states (the gate must rediscover them):

| mutation | the defect | must break |
|---|---|---|
| `AckIgnoresWithheld` | review 2026-09-12 inbox-1: a path not published, answered `ok` (the model's park is a withheld upload, `gone`; parking left with `jParked`) | AK1 |
| `AckReadsOwnReport` | H10: the honour's `dropped` reads only its own barrier, never the carrier (the pre-L-110 code) | AK1 |
| `AckIgnoresUncited` | H10: a peer's delete after the carrying CAS answered `ok` | AK1 |
| `FoldClobbers` | settle-before-consume: a touch replaces the pending instead of folding (`Touch` sets `nonces := {new}`) | AK3 |
| `StampIndependent` | the bucket's stamp and the ack's clock computed apart | AK4 |
| `Rematerialise` | `Restart` materialises the baseline where the tree lacks it (a re-checkout over a live tree, `checkout.rs:11-13`) | NR1 |
| `ConsumeTakesDeleted` | `Owed` takes an absent dirty path when the document moved (`AGENTS.md:240` says a dirty path is never removed that way) | NR1 |
| `DeletesUnderLease = FALSE` | L-132: a holder deposed after its CAS collects under its successor | DP2 |
| `InstallFenced = FALSE` | H2: a holder deposed before its CAS installs onto its successor's document | DP1 |
| `FinishReleasesForeign` | `Finish` sets `holder' = "none"` unconditionally: a straggler releases its successor's lease | `Inv_OneHolder` |

`RestartByMtime` (U22) is NOT in the table: its defect needs a touch
without a nonce, which the model does not have (§9, D8).

**Probes** (must fire): an ack is written `partial`; an honour is
deferred; a cadence barrier carries a declaration; a pending is retired by
its ack after a restart; an unpublished delete survives a restart and is
then published; a straggler is fenced before its CAS and its pending is
honoured by its next barrier; a straggler fenced after its CAS leaves a
leak the next holder sweeps.

Sizes: unknown until built. The one-writer worlds should be minutes; the
two-writer worlds are the risk, and get the EC2 treatment only if they need
it (ask first).

## 6. The trace check

`lean/formal/trace/ndjson2core.py` maps the conformance traces' events onto
`LeanP1.tla`'s steps. The syncer already traces `ack` (`sentinel.rs:657`),
`claim`, `renew` and `fence` (`lease.rs`, `barrier.rs:1405`); the converter
maps `claim` and drops `ack` today (`:243`, `:303`). To add: a `fold` event
at `fold_into_pending`; the mappings `fold` → `Touch`, `ack` → `Honor` /
`HonorReader`, a takeover `claim` → `Depose`, `fence` → `Fence`. Add
conformance traces for: a declared delete; an ack after a deferred honour;
a partial ack from a peer's delete after the carrying CAS; a delete, a
restart, then the publish; a deposal mid-commit (from
`a_dead_holder_mid_commit_is_deposed_by_the_next_barrier`). Then
`trace-check-core.sh` checks the code's acks, restarts and fences are the
model's. Row 14's multi-writer replay comes after this: it needs these
events.

## 7. The proof (M7, M8, M9)

**What it costs the existing proof.** Three new variables change `vars`
and every action's frame, the `Writer` record changes `TypeOK`, and seven
new actions need a case in every claim's step lemmas: every module from M0
to L1 is re-proved (19,235 obligations; the full record run, about 2 h on
the box). The step lemmas are generated
(`results/2026-10-06-tlaps-typeok/gen-proof.py` and its chain; the
tlapm-readable copy by `make-anc.py` in the same directory), so the frame
change is a generator edit, not 19,000 hand edits; the new actions' cases
are mostly `UNCHANGED` on everything the old claims read. M1's theorem
changes (`Inv_OneHolder` restated). L1's `GA(p)` gains the new variables
in its `UNCHANGED`. Expect a few obligations to need re-stating: adding
definitions alone perturbed 4 of M0's on 2026-10-14. Doing rows 15 and 17
in one edit means this happens once.

**What each module proves.**
- **M7 (the ack):** AK1, AK3, AK4, by an inductive strengthening over the
  ghosts: `dset`/`decl` record what the requests declared, the carrier
  agrees with them where the shipped rules hold, and `carrier.uncited`
  agrees with `bound.doc`.
- **M8 (the restart):** NR1, per action: about thirty obligations, the
  only new definition being the property. No new state, so it could go
  first; it goes after the frame change so it is proved once.
- **M9 (deposal):** `Inv_OneHolder` restated (replacing M1's), DP1, DP2;
  then S1 row 3 as a corollary of AK1 and DP1.
- Controls: each rule the proof cites dropped from `Shipped` must fail
  exactly its citations, as M5D, M6 and L1 did.

**L2**, if D6 says so, as a second liveness module beside L1.

## 8. Order and cost

0. **Code first**, each a finding of the 2026-10-14 validation, with tests
   on the box: the drain's ack written once with the `drain` clock
   (`sentinel.rs:823` takes `source`; the re-write at `:1152-1159` goes);
   the carrier surviving a fold if D7 says so; the stale `recovered`
   comment. Half a day.
1. The model additions, `MaxTouches = MaxDeposals = 0` regression run on
   the 62 box worlds: 2 days.
2. The ack, restart and deposal worlds, mutations and probes; the gate
   run: 3 days.
3. The trace events and five conformance traces: 1.5 days.
4. The proof: the frame change through the generators and the full
   re-record (1 day), M8 (1 day), M7 (2-4 days, the carrier strengthening
   is the unknown), M9 (2-3 days).
5. L2, if chosen: 2 days.

About three weeks, longer if the two-writer worlds or the carrier
strengthening resist. No cloud unless a world needs it (asked first).
Doing row 15 alone first and row 17 later would cost the full re-record
twice.

## 9. Decisions for the owner

- **D1. The `sync` verb's ack.** Its claims ("never a silent winner": every
  conflict of the sync rides the ack in full) are the same machinery.
  Proposal: publish first (this plan), sync as a follow-up row.
- **D2. `MAX_NONCES`.** The pending keeps 32 nonces and drops the oldest
  (`sentinel.rs:58, 588`), so the 33rd coalesced touch orphans the first.
  The contract already says so (`AGENTS.md:105`, `:120`: fall back to the
  mtime rule). Proposal: state AK3 with the cap as a visible ghost
  (`capped`, and `ProbeCapDropped` must fire), and no code change, unless
  an agent is known to coalesce that many.
- **D3. Row 3, a fenced writer never answers `ok`.** Resolved by this plan:
  a corollary of AK1 and DP1 (§4), once deposal is modelled.
- **D4. The `drain` clock.** Fix the code first (§8 step 0): one write,
  stamped from `source`. Then the drain is a third clock value and one
  action in the model. The alternative, modelling the two writes, makes
  AK4 false by construction.
- **D5. Row 2 folded into AK1** under P1-lite, rather than a separate claim.
- **D6. L2's fairness.** A writer that keeps losing its CAS to peers, or
  keeps being deposed, never answers; the honest statement assumes peers
  eventually stop committing or bounds them. Proposal: decide after M7,
  with the counterexample in hand.
- **D7. A coalesced touch and the carrier.** The pending's `id` is fresh at
  every fold and the carrier is matched on it, so a second touch orphans
  the first touch's carrier. Route: touch n1; a carrying install at seq N;
  the ack write fails or the pod restarts before the ack (H10's routes); a
  peer deletes the declared path; touch n2 folds with a fresh id; the
  honour's consume removes the now-clean path (`report.consumed` counts
  it), its barrier is pull-only, `honor_publish` filters the old carrier
  out (`sentinel.rs:798-803`), and the ack is `ok` for n1 and n2 at a seq
  that does not cite the path. SAFETY S1 row 1 promises `partial` here;
  `AGENTS.md:240` says a clean path may disappear and `consumed` counts it.
  `AckHolds1` will produce this route through the gateway's delete.
  Options: (a) the SAFETY wording is the promise: the carrier survives a
  fold (one record per pending, merged across folds; the `uncited` of an
  earlier fold's install still makes the ack `partial`), and AK1 expects
  it; (b) the contract wording is the promise: AK1's ghosts are reset at
  every fold and `ok` is right. Proposal: (a); it is what the H10 fix
  meant, and the code change is small.
- **D8. A bare touch and the mtime rule (U22).** The restart rule's
  bare-touch arm decides by mtime equality (`sentinel.rs:692-694`), and
  U22's defect is reachable only through it. Options: (a) model a touch
  without a nonce, with a consume-order ghost standing in for the mtime,
  and keep a `RestartByMtime` mutation; (b) leave bare touches out of the
  model and say so, the test `a_stale_ack_never_retires_a_fresh_bare_touch`
  being the pin. Proposal: (b); the rule compares file times, which the
  model has no vocabulary for.
- **D9. How liberally the model deposes.** The code deposes only after a
  minute of stillness and a live holder renews at half that; the model
  deposes a holder at any point of its section. The safety claims are then
  stronger than the code needs, and L1 is unaffected (the gateway's CAS
  never waits for the lease). Proposal: accept; `MaxDeposals` bounds it.

**Approved 2026-10-14:** the §3 changes to `LeanP1.tla` and its gate, the
step-0 code fixes, and the proof plan of §7, with D1-D9 as proposed.
