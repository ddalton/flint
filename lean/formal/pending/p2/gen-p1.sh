#!/usr/bin/env bash
# P1-lite (LeanCoreP1.tla) and its same-coverage baseline (LeanCoreP2R.tla:
# P2 with the same restart and sync), in PAIRS at the same bounds: P2Holds'
# bounds plus one restart and one sync.  Expectations: WORLDS-P1.tsv,
# written before any run.
set -eu
cd "$(dirname "$0")"
COMMON="Paths Free Writers MaxMint MaxUI MaxRemovals MaxBarriers MaxRestarts MaxSyncs \
CommitSurfacesForeign CommitVerifiesUploads SweepUnderLease ForeignPerPath \
GatewayIgnoresLease GatewaySurfacesForeign GatewaySweepGrace RenameAtomic"
P1ONLY="ContentConverges DeleteWinsPreserved UIAdoptsUncited"
P2RONLY="QueueYieldsToSync"
emit() { # <name> <module P1|P2R> <spec> <invs> <props> <overrides...>
  local name=$1 mod=$2 spec=$3 invs=$4 props=$5; shift 5
  local c_Paths='{p1, p2}' c_Free='{p2}' c_Writers='{A, B}'
  local c_MaxMint=3 c_MaxUI=1 c_MaxRemovals=1 c_MaxBarriers=2 c_MaxRestarts=1 c_MaxSyncs=1
  local c_CommitSurfacesForeign=TRUE c_CommitVerifiesUploads=TRUE c_SweepUnderLease=TRUE c_ForeignPerPath=TRUE
  local c_GatewayIgnoresLease=TRUE c_GatewaySurfacesForeign=TRUE c_GatewaySweepGrace=TRUE c_RenameAtomic=TRUE
  local c_QueueYieldsToSync=TRUE c_ContentConverges=TRUE c_DeleteWinsPreserved=TRUE c_UIAdoptsUncited=FALSE
  local kv k v keys="$COMMON"
  if [ "$mod" = P1 ]; then keys="$COMMON $P1ONLY"; else keys="$COMMON $P2RONLY"; fi
  for kv in "$@"; do k=${kv%%=*}; v=${kv#*=}; eval "c_$k=\"\$v\""; done
  {
    echo "SPECIFICATION $spec"
    echo "CHECK_DEADLOCK FALSE"
    echo "CONSTANTS"
    echo "  Nil = Nil"
    for k in $keys; do eval "echo \"  $k = \$c_$k\""; done
    local i
    for i in ${invs//,/ }; do echo "INVARIANT $i"; done
    for i in ${props//,/ }; do echo "PROPERTY $i"; done
  } > "$name.cfg"
}
INV="TypeOK,Inv_CitationsLive,Inv_OneName,Inv_AckedNamed,Inv_OneHolder,Inv_NoRegress"
# THE PAIRS: the same bounds, the same claims (P1-lite adds its M3 property).
emit P1Holds P1 Spec "$INV" Prop_NoSilentRevert,Prop_DeleteSettles
emit P2RHolds P2R Spec "$INV" Prop_NoSilentRevert
# The two-path pair WITHOUT restarts: the rename and delete paths at a size
# a laptop finishes (the pair above passed 75M states and 21 GB of disk).
# Restarts are covered at one path and three barriers (the 1p3b pair).
emit P1Holds2pNoRestart P1 Spec "$INV" Prop_NoSilentRevert,Prop_DeleteSettles MaxRestarts=0
emit P2RHolds2pNoRestart P2R Spec "$INV" Prop_NoSilentRevert MaxRestarts=0
emit P1ProbeRepublish P1 Spec TypeOK ProbeRepublish
emit P2RProbeRepublish P2R Spec TypeOK ProbeRepublish
emit P1ProbeRestartAfterCas P1 Spec TypeOK ProbeRestartAfterCas
emit P2RProbeRestartAfterCas P2R Spec TypeOK ProbeRestartAfterCas
# P1-lite's controls: each must break its claim.
# L-123 lives in the QUEUE: the baseline without its prune must regress;
# P1-lite has no prune to remove (P1Holds checks Inv_NoRegress).
# Its route needs three barriers (queue v2, sync past it to v3, drain v2),
# so the trio runs at one path and three barriers (r44's shape), matched.
Q1="Paths={p1} Free={} MaxBarriers=3 MaxRemovals=0"
emit P2RSyncNoPrune P2R Spec TypeOK,Inv_NoRegress "" QueueYieldsToSync=FALSE $Q1
emit P2RSyncPruned P2R Spec TypeOK,Inv_NoRegress "" $Q1
emit P1SyncNoRegress P1 Spec TypeOK,Inv_NoRegress "" $Q1
# The full claims at the same bounds: L-126 (a withheld upload re-published
# over a version the tree never integrated) needs three barriers.
emit P1Holds1p3b P1 Spec "$INV" Prop_NoSilentRevert,Prop_DeleteSettles $Q1
emit P2RHolds1p3b P2R Spec "$INV" Prop_NoSilentRevert $Q1
emit P1WithoutP2 P1 Spec TypeOK,Inv_NoRegress "" UIAdoptsUncited=TRUE
emit P1DeleteOutranked P1 Spec TypeOK Prop_DeleteSettles DeleteWinsPreserved=FALSE
emit P1CommitBlind P1 Spec TypeOK Prop_NoSilentRevert CommitSurfacesForeign=FALSE
emit P1VerifyOff P1 Spec TypeOK,Inv_CitationsLive "" CommitVerifiesUploads=FALSE
# P1-lite's probes: each must be violated (the step is reachable).
emit P1ProbeConverged P1 Spec TypeOK ProbeConverged
emit P1ProbeDeleteOverTheirs P1 Spec TypeOK ProbeDeleteOverTheirs
# Record the verdict.
emit P1ForeignFlat P1 Spec "$INV" Prop_NoSilentRevert,Prop_DeleteSettles ForeignPerPath=FALSE
emit P1NoConvergence P1 Spec "$INV" Prop_NoSilentRevert,Prop_DeleteSettles ContentConverges=FALSE
emit P1LiveHolds P1 LSpec TypeOK Prop_UISaveCompletes
