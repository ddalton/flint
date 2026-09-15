# 2026-09-15 live drill — host legs and the multi-node storm, on real S3

Four `i4i.large` one-time spot instances in `us-west-1` (AL2023, the
instance-store NVMe as the workspace disk), one bucket
(`flint-lean-drill-20260915`, deleted at the end), binaries built on the node
from the committed source. Three rounds, because the first two found defects in
the product and in this rig:

| round | syncer source | rig | what ran |
|---|---|---|---|
| 1 | `327a3541` (+ the F2 control patch, `dbe6b84f`) | `327a3541` | host legs H6 H5 H1 H2 H3, fixed and control; storm S0, S1 |
| 2 | `45420060` (F15 fixed, `6fb3f8e7`) | `45420060` | storm R2-S0 … R2-S3 |
| 3 | `45420060` (same binaries) | `1f6d8c2f` | storm R3-S0 … R3-S5 |

Cost: about 6 instance-hours of spot (~$0.25) and ~300k S3 requests (~$0.75).

## Host legs (round 1, real S3)

`hostlegs/hostlegs-summary.txt`, one directory per arm. Every fixed arm PASSED
and every control arm FAILED with its defect's signature:

| leg | defect | fixed | control |
|---|---|---|---|
| H6 | the queued tombstone over an adopted UI write (`b52324fc`) | PASS | FAIL (no checkout serves the acked UI write) |
| H5 | finding 10, the untracked-upload sweep (`bdf71176`) | PASS | FAIL (B and a fresh checkout disagree on 2 paths) |
| H1 | F1, the GC gap | PASS | FAIL after re-judging (see below) |
| H2 | F2, the adopt window | PASS | FAIL (the commit cites an object with HEAD 404) |
| H3 | F3, the sync overlay | PASS | FAIL (B keeps a file the manifest deleted) |

H1's control first read **VOID**: its unconditional GC did delete B's fresh PUT
inside the hold, but finding 13's re-read of every own PUT then found the object
gone, withheld the citation, and B re-uploaded it next barrier. The end-state
signature (a dangling citation) cannot fire while that second defence stands, so
H1 now also accepts F1's direct fingerprint — GC deleted, and B's re-read found
its PUT gone (`261da1c5`). `verdict.rejudged-261da1c5.json` holds the re-judged
verdicts of the same frozen evidence.

## Storm (3 nodes, 2 writers each, plus a UI actor on node 0)

`storm/<round>/<leg>/`: `verdict.node0.json` as the node judged it at the time,
`verdict.rejudged.json` from this repo's current `oracle.py`, and
`collect-slim.tgz` (the collect layout minus the raw copies, enough to re-judge:
`python3 oracle.py <collect> --oracles O1,O2,O3,O4,O5 --wall-slack-ms 200`).

| leg | mode | faults | verdict (re-judged) |
|---|---|---|---|
| round 1 S0-smoke | hot + UI | 3 | **FAIL O2** — F15, below |
| round 1 S1-hot-ui | hot + UI | 0 | **FAIL O2** — rig: the nodes did not quiesce together |
| round 2 R2-S0 … R2-S3 | hot, churn, vocab | 3 | PASS, PASS, PASS, **FAIL O2** (same rig quiesce) |
| round 3 R3-S0 … R3-S4 | hot, churn, vocab, hot+kills | 3, 0, 0, 0, 6 | PASS |
| round 3 R3-S5-churn-kills | churn + UI | 6 | **FAIL O2** — rig: the fleet was still publishing 4 s before the drain |

Faults are per leg across the fleet: odd ones kill the syncer and restart it on
the same tree, even ones replace the pod (tree and state directory go, a new
writer checks out).

## What the drill found

**One product defect.** F15 (`6fb3f8e7`, CHANGELOG "Unreleased"): a writer that
consumed a UI write re-cites it at its next commit; when a peer replaced the
object between the repair's HEAD and the commit-section re-read, the citation
was withheld and the path PARKED. Parking kept the replacing version out of that
writer's queue while its merge base moved past it, and the tree — clean against
its baseline — kept the UI bytes for good. No bytes were lost (the peer
preserved them), but one tree of six disagreed with the manifest and with every
other writer. Found by round 1's first 60-second leg; reproduced by
`a_repair_citation_withheld_at_the_commit_still_receives_the_version_that_replaced_it`,
which the control patch `f15-repair-withheld-parks` fails.

**Six rig defects**, each of which would have faked a result (all are in
`lean/FINDINGS.md` Table 2):

1. O3 refused to link a rewrite chain through a syncer's DROPPED write — 4
   phantom losses in S0 (`e2fa0878`).
2. O3 ordered an unanswered delete by op time against a no-op rewrite of the
   same bytes — 1 phantom loss in R2-S2 (`929efe4b`).
3. O3 treated a UI write answered with a 5xx as a refusal — 1 phantom loss in
   R3-S5, though its bytes are in a preserved copy (`8bc0c1a3`).
4. The nodes ended their load 9–13 s apart and drained independently — 16
   phantom divergences in S1, 1 in R2-S3 (`1f6d8c2f`).
5. The judge downloaded each preserved copy with its own CLI process (4,985 in
   S1, over 30 minutes), which pushed the next leg's start past the followers'
   wait and desynchronised every later leg (`45420060`).
6. The F2 control patch was anchored on a line finding 13's re-read had
   replaced, so H2 had no control binary until it was re-anchored (`dbe6b84f`).

**One rig limit that is a fact about the protocol.** A peer's change reaches a
tree through the barrier that queues it and the consume after it, so a fleet
needs about two barrier cycles after its last publish to converge. In a churn
leg with faults the backlog of parked paths kept publishing for tens of seconds
after every agent had stopped. The storm's idle now begins only once the
manifest pointer has held still for three floor ticks (`8bc0c1a3`); a leg that
never settles fails its verdict rather than judging a moving fleet.

## Scale seen here

One 300-second hot leg (6 writers and a UI over 30 shared paths) produced 4,985
preserved conflict copies and 5,306 objects under the prefix. Conflict copies
are never collected; an operator running agents this adversarial wants a bucket
lifecycle rule on `.flint/lean/conflicts/`.
