#!/usr/bin/env bash
# One-shot generator for the LeanSubtree run configs.  Kept in-tree so the
# cfg matrix is regenerable; the cfgs themselves are committed.
# (Plain variables, not associative arrays: macOS ships bash 3.2.)
set -eu
cd "$(dirname "$0")"

KEYS="MaxGen MaxSeq MaxHitl MaxBarriers MaxCrashes MaxRestarts MaxSyncs \
AllowStall InboxEnabled MergeCapable ConflictSurfacing WindowCheck Rotation \
EpochCheck GuardedGC DeletesAfterCAS RematerializeOnRestart SyncEnabled \
SyncScanFirst SyncScope ScopedInstBase GatedCitation AtomicCitation \
GCKeepsCurrent CiteDropsInflightHitl BackstopEnabled MineIsNotForeign \
MaxTouches \
SentinelEnabled FoldPending AckFromInstall RefuseOnFence FastPathGuards \
AckHonest LaneCancelsStaged GatedRepair StampBoundarySource"

emit() { # <name> <invariants (comma-sep)> <overrides (key=val ...)>
  local name=$1 invs=$2; shift 2
  local c_MaxGen=4 c_MaxSeq=6 c_MaxHitl=1 c_MaxBarriers=3
  local c_MaxCrashes=1 c_MaxRestarts=1 c_MaxSyncs=0 c_AllowStall=FALSE
  local c_InboxEnabled=TRUE c_MergeCapable=TRUE c_ConflictSurfacing=TRUE
  local c_WindowCheck=TRUE c_Rotation=TRUE c_EpochCheck=TRUE
  local c_GuardedGC=TRUE c_DeletesAfterCAS=TRUE c_RematerializeOnRestart=FALSE
  local c_SyncEnabled=FALSE c_SyncScanFirst=TRUE
  # tranche 3 product 4: FALSE in every pre-existing cfg, so tranche-1/2
  # state spaces are preserved by construction (scope collapses to Paths).
  local c_SyncScope=FALSE c_ScopedInstBase=TRUE
  # tranche 3 product 2: GatedCitation FALSE in every pre-existing cfg, so
  # the gated actions are disabled and versions/stage/stageBase/withheldDel
  # are frozen at Init — those state spaces are preserved by construction.
  local c_GatedCitation=FALSE c_AtomicCitation=TRUE c_GCKeepsCurrent=TRUE
  local c_CiteDropsInflightHitl=TRUE
  local c_BackstopEnabled=FALSE
  # The fix TLC forced: our own baseline is never a foreign change.
  local c_MineIsNotForeign=TRUE
  # tranche 3 product 1: SentinelEnabled FALSE in every pre-existing cfg,
  # so every sentinel action is disabled, the fast path is unreachable and
  # the new sc fields stay at their empty Init values — those state spaces
  # are preserved by construction.
  local c_MaxTouches=0 c_SentinelEnabled=FALSE c_FoldPending=TRUE
  local c_AckFromInstall=TRUE c_RefuseOnFence=TRUE c_FastPathGuards=TRUE
  # C6: FALSE in every pre-existing cfg. The drop it governs exists only
  # under GatedCitation, and no pre-existing cfg pairs that with
  # SentinelEnabled — which was the finding.
  local c_AckHonest=FALSE c_LaneCancelsStaged=FALSE c_GatedRepair=FALSE
  # The provenance stamp is TRUE everywhere: it is not an arm, it is the
  # shipped behaviour after the drill found the bucket and the ack naming
  # two different clocks for one boundary.
  local c_StampBoundarySource=TRUE
  local kv
  for kv in "$@"; do eval "c_${kv%%=*}=${kv#*=}"; done
  {
    echo "SPECIFICATION Spec"
    echo "CHECK_DEADLOCK FALSE"
    echo "CONSTANTS"
    echo "  Paths = {p1, p2}"
    local k v
    for k in $KEYS; do
      eval "v=\$c_$k"
      echo "  $k = $v"
    done
    local i
    for i in ${invs//,/ }; do echo "INVARIANT $i"; done
  } > "$name.cfg"
  echo "wrote $name.cfg"
}

ALLINV="TypeOK,Inv_HITLDurable,Inv_NoDangling,Inv_NoStragglerInstall,Inv_NoDeposedPut,Inv_NoResurrection"

# ---- strict runs -----------------------------------------------------------
# Breadth budget note: MaxBarriers=3 + MaxGen=4 blows past an hour of
# TLC; 2 barriers reach every stamp site INCLUDING the two-scan delete
# completion. The rich budget survives as LeanSubtreeDeep (not in the
# gate — an opt-in overnight run).
emit LeanSubtree "$ALLINV" MaxGen=3 MaxBarriers=2
emit LeanSubtreeDeep "$ALLINV"
emit LeanSubtreeTakeover "TypeOK,Inv_NoDangling,Inv_NoStragglerInstall,Inv_NoDeposedPut" \
  AllowStall=TRUE MaxHitl=0 MaxCrashes=0 MaxRestarts=0 MaxGen=3 MaxBarriers=2
# NOT a holds-run: TLC refuted the "merge alone closes amputation"
# claim (delete-after-absorption at depth 12) — a preserved-but-never-
# integrated foreign entry dies to a later local delete once Finish
# absorbs it into the merge base. THE INBOX IS LOAD-BEARING. Pinned as
# a mutation.
emit LeanDirectMergeInsufficient "Inv_HITLDurable" \
  InboxEnabled=FALSE MaxCrashes=0 MaxRestarts=0 MaxGen=3
emit LeanNoWindowHolds "TypeOK,Inv_HITLDurable,Inv_NoDangling" \
  WindowCheck=FALSE MaxCrashes=0 MaxRestarts=0 MaxGen=3
emit LeanEpochOnlyHolds "TypeOK,Inv_NoDangling,Inv_NoStragglerInstall,Inv_NoDeposedPut" \
  AllowStall=TRUE Rotation=FALSE MaxHitl=0 MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxBarriers=2

# ---- mutations (each REQUIRED to find its counterexample) ------------------
emit LeanAmputation "Inv_HITLDurable" \
  InboxEnabled=FALSE MergeCapable=FALSE MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxBarriers=2
emit LeanLocalWins "Inv_HITLDurable" \
  ConflictSurfacing=FALSE MaxCrashes=0 MaxRestarts=0 MaxGen=3 MaxBarriers=1
emit LeanGCUnguarded "Inv_HITLDurable" \
  GuardedGC=FALSE MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxBarriers=1
emit LeanDanglingOrder "Inv_NoDangling" \
  DeletesAfterCAS=FALSE MaxHitl=0 MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxBarriers=1
emit LeanNoRotate "Inv_NoStragglerInstall" \
  AllowStall=TRUE Rotation=FALSE EpochCheck=FALSE MaxHitl=0 MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxBarriers=1
emit LeanNoEpochCheck "Inv_NoDeposedPut" \
  AllowStall=TRUE EpochCheck=FALSE MaxHitl=0 MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxBarriers=1
emit LeanRematerialize "Inv_NoResurrection" \
  RematerializeOnRestart=TRUE MaxHitl=0 MaxCrashes=0 MaxGen=2 MaxBarriers=1

# ---- non-vacuity probes (each REQUIRED to be violated) ---------------------
emit LeanProbeBarrier "ProbeBarrierDone" \
  MaxHitl=0 MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxBarriers=1
emit LeanProbeHITLCited "ProbeHITLCited" \
  MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxBarriers=1
emit LeanProbeTakeover "ProbeTakeover" \
  MaxHitl=0 MaxRestarts=0 MaxGen=2 MaxBarriers=1
emit LeanProbeStragglerAttempt "ProbeStragglerAttempt" \
  AllowStall=TRUE MaxHitl=0 MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxBarriers=1
emit LeanProbePark "ProbePark" \
  MaxCrashes=0 MaxRestarts=0 MaxGen=3 MaxBarriers=1
emit LeanProbeGC "ProbeGC" \
  MaxHitl=0 MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxBarriers=1
emit LeanProbeRefusal "ProbeRefusal" \
  MaxHitl=0 MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxBarriers=1
emit LeanProbeAdoptOwn "ProbeAdoptOwn" \
  MaxHitl=0 MaxCrashes=0 MaxRestarts=1 MaxGen=2 MaxBarriers=2

# ---- tranche 2: the sync verb x barrier product ----------------------------
# Every cfg above keeps SyncEnabled=FALSE, so Sync is never enabled and the
# tranche-1 state spaces are preserved by construction (lastDirty stays {}).
emit LeanSyncHolds "$ALLINV,Inv_SyncNeverDestroysDirty" \
  SyncEnabled=TRUE MaxSyncs=1 MaxHitl=1 MaxGen=3 MaxBarriers=1 \
  MaxCrashes=0 MaxRestarts=0
emit LeanSyncStaleDirt "Inv_SyncNeverDestroysDirty" \
  SyncEnabled=TRUE SyncScanFirst=FALSE MaxSyncs=1 MaxHitl=1 MaxGen=3 \
  MaxBarriers=1 MaxCrashes=0 MaxRestarts=0
emit LeanProbeSyncApplied "ProbeSyncApplied" \
  SyncEnabled=TRUE MaxSyncs=1 MaxHitl=1 MaxGen=3 MaxBarriers=1 \
  MaxCrashes=0 MaxRestarts=0
emit LeanProbeSyncConflict "ProbeSyncConflict" \
  SyncEnabled=TRUE MaxSyncs=1 MaxHitl=1 MaxGen=3 MaxBarriers=1 \
  MaxCrashes=0 MaxRestarts=0

# ---- tranche 3, product 4: scoped sync x the merge base (D4) ---------------
# `instBase` is the object the model has refuted naive designs on twice.
# D4 rewrites its PER-PATH semantics, so it is modelled before the rule is
# trusted — the FlintTierSession precedent.
#
# WORLD NOTE (found by running it, and it cost a wrong first cfg).  The D4
# loss needs an out-of-scope change that lives in the MANIFEST, not in the
# inbox: an inbox-overlaid change survives a wholesale instBase advance
# untouched, because the entry itself is still queued.  In this design the
# only legitimate foreign manifest installer is a takeover SUCCESSOR, so
# these runs need AllowStall + a second barrier — with MaxBarriers=1 and no
# stall arm the hazard is UNREACHABLE and the mutation runs green against a
# state space that never contained the bug.  The same mistake made the first
# Rust test for this rule vacuous.
# Budget: MaxGen=2 + MaxHitl=0 (the takeover cfgs' depth-buying trick).
# Verified as a pilot BEFORE it was locked in: at this budget the holds run
# completes in ~9 s AND both the mutation and the probe still fire, so the
# strict run is not checking a smaller world than the bug lives in. At
# MaxGen=3/MaxHitl=1 the holds run passed 30M states without terminating.
SCOPEWORLD="SyncEnabled=TRUE SyncScope=TRUE AllowStall=TRUE MaxSyncs=1 \
MaxHitl=0 MaxGen=2 MaxBarriers=2 MaxCrashes=0 MaxRestarts=0"
# U41: $ALLINV carries Inv_HITLDurable and Inv_NoResurrection, and this
# world runs MaxHitl=0 / MaxRestarts=0 — `HitlWrite` and `Restart` are both
# disabled, so both lines were unfalsifiable by construction and inflated
# the product's apparent coverage. Dropped rather than enabled: turning on
# HITL and restarts here would change what the SCOPE product tests, and
# both invariants are checked where they can actually fire (LeanSubtree,
# LeanGatedHolds, LeanSentinelRestart).
emit LeanScopedSyncHolds "TypeOK,Inv_NoDangling,Inv_NoStragglerInstall,Inv_NoDeposedPut,Inv_SyncNeverDestroysDirty,Inv_NoForeignLost" \
  $SCOPEWORLD
emit LeanScopedSyncWholeBase "Inv_NoForeignLost" \
  $SCOPEWORLD ScopedInstBase=FALSE
emit LeanProbeScopedDeferral "ProbeScopedDeferral" \
  $SCOPEWORLD
# U16: `ProbeScopedDeferral` fires on the DEFERRAL, a stamp written inside
# `Sync`. D4 is only acceptable because the deferred entry ARRIVES through
# the merge -> inbox -> consume flow, and `Inv_NoForeignLost` is likewise a
# stamp inside `Sync` rather than an eventual-integration property. This is
# the arrival half: a path a scoped sync deferred is integrated by a LATER
# consume. Without it, "deferred" and "lost" are the same trace, which is
# exactly what drill leg B5's own anti-vacuity guard asserts and the model
# did not. MaxBarriers is raised by one over SCOPEWORLD: the flow needs a
# barrier to QUEUE the foreign entry and a later one to consume it, which
# is the two-barrier shape the Rust fixture also had to use.
# NOT in check.sh: this probe does not fire, and that is U16's finding
# (see LeanSubtree.tla). Emitted so the next attempt starts from a cfg
# rather than from scratch.
emit LeanProbeOutOfScopeLater "ProbeOutOfScopeLater" \
  $SCOPEWORLD MaxBarriers=3

# ---- tranche 3, product 2: gated citation x version GC x the backstop ------
# The substrate this product adds is `versions[p]` — what is still STORED,
# which on a versioned bucket is a different question from what the key
# reads as. Inv_NoDangling ("the object exists") was the right question
# until D7; gated staging makes the CITED version noncurrent, so an object
# can exist, read as newer uncited bytes, and have nothing behind its
# citation. Inv_CitedVersionLives is the corrected question, and it was
# VIOLATED IN SHIPPED CODE this session (put_whole reported its ObjectMeta
# before the version id was minted), which is why it is modelled at all.
#
# WORLD NOTE: MaxHitl=1 is load-bearing, not breadth. The D7 stale-base
# mutation needs a foreign write to arrive AFTER a path was staged and be
# consumed before the citation; with MaxHitl=0 that interleaving does not
# exist and the mutation checks a state space its bug cannot live in — the
# same trap product 4 fell into from the other side.
# U41: MaxRestarts was 0 while GATEDINV listed `Inv_NoResurrection`, whose
# only writer is `Restart` — the line read as coverage and could not fire.
# Gated is the mode where it matters most (a delete stays cited until a
# citation, which IS the shape `res` tests), and the run costs 2 s.
GATEDWORLD="GatedCitation=TRUE MaxGen=3 MaxSeq=6 MaxHitl=1 MaxBarriers=2 \
MaxCrashes=0 MaxRestarts=1"
GATEDINV="TypeOK,Inv_HITLDurable,Inv_NoResurrection,Inv_CitedVersionLives,Inv_NoUncitedGC,Inv_BoundaryAtomic"
emit LeanGatedHolds "$GATEDINV" $GATEDWORLD
# ...and the probe that stops the knob from being cosmetic.
emit LeanProbeGatedRestart "ProbeGatedRestartReachable" $GATEDWORLD
emit LeanGatedBackstop "Inv_CitedVersionLives" $GATEDWORLD BackstopEnabled=TRUE
# THE ONE THIS TRANCHE PAID FOR. The shipped reaper's rule was "delete
# every version of a touched key except the one the installed manifest
# cites" — and a HITL write that landed between the lane and the citation
# is neither, so it was deleted. It was CURRENT, it was acked, and the
# inbox entry then 412d on its next consume and was dropped as superseded.
# The model found it on its FIRST strict run.
# Defence in depth, PINNED as such: with the inbox guard removed, the
# keep-current rule is the only thing between a citation and destroying
# live acked bytes. Two arms in one cfg on purpose — that is the claim.
emit LeanGatedReapsCurrent "Inv_NoUncitedGC" $GATEDWORLD \
  GCKeepsCurrent=FALSE CiteDropsInflightHitl=FALSE
emit LeanGatedSplitCitation "Inv_BoundaryAtomic" $GATEDWORLD AtomicCitation=FALSE
# THE CITATION'S CRASH MATRIX. Every gated cfg ran MaxCrashes=0 and
# §10.1c deferred this to product 1 — whose cfgs are MaxCrashes=0 too, so
# it belonged to no product at all (review: U12). It could not simply be
# switched on: `stage` was framed at the Next composition, so a crashed
# incarnation's staged set SURVIVED pod replacement — the opposite of
# §4's substrate rule — and a frozen `citeDone` was then quantified by
# Inv_BoundaryAtomic against a Valid(s) that kept moving. CrashPodGated
# clears both; these two runs are what that bought.
emit LeanGatedCrash "$GATEDINV" $GATEDWORLD MaxCrashes=1
emit LeanProbeGatedCrash "ProbeGatedCrashReachable" $GATEDWORLD MaxCrashes=1
# A citation naming bytes that PREDATE a user's write. D7 wrote a
# base-version guard for this; the model showed that guard unreachable
# (the lane never advances the baseline, so a staged path is always
# locally dirty, and consume/sync refuse dirty paths) and the REACHABLE
# arm — an inbox entry still in flight, on a lane that opens no window —
# had no guard at all. That is the whole return on this tranche.
# LaneCancelsStaged stays FALSE here, and that is load-bearing rather than
# inherited: this mutation's counterexample runs through a path the agent
# DELETED while the stage still held a version for it, which is exactly the
# shape C3's lane cancellation closes. With the cancellation on, every
# remaining route to citing over an acked HITL write in this world is
# already conflict-surfaced (the park arm records one), so the mutation
# goes green and proves nothing. What the drop-inflight rule actually
# guards in shipped code — a HITL write landing between the lane's consume
# and the citation's window — this model cannot express: its gated lane
# reuses `Scan`, which OPENS the window, while the shipped lane
# deliberately opens none. Named here rather than papered over; making the
# gated lane window-free is the fidelity fix, and it is not free.
emit LeanGatedInflightHitl "Inv_HITLDurable" $GATEDWORLD CiteDropsInflightHitl=FALSE
emit LeanProbeCitationInstalled "ProbeCitationInstalled" $GATEDWORLD
emit LeanProbeWithheldDelete "ProbeWithheldDelete" $GATEDWORLD
# U15: §4 asked for a `ProbeGC` re-run with GatedCitation=TRUE and it was
# never built. A plain ProbeGC re-run would not have answered it either:
# `gh.gc` is bumped by the CADENCE `GCDelete` as well, so the probe would
# fire on a path the gated design does not use. `gcCited` is written only
# by `CiteFinish`, so this proves a withheld delete actually LANDS at a
# citation — the step where Inv_CitedVersionLives and Inv_NoUncitedGC are
# most exposed, and which LeanGatedHolds could otherwise hold over with
# `dels = {}` at every boundary.
emit LeanProbeGatedGC "ProbeGatedGC" $GATEDWORLD
emit LeanProbeForcedCite "ProbeForcedCite" $GATEDWORLD
# MaxHitl=0 is the whole point of this cfg, not an inherited default.
# With HITL on, `HitlWrite` is enabled in the INITIAL state and sets
# objects[p] # manifest[p] in one step — so TLC's counterexample is an
# ordinary HITL write, reachable before any StagePut and equally
# reachable in cadence and hybrid. The probe then proves nothing about
# §3 residual 11 (the gated lane's uncited CURRENT version), and the
# regression fence §4 assigns it does not work: a design that abolished
# uncited staging entirely would still fail it (review: U14). With HITL
# off, the only way to separate objects from manifest is the staging
# lane, which is the exposure being pinned.
emit LeanProbeRawUncited "ProbeRawReaderSeesUncited" $GATEDWORLD MaxHitl=0

# ---- tranche 3, product 1: the boundary VERB x barrier x inbox -------------
# The ack/fence/crash matrix is where the plan retracted its own per-crash
# prescriptions, so `settle_pending_at_startup` is currently justified by
# one ordering out of many — and `Inv_NoNonceOrphan` under coalesce +
# restart + deposal is an interleaving property that unit tests SAMPLE
# rather than search.
#
# WORLD NOTE: MaxTouches=2 is load-bearing in the same way MaxHitl=1 is for
# product 2. The orphan hazard needs a SECOND consume landing on a live
# pending record; with one touch the fold rule has nothing to fold and the
# mutation checks a state space its bug cannot live in. ProbeCoalescedAck
# is what proves the second touch is actually reached.
#
# Budget, PILOTED before it was locked in (section 4's obligation).
# MaxGen=3 with MaxRestarts=1 passed 4M states at depth 20 without
# terminating; the two worlds are therefore split, and each mutation
# runs in the smaller world its counterexample actually needs:
#   SENTWORLD    MaxGen=3 MaxRestarts=0   -- 28 s, the wide generation world
#   SENTRESTART  MaxGen=2 MaxRestarts=1   -- 13 s, the crash-matrix world
# A pod REPLACEMENT takes the agent and the tree with the pending file,
# so it forgives every owed nonce by construction and buys no coverage
# here; the restart is the interesting one, because the pending file
# survives it and `honored` does not.
SENTWORLD="SentinelEnabled=TRUE MaxTouches=2 MaxGen=3 MaxSeq=6 MaxHitl=1 \
MaxBarriers=2 MaxCrashes=0 MaxRestarts=0"
SENTRESTART="SentinelEnabled=TRUE MaxTouches=2 MaxGen=2 MaxSeq=6 MaxHitl=1 \
MaxBarriers=2 MaxCrashes=0 MaxRestarts=1"
# The stall/takeover world buys its depth the way the tranche-1 takeover
# cfgs do — MaxGen=2, MaxHitl=0, one touch. At MaxGen=3 the deposal run
# passed 1.3 GB of TLC scratch without terminating: two live sidecars,
# each with its own sentinel/pending/ack, is a different scale from one.
SENTSTALL="SentinelEnabled=TRUE MaxTouches=1 MaxGen=2 MaxSeq=6 MaxHitl=0 \
MaxBarriers=2 MaxCrashes=0 MaxRestarts=0 AllowStall=TRUE"
SENTINV="TypeOK,Inv_HITLDurable,Inv_NoDangling,Inv_NoResurrection,\
Inv_AckImpliesCited,Inv_AckBoundaryCoherent,Inv_NoNonceOrphan,\
Inv_NoFencedOkAck"
emit LeanSentinelHolds "$SENTINV" $SENTWORLD
# The crash-matrix world: the pending file outlives the restart, the
# in-memory `honored` flag does not, and the merge base can be behind an
# install this workspace made.
emit LeanSentinelRestart "$SENTINV" $SENTRESTART
# The deposal arm the draft's cfgs never had: `Inv_AckImpliesCited` was
# never checked ACROSS A FENCE, which is the one place an ack can name a
# boundary a successor has already moved past.
emit LeanSentinelDeposal "$SENTINV,Inv_NoStragglerInstall" $SENTSTALL
# The consume that CLOBBERS the standing pending record instead of folding
# into it: the first agent's nonce is never named by any ack, and it waits
# forever on a boundary that did happen.
emit LeanSentinelOrphan "Inv_NoNonceOrphan" $SENTWORLD FoldPending=FALSE
# The shortcut the crash-matrix review retracted: ack from persisted state.
# Pending-and-no-matching-ack is the SAME observable state for
# crash-before-CAS as for crash-after-step-7, so acking from it asserts
# publication of writes that never uploaded.
emit LeanSentinelAckEarly "Inv_AckImpliesCited" $SENTWORLD AckFromInstall=FALSE
# Success-ack-after-fence: a deposed incarnation telling a waiting agent
# its boundary landed.
emit LeanSentinelFencedAck "Inv_NoFencedOkAck" $SENTSTALL RefuseOnFence=FALSE
# §10.1's DELIBERATE deviation, machine-checked. §2.1 says a pending
# sentinel must defeat the skip-on-no-diff fast path; the shipped code
# lets it through, on the argument that the fast path only fires when
# every local byte is already cited. Drop the two guards that carry that
# argument — no citation repair owed, and the remote manifest where we
# left it — and the ack must be caught claiming an uninstalled boundary.
emit LeanSentinelFastPathUnguarded "Inv_AckBoundaryCoherent" \
  $SENTWORLD FastPathGuards=FALSE
emit LeanProbeSentinelHonored "ProbeSentinelHonored" $SENTWORLD
emit LeanProbeRefusedAck "ProbeRefusedAck" $SENTSTALL
emit LeanProbeAckAfterCrash "ProbeAckAfterCrash" $SENTRESTART
emit LeanProbeCoalescedAck "ProbeCoalescedAck" $SENTWORLD
emit LeanProbeFastPathHonor "ProbeFastPathHonor" $SENTWORLD
# The merge base is rewritten at step 7, so a restart between the
# manifest CAS and that rewrite leaves the workspace's OWN installed
# entry looking foreign at the next merge — and delete/modify resolves
# conservatively against the agent's own delete, dropping it from the
# boundary it is about to be acked for. TLC found this in shipped code
# on the third strict run of this product.
emit LeanSentinelStaleMergeBase "Inv_AckImpliesCited" \
  $SENTRESTART MineIsNotForeign=FALSE

# ---- C6: the sentinel over the CITATION lane ------------------------------
# The gap the verified review named: no cfg paired SentinelEnabled with
# GatedCitation, so `Inv_AckImpliesCited` was never evaluated over a
# citation-lane honor even though `CiteFinish` sets `honored` under
# SentinelEnabled — the module could express it, the matrix never asked.
#
# WORLD NOTE: one touch, MaxGen=4, and the generation budget is load-bearing
# in the way MaxHitl=1 is for product 2 — PILOTED, not guessed, because the
# probe said so. The in-flight drop needs FOUR things minted: a second
# staged path (without one the citation never fires — a citation installs
# `Valid(s)`, and the dropped path is by definition not in it), the dropped
# path's own generation, the HITL generation that lands on it, and the
# declaration's mint watermark. At MaxGen=3 (two mints) `ProbeDeclaredDrop`
# holds — no drop is reachable AT ALL — and both mutations in this world go
# green against a state space their bug cannot live in. A second NONCE buys
# nothing here (that is product 1's business, and it doubles the space).
#
# The same probe says something about the runs that came BEFORE it:
# `CiteDropsInflightHitl`, product 2's rule, has never had a positive
# reachability probe. Its mutation fires through a different shape (a path
# the agent deleted while the stage still held a version for it), and the
# state the rule actually guards is unreachable in GATEDWORLD for exactly
# the reason above. That is a coverage hole in a gate that was already
# green, found by adding one anti-vacuity probe.
# Costs ~20 s of the gate at this budget (1.5M distinct states); measured.
SENTGATED="SentinelEnabled=TRUE MaxTouches=1 GatedCitation=TRUE \
MaxGen=4 MaxSeq=6 MaxHitl=1 MaxBarriers=2 MaxCrashes=0 MaxRestarts=0 \
AckHonest=TRUE LaneCancelsStaged=TRUE GatedRepair=TRUE"
emit LeanSentinelGatedHolds \
  "TypeOK,Inv_HITLDurable,Inv_NoResurrection,Inv_CitedVersionLives,\
Inv_NoUncitedGC,Inv_BoundaryAtomic,Inv_AckImpliesCited,\
Inv_AckBoundaryCoherent,Inv_NoNonceOrphan,Inv_NoFencedOkAck" $SENTGATED
# What shipped: `status: "ok"` whatever the citation dropped, with no
# field in the ack schema that could express the exception. The agent
# that declared a point containing p is told the point landed while the
# manifest at the acked seq still cites p's previous generation.
emit LeanSentinelGatedOkOverDrop "Inv_AckImpliesCited" \
  $SENTGATED AckHonest=FALSE
# Anti-vacuity, both halves: the drop is REACHED inside a declared
# boundary, and the honest answer actually fires.
emit LeanProbeDeclaredDrop "ProbeDeclaredDrop" $SENTGATED
emit LeanProbePartialAck "ProbePartialAck" $SENTGATED
# The defect TLC found in this world, in a fix two hours old: the stage
# and the withheld-delete set both reach the citation, and `merge` has no
# ordering between them. Cancel neither and the boundary an ok ack names
# cites a file the agent deleted before declaring.
emit LeanSentinelGatedStaleStage "Inv_AckImpliesCited" \
  $SENTGATED LaneCancelsStaged=FALSE
# C2, as a model artifact rather than a battery-only fix: the repair the
# fused barrier has and the citation lane did not. Without it an ok ack
# names a manifest that does not cite a HITL write this workspace has
# already integrated into its own tree.
emit LeanSentinelGatedNoRepair "Inv_AckBoundaryCoherent" \
  $SENTGATED GatedRepair=FALSE

# ---- the ack's PROVENANCE: one boundary, one clock -------------------------
# `Inv_AckBoundaryCoherent` asks whether the acked boundary is a coherent
# POINT.  It never asked which CLOCK installed it — and that is a separate
# question with a separate reader: the agent reads the ack, the fleet reads
# the manifest's stamp, and an operator asking "did my agent's publish land,
# or was that the floor?" has only the bucket to ask.
#
# Shipped code computed the two independently. The bucket drill found it
# twice in one session: the barrier installed through an UNSTAMPED CAS (so
# every cadence and hybrid workspace reported an unknown clock, including
# through the gateway's /status), and then the fix produced a DISAGREEMENT —
# a drain that rewrote its own ack to `drain` over a manifest still stamped
# `sentinel`. It existed in both modes; fixing the cadence path left the
# gated one, and only leg B11a caught that. One invariant would have caught
# both at once, which is the argument for this pair of runs.
emit LeanSentinelClockHolds \
  "TypeOK,Inv_AckImpliesCited,Inv_AckBoundaryCoherent,Inv_BoundaryNamesItsClock" \
  $SENTWORLD
# The mutation is the shipped shape, not a strawman: the install goes
# through an unstamped CAS, so a sentinel honor lands a boundary the bucket
# reports as the default clock while the ack tells the agent otherwise.
emit LeanSentinelClockUnstamped "Inv_BoundaryNamesItsClock" \
  $SENTWORLD StampBoundarySource=FALSE
# The same claim over the CITATION lane, which is a different installer and
# is where the second half of the shipped defect actually lived.
emit LeanSentinelGatedClockUnstamped "Inv_BoundaryNamesItsClock" \
  $SENTGATED StampBoundarySource=FALSE

# ---- the PAIR the plan predicted and the matrix never ran -----------------
# §10.3: "two products that share an action are not covered by running them
# separately", and it named SyncScope x GatedCitation as the obvious next
# one — a scoped sync and a citation lane both advance `instBase`, by
# different rules. No cfg had ever set both TRUE. This is that run.
#
# The shared object is `instBase`: D4 advances it per-path (only where the
# sync applied or verified in scope) while the citation lane advances it
# for everything it installs. If the citation's advance is not scope-aware,
# an out-of-scope foreign entry reads as already-integrated at the next
# merge and is lost from the inbox flow forever — which is exactly what
# Inv_NoForeignLost exists to catch, and it had never been evaluated in a
# world where a citation could do the advancing.
# WORLD NOTE, and it is the trap §10.3 named, walked into from the third
# side: the first budget for this pair used MaxHitl=1 as its foreign source
# and AllowStall=FALSE. The holds run went green over 70,701 states, the
# deferral probe FIRED, and the D4 mutation still generated a byte-identical
# state count — `ScopedInstBase` was unreachable, so the pair run was green
# over a world its bug cannot live in. The foreign source has to be the one
# SCOPEWORLD uses: a second sidecar installing while ours is stalled.
SCOPEGATED="SyncEnabled=TRUE SyncScope=TRUE GatedCitation=TRUE MaxSyncs=1 \
AllowStall=TRUE MaxHitl=0 MaxGen=2 MaxSeq=6 MaxBarriers=2 \
MaxCrashes=0 MaxRestarts=0"
emit LeanScopedGatedHolds \
  "TypeOK,Inv_HITLDurable,Inv_NoResurrection,Inv_CitedVersionLives,\
Inv_NoUncitedGC,Inv_BoundaryAtomic,Inv_SyncNeverDestroysDirty,Inv_NoForeignLost" \
  $SCOPEGATED
# Anti-vacuity for the pair, and it is the whole reason the run exists: if
# the scoped-deferral action is unreachable in this world then the pair is
# not being exercised and the holds run above is green over nothing.
emit LeanProbeScopedGated "ProbeScopedDeferral" $SCOPEGATED
# The D4 mutation, re-run inside the gated world: whole-instBase advance
# must still be caught when a citation lane is also advancing it.
emit LeanScopedGatedWholeBase "Inv_NoForeignLost" \
  $SCOPEGATED ScopedInstBase=FALSE
