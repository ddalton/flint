#!/usr/bin/env bash
# LESSER FINDING 5 (2026-10-02): the sweep's HEAD and DELETE as two steps.
# base.cfg.in is ForgeSyncRewindHolds.cfg (forge-needed, md5-pinned module
# 5ee1b260) plus SweepSplit/SweepRechecks/MaxRetries. Each world overrides
# CONSTANT lines: emit <name> <extra INVARIANT/PROPERTY lines> <K=V...>.
set -eu
cd "$(dirname "$0")"
emit() {
  local name=$1 extra=$2; shift 2
  cp base.cfg.in "ForgeSyncSweepWindow$name.cfg"
  for kv in "$@"; do
    k=${kv%%=*}; v=${kv#*=}
    grep -q "^  $k = " "ForgeSyncSweepWindow$name.cfg" || { echo "no constant $k"; exit 1; }
    sed -i.bak "s/^  $k = .*/  $k = $v/" "ForgeSyncSweepWindow$name.cfg"
  done
  rm -f "ForgeSyncSweepWindow$name.cfg.bak"
  [ -z "$extra" ] || printf '%b\n' "$extra" >> "ForgeSyncSweepWindow$name.cfg"
}
# The retry world: a client retries a failed push (same pack name). No
# folds, rewinds or re-pushes: the window alone.
R="MaxFolds=0 MaxRewinds=0 MaxResends=0 MaxRetries=1"
emit Retry ""                                   $R SweepSplit=TRUE  SweepRechecks=FALSE
emit RetryRechecks ""                           $R SweepSplit=TRUE  SweepRechecks=TRUE
emit RetryUnderLease ""                       $R SweepSplit=TRUE  SweepRechecks=FALSE SweepUnderLease=TRUE
emit RetryAtomic ""                             $R SweepSplit=FALSE SweepRechecks=FALSE
emit ProbeRetrySweepTakesNamed "PROPERTY\n  ProbeSweepTakesNamed" $R SweepSplit=TRUE SweepRechecks=FALSE
emit ProbeUnderLeaseSweepDeleted "PROPERTY\n  ProbeSweepDeleted" $R SweepSplit=TRUE SweepRechecks=FALSE SweepUnderLease=TRUE
