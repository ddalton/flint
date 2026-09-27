#!/usr/bin/env bash
# LeanP2.tla's worlds: the shipped claims, one mutation per rule, the probes.
# Expectations are WORLDS-LeanP2.tsv, written before any run (2026-09-25).
# Bounds are the P2 sandbox's (pending/p2, P2Holds: 11.6M states), plus the
# copy budget a withheld re-upload needs; the L-126 / L-123 routes need one
# path and three barriers (the 1p3b shape the P1-lite pair used).
set -eu
cd "$(dirname "$0")"
KEYS="Paths Free Writers MaxMint MaxUI MaxRemovals MaxBarriers MaxRestarts MaxSyncs MaxCopies \
CommitSurfacesForeign CommitVerifiesUploads SweepUnderLease ForeignPerPath CollectorSparesCited \
ParkedKeepsMergeBase CommitRecordsDeleteOverride QueueYieldsToSync \
GatewayIgnoresLease GatewayJudgesRead GatewaySweepGrace RenameAtomic"
emit() { # <name> <spec> <invariants> <properties> <overrides...>
  local name=$1 spec=$2 invs=$3 props=$4; shift 4
  local c_Paths='{p1, p2}' c_Free='{p2}' c_Writers='{A, B}'
  local c_MaxMint=3 c_MaxUI=1 c_MaxRemovals=1 c_MaxBarriers=2 c_MaxRestarts=0 c_MaxSyncs=0 c_MaxCopies=1
  local k kv v
  for k in CommitSurfacesForeign CommitVerifiesUploads SweepUnderLease ForeignPerPath CollectorSparesCited \
           ParkedKeepsMergeBase CommitRecordsDeleteOverride QueueYieldsToSync \
           GatewayIgnoresLease GatewayJudgesRead GatewaySweepGrace RenameAtomic; do
    eval "local c_$k=TRUE"
  done
  for kv in "$@"; do k=${kv%%=*}; v=${kv#*=}; eval "c_$k=\"\$v\""; done
  {
    echo "SPECIFICATION $spec"
    echo "CHECK_DEADLOCK FALSE"
    echo "CONSTANTS"
    echo "  Nil = Nil"
    for k in $KEYS; do eval "echo \"  $k = \$c_$k\""; done
    local i
    for i in ${invs//,/ }; do echo "INVARIANT $i"; done
    for i in ${props//,/ }; do echo "PROPERTY $i"; done
  } > "LeanP2$name.cfg"
}
INV="TypeOK,Inv_CitationsLive,Inv_OneName,Inv_AckedNamed,Inv_OneHolder,Inv_NoRegress"
Q1="Paths={p1} Free={} MaxBarriers=3 MaxRemovals=0 MaxRestarts=1 MaxSyncs=1 MaxCopies=2"
# The shipped shape.
emit Holds Spec "$INV" Prop_NoSilentRevert
emit Holds1p3b Spec "$INV" Prop_NoSilentRevert $Q1
emit LiveHolds LSpec TypeOK Prop_UISaveCompletes
# One mutation per rule.
emit NoR7 Spec TypeOK Prop_NoSilentRevert CommitSurfacesForeign=FALSE
emit VerifyOff Spec "$INV" "" CommitVerifiesUploads=FALSE
emit SweepFree Spec "$INV" "" SweepUnderLease=FALSE
emit ForeignFlat Spec "$INV" Prop_NoSilentRevert ForeignPerPath=FALSE
emit CollectorGreedy Spec "$INV" Prop_NoSilentRevert CollectorSparesCited=FALSE
emit WithheldReverts Spec TypeOK Prop_NoSilentRevert ParkedKeepsMergeBase=FALSE $Q1
emit DeleteOverrideOff Spec "$INV" Prop_NoSilentRevert CommitRecordsDeleteOverride=FALSE
emit SyncNoPrune Spec TypeOK,Inv_NoRegress "" QueueYieldsToSync=FALSE $Q1
emit LiveWaitsOnLease LSpec TypeOK Prop_UISaveCompletes GatewayIgnoresLease=FALSE
emit GatewayBlind Spec TypeOK Prop_NoSilentRevert GatewayJudgesRead=FALSE
emit SweepNoGrace Spec "$INV" "" GatewaySweepGrace=FALSE
emit RenameTwoCAS Spec "$INV" "" RenameAtomic=FALSE
# Probes: each must fire (the step is reachable).
emit ProbeStalledSave Spec ProbeStalledSave ""
emit ProbeSavedUnderLease Spec TypeOK ProbeSavedUnderLease
emit ProbeSaveRefused Spec TypeOK ProbeSaveRefused
emit ProbeRenamed Spec ProbeRenamed ""
emit ProbeSurfaced Spec ProbeSurfaced ""
emit ProbeDeleteOverridden Spec ProbeDeleteOverridden ""
emit ProbeRepublish Spec TypeOK ProbeRepublish $Q1
emit ProbeRestartAfterCas Spec TypeOK ProbeRestartAfterCas $Q1
