\* NON-VACUITY PROBES for the A2 tranche. Each probe is an invariant TLC must
\* VIOLATE, and the violation is what licenses citing the corresponding green
\* run.
\*
\* A green run whose situation never arises is the failure mode this whole
\* tranche exists to correct (FlintReplicationRaidReconcile.cfg went green on
\* a hazard a scalar could not represent). So the probes are permanent, and
\* re-runnable, rather than a thing I did once by hand.
\*
\* 2026-07-30 — AND THAT IS EXACTLY WHAT HAPPENED ANYWAY, because PROBE 2
\* below tests the WRONG PREDICATE. It asks whether the NAIVE A2's guard ever
\* becomes satisfiable; it does not ask whether the BELTED action ever fires.
\* Those differ precisely where it matters: the naive guard wants
\* `vaNode # VaTruth` (the dangerous case) and the belt refuses exactly that,
\* so PROBE 2 can violate — "the hazard arises!" — in a run where
\* AgentBootReconcile is disabled from beginning to end. It was, over the
\* complete 1,261,953-state graph of FlintReplicationA2Staging.cfg.
\*
\* The lesson is narrow and worth stating exactly: a non-vacuity probe must
\* name the ACTION, not the SITUATION. PROBE 3 does. It is kept alongside the
\* broken one rather than replacing it, because "I built a probe for this and
\* it still got past me" is the part worth remembering.
\*
\* THE STANDING RULE: no cfg may claim to exercise A2 without a paired
\* ProbeA2Fires run in check-tla.sh that TLC VIOLATES.
---------------------------- MODULE FlintA2Probe ----------------------------
EXTENDS FlintReplication

\* PROBE 1, for FlintReplicationUncontrolledBlind.cfg. That run is green,
\* meaning "no interleaving recovers the volume". Worthless unless the tgt
\* actually dies in it. TLC must violate this.
ProbeTgtDeathReachable == ~raidLostOnce

\* PROBE 2, for FlintReplicationA2Staging.cfg. That run is green with the
\* local-staging belt on. Worthless unless the DANGEROUS SITUATION arises —
\* i.e. unless a state is reachable where the naive A2's guard is satisfied
\* (the VA names a node that is not where the consumer is, nothing is
\* assembled there, and there is something to assemble from). If this is
\* reachable and the belted run is still green, the belt is doing real work.
\* If it is NOT reachable, the green means only that a disabled action is
\* harmless.
ProbeA2WouldHaveFired ==
  ~(  vaNode # VaTruth
   /\ vaNode \in Legs \cup {"remote"}
   /\ vaNode \notin raidHosts
   /\ UpInSync # {})

\* PROBE 3 (2026-07-30) — THE ONE THAT ASKS THE RIGHT QUESTION.
\* `a2Created` is A2's provenance set and AgentBootReconcile is its only
\* writer, so this holds exactly when A2 never executed anywhere in the
\* reachable state space. Any run that cites the A2 belt must VIOLATE it.
\*
\* Why A2Staging could not violate it: A2 answers a CLASS-3 destroyer (a tgt
\* dying with node and consumer in place, leaving `staged` set), and its belt
\* requires `stagedAt = vaNode`. A2Staging pins UncontrolledTgtDeath = FALSE,
\* so the only destroyers available are the ones that CLEAR staging — pod
\* delete and relocation. The belt's precondition and the reachable
\* destroyers were mutually exclusive, so the guard was unsatisfiable by
\* construction. FlintReplicationA2Armed.cfg arms the class-3 death and the
\* probe violates in three states.
ProbeA2Fires == a2Created = {}

=============================================================================
