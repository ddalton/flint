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
GatewayIgnoresLease GatewayJudgesRead GatewaySweepGrace RenameAtomic \
Scopes MaxRescopes ConsumeHonorsScope RescopeUnciteFirst RescopeKeepsDirty WidenKeepsLocal UnlinkChecksBytes \
MaxFetchFails ConsumeKeepsLeft SyncKeepsLeft Readers ReaderRechecksOwed"
emit() { # <name> <spec> <invariants> <properties> <overrides...>
  local name=$1 spec=$2 invs=$3 props=$4; shift 4
  local c_Paths='{p1, p2}' c_Free='{p2}' c_Writers='{A, B}'
  local c_MaxMint=3 c_MaxUI=1 c_MaxRemovals=1 c_MaxBarriers=2 c_MaxRestarts=0 c_MaxSyncs=0 c_MaxCopies=1 c_MaxAges=1
  local c_Scopes= c_MaxRescopes=0 c_MaxFetchFails=0 c_Readers='{}'
  local k kv v
  for k in CommitSurfacesForeign CommitVerifiesUploads SweepUnderLease CollectorSparesCited \
           CommitRecordsDeleteOverride DeleteWinsPreserved ContentConverges RecheckSkipped CommitAdvanceGuarded RetireAge \
           GatewayIgnoresLease GatewayJudgesRead GatewaySweepGrace RenameAtomic \
           ConsumeHonorsScope RescopeUnciteFirst RescopeKeepsDirty WidenKeepsLocal UnlinkChecksBytes \
           ConsumeKeepsLeft SyncKeepsLeft ReaderRechecksOwed; do
    eval "local c_$k=TRUE"
  done
  for kv in "$@"; do k=${kv%%=*}; v=${kv#*=}; eval "c_$k=\"\$v\""; done
  # Unscoped unless a world names its scopes: every tree holds every path.
  [ -n "$c_Scopes" ] || c_Scopes="{$c_Paths}"
  {
    echo "SPECIFICATION $spec"
    echo "CHECK_DEADLOCK FALSE"
    echo "CONSTANTS"
    echo "  Nil = Nil"
    [ -z "${XC:-}" ] || printf '%s\n' $XC | sed 's/^/  /; s/=/ = /'
    for k in $KEYS; do eval "echo \"  $k = \$c_$k\""; done
    local i
    for i in ${invs//,/ }; do echo "INVARIANT $i"; done
    for i in ${props//,/ }; do echo "PROPERTY $i"; done
  } > "LeanP1$name.cfg"
}
# 2026-10-14: the restated claims.  AN5 replaces Inv_AckedNamed and NR5 (an
# action property) Inv_NoRegress: both originals are FALSE once the gateway
# may remove three times (results/2026-10-07-tlaps-m5/NOTES.txt E6, E8), and
# both restatements are PROVED for every bound (m5c/, m5d/).  The originals
# stay in LeanP1.tla as the record.
INV="TypeOK,Inv_CitationsLive,Inv_OneName,AN5,Inv_OneHolder,Inv_ShortcutSound,Inv_ReaderFetches"
PROPS="Prop_NoSilentRevert,Prop_DeleteSettles,NR5"
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
# 2026-10-02: SCOPE AND RESCOPE. One writer, scoped ({p1}) or not; one
# rescope (a widen from {p1}, or a narrow from {p1, p2}); one restart, so
# the replay after a crash between the halves is reachable. p2 starts
# uncited (Free), so a UI save or rename can change it out of scope. One
# writer: what moves the document under a scoped tree is the gateway's
# verbs as much as a peer's commit, and the merge between writers is
# LeanP1Holds's. No retire age (orthogonal; LeanP1Holds checks it). Two
# writers at these bounds grew 2.5x a level past 21M states at depth 12.
SC="Scopes={{p1},{p1,p2}} MaxRescopes=1 MaxRestarts=1 Writers={A} MaxAges=0"
SPROPS="Prop_NoSilentRevert,Prop_DeleteSettles,Prop_AgentWorkKept,Prop_ScopeRespected,Prop_NarrowNeverDeletes"
emit ScopeHolds Spec "$INV" "$SPROPS" $SC
emit ScopeIgnored Spec TypeOK Prop_ScopeRespected ConsumeHonorsScope=FALSE $SC
emit WidenOverwrites Spec TypeOK Prop_AgentWorkKept WidenKeepsLocal=FALSE $SC
emit NarrowUnlinkFirst Spec TypeOK Prop_NarrowNeverDeletes RescopeUnciteFirst=FALSE $SC
# RescopeKeepsDirty (the replay keeps a still-cited dirty path) is NOT a
# world: under the unlink's byte check it is not load-bearing for the
# agent's work (an uncited dirty file is kept and publishes as an add; R7
# records what it lands over). Measured: RescopeKeepsDirty=FALSE ran 600 s
# without a violation of Prop_AgentWorkKept.
emit UnlinkBlind Spec TypeOK Prop_AgentWorkKept UnlinkChecksBytes=FALSE $SC
emit ProbeNarrowed Spec TypeOK ProbeNarrowed $SC
emit ProbeWidened Spec TypeOK ProbeWidened $SC
emit ProbeRescopeReplayed Spec TypeOK ProbeRescopeReplayed $SC
emit ProbeOutOfScopePublished Spec ProbeOutOfScopePublished "" $SC
# 2026-10-02: A FAILED FETCH. One fetch or write may fail in a consume, a
# sync or a widen; the path stays owed and nothing is recorded as derived.
# A sync is reachable (MaxSyncs=1); no retire age (LeanP1Holds checks it).
# Two writers on one path, LiveHoldsSmall's shape: at LeanP1Holds's two
# paths the failed fetch grew 2.2x a level past 15M states at depth 13.
FF="Paths={p1} Free={} MaxRemovals=0 MaxFetchFails=1 MaxSyncs=1 MaxAges=0"
emit FetchHolds Spec "$INV" "$PROPS" $FF
emit ConsumeLeftRecorded Spec TypeOK,Inv_ShortcutSound "" ConsumeKeepsLeft=FALSE $FF
emit SyncLeftRecorded Spec TypeOK,Inv_ShortcutSound "" SyncKeepsLeft=FALSE $FF
emit ProbeFetchFailed Spec TypeOK ProbeFetchFailed $FF
emit ScopeFetchHolds Spec "$INV" "$SPROPS" $SC MaxFetchFails=1
# 2026-10-02: A READER. B has read access: it never publishes, and its tick
# syncs the whole tree when the pointer moved since its last pull or its
# baseline says something is still owed (L-129). One fetch may fail.
RD="Paths={p1} Free={} MaxRemovals=0 MaxFetchFails=1 MaxAges=0 Readers={B}"
emit ReaderHolds Spec "$INV,Inv_ReaderSound" "$PROPS,Prop_AgentWorkKept" $RD
emit ReaderOwedIgnored Spec TypeOK,Inv_ReaderSound "" ReaderRechecksOwed=FALSE $RD
emit ProbeReaderPulled Spec TypeOK ProbeReaderPulled $RD
emit ProbeReaderSkipped Spec ProbeReaderSkipped "" $RD
# The reader over two paths, scoped or not. No UI removal and no re-upload
# copy: with them this grew ~1.75x a level past 27M states at depth 14.
emit ReaderScopeHolds Spec "$INV,Inv_ReaderSound" "$PROPS,Prop_AgentWorkKept,Prop_ScopeRespected" \
  Free={p2} MaxRemovals=0 MaxCopies=0 MaxFetchFails=1 MaxAges=0 Readers={B} "Scopes={{p1},{p1,p2}}"
# 2026-10-03: ALL ON. ReaderScopeHolds's shape plus one rescope and one
# restart (the replay), so the reader can rescope and pull mid-rescope.
# Checked against MCLeanP1All.tla (LeanP1 plus the two probes below).
AL="Free={p2} MaxRemovals=0 MaxCopies=0 MaxFetchFails=1 MaxAges=0 Readers={B} Scopes={{p1},{p1,p2}} MaxRescopes=1 MaxRestarts=1"
emit AllHolds Spec "$INV,Inv_ReaderSound" "$SPROPS" $AL
emit ProbeReaderRescoped Spec TypeOK ProbeReaderRescoped $AL
emit ProbeReaderPulledMidRescope Spec TypeOK ProbeReaderPulledMidRescope $AL
# 2026-10-14: THREE REMOVALS AND THE RETIRE AGE.  Every world above has
# MaxRemovals <= 1, where Inv_NoRegress and Inv_AckedNamed cannot fail; both
# fail at three (E6, E8).  One writer (two committing writers at these bounds
# grew past 125M at depth 12, W2Rem3, undecided).  7,435,952 and 77,160,864
# distinct (E35, E41).
R3="Writers={A} MaxRemovals=3 MaxCopies=0 MaxAges=0"
emit Rem3 Spec "$INV" "$PROPS" $R3
emit Rem3Age Spec "$INV" "$PROPS" $R3 MaxAges=1
# The model mutations (gen-leanp1-mutants.py; the world's fourth column in
# WORLDS-LeanP1.tsv names the mutant): a delete not acknowledged breaks NR5,
# a delete that records nothing breaks AN5.
emit MN2Rem3 Spec TypeOK NR5 $R3
emit MA1Rem3Age Spec TypeOK,AN5 "" $R3 MaxAges=1
# 2026-10-14: THE SCRIPTED TRACES (MCLeanP1Trace*.tla): hand-found behaviours
# too deep for BFS (27-31 steps in worlds of billions), each step a Next step
# (HIsNext).  Each must reach its end with the claims holding (HReach is the
# end), and break what it was found against: E27, E31, E33 the old Edit base
# (the EditBase mutant), E29 Inv_NoRegress, E38, E39 Inv_AckedNamed.
TW="Writers={A,B} Readers={B} MaxUI=0 MaxRemovals=3 MaxCopies=0 MaxAges=0"
for e in E27 E31 E33; do
  XC="WA=A WB=B P1=p1 P2=p2" emit Trace$e HSpec TypeOK,AN5,HReach NR5,HIsNext $TW
  XC="WA=A WB=B P1=p1 P2=p2" emit Trace${e}EditBase HSpec TypeOK NR5,HIsNext $TW
done
T29="Writers={A,B} Readers={B} MaxMint=4 MaxUI=1 MaxRemovals=3 MaxRestarts=1 MaxCopies=0 MaxAges=0"
XC="WA=A WB=B P1=p1 P2=p2" emit TraceE29 HSpec TypeOK,AN5,HReach NR5,HIsNext $T29
XC="WA=A WB=B P1=p1 P2=p2" emit TraceE29Old HSpec TypeOK,Inv_NoRegress HIsNext $T29
T38="Writers={A} MaxMint=5 MaxUI=2 MaxRemovals=1 MaxBarriers=3 MaxRestarts=1 MaxCopies=1 MaxAges=1 MaxFetchFails=1"
XC="WA=A P1=p1 P2=p2" emit TraceE38 HSpec TypeOK,AN5,HReach NR5,HIsNext $T38
XC="WA=A P1=p1 P2=p2" emit TraceE38Old HSpec TypeOK,Inv_AckedNamed HIsNext $T38
T39="Paths={p1,p2,p3} Free={p2,p3} Writers={A} MaxMint=6 MaxUI=3 MaxRemovals=2 MaxBarriers=3 MaxRestarts=1 MaxCopies=1 MaxAges=1 MaxFetchFails=1"
XC="WA=A P1=p1 P2=p2 P3=p3" emit TraceE39 HSpec TypeOK,AN5,HReach NR5,HIsNext $T39
XC="WA=A P1=p1 P2=p2 P3=p3" emit TraceE39Old HSpec TypeOK,Inv_AckedNamed HIsNext $T39
