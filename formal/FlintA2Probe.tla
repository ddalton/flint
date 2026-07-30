\* SCRATCH MODULE — non-vacuity probes for the A2 tranche's two GREEN runs.
\* Not part of the gate; each probe is an invariant TLC must VIOLATE, and the
\* violation is what licenses citing the corresponding green run.
\*
\* A green run whose situation never arises is the failure mode this whole
\* tranche exists to correct (FlintReplicationRaidReconcile.cfg went green on
\* a hazard a scalar could not represent). So the probes are permanent, and
\* re-runnable, rather than a thing I did once by hand.
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

=============================================================================
