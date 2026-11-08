#!/bin/bash
# usage: run.sh NAME [extra tla-mc args]; runs cfg/NAME.cfg on the spec its header names; log in logs/NAME.log
cd "$(dirname "$0")/../.."
R=results/2026-11-07-resize
n=$1; shift
spec=$(grep -m1 '^\\\* spec:' $R/cfg/$n.cfg | awk '{print $3}')
ulimit -n 20000 2>/dev/null; s=$(date +%s)
/Users/ddalton/.cargo/bin/tla-mc -compile -workers 16 "$@" -config $R/cfg/$n.cfg $spec > $R/logs/$n.log 2>&1
echo "exit=$? wall=$(( $(date +%s)-s ))s" >> $R/logs/$n.log
