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
AckHonest LaneCancelsStaged GatedRepair StampBoundarySource \
TwoScanDelete MaxNarrows NarrowAtomic NarrowUnlinkFirst \
MaxRemovals DeclaredSkipsWalk EarlyInboxDrop RenameWaitsForDestination \
BarrierLease Ticket DeadHandoffSkip InfiniteBarriers ConditionalGC VerifyAdoptedCitations \
HitlOverwritesTrackedOnly SyncKeepsHiddenBase MaxSameBytes VerifyUploadedCitations"

emit() { # <name> <invariants (comma-sep)> <overrides (key=val ...)>
         # Spec=<name> selects the SPECIFICATION (default Spec; FairSpec
         # for the liveness runs); Props=<a,b> adds PROPERTY lines.
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
  # tranche 4: the NARROW verb (scoped-read design §4).  All four FALSE/0
  # in every pre-existing cfg, so their state spaces are preserved by
  # construction: MaxNarrows=0 makes the action unreachable and the new sc
  # fields stay at Init, and TwoScanDelete=FALSE keeps the one-scan delete
  # rule those cfgs were written against.  Turning the two-scan rule on
  # globally would SHRINK every earlier run's delete space, and a pinned
  # mutation that stops finding its counterexample is the failure this
  # harness exists to prevent.
  local c_TwoScanDelete=FALSE c_MaxNarrows=0
  local c_NarrowAtomic=TRUE c_NarrowUnlinkFirst=FALSE
  # tranche 5: DECLARED removals.  MaxRemovals=0 in every pre-existing
  # cfg, so HitlRemove/HitlRename are unreachable and `removals` stays
  # {} — those state spaces are preserved by construction.
  # EarlyInboxDrop=FALSE everywhere is NOT an arm kept for preservation:
  # it is the shipped rule since 2026-09-12 (consumed entries leave the
  # cell after the manifest cites them), and the whole gate was re-run
  # against it.  TRUE is the old rule, kept as the mutation that loses a
  # consumed write to a pod replacement.
  local c_MaxRemovals=0 c_DeclaredSkipsWalk=TRUE c_EarlyInboxDrop=FALSE
  local c_RenameWaitsForDestination=TRUE
  # The path set.  NPaths=3 FreeLast=TRUE gives the removal runs a third
  # path that starts UNPUBLISHED — a rename needs a free destination,
  # and with every path at gen 1 there was none: the rename probe was
  # green because the action never fired.  Every pre-existing cfg keeps
  # {p1, p2} with FreePaths = {}.
  local c_NPaths=2 c_FreeLast=FALSE
  # tranche 6: the PER-BARRIER lease.  BarrierLease=FALSE in every
  # pre-existing cfg: StartA/ClaimB keep the life lease, Scan opens the
  # window, every fence kills, and cellQueue/cellHandoff/cellReleased
  # stay frozen at Init — those state spaces are preserved by
  # construction (verified by distinct-state count against the HEAD
  # module: see the README).  Ticket/DeadHandoffSkip are the shipped
  # rules (TRUE), each with its mutation; InfiniteBarriers is the
  # liveness abstraction and is FALSE in every safety run.
  local c_BarrierLease=FALSE c_Ticket=TRUE c_DeadHandoffSkip=TRUE
  local c_InfiniteBarriers=FALSE
  # ConditionalGC=TRUE is the atomic If-Match delete this module has always
  # modelled; FALSE is the shipped HEAD-then-DELETE, modelled only under
  # BarrierLease, where the model found it unsafe.
  local c_ConditionalGC=TRUE
  # VerifyAdoptedCitations=TRUE re-verifies an adopted entry under the
  # lease; FALSE is the shipped blind adopt, which the model refuted at
  # depth 28 of the first two-writer stall run.  BarrierLease only.
  local c_VerifyAdoptedCitations=TRUE
  # SyncKeepsHiddenBase=FALSE is the sync verb's shipped advance, kept in
  # every pre-existing cfg; TRUE is the 2026-09-13 fix, whose control is
  # LeanBarrierLeaseSyncOverlayHolds.
  local c_SyncKeepsHiddenBase=FALSE
  # HitlOverwritesTrackedOnly=FALSE is the gateway as it shipped (it
  # overwrote whatever object was current); TRUE is the 2026-09-13 rule
  # (`inbox::hitl_may_overwrite`), on in the barrier-lease worlds with HITL.
  local c_HitlOverwritesTrackedOnly=FALSE
  # Finding 13 (runcv A3): identical bytes share an etag.  MaxSameBytes=0
  # in every pre-existing cfg, so AgentWriteSame is unreachable and
  # `touched` stays {}; VerifyUploadedCitations=FALSE keeps CASInstall's
  # withheld set to the adopted entries, as it was — both preserve the
  # earlier state spaces by construction.  TRUE is the 79e7dac9 fix.
  local c_MaxSameBytes=0 c_VerifyUploadedCitations=FALSE
  local c_Spec=Spec c_Props=""
  local kv
  for kv in "$@"; do eval "c_${kv%%=*}=${kv#*=}"; done
  {
    echo "SPECIFICATION $c_Spec"
    echo "CHECK_DEADLOCK FALSE"
    echo "CONSTANTS"
    local paths="p1" j
    for ((j = 2; j <= c_NPaths; j++)); do paths="$paths, p$j"; done
    echo "  Paths = {$paths}"
    if [ "$c_FreeLast" = TRUE ]; then echo "  FreePaths = {p$c_NPaths}"; else echo "  FreePaths = {}"; fi
    local k v
    for k in $KEYS; do
      eval "v=\$c_$k"
      echo "  $k = $v"
    done
    local i
    for i in ${invs//,/ }; do echo "INVARIANT $i"; done
    for i in ${c_Props//,/ }; do echo "PROPERTY $i"; done
  } > "$name.cfg"
  echo "wrote $name.cfg"
}

ALLINV="TypeOK,Inv_HITLDurable,Inv_NoDangling,Inv_NoStragglerInstall,Inv_NoDeposedPut,Inv_NoResurrection,Inv_HITLTracked"

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
# passed 1.3 GB of TLC scratch without terminating: two live syncers,
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
# ---- tranche 4: the NARROW verb x the barrier (scoped-read design §4) ------
#
# WORLD NOTE. The narrow's two failure modes are read by `classify`, so the
# world has to contain a barrier that actually classifies: MaxBarriers>=2,
# because the unlink-first arm needs ONE scan to make the path delete-
# eligible under the two-scan rule and a SECOND to run the GC. A one-barrier
# world would let the unlink-first mutation come back green — over a world
# its bug cannot reach.
#
# TwoScanDelete=TRUE here and nowhere else: this is the first tranche to
# model `prev_scan` at all, and the narrow invariant is the only claim that
# depends on it.
NARROWWORLD="TwoScanDelete=TRUE MaxNarrows=1 MaxHitl=0 MaxGen=3 MaxSeq=6 \
MaxBarriers=2 MaxCrashes=0 MaxRestarts=0 AllowStall=FALSE"

emit LeanNarrowHolds \
  "TypeOK,Inv_HITLDurable,Inv_NoDangling,Inv_NoResurrection,\
Inv_NarrowNeverDeletes,Inv_NarrowNeverRecites" \
  $NARROWWORLD
# Anti-vacuity: if Narrow is unreachable in this world the holds run above
# is green over nothing. Probes the ACTION via a ghost only Narrow writes.
emit LeanProbeNarrow "ProbeNarrow" $NARROWWORLD
# §4.2's first naive order: unlink the file, leave the citation. The next
# scan reads present-in-baseline/absent-from-scan-and-prevScan as a DELETE
# and the GC publishes it — the workspace takes the bucket's copy with it.
emit LeanNarrowUnlinkFirst "Inv_NarrowNeverDeletes" \
  $NARROWWORLD NarrowAtomic=FALSE NarrowUnlinkFirst=TRUE
# §4.2's second naive order: uncite the path, leave the file. The next scan
# reads present-in-scan/absent-from-baseline as a local ADD, uploads it and
# re-cites it — the barrier silently undoes the narrow.
emit LeanNarrowUncieFirst "Inv_NarrowNeverRecites" \
  $NARROWWORLD NarrowAtomic=FALSE NarrowUnlinkFirst=FALSE

# ---- tranche 5: DECLARED removals — delete and rename from outside the pod -
# (docs/plans/flint-lean-delete-rename-design.md, phase E.)  TwoScanDelete
# is ON here: the design's atomicity claim is exactly that a declared
# removal need not wait out the two-scan guard, so the mutation that
# routes it through the walk is only visible against that guard.
REMOVALWORLD="TwoScanDelete=TRUE NPaths=3 FreeLast=TRUE MaxRemovals=1 MaxHitl=1 \
MaxGen=3 MaxSeq=6 MaxBarriers=2 MaxCrashes=0 MaxRestarts=0"
# Crash + restart with the rename's own write as the only HITL write:
# the crash world is where the late inbox drop and the intent journal
# earn their keep, and MaxHitl=1 on top of it is an hour of TLC.
REMOVALCRASH="TwoScanDelete=TRUE NPaths=3 FreeLast=TRUE MaxRemovals=1 MaxHitl=0 \
MaxGen=3 MaxSeq=6 MaxBarriers=2 MaxCrashes=1 MaxRestarts=1"
emit LeanRemovalHolds "$ALLINV,Inv_RenameAtomic,Inv_RenameNoHole" $REMOVALWORLD
emit LeanRemovalCrashHolds "$ALLINV,Inv_RenameAtomic,Inv_RenameNoHole" $REMOVALCRASH
# §4's claim, pinned: route the declared removal through the walk and a
# performed rename lands in two generations — the manifest cites both
# names for a barrier.
emit LeanRemovalViaWalk "Inv_RenameAtomic" $REMOVALWORLD DeclaredSkipsWalk=FALSE
# The rule that shipped until 2026-09-12: a consume clears the cell at
# once, and a pod replacement before the manifest CAS leaves an acked
# write tracked by nothing.  No removal needed to see it.
emit LeanEarlyInboxDropLosesHitl "Inv_HITLTracked" $REMOVALCRASH EarlyInboxDrop=TRUE \
  MaxRemovals=0 MaxHitl=1 MaxRestarts=0
# ...and a rename's destination is lost the same way, while the source
# removal waits forever for a destination that will never arrive.
emit LeanEarlyInboxDropLosesRename "Inv_HITLTracked" $REMOVALCRASH EarlyInboxDrop=TRUE \
  MaxRestarts=0
# Without the wait, that same crash cites the source OUT while the
# destination is lost: a HOLE — the outcome §5 forbids.
emit LeanRenameNoDestinationGuard "Inv_RenameNoHole" $REMOVALCRASH EarlyInboxDrop=TRUE \
  RenameWaitsForDestination=FALSE MaxRestarts=0
emit LeanProbeRemoval "ProbeRemovalApplied" $REMOVALWORLD
emit LeanProbeRemovalRefused "ProbeRemovalRefused" $REMOVALWORLD
emit LeanProbeRename "ProbeRenameApplied" $REMOVALWORLD

# ---- tranche 6: the PER-BARRIER lease (writer-lease design §4-§5) --------
# The cell is held for one barrier's commit section with a FIFO ticket;
# both writers run from the start and the takeover is the generic
# deposal.  Three safety worlds, each two-writer:
#   BLWORLD   the breadth run: HITL + crash + restart.  MaxGen=2, not
#             LeanSubtree's 3: two live writers with a crash AND a
#             restart passed 2.8M states at depth 16 in the first minute
#             with the queue spilling to disk (the crash-matrix split
#             SENTRESTART made for the same reason)
#   BLSENT    the sentinel over a commit that had to WAIT for the cell
#   BLSTALL   the takeover world: A freezes INSIDE its commit section and
#             B deposes it; the thawed A abandons (MaxBarriers=2 — the
#             retry after a fence is BLADOPT's subject, and cheaper there)
#   BLADOPT   a restart between the CAS and the baseline, so the next
#             barrier ADOPTS its own earlier upload — the adopt race
# and the liveness world BLLIVE, which is where the ticket is proved
# load-bearing: InfiniteBarriers makes the barrier loop a CYCLE (every
# counter a barrier moves is monotone, so under budgets TLC has no lasso
# to find and "claims forever" is unstatable), one path, no writes, and
# FairSpec's WF on each writer's own barrier step.
BLWORLD="BarrierLease=TRUE HitlOverwritesTrackedOnly=TRUE MaxGen=2 MaxSeq=6 MaxHitl=1 MaxBarriers=2 \
MaxCrashes=1 MaxRestarts=1"
BLSENT="BarrierLease=TRUE HitlOverwritesTrackedOnly=TRUE SentinelEnabled=TRUE MaxTouches=2 MaxGen=3 MaxSeq=6 \
MaxHitl=1 MaxBarriers=2 MaxCrashes=0 MaxRestarts=0"
BLSTALL="BarrierLease=TRUE AllowStall=TRUE MaxHitl=0 MaxGen=2 MaxSeq=6 \
MaxBarriers=2 MaxCrashes=0 MaxRestarts=0"
BLLIVE="BarrierLease=TRUE InfiniteBarriers=TRUE NPaths=1 MaxGen=1 MaxSeq=3 \
MaxHitl=0 MaxBarriers=2 MaxCrashes=0 MaxRestarts=0 Spec=FairSpec Props=NoStarvation"
BLINV="TypeOK,Inv_HITLDurable,Inv_NoDangling,Inv_NoStragglerInstall,\
Inv_NoDeposedPut,Inv_NoResurrection,Inv_HITLTracked,\
Inv_CommitExclusive,Inv_CellHeldByHolder,Inv_NoStaleOverride"
BLPROBE="BarrierLease=TRUE MaxHitl=0 MaxCrashes=0 MaxRestarts=0 MaxGen=2 MaxSeq=6 MaxBarriers=2"
BLSTALLINV="TypeOK,Inv_NoDangling,Inv_NoStragglerInstall,Inv_NoDeposedPut,\
Inv_HITLTracked,Inv_CommitExclusive,Inv_CellHeldByHolder,Inv_NoStaleOverride"
emit LeanBarrierLeaseHolds "$BLINV" $BLWORLD
emit LeanBarrierLeaseSentinel "$SENTINV,Inv_HITLTracked,Inv_CommitExclusive,Inv_CellHeldByHolder,Inv_NoStaleOverride" \
  $BLSENT
# THE FINDING of the gate run (2026-09-13).  The gateway overwrote whatever
# object was current.  Under the barrier lease no window holds it off
# during a writer's uploads, so a UI write lands over A's UNCITED upload;
# B consumes the UI write, cites it, drops the entry; A's commit re-cites
# its own upload over it.  16 steps: the acked HITL write is uncited,
# untracked, preserved nowhere.  The breadth world (MaxGen=2) could not
# reach it — the interleaving needs a third generation.
emit LeanBarrierLeaseHitlOverUncited "Inv_HITLDurable" $BLSENT HitlOverwritesTrackedOnly=FALSE
# After 79e7dac9 the tracked-only rule is no longer what saves the UI
# write: the uploader's commit re-reads its own citation and withholds
# it.  The pair below lets the gateway overwrite ANY current object —
# a superset of its untracked-object escapes (the grace, and "no writer
# has a live heartbeat", which is why the heartbeat carries no safety)
# — with the sentinel off so the strict half exhausts on a laptop.  The
# control is the same world without the re-read: finding 4 again.
BLOVER="$BLSENT SentinelEnabled=FALSE MaxTouches=0 HitlOverwritesTrackedOnly=FALSE"
emit LeanBarrierLeaseHitlOverAnyUnverified "Inv_HITLDurable" $BLOVER
emit LeanBarrierLeaseHitlOverAnyVerified "Inv_HITLDurable,Inv_NoDangling,Inv_NoStaleOverride,Inv_HITLTracked" $BLOVER \
  VerifyUploadedCitations=TRUE
emit LeanBarrierLeaseDeposal "$BLSTALLINV" $BLSTALL
# The redundancy A/B, as in the life-lease world: each fence alone holds.
emit LeanBarrierLeaseEpochOnly "$BLSTALLINV" $BLSTALL Rotation=FALSE
emit LeanBarrierLeaseRotationOnly "$BLSTALLINV" $BLSTALL EpochCheck=FALSE
# Both fences off: the thawed straggler's manifest CAS lands.
emit LeanBarrierLeaseNoRotate "Inv_NoStragglerInstall" $BLSTALL \
  Rotation=FALSE EpochCheck=FALSE
# THE FINDING.  The shipped GC delete is a HEAD then an unconditional
# DELETE (barrier.rs step 6).  Under the life lease the lease covered that
# window; under the barrier lease the other writer's uploads hold no lease,
# and its supersede landing between A's HEAD and A's DELETE leaves B's
# citation dangling.  No stall, no crash, no HITL: two live writers.
# MaxHitl=0 on purpose — the model clears the window at the CAS while the
# code holds it through the deletes, so a HITL write in that gap would be
# a false positive here; the writer race is the real one.
emit LeanBarrierLeaseGCUnconditional "Inv_NoDangling" $BLPROBE ConditionalGC=FALSE
# NOT a holds-run.  The scoped sync was to be re-checked in the world it
# will actually run in — the second writer is a legitimate foreign
# manifest installer, no stall arm needed — and TLC REFUTED D4 there:
# the sync's remote truth is the manifest overlaid by live inbox entries
# (`sync.rs` step 2), which reads a queued foreign entry as newer than
# the manifest; a second LIVE writer can move the manifest past that
# entry (delete the path) while the entry's object still exists, so the
# sync "verifies" the path unchanged against the overlay and advances the
# merge base to the manifest — the silent, permanent D4 loss.  One writer
# cannot produce it: the only party that moves a manifest past its own
# queued entry is dead.  Pinned as the must-fail it is.  The fix is the
# sync verb's — do not advance the base for a path the overlay hid
# (`SyncKeepsHiddenBase`, built in `sync.rs` the same day) — and
# LeanBarrierLeaseSyncOverlayHolds is its control: the same world, the
# one constant moved.
BLSCOPE="BarrierLease=TRUE SyncEnabled=TRUE SyncScope=TRUE MaxSyncs=1 \
MaxHitl=0 MaxGen=2 MaxSeq=6 MaxBarriers=2 MaxCrashes=0 MaxRestarts=0"
emit LeanBarrierLeaseSyncOverlayStale "Inv_NoForeignLost" $BLSCOPE
emit LeanBarrierLeaseSyncOverlayHolds "TypeOK,Inv_NoForeignLost,Inv_NoDangling,Inv_NoStaleOverride" $BLSCOPE \
  SyncKeepsHiddenBase=TRUE
# THE SECOND FINDING.  A barrier that restarts between its CAS and its
# baseline re-uploads next time and finds its own bytes already there:
# `upload_one` adopts (CRC match, no PUT).  Between that adopt and the
# adopter's CAS the OTHER writer's commit can uncite the path and its GC
# — HEAD-guarded on an etag it learned at checkout — deletes the object.
# The adopter's merge then upserts a citation over a deleted object.
# The fix re-verifies adopted entries INSIDE the commit section, where no
# GC can run, and withholds what is gone.  ONE path: the race needs one,
# and with two the mutation's counterexample sat past 1M states at depth
# 20 while its strict control would have had to exhaust the lot.
BLADOPT="BarrierLease=TRUE NPaths=1 MaxHitl=0 MaxGen=2 MaxSeq=6 MaxBarriers=3 \
MaxCrashes=0 MaxRestarts=1"
emit LeanBarrierLeaseAdoptVerified "$BLSTALLINV,Inv_HITLDurable,Inv_NoResurrection" $BLADOPT
emit LeanBarrierLeaseAdoptBlind "Inv_NoDangling" $BLADOPT VerifyAdoptedCitations=FALSE
emit LeanProbeAdoptWithheld "ProbeAdoptWithheld" $BLADOPT
# FINDING 13, from the live drill (runcv A3), not from the model: S3's
# etag for a whole PUT is the MD5 of the bytes.  A deletes the path; B
# rewrites it with the SAME bytes and uploads, lease-free, If-Match its
# baseline — which is that very etag, so the PUT lands and the object
# reads as the version A's GC recognises.  A's GC deletes it; B's commit
# cites it.  This module minted a fresh generation for every write, so
# no GC could ever recognise another writer's upload — the abstraction
# was the bug.  `AgentWriteSame` writes a RECOGNISED generation; the fix
# (79e7dac9) re-verifies every citation the commit adds, and
# LeanBarrierLeaseSameBytesVerified is its control.  Both verifications
# on: the adopt fix alone does not cover it.  The adopt world, so the
# restart that makes an adopt reachable is in it too.
BLSAME="$BLADOPT MaxSameBytes=1"
emit LeanBarrierLeaseSameBytesVerified "$BLSTALLINV,Inv_HITLDurable,Inv_NoResurrection" $BLSAME \
  VerifyUploadedCitations=TRUE
emit LeanBarrierLeaseSameBytesUnverified "Inv_NoDangling" $BLSAME
# The second route, which the drill never showed and the probe below
# found: B's identical-bytes upload lands, A's new bytes land over it
# If-Match the same etag and A COMMITS; B's commit cites its generation
# over A's.  Nothing dangles, so it needs its own invariant.
emit LeanBarrierLeaseSameBytesOverride "Inv_NoStaleOverride" $BLSAME
emit LeanProbeUploadWithheld "ProbeUploadWithheld" $BLSAME VerifyUploadedCitations=TRUE
# Opt-in, NOT in the gate (like LeanSubtreeDeep): the same-bytes write in
# BLWORLD — two paths, HITL, crash, restart — with the fix.  Stopped on the
# Mac for disk at depth 19, 30,265,184 distinct states and 11.6M queued,
# no violation; it needs the TLC box.
emit LeanBarrierLeaseSameBytesDeep "$BLINV" $BLWORLD MaxSameBytes=1 VerifyUploadedCitations=TRUE
# Liveness: the ticket (falsifier L5) and the dead-handoff skip.
emit LeanBarrierLeaseLive "TypeOK" $BLLIVE
emit LeanBarrierLeaseLiveCrash "TypeOK" $BLLIVE MaxCrashes=1
emit LeanBarrierLeaseRandomArbitration "TypeOK" $BLLIVE Ticket=FALSE
emit LeanBarrierLeaseDeadHandoffWedge "TypeOK" $BLLIVE MaxCrashes=1 DeadHandoffSkip=FALSE
# Probes.
emit LeanProbeWritersInterleave "ProbeWritersInterleave" $BLPROBE
emit LeanProbeHandoff "ProbeHandoffFired" $BLPROBE
emit LeanProbeEnqueued "ProbeEnqueued" $BLPROBE
emit LeanProbeDeposalMidCommit "ProbeDeposalMidCommit" $BLSTALL
emit LeanProbeFenceAbandoned "ProbeFenceAbandoned" $BLSTALL
emit LeanProbeDeadHandoffSkipped "ProbeDeadHandoffSkipped" \
  BarrierLease=TRUE NPaths=1 MaxGen=1 MaxSeq=6 MaxHitl=0 MaxCrashes=1 MaxRestarts=0 MaxBarriers=3
