# Catch-up design review, 2026-10-28

For flint-e4 (model) and flint-91 (code). Read from the code at d4196afc and
the branch catchup-source-tie at ab4e5387, and from FlintCatchupN.tla rev 7
(md5 05918a21); nothing was run. The live copy of this review is a Claude Doc:
https://claude.ai/artifact/WiCLxkFCqztaYWrWtgpJye

## Verdict

Ship the three-part fix, then move trust from the leg to the epoch. The
recommendation flint-e4 wrote is right in direction: the §5 mechanism is
sound, and today's fixes all patch one wrong assumption, that a
record-in_sync replica is a healthy source. Its item 2 keeps that
assumption's shape. "Provisional until checked at admission" is still
revocable, leg-level trust, evaluated late.

The root fix is narrower. Trust should be a property of a leg's epoch, fixed
at the cut by the raid's own base list and written by the authoritative
raid's node. Under that rule the floor, the forced re-revert, the admission
tie, §3's exclusion and three of the four hot-rejoin parts become
consequences of one check made before a copy starts.

Three facts drive this review:

- The record is never written at NodeUnstage. The raid's final base list is
  read nowhere before `bdev_raid_delete`.
- The model's admission set is an oracle (`path = "ok"`). The code's is
  "attached and record in_sync". The lost-mark case at stage is therefore
  unrepresentable in FlintCatchupN.
- The tie demotes a complete standby whenever its source is merely
  unattached at the next stage.

## What the design rests on

§5's correctness note is one-sided. Its three pieces, the backed-off base,
the revert and the base-inclusive copy, bound how far the destination can
diverge. The source's completeness is assumed in one sentence: "raid1
acknowledges a write only after all live bases complete it". That holds for
a base while it is configured. A base in its failure window is live to the
record and not to the raid.

At two replicas the assumption is free. The source is the sole survivor, and
an acked write was completed by some leg, so by it. At three, "some leg
completed it" is not "the source completed it". That is the whole 3-replica
finding, and it is a premise gap, not a mechanism gap: every control at
three replicas still violates without its piece.

The ground truth the design needs is the raid's base list. raid1 fails a
base at its first failed write, so a configured base has completed every
acked write. The model's RaidFailsAtDrop arm and the kind repro both saw the
leg fail as the stuck write completed. Said once, for both paths:

**A leg's content is authoritative through time t iff the leg was a
configured base of the authoritative raid through t.** The record's
`in_sync` is a lagging, lossy proxy for that, and every guard added today
reconstructs the proxy from the absence of a stale mark.

## How the record learns a leg's health

The record's `in_sync` is written from three observations and consumed as
truth by four readers. The unstage row is the one missing from the pasted
recommendation.

| Writer | Observation | When | Gap |
| --- | --- | --- | --- |
| Node monitor, `monitor_raid_health` (node_agent.rs:4669) | the raid's base list, the right one | every 60 s tick, on any node that holds a raid | the lag; a zombie's raid marks the fenced healthy legs stale and keeps its co-located leg in_sync |
| Assembly, `record_assembly_sync_state` (driver.rs:3027) | which legs attached | at NodeStage | reachability only, nothing about content; a silently failed leg that attaches is a member |
| Chase demotion, maintenance drain | lineage coverage, operator intent | on their ticks | not health |
| NodeUnstage, `teardown_volume_spdk_state` (driver.rs:3726) | none | | the raid's final base list is read nowhere before `bdev_raid_delete`; driver.rs has one sync-record write, the assembly's, and the node agent's two are inside the monitor |

Who reads `in_sync` as truth:

- `plan_cut` (epoch_scheduler.rs:143) cuts every record-in_sync leg over the
  control plane. A "common" epoch is recorded on a leg the raid has already
  failed. This is what turns the lag into a recorded lie.
- `in_sync_sources` (catchup.rs:952), behind every copy source: bulk, chase,
  final delta, hot window, backfill. It prefers the non-consumer leg, which
  after a consumer-to-leg partition is exactly the failed one.
- NodeStage membership, `attached_in_sync`.
- The F36c writer set, shrunk only by `mark_stale`, so it carries the same
  lost mark.

Two channels write the record, and both lag or miss. The unstage path, the
one place the raid's last observation exists at a clean pod move, writes
nothing.

## The three-part fix, mapped onto the rule

