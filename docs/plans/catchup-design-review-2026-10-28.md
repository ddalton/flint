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

- [ ] Let `Stage` and `StageAdmit` admit any Reachable in-sync leg, as the
      code does. Keep the oracle version as a control.
- [ ] Add `Inv_MembersHold`: every configured member holds every acked write
      with no write in flight. Expect it to fail by design in the
      survivor-divergence case; the point is to see which observation
      premise makes it hold.
- [ ] A per-consumer reachability, or a Restage action that resets `path`
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

- [ ] The three model changes above, on FlintCatchupN rev 7, with the
      shipped and tie arms as controls.
- [ ] A `PerEpochTrust` arm: `recorded` carries the configured set per
      epoch, `Sources` and `BaseCandidates` apply the rule, no tie, no
      floor, no `mustRev`. Expect HOLD under both premises and under the
      lost-mark arm.
- [ ] Say in the gate's RESULTS which premise each HOLD rests on.

flint-91:

- [ ] Ship the stage fix as it stands; add the lost-mark caveat to the
      CHANGELOG entry.
- [ ] The unstage capture first: the smallest change, and it closes the
      most.
- [ ] Then the per-epoch membership in the sync record's epoch entries, and
      the rule in `select_base_epoch` and `in_sync_sources`, behind the
      model's arm.
