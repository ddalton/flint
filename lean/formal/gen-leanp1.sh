#!/usr/bin/env bash
# LeanP1.tla's worlds (P2 + P1-lite, the code after step 5 slice 4): the
# shipped claims, one mutation per rule, the probes. Expectations are
# WORLDS-LeanP1.tsv, written before any run (2026-09-25).
# Bounds are the P2 sandbox's (pending/p2, P2Holds: 11.6M states), plus the
# copy budget a withheld re-upload needs; the L-126 / L-123 routes need one
# path and three barriers (the 1p3b shape the P1-lite pair used).
set -eu
cd "$(dirname "$0")"
KEYS="Paths Free Writers MaxMint MaxUI MaxRemovals MaxBarriers MaxRestarts MaxSyncs MaxCopies MaxAges \
CommitSurfacesForeign CommitVerifiesUploads SweepUnderLease CollectorSparesCited \
CommitRecordsDeleteOverride DeleteWinsPreserved ContentConverges RecheckSkipped CommitAdvanceGuarded RetireAge \
GatewayIgnoresLease GatewayJudgesRead GatewaySweepGrace RenameAtomic"
emit() { # <name> <spec> <invariants> <properties> <overrides...>
  local name=$1 spec=$2 invs=$3 props=$4; shift 4
  local c_Paths='{p1, p2}' c_Free='{p2}' c_Writers='{A, B}'
  local c_MaxMint=3 c_MaxUI=1 c_MaxRemovals=1 c_MaxBarriers=2 c_MaxRestarts=0 c_MaxSyncs=0 c_MaxCopies=1 c_MaxAges=1
  local k kv v
  for k in CommitSurfacesForeign CommitVerifiesUploads SweepUnderLease CollectorSparesCited \
           CommitRecordsDeleteOverride DeleteWinsPreserved ContentConverges RecheckSkipped CommitAdvanceGuarded RetireAge \
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
  } > "LeanP1$name.cfg"
}
INV="TypeOK,Inv_CitationsLive,Inv_OneName,Inv_AckedNamed,Inv_OneHolder,Inv_NoRegress,Inv_ShortcutSound,Inv_ReaderFetches"
PROPS="Prop_NoSilentRevert,Prop_DeleteSettles"
Q1="Paths={p1} Free={} MaxBarriers=3 MaxRemovals=0 MaxRestarts=1 MaxSyncs=1 MaxCopies=2"
# The shipped shape.
emit Holds Spec "$INV" "$PROPS"
emit Holds1p3b Spec "$INV" "$PROPS" $Q1
emit LiveHolds LSpec TypeOK Prop_UISaveCompletes
# One mutation per rule.
emit NoR7 Spec TypeOK Prop_NoSilentRevert CommitSurfacesForeign=FALSE
emit VerifyOff Spec "$INV" "" CommitVerifiesUploads=FALSE
emit SweepFree Spec "$INV" "" SweepUnderLease=FALSE
emit CollectorGreedy Spec "$INV" "$PROPS" CollectorSparesCited=FALSE
emit DeleteOverrideOff Spec "$INV" "$PROPS" CommitRecordsDeleteOverride=FALSE
emit DeleteOutranked Spec TypeOK Prop_DeleteSettles DeleteWinsPreserved=FALSE
emit NoConvergence Spec "$INV" "$PROPS" ContentConverges=FALSE $Q1
emit OwedUnmarked Spec TypeOK,Inv_ShortcutSound "" RecheckSkipped=FALSE
emit AdvanceUnguarded Spec TypeOK,Inv_ShortcutSound "" CommitAdvanceGuarded=FALSE
emit ReaderLoses Spec TypeOK,Inv_ReaderFetches "" RetireAge=FALSE
emit LiveWaitsOnLease LSpec TypeOK Prop_UISaveCompletes GatewayIgnoresLease=FALSE
# 2026-09-28: the liveness claim at a bound TLC can finish. LiveHolds has
# LeanP1Holds's bounds (>1B states; its final liveness check cannot fit 20
# GB). One path, no removals: 10,676,334 distinct, depth 36 (tlc-rs, safety
# only). The control must still fire at the same bound.
LS="Paths={p1} Free={} MaxRemovals=0"
emit LiveHoldsSmall LSpec TypeOK Prop_UISaveCompletes $LS
emit LiveWaitsOnLeaseSmall LSpec TypeOK Prop_UISaveCompletes GatewayIgnoresLease=FALSE $LS
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
emit ProbeDeleteOverTheirs Spec TypeOK ProbeDeleteOverTheirs
emit ProbeShortcut Spec TypeOK ProbeShortcut
emit ProbeReaped Spec TypeOK ProbeReaped
emit ProbeConverged Spec TypeOK ProbeConverged $Q1
emit ProbeRepublish Spec TypeOK ProbeRepublish $Q1
emit ProbeRestartAfterCas Spec TypeOK ProbeRestartAfterCas $Q1