Keep it. Each part encodes a true fact, and the model shows each is needed.
The parts look like "many guards" only because the record holds no positive
vouch, so each path reconstructs one from the absence of a stale mark.

| Part | The fact it encodes | What it approximates |
| --- | --- | --- |
| Tie (`copied_from`, checked at admission) | the source stayed a configured base through the copied epochs | "still in_sync, same `since`, same head, attached" stands in for an observation the record never took |
| Base floor (MIN over the episode's reverts) | a copied epoch carries no provenance, so it is never trusted as a base | strictly conservative: a copy from a vouched source could be trusted |
| `must_revert` on demotion | a copy from a source whose vouch was revoked cannot be undone per cluster | needed only because trust is revocable |

Hot rejoin's `win`, `pad` and `tie` are the same facts on the live raid.
`reap` is a plain mechanism bug: `localize()` reaps nothing, where
`catchup_stale` reaps at its revert.

## What FlintCatchupN cannot show

The model's admission set is an oracle. `Stage` and `StageAdmit` take T as
the in-sync legs with `path = "ok"`. The code takes the attached in-sync legs
(`attached_in_sync`, `admit_one_standby` at catchup.rs:2524). And `path` is
per leg, not per consumer node: a leg that once failed can never re-attach at
a plain Stage in the model, while in the code it attaches fine from the new
node.

So the lost-mark case at stage is unrepresentable. The mark never lands
(unstage before the tick, a crashed consumer, the RBAC repro), the lagging
leg attaches at the new consumer, the tie passes, and that leg can even be
the final-delta source. No invariant would see it anyway: `Inv_MembersAgree`
ranges over the oracle's healthy set, `Inv_AdmitFaithful` compares against
whichever source was chosen, and `Inv_AckedHeld` holds while the third leg is
in_sync. The loss needs one failure more than MaxFails.

flint-e4 adds, and I agree: the lost mark is first a raid-level hazard. The
new assembly takes the lagging leg in as an in_sync member, so members
diverge with no catch-up involved. That is why the unstage capture comes
first, and why every stage HOLD rests on the premise that the record learns
a leg's failure before the next assembly.

The branch's CHANGELOG entry names the lost mark in its problem statement.
The fix is checked under the lag premise and the §3 premise only. The entry
should say so.

Model changes to ask for:

- [x] Let `Stage` and `StageAdmit` admit any Reachable in-sync leg, as the
      code does. Keep the oracle version as a control.
- [x] Add `Inv_MembersHold`: every configured member holds every acked write
      with no write in flight. Expect it to fail by design in the
      survivor-divergence case; the point is to see which observation
      premise makes it hold.
- [x] A per-consumer reachability, or a Restage action that resets `path`
      for reachable legs, so a consumer move is distinguishable from a leg's
      return.

## The cost of revocable trust

The tie demotes a standby whose source is in_sync but simply not attached at
the next stage. `source_tie_broken` answers "is not attached to this
assembly", and `admit_one_standby` marks the standby stale with
`must_revert`. A three-replica volume whose source node is down at the pod
move throws away a complete standby and re-copies it from the floor, which
after a full build is the whole volume.

Not a safety bug. It is the clearest sign that the trust is attached to the
wrong object: the source's epochs were complete when they were cut, and its
reachability now says nothing about that.

## Recommendation: trust belongs to the epoch

Record membership per epoch, written by the authoritative raid's node. At
each cut the scheduler reads the base list on the VolumeAttachment's node
and records which legs were configured. The rule:

**A leg's epoch e is trusted iff the leg was configured at e's cut and still
configured at the first observation after every write submitted before the
cut has completed.**

The second observation covers raid1's asynchronous failure marking; the next
cut or the next monitor tick serves once T_back has passed, and a drained
unstage capture serves at once, since the raid drains in-flight I/O before
the delete. Trust is then monotone: fixed at the cut, never revoked, and
held per leg and epoch. A copied epoch inherits trust only when its source
epoch was trusted and the destination's base was trusted too. A chase that
copies only epochs older than T_back trails by one T_back more and never
has a copy retracted under it.

| Today's guard | Under the per-epoch rule |
| --- | --- |
| Source selection by record `in_sync`, tie at admission | a source is any leg whose epochs through the target are trusted, checked before the copy starts; admission checks only the final delta's source |
| Back-off from `since` by T_back on the destination | the same rule on the destination's own epochs; "step back one extra epoch" becomes exact |
| Base floor | a copied epoch inherits trust from a trusted source epoch onto a trusted base; no floor |
| `must_revert` | a copy starts only on epochs older than T_back, so trust is never revoked mid-copy; the flag goes |
| §3 "co-located leg excluded until hygiene" | the zombie's leg is not configured in the authoritative raid, so it is untrusted and never cut or copied |
| F36c writer set | the configured set at the last observation |
| Hot rejoin `win`, `pad`, `tie` | the same rule: window source configured under the quiesce, pad base = own last trusted epoch, backfill from any trusted leg; `reap` stays |

Cost: one base-list read on the consumer node per cut, the same RPC the
monitor already makes, plus one record write at unstage. The epoch entries
of the sync record grow by one set of uuids each.

## The record needs three positive writes

These make the record trustworthy enough for the rule, and the first one is
worth doing even if nothing else changes.

| Write | Where | What it closes |
| --- | --- | --- |
| Capture the raid's base list at NodeUnstage, before `bdev_raid_delete`, and mark the non-configured legs stale in the same write, only when this node is still the volume's consumer (the attached VolumeAttachment names it and the record's writer set matches the raid); a late unstage on a former consumer writes nothing, since its raid lacks exactly the healthy legs fenced off from it (flint-91) | `teardown_volume_spdk_state` (driver.rs:3726) | every clean pod move inside the monitor's 60 s window; missing from the pasted list |
| Gate the monitor's stale marks on being the VolumeAttachment's node | `monitor_raid_health` (node_agent.rs:4669) | a zombie's monitor marking the fenced healthy legs stale |
| Mark the co-located leg stale at the fence flip | the fence path | §3's exclusion, today not enforced anywhere |

The residual is a consumer that crashes inside a failure window. No
observation exists after the last tick, the survivors can disagree, and no
survivor is provably complete. §6 already accepts that as survivor
divergence; it should be named as this fix's residual. A per-cluster
reconcile cannot resolve it without write versions, which raid1 does not
keep.

## Proof shape, and asks per session

The N-replica proof of a monotone rule should be shorter than a proof of the
tie. The proved fact, a configured base has completed every acked write, is
close to the raid1 axiom, and nothing in the trusted set is ever retracted,
so the inductive invariant has no "was trusted, now is not" case. The 1.5 to
3 day estimate is plausible for that shape. I would not vouch for it on the
tie, whose invariant must carry `used`, `tainted`, the floor and `mustRev`
through every demotion.

flint-e4's estimate after reading this: 2 to 4 days for the trust rule's
stage proof against 3 to 5 for the tie. The recursion-free lineage (a ghost
ancestor set, since tlapm refuses RECURSIVE) and the pairwise clean-leg
agreement modulo cut skew are common to both, so that work starts first
either way.

flint-e4:

- [x] The three model changes above, on FlintCatchupN rev 7, with the
      shipped and tie arms as controls.
- [x] A `PerEpochTrust` arm: `recorded` carries the configured set per
      epoch, `Sources` and `BaseCandidates` apply the rule, no tie, no
      floor, no `mustRev`. Expect HOLD under both premises and under the
      lost-mark arm.
- [ ] Say in the gate's RESULTS which premise each HOLD rests on.

flint-91:

- [x] Ship the stage fix as it stands; add the lost-mark caveat to the
      CHANGELOG entry.
- [x] The unstage capture first: the smallest change, and it closes the
      most.
- [ ] Then the per-epoch membership in the sync record's epoch entries, and
      the rule in `select_base_epoch` and `in_sync_sources`, behind the
      model's arm.

## Update, 2026-10-29: the decision after this review

The user took record-before-ack in the on-disk shape. The design pass is
§12 of `spdk-csi-driver/docs/incremental-replica-rebuild.md`, rev 8
(1fe6b956, amended the same day). raid1 writes a membership stamp (seq,
gen, size, members, clock) at the tail of every surviving leg before it
releases the completions a base failure degraded, and the stage reads the
stamps before `bdev_raid_create`. A stamp naming a majority of the
assembly is one a zombie cannot forge, because placement puts one leg per
node and the fence holds the rest. A continuation on a minority (two legs
lost at once, or any loss on a two-replica volume) also waits for the sync
record, gated on the VolumeAttachment's node, so a zombie never
acknowledges. The model arm comes first (FlintCatchupS, flint-e4); the
raid1 patch after it holds.

What the model found between this review and the decision (FlintCatchupT,
code-faithful, bound D unless noted):

| Finding | Verdict | Consequence |
| --- | --- | --- |
| Shipped policy with the unstage capture, bound C | VIOLATED at depth 22 | the failure-window loss stands with the capture; bound D never separated the policies |
| Per-epoch trust, full invariant | HOLD, 34.8M states at bound D; clean to depth 26 at bound C, not a full pass | the rule holds where the shipped policy fails |
| Trust without "raid1 drops a base at its first failed write" | VIOLATED at depth 12 | premise 1 of every trust HOLD; observed once on gate1613 (2026-10-30: a black-holed leg dropped after 49 s, the two configured legs holding every acked write; kill arm void, scripts uncommitted), still to be pinned by a committed cluster test; after an spdk-tgt restart the dead-target options can be refused and the leg never faults (liveness, fix on `p4-first-attach`, not pushed) |
| A zombie writing a leg the raid configures | VIOLATED at depth 15 | premise 2, the fence; its fail-open probe shipped fixed in 044f2010; held live on gate1613 (2026-10-30): a real zombie's writes reached neither leg of the new raid, its own leg took all 16,384 after the move, and the monitor gate eb7c28fd recorded nothing for it |
| Zombie move with the assembly's stale write never landing | VIOLATED at depth 17 | that write is load-bearing and best effort in code; it should be a precondition of serving |
| F36c's last-writer defer on, zombie move | `Inv_MembersAgree` VIOLATED at depth 4, every policy | the defer seam: hygiene frees the co-located leg back into `in_sync` |
| A consumer leaving inside a failure window (the residual above) | `Inv_NoNewLoss` VIOLATED at depth 9, every policy, no admission in the trace | not closable by any admission rule |
| A write quorum of two alone | VIOLATED at depth 18 | a lost mark spreads the gap onto a second leg; costs availability |
| Record-before-ack with the defer dropped | HOLD: 88.6M states zombie move, 108.5M crash, 40.7M lost mark with the capture off | closes the residual on its own |

What it changes in the recommendations above:

| Recommendation in this review | Status on 2026-10-29 |
| --- | --- |
| Ship the three-part fix | shipped: stage 1050134c, hot rejoin 395b442d, with the CHANGELOG caveats |
| The unstage capture | shipped ef3a6f48, gated on the VolumeAttachment node and the writer set; a belt under the stamp |
| The monitor gated on the VolumeAttachment node | shipped eb7c28fd (2026-10-30): a node records for a raid only when it holds an attached VolumeAttachment for the raid's PV, and still records when the lookup itself fails, the direction that cannot hold back the consumer's own marks; live on gate1613 the zombie's monitor recorded nothing; stays as a belt, and copies the stamp's clock into `since` once the stamp exists so `T_back` covers the I/O timeout alone |
| Mark the co-located leg stale at the fence flip | becomes the stage's decree, written only by a stage whose read is conclusive without that leg (attached `in_sync` legs a majority of the newest stamp's size, or a confirmed sole continuation on an attached leg); otherwise the stage waits for it as F36c's last writer |
| Trust belongs to the epoch | stays for the scheduler and the catch-up, since the record trails a majority drop by one poll; modelled as FlintCatchupT (rev 15, md5 8ac3cf00), not a FlintCatchupN arm |
| The tie's demotion of an unattached source | the conjunct becomes "named in the newest valid stamp"; the demotion stands only for a source the raid dropped |
| Base floor and forced re-revert | retire once per-epoch membership is in the record |
| The residual, a consumer crash inside a failure window | closed by the stamp for a crash and for a move; the raid1 write hole on writes never acknowledged remains |
| TLAPS proof of the trust rule | pass 7 proved all 7,673 obligations on a seeded cache (2026-10-29); the fresh-cache record done 2026-10-31 (all 7,673 proved, three md5s asserted; twelve of 13 controls fail at their premise's steps, the select-time base bound proves without its rule), cited in §5; the proof covers the clean setting only, never a crash or a zombie; the stamp rule's own proof is done (2026-11-01: 19,016 obligations, any replica count, the final setting; record composed of sixteen fresh-cache ranges plus two Isabelle steps on the box with their inputs asserted; results section written; controls the user's call, about $8 on EC2), see §12's gate item 2; the confirmation batch c15 closed 2026-10-30 (67 arms on `8ac3cf00`, most with the pre-fix cut, see §5's status) |

Five gaps were found by the model ahead of any code, all closed by
rules now in §12. First, a stamp read among the attached legs is conclusive
only when those legs number a majority of the newest stamp's size, since
two majorities of one size intersect; with one leg attached after three
failures an older stamp looks newest, and the rule defers instead, which
is the outcome record-before-ack gives, unavailable rather than lost.
Second, a minority stamp the record never confirmed overwrote the only
copies of the majority stamp before it, so a reader that skipped it as
invalid fell back to an older stamp and served a leg missing an
acknowledged write; every stamp now carries the last released generation
and its members, and an unconfirmed stamp stands for that view. Third,
the decree as first written stranded a volume: two legs failed, the raid
held at the third for the record, the monitor marked the two stale during
the hold, the pod moved before the confirm, and the decree marked the
third stale as well, leaving every acknowledged write on a leg the record
no longer trusted. Sparing the record's last `in_sync` leg closed that
and not the next: with the record holding a leg `in_sync` that had missed
a write, a vouch that counted the previous consumer's leg stale by decree
served without it. The decree is now written only by a stage whose read
is conclusive without that leg; otherwise the stage waits for it. F36c's
180 s bound, after which the gate served the reachable legs with the risk
surfaced, was outside the model's HOLDs. The user had it measured (the
expiry arm loses an acknowledged write at depth 12 when the leg holding
it is unreachable at the stage) and dropped it: the timer is an alert,
the node-gone serve and the operator's cut are the only serves with risk,
and the model's HOLDs are the product's claim. Step 2 gained the
even-size vouch with it, so a two-replica volume does not wait for an
absent peer. Fourth, a fenced old consumer's unacknowledged writes on its
own leg, freed by hygiene before the first stage attempt, entered the new
assembly as a member's content, survived the first cut inside a trusted
epoch and came back through every later catch-up; a leg whose newest
stamp is an unconfirmed continuation is now a torn leg, marked stale at
the stage whenever a clean member is attached, never serving alone on a
vouch, and the write hole shrinks to the writes in flight at a crash or
at the fence. Fifth, from the stamp proof (2026-10-31): the read's vouch
by every unattached leg being record-stale needs the record's in_sync
set to keep a leg of the last released membership, which the model kept
only by never failing a raid's last member; two single-leg stale marks
after a majority release strand the earlier-lost leg as the only in_sync
one (rev 14, depth 16). The user adopted the in-write rule, that no
sync-record write may leave in_sync without a member of the live raid as
read, the stage's write included, and the whole mark, which the monitor
already does, only for an online raid, and becomes a pin; the proof uses
the rule in place of the premise, its target rev 16. The drain probe
showed the in-write guard carries the data invariant and a stamp on a
planned removal carries the barrier claim; whether a planned removal
stamps was adopted by the user on 2026-10-31: the hook sits on SPDK's
shared removal path, a planned removal begins the hold at the RPC and
stamps and holds like a loss, and the drain enters the proof. A model correction followed: the model
let a zombie acknowledge on a confirmed continuation's sequence and
generation rather than on the confirmed triple naming its leg alone,
which §12 requires; at four replicas that loses a write (rev 16, depth
11), rev 17 tests the triple, every three-replica row stands as a claim
to be rechecked, and the four-replica arm awaits a rerun on rev 17.
flint-91's five raid1 traps for the completion hold (split I/O, the
processed channel, process reads, a reset aborting a stamp write, the app
thread's channels) are in §12 with them.

Resize (2026-10-30, two windows from flint-e4 at the user's word): the
raid follows an lvol's growth at once, so the stamp's move is made by
whoever holds the leg, the raid on the growth event or the driver
through the RPCs when no raid holds it: the stamp copied to the new tail
with the old tail recorded, the old region zeroed, the record cleared;
an interrupted move is finished at assembly before the raid comes
online, and the reserve comes off in `raid1_resize` as in configure, or
the first grow exposes the new tail. A leg absent during a grow is
either marked, which the expand's belt refuses outright, or a lost-mark
leg whose stamp the barrier made older at its loss; the leg-size guard,
stock raid1's refusal of a shorter base and catch-up's F56 grow keep it
from serving until it is grown and re-stamped. The model has fixed-size
legs; no verdict row covers the resize rule.

Verdicts (2026-10-30, one pinned FlintCatchupS md5 per batch; the table
at the end of §12): at two replicas the full rule set holds over the
full state space on the zombie move (rev 11 `46c89207`, 29,120,225
states, depth 39), with and without a late capture, and each rule has a
control that fails without it in the same configuration; the lost-mark
and crash arms hold at two replicas as well. At three replicas the gated
decree (rev 7 `6859c997`) is clean to depth 23 on the lost mark and to
depth 22 on the crash, bounded at the box's hour; its zombie arm
fails at depth 17 on the pre-stamp write, the write hole, which rev 7
could not count as legal and rev 10 does. Under the full rule set (rev
12 `8e54566d`, the user's 192-core run, c27) three replicas are a full
pass on the lost mark and the crash (1,774,204,193 and 2,325,892,812
states) and the zombie arm is checked to depth 27 without a violation,
bounded, not a full pass. c21 closed on 2026-10-31 with every remaining
rev 7 three-replica arm bounded at the box's cap, an hour, or half an
hour for the acknowledgement stamp (depths 20 to 24: three failures
allowed, the member-only read, the acknowledgement stamp, the record
unwritable, the late capture), its zombie arms at the depth 17
hole, and its three-replica liveness arm at no verdict; the barrier's
termination was then shown at two replicas over the full space (c28,
2026-10-31, rev 12: the lost mark, the crash and the zombie hold under
the spec's weak fairness, each record-down control lassos; c30 drops
the fairness on a failed path's return and the three hold unchanged,
with a positive control showing it was dropped) and stays
unshown at three; the user accepted that as the claim on 2026-10-31 on
rev 12, and the same day the rerun on the final setting (the rule, the
drain, the triple) refuted the property as stated by a dead raid, every member leaving
under a held loss by two failures or by a drain beside a failing leg,
nothing acknowledged lost; restated as "a barrier ends, or the raid has no member left" it holds
over the full space in every fault on the final setting, and the user
accepted it as the claim on 2026-10-31, with a drain guard asking for a healthy member as the
design question beside it; the monitor-skips arm neither confirms nor
refutes the
strand's alternative fix, since the hole masks it. The expiry
action loses an acknowledged write at depth 12, the measurement that
decided the bound. Two of c21's controls ran in the wrong configuration
and are not cited. One claim was sharpened by a permitted trace: a
zombie never acknowledges on an unconfirmed continuation, but a sole
continuation the record confirmed before the move goes on acknowledging
on its leg, which the next stage waits for, so nothing acknowledged
there is left behind. The fourth gap's trace was reclassed by the later
knobs: the write that finds the fence lands before the stamp and is the
write hole, legal and bounded; the torn-leg rule covers the writes a
fenced writer lands after its stamp, which the controls without it serve
(beside a clean leg at depth 17, alone on the vouch at depth 7). §5's
two rev 7 asks were answered by c16 (FlintCatchupT rev 15 `8ac3cf00`,
TieNA `780c1009`, every arm a full pass): the fence-flip mark closes the
defer seam's catch-up claim under trust only and leaves §6's residual,
which needs record-before-ack; the tie's attached conjunct is killed by
no arm, with or without record-before-ack, so nothing credits it.
Nothing in §12 is built. On 2026-10-31 the user gated the raid1 patch
on the models and proofs being complete; the checklist is under §12's
delivery order (the trust rule's fresh-cache record, a TLAPS proof of
the stamp rule, which does not exist yet, the three-replica zombie and
liveness arms as full passes, the unrun c27 arms, resize modelled or
excluded, the model files on main). flint-e4's estimates, 2026-10-31:
the stamp proof five to ten days, or two to three for a proof of the
read rule alone; the three-replica zombie full pass not feasible on the
box; liveness full at two replicas only; one EC2 session of a few hours
for the three-replica acknowledgement stamp and the unrun arms; a
resize arm one to two days. On 2026-10-31 the user chose the full stamp
proof and approved the EC2 session (tla-mc 0.2.2 compiled, built
first); two-replica liveness as the claim, resize, and the model-files
push await their word.
