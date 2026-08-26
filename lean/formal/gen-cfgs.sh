#!/usr/bin/env bash
# One-shot generator for the LeanSubtree run configs.  Kept in-tree so the
# cfg matrix is regenerable; the cfgs themselves are committed.
# (Plain variables, not associative arrays: macOS ships bash 3.2.)
set -eu
cd "$(dirname "$0")"

KEYS="MaxGen MaxSeq MaxHitl MaxBarriers MaxCrashes MaxRestarts MaxSyncs \
AllowStall InboxEnabled MergeCapable ConflictSurfacing WindowCheck Rotation \
EpochCheck GuardedGC DeletesAfterCAS RematerializeOnRestart SyncEnabled \
SyncScanFirst SyncScope ScopedInstBase"

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
emit LeanScopedSyncHolds "$ALLINV,Inv_SyncNeverDestroysDirty,Inv_NoForeignLost" \
  $SCOPEWORLD
emit LeanScopedSyncWholeBase "Inv_NoForeignLost" \
  $SCOPEWORLD ScopedInstBase=FALSE
emit LeanProbeScopedDeferral "ProbeScopedDeferral" \
  $SCOPEWORLD
