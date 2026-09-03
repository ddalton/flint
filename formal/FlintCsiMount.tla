----------------------------- MODULE FlintCsiMount -----------------------------
(***************************************************************************)
(* The s3.chert.us CSI node driver's volume lifecycle, modelled AFTER the   *)
(* code and AFTER the kind drills — because the drills found a DATA LOSS    *)
(* the design review had not, and the reason it was invisible is a          *)
(* modelling lesson this module exists to make permanent.                   *)
(*                                                                          *)
(* THE DEFECT (spdk-csi-driver/src/s3csi/fuse.rs, node.rs; fixed in the     *)
(* commit that adds this module's fix, found on kind 2026-09-03).  The      *)
(* driver decides what a `NodePublishVolume` retry means by asking whether  *)
(* the target is already a mount point:                                     *)
(*                                                                          *)
(*     if phase = published /\ is_mountpoint(target) -> republish (refresh) *)
(*     else if is_mountpoint(src)                    -> rebind              *)
(*     else                                          -> cleanup, start over *)
(*                                                                          *)
(* and `cleanup` removes the volume directory, which for a lean workspace   *)
(* IS THE TENANT'S TREE.  `is_mountpoint` compared the target's st_dev with *)
(* its parent's.  A `MS_BIND` of a directory into a target on the SAME      *)
(* filesystem keeps the device number — and every lean bind is exactly      *)
(* that (tree and target both under /var/lib/kubelet).  So the sensor       *)
(* answered "not mounted" about a live mount, the driver took the           *)
(* start-over branch, and the tree went away under a running agent.  The    *)
(* syncer then published its (truncated) view as a full snapshot, and the   *)
(* files the app had already been told were durable were gone from the      *)
(* bucket.  Measured signature: a manifest citing the LAST 22 of 200 seeded *)
(* files.                                                                   *)
(*                                                                          *)
(* WHY NO EXISTING MODEL COULD HAVE CAUGHT IT.  The state machine is right: *)
(* idle -> checkingout -> published -> drain -> unpublish is the shape the  *)
(* code implements and a model of it verifies happily.  What was wrong is a *)
(* PREDICATE every such model takes as ground truth — "the driver can tell  *)
(* whether the target is mounted".  That is the recurring lesson of this    *)
(* directory (THE ABSTRACTION WAS THE BUG, three times before this one),    *)
(* so here the mount test is a FIRST-CLASS STATE VARIABLE that can disagree *)
(* with the kernel:                                                         *)
(*                                                                          *)
(*     RealMounted(c)   the kernel's truth                                  *)
(*     SensorTarget(c)  what is_mountpoint(target) ANSWERS                  *)
(*                                                                          *)
(* Under MountOracle = "mountinfo" (the fix: read /proc/self/mountinfo) the *)
(* two agree.  Under "dev" (the defect) the sensor is blind to a            *)
(* same-filesystem bind, and TLC finds the loss in a handful of steps.      *)
(* SameFsBind = FALSE recovers the passthrough arm, whose source is a FUSE  *)
(* mount on its own device: the dev oracle SEES that one, which is exactly  *)
(* why months of passthrough drills never showed the bug.                   *)
(*                                                                          *)
(* THE MULTI-CLUSTER AXIS, because it is the common case.  Agents run on    *)
(* several clusters against ONE project prefix in ONE bucket; nothing in    *)
(* flint spans clusters, so the bucket IS the coupling.  Each cluster has   *)
(* its own node plugin, its own worker and its own copy of the tree, and    *)
(* the only cross-cluster machinery is the prefix's lease and the manifest. *)
(* Two orderings must hold or the shared prefix loses data:                 *)
(*                                                                          *)
(*   - LeaseCheck: a checkout takes the prefix lease first, so two clusters *)
(*     never serve the same workspace at once (drill leg M3's EXCLUSIVITY   *)
(*     assertion: cluster 2's pod stays ContainerCreating while cluster 1   *)
(*     holds it).                                                           *)
(*   - DrainBeforeRelease: the departing cluster PUBLISHES its final state  *)
(*     before it releases the lease.  Release first and the next cluster    *)
(*     checks out a stale manifest and republishes it — the departing       *)
(*     cluster's late files are lost with no error anywhere.                *)
(*   - LeaseExpiry: a cluster RECLAIMED while holding the workspace (spot   *)
(*     is the routine event here) leaves the prefix stamped with a holder   *)
(*     that no longer exists.  Safety survives it; without the supersede    *)
(*     arm, liveness does not, and the project is unreachable from every    *)
(*     cluster in the fleet.                                                *)
(*                                                                          *)
(* Both are mutations here, and both violate the same durability invariant  *)
(* the mount sensor does: a file the app was TOLD is published never        *)
(* vanishes from the bucket.  That is the point of putting the two axes in  *)
(* one module — the sensor bug is only fully visible when the truncated     *)
(* tree's contents flow to the shared prefix, which is where the drill saw  *)
(* it.                                                                      *)
(*                                                                          *)
(* WHAT THE LEASE IS HERE.  A holder variable, not a protocol: the CAS     *)
(* cell that implements it in the bucket — acquire on If-None-Match,       *)
(* supersede on If-Match with epoch+1, renew, the quiet-poll takeover — is *)
(* FlintTierEpoch.tla's subject and is not re-modelled.  What this module  *)
(* adds is what that lease is FOR across clusters: which side of it the    *)
(* checkout and the final publish fall on.                                 *)
(*                                                                          *)
(* SCOPE.  Credentials, the broker exchange and the token's cluster scoping *)
(* are one-step authorization checks, not interleavings, and the drill      *)
(* covers them (leg M2: a token minted on cluster 1 is refused by cluster   *)
(* 2's broker at TokenReview).  The FUSE mounter's own liveness is          *)
(* FlintTierMarker's kind of question, not this one.  Apps here are         *)
(* append-only: a file the app deletes is not a durability violation, and   *)
(* modelling deletes would only weaken the invariant.                       *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Clusters,           \* the clusters sharing ONE bucket prefix
  Files,              \* what an app can write (two is enough: one per cluster)
  MountOracle,        \* "mountinfo" = the fix (read /proc/self/mountinfo)
                      \* "dev"       = the defect (compare st_dev with parent)
  SameFsBind,         \* TRUE  = source and target share a filesystem — EVERY
                      \*         lean bind (tree and target under the kubelet
                      \*         root), the case the dev oracle cannot see.
                      \* FALSE = a foreign-filesystem source (passthrough's
                      \*         FUSE mount), which both oracles do see.
  CleanupGuard,       \* TRUE = cleanup REFUSES a published lean volume — the
                      \* second line of defence added with the fix.  Its own
                      \* run proves it is load-bearing rather than decorative.
  LeaseCheck,         \* TRUE = a checkout takes the prefix lease first
  DrainBeforeRelease, \* TRUE = unpublish publishes the final tree BEFORE it
                      \* releases the lease
  LeaseExpiry,        \* TRUE = a lease whose holder is GONE can be taken over
                      \* (FlintTierEpoch's quiet-poll supersede).  FALSE is the
                      \* mutation: a cluster that dies holding the workspace
                      \* holds it forever and the fleet starves.
  MaxWrites,          \* app writes per cluster
  MaxCycles,          \* pod generations per cluster
  MaxCrashes          \* node losses across the fleet (spot reclamation is the
                      \* routine event here, and it takes the tree with it)

NoHolder == "none"
NoOrigin == "none"

VARIABLES
  phase,    \* [Clusters -> {"idle","checkingout","published","draining"}]
  pod,      \* [Clusters -> {"absent","waiting","running","gone"}]
  tree,     \* [Clusters -> SUBSET Files] — the plugin-owned checkout
  bound,    \* [Clusters -> BOOLEAN] — the KERNEL's truth about the bind
  manifest, \* SUBSET Files — the bucket's published snapshot (full, not delta)
  acked,    \* SUBSET Files — files an app was told are durable
  origin,   \* [Files -> Clusters \cup {NoOrigin}] — who wrote each file
  lease,    \* Clusters \cup {NoHolder} — the prefix lease
  writes,   \* [Clusters -> Nat]
  cycles,   \* [Clusters -> Nat]
  crashes,  \* node losses so far
  wiped,    \* WITNESS: a published volume's tree was removed under a live pod
  drainLost \* WITNESS: a pod exited cleanly and its FINAL publish was refused
            \* because the prefix had been superseded under it

vars == <<phase, pod, tree, bound, manifest, acked, origin, lease,
          writes, cycles, crashes, wiped, drainLost>>

TypeOK ==
  /\ phase \in [Clusters -> {"idle", "checkingout", "published", "draining"}]
  /\ pod \in [Clusters -> {"absent", "waiting", "running", "gone"}]
  /\ tree \in [Clusters -> SUBSET Files]
  /\ bound \in [Clusters -> BOOLEAN]
  /\ manifest \subseteq Files
  /\ acked \subseteq Files
  /\ origin \in [Files -> Clusters \cup {NoOrigin}]
  /\ lease \in Clusters \cup {NoHolder}
  /\ writes \in [Clusters -> 0..MaxWrites]
  /\ cycles \in [Clusters -> 0..MaxCycles]
  /\ crashes \in 0..MaxCrashes
  /\ wiped \in BOOLEAN
  /\ drainLost \in BOOLEAN

Init ==
  /\ phase = [c \in Clusters |-> "idle"]
  /\ pod = [c \in Clusters |-> "absent"]
  /\ tree = [c \in Clusters |-> {}]
  /\ bound = [c \in Clusters |-> FALSE]
  /\ manifest = {}
  /\ acked = {}
  /\ origin = [f \in Files |-> NoOrigin]
  /\ lease = NoHolder
  /\ writes = [c \in Clusters |-> 0]
  /\ cycles = [c \in Clusters |-> 0]
  /\ crashes = 0
  /\ wiped = FALSE
  /\ drainLost = FALSE

(***************************************************************************)
(* THE SENSOR.  `bound` is the kernel; these two are what the driver's      *)
(* is_mountpoint() ANSWERS about the target and about the source.          *)
(*                                                                          *)
(* The dev oracle compares st_dev with the parent's, so it sees a mount     *)
(* only when the mounted filesystem differs from the one underneath — true  *)
(* of a FUSE source, FALSE of a bind within one filesystem.  The mountinfo  *)
(* oracle reads the kernel's own table and cannot be fooled either way.     *)
(*                                                                          *)
(* The source test has no oracle dimension: for lean the source is a plain  *)
(* directory (never a mount, under any oracle) and for passthrough it is a  *)
(* foreign-filesystem FUSE mount (seen by both).                            *)
(***************************************************************************)

SensorTarget(c) ==
  IF MountOracle = "mountinfo" THEN bound[c] ELSE (bound[c] /\ ~SameFsBind)

SensorSrcIsMount == ~SameFsBind

(***************************************************************************)
(* Kubelet admits the pod and calls NodePublishVolume.  The volume is not   *)
(* published yet: no container of the pod has started (the CSI gate the     *)
(* webhook design could never give an init container).                     *)
(***************************************************************************)

PodAdmitted(c) ==
  /\ pod[c] = "absent"
  /\ phase[c] = "idle"
  /\ cycles[c] < MaxCycles
  /\ pod' = [pod EXCEPT ![c] = "waiting"]
  /\ cycles' = [cycles EXCEPT ![c] = cycles[c] + 1]
  /\ UNCHANGED <<phase, tree, bound, manifest, acked, origin, lease, writes,
                 crashes, wiped, drainLost>>

(***************************************************************************)
(* The publish path.  Taking the prefix lease is what keeps two clusters    *)
(* off one workspace; LeaseCheck = FALSE is the mutation that removes it.   *)
(***************************************************************************)

StartPublish(c) ==
  /\ pod[c] = "waiting"
  /\ phase[c] = "idle"
  /\ (LeaseCheck => lease = NoHolder)
  /\ lease' = IF LeaseCheck THEN c ELSE lease
  /\ phase' = [phase EXCEPT ![c] = "checkingout"]
  /\ UNCHANGED <<pod, tree, bound, manifest, acked, origin, writes, cycles,
                 crashes, wiped, drainLost>>

\* The checkout materialises the published snapshot into the tree, writes
\* the marker, binds the tree into the pod, and only THEN do the pod's
\* containers start.  One step: the drill proved the gate holds for the
\* app and for its init container, so the interesting interleavings are
\* not inside it.
Checkout(c) ==
  /\ phase[c] = "checkingout"
  /\ tree' = [tree EXCEPT ![c] = manifest]
  /\ bound' = [bound EXCEPT ![c] = TRUE]
  /\ phase' = [phase EXCEPT ![c] = "published"]
  /\ pod' = [pod EXCEPT ![c] = "running"]
  /\ UNCHANGED <<manifest, acked, origin, lease, writes, cycles, crashes,
                 wiped, drainLost>>

(***************************************************************************)
(* The app and its syncer.  Both are gated on the POD being alive and not   *)
(* on the driver's phase — that is not a modelling convenience, it is the   *)
(* mechanism: the syncer is a process in a worker pod that keeps scanning   *)
(* and publishing whatever the tree holds, and it neither knows nor cares   *)
(* that the node plugin has decided to start the volume over.  A publish is *)
(* a FULL SNAPSHOT of the tree, which is why a truncated tree does not just *)
(* stall — it overwrites.                                                   *)
(***************************************************************************)

AppWrite(c, f) ==
  /\ pod[c] = "running"
  /\ writes[c] < MaxWrites
  /\ f \notin tree[c]
  /\ tree' = [tree EXCEPT ![c] = tree[c] \cup {f}]
  /\ origin' = [origin EXCEPT ![f] = c]
  /\ writes' = [writes EXCEPT ![c] = writes[c] + 1]
  /\ UNCHANGED <<phase, pod, bound, manifest, acked, lease, cycles, crashes,
                 wiped, drainLost>>

\* FENCED on the lease: the syncer's publish is a conditional write against
\* the prefix's epoch (FlintTierEpoch's If-Match), so a holder that has
\* been superseded cannot land bytes.  Modelling the publish as
\* unconditional would make the model WEAKER than the code and would
\* produce counterexamples the real system refuses.
DeclaredPublish(c) ==
  /\ pod[c] = "running"
  /\ (LeaseCheck => lease = c)
  /\ manifest' = tree[c]
  /\ acked' = acked \cup tree[c]
  /\ UNCHANGED <<phase, pod, tree, bound, origin, lease, writes, cycles,
                 crashes, wiped, drainLost>>

(***************************************************************************)
(* THE REPUBLISH LADDER — the branch the sensor decides, verbatim from      *)
(* node.rs.  Kubelet re-drives NodePublishVolume for every mounted volume   *)
(* every ~60-90 s (requiresRepublish), so this runs constantly under a      *)
(* healthy pod; it is not an error path.                                    *)
(***************************************************************************)

RepublishRefresh(c) ==       \* the sensor sees the mount: refresh credentials
  /\ pod[c] = "running"
  /\ phase[c] = "published"
  /\ SensorTarget(c)
  /\ UNCHANGED vars

RepublishRebind(c) ==        \* target unseen, source seen: rebind, no data touched
  /\ pod[c] = "running"
  /\ phase[c] = "published"
  /\ ~SensorTarget(c)
  /\ SensorSrcIsMount
  /\ bound' = [bound EXCEPT ![c] = TRUE]
  /\ UNCHANGED <<phase, pod, tree, manifest, acked, origin, lease, writes,
                 cycles, crashes, wiped, drainLost>>

RepublishGuarded(c) ==       \* the fix's second line: cleanup refuses a published volume
  /\ pod[c] = "running"
  /\ phase[c] = "published"
  /\ ~SensorTarget(c)
  /\ ~SensorSrcIsMount
  /\ CleanupGuard
  /\ UNCHANGED vars

\* THE LOSS.  Neither test saw a mount, so the driver concludes this is an
\* unfinished publish and starts over: the volume directory — the tree —
\* is removed while the pod runs on.
RepublishCleanup(c) ==
  /\ pod[c] = "running"
  /\ phase[c] = "published"
  /\ ~SensorTarget(c)
  /\ ~SensorSrcIsMount
  /\ ~CleanupGuard
  /\ tree' = [tree EXCEPT ![c] = {}]
  /\ bound' = [bound EXCEPT ![c] = FALSE]
  /\ phase' = [phase EXCEPT ![c] = "idle"]
  /\ wiped' = TRUE
  /\ UNCHANGED <<pod, manifest, acked, origin, lease, writes, cycles, crashes, drainLost>>

\* Kubelet's retry re-materialises the tree under the still-running pod.
\* Whether this lands before or after the syncer's next publish is the
\* whole difference between a scare and a data loss.
RepublishRetry(c) ==
  /\ pod[c] = "running"
  /\ phase[c] = "idle"
  /\ tree' = [tree EXCEPT ![c] = manifest]
  /\ bound' = [bound EXCEPT ![c] = TRUE]
  /\ phase' = [phase EXCEPT ![c] = "published"]
  /\ UNCHANGED <<pod, manifest, acked, origin, lease, writes, cycles, crashes,
                 wiped, drainLost>>

(***************************************************************************)
(* Teardown.  NodeUnpublishVolume runs only after every container has       *)
(* exited, which is what makes a final drain possible at all.  The ORDER of *)
(* the drain and the lease release is the cross-cluster hinge.             *)
(***************************************************************************)

DeletePod(c) ==
  /\ pod[c] = "running"
  /\ pod' = [pod EXCEPT ![c] = "gone"]
  /\ UNCHANGED <<phase, tree, bound, manifest, acked, origin, lease, writes,
                 cycles, crashes, wiped, drainLost>>

Unpublish(c) ==
  /\ pod[c] = "gone"
  /\ phase[c] \in {"published", "idle"}
  /\ IF DrainBeforeRelease
       THEN /\ manifest' = tree[c]
            /\ acked' = acked \cup tree[c]
            /\ lease' = IF lease = c THEN NoHolder ELSE lease
            /\ tree' = [tree EXCEPT ![c] = {}]
            /\ bound' = [bound EXCEPT ![c] = FALSE]
            /\ phase' = [phase EXCEPT ![c] = "idle"]
            /\ pod' = [pod EXCEPT ![c] = "absent"]
       ELSE \* the mutation: the lease goes first and the drain follows
            /\ lease' = IF lease = c THEN NoHolder ELSE lease
            /\ phase' = [phase EXCEPT ![c] = "draining"]
            /\ UNCHANGED <<manifest, acked, tree, bound, pod>>
  /\ UNCHANGED <<origin, writes, cycles, crashes, wiped, drainLost>>

DrainLate(c) ==
  /\ phase[c] = "draining"
  /\ (LeaseCheck => lease = c)
  /\ manifest' = tree[c]
  /\ acked' = acked \cup tree[c]
  /\ tree' = [tree EXCEPT ![c] = {}]
  /\ bound' = [bound EXCEPT ![c] = FALSE]
  /\ phase' = [phase EXCEPT ![c] = "idle"]
  /\ pod' = [pod EXCEPT ![c] = "absent"]
  /\ UNCHANGED <<origin, lease, writes, cycles, crashes, wiped, drainLost>>

\* THE LOSS THE ORDERING RULE EXISTS TO PREVENT.  The pod exited cleanly,
\* so everything it wrote should be in the bucket — but the lease was
\* released before the final publish, another cluster superseded the
\* prefix in the gap, and this drain's conditional write is refused.  The
\* agent's last work is gone and nothing in the data plane says so: the
\* pod is Terminated, the volume is unpublished, the bucket is intact and
\* simply older than it should be.
DrainRefused(c) ==
  /\ phase[c] = "draining"
  /\ LeaseCheck
  /\ lease # c
  /\ tree[c] # {}
  /\ tree' = [tree EXCEPT ![c] = {}]
  /\ bound' = [bound EXCEPT ![c] = FALSE]
  /\ phase' = [phase EXCEPT ![c] = "idle"]
  /\ pod' = [pod EXCEPT ![c] = "absent"]
  /\ drainLost' = TRUE
  /\ UNCHANGED <<manifest, acked, origin, lease, writes, cycles, crashes,
                  wiped>>

(***************************************************************************)
(* THE COMMON MULTI-CLUSTER FAILURE: a cluster DIES holding the workspace.  *)
(* Spot reclamation is the routine event on this fleet, and it takes the    *)
(* node — plugin, worker and the plugin-owned tree — with it.  Nothing      *)
(* drains: whatever the agent had written and not published is gone, which  *)
(* is a known loss with a named recovery (recover-staged) and NOT a         *)
(* violation of durability, because durability here is about what the app   *)
(* was TOLD.  What must not happen is the fleet stalling: the prefix's      *)
(* lease is still stamped with a holder that no longer exists.              *)
(*                                                                          *)
(* LeaseExpiry is the supersede arm of the bucket's CAS cell                *)
(* (FlintTierEpoch's quiet-poll takeover, not re-modelled here).  With it   *)
(* on, another cluster proceeds; with it off, the workspace is unreachable  *)
(* forever and the liveness mutation says so.                               *)
(***************************************************************************)

ClusterCrash(c) ==
  /\ pod[c] = "running"
  /\ crashes < MaxCrashes
  /\ pod' = [pod EXCEPT ![c] = "absent"]
  /\ phase' = [phase EXCEPT ![c] = "idle"]
  /\ tree' = [tree EXCEPT ![c] = {}]
  /\ bound' = [bound EXCEPT ![c] = FALSE]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<manifest, acked, origin, lease, writes, cycles, wiped, drainLost>>

LeaseExpire ==
  /\ LeaseExpiry
  /\ lease # NoHolder
  /\ pod[lease] = "absent"
  /\ phase[lease] = "idle"
  /\ lease' = NoHolder
  /\ UNCHANGED <<phase, pod, tree, bound, manifest, acked, origin, writes,
                  cycles, crashes, wiped, drainLost>>

Next ==
  \/ \E c \in Clusters :
       \/ PodAdmitted(c) \/ StartPublish(c) \/ Checkout(c)
       \/ DeclaredPublish(c)
       \/ RepublishRefresh(c) \/ RepublishRebind(c)
       \/ RepublishGuarded(c) \/ RepublishCleanup(c) \/ RepublishRetry(c)
       \/ DeletePod(c) \/ Unpublish(c) \/ DrainLate(c) \/ DrainRefused(c)
       \/ ClusterCrash(c)
  \/ LeaseExpire
  \/ \E c \in Clusters, f \in Files : AppWrite(c, f)

\* The driver's own steps are weakly fair.  The app's writes are the
\* environment and are NOT — nothing may depend on an agent writing.
\*
\* DeletePod IS fair, and that is an assumption worth stating out loud:
\* the liveness question here is "does a cluster waiting on another
\* cluster's lease eventually get served, or is it stuck?", and that
\* question only has an answer if the holder's pod eventually ends.  A
\* pod that never exits holds the workspace forever BY DESIGN — one
\* holder at a time is the whole point — so without this the property
\* would be false for an uninteresting reason.
Fairness ==
  /\ \A c \in Clusters :
       /\ WF_vars(StartPublish(c)) /\ WF_vars(Checkout(c))
       /\ WF_vars(RepublishRetry(c))
       /\ WF_vars(DeletePod(c))
       /\ WF_vars(Unpublish(c)) /\ WF_vars(DrainLate(c))
       /\ WF_vars(DrainRefused(c))
  /\ WF_vars(LeaseExpire)

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* THE INVARIANTS.                                                          *)
(*                                                                          *)
(* Inv_NoAckedLoss is the durability statement and the one every mutation   *)
(* here violates: a file the app was told is published is still in the      *)
(* bucket.  It is deliberately about the BUCKET and not about any cluster's *)
(* tree — the tree is a cache, the prefix is the project.                   *)
(*                                                                          *)
(* Inv_NoTreeLoss is the local face: nobody removes a published volume's    *)
(* tree while its pod is alive.  A design can satisfy it and still lose     *)
(* data (that is why both are here), but it is the one an operator can      *)
(* check on a node.                                                          *)
(*                                                                          *)
(* Inv_SingleWriter is the cross-cluster exclusivity the drill asserts on   *)
(* the wire: at most one cluster serves the workspace at a time.            *)
(***************************************************************************)

Inv_NoAckedLoss == acked \subseteq manifest
Inv_NoTreeLoss == ~wiped

\* A pod that exits CLEANLY always gets its final publish in.  This is the
\* half of durability that `acked` cannot see: the last work an agent did
\* was never acknowledged to anyone, so losing it violates no promise made
\* to the app — but it is the promise the drain itself makes, and the
\* ordering of the drain and the lease release is the only thing keeping
\* it.  (A cluster RECLAIMED mid-run is a different story with a named
\* recovery: nothing drains, and nothing claimed it would.)
Inv_NoLostDrain == ~drainLost
Inv_SingleWriter ==
  Cardinality({c \in Clusters : phase[c] = "published"}) <= 1

Inv == TypeOK /\ Inv_NoAckedLoss /\ Inv_NoTreeLoss /\ Inv_SingleWriter
       /\ Inv_NoLostDrain

\* Durability alone, for the runs where a wipe is expected but must not
\* reach the bucket (the CleanupGuard=FALSE-with-mountinfo world does not
\* exist; the guarded dev-oracle world does).
InvDurableOnly == TypeOK /\ Inv_NoAckedLoss /\ Inv_SingleWriter

(***************************************************************************)
(* LIVENESS: a pod that is admitted eventually gets its volume, so a        *)
(* cluster waiting on another cluster's lease is delayed and never stuck.   *)
(* Bounded by MaxCycles, hence the disjunct.                                *)
(***************************************************************************)

EventuallyServed ==
  \A c \in Clusters : (pod[c] = "waiting") ~> (pod[c] \in {"running", "absent"})

\* And the durability half of liveness: whatever a live agent has in its
\* tree eventually reaches the bucket, without the agent declaring a
\* publish — that is what the drain at NodeUnpublishVolume is for.
EventuallyDrained ==
  \A c \in Clusters :
    (pod[c] = "gone") ~> (phase[c] = "idle" /\ pod[c] = "absent")

(***************************************************************************)
(* VACUITY PROBES — required-fail runs.  A green strict run over a state    *)
(* space where the interesting thing never happens is a green light over an *)
(* empty road, and this module has two roads that must be shown open.       *)
(***************************************************************************)

\* The handoff itself: a cluster serving a file ANOTHER cluster wrote.
\* TLC must violate this, or the multi-cluster tranche proves nothing.
Probe_HandoffReachable ==
  ~(\E c \in Clusters, f \in Files :
      /\ phase[c] = "published"
      /\ f \in tree[c]
      /\ origin[f] # c
      /\ origin[f] # NoOrigin)

\* The republish ladder's cleanup branch must be REACHABLE under the dev
\* oracle, or the mutation that depends on it is checking nothing.
Probe_CleanupReachable == ~wiped

================================================================================
