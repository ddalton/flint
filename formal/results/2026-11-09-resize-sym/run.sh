#!/bin/bash
# The two-replica validation runs, on the Mac (as ../2026-11-07-resize/run.sh):
#   ./run.sh NAME... [-- extra tla-mc args]
# runs each cfg/NAME.cfg on the spec its header names, from this directory (the
# wrappers EXTEND the copy of FlintCatchupSR.tla here, asserted to be formal's);
# log in logs/NAME.log, one line per run in run.out.
cd "$(dirname "$0")"
want=$(md5 -q ../../FlintCatchupSR.tla)
[ "$(md5 -q FlintCatchupSR.tla)" = "$want" ] || { echo "run: FlintCatchupSR.tla here is not formal's ($want)" >&2; exit 2; }
names=(); extra=()
while [ $# -gt 0 ]; do [ "$1" = -- ] && { shift; extra=("$@"); break; }; names+=("$1"); shift; done
mkdir -p logs
ulimit -n 20000 2>/dev/null
for n in "${names[@]}"; do
    spec=$(grep -m1 '^\\\* spec:' "cfg/$n.cfg" | awk '{print $3}')
    s=$(date +%s)
    /Users/ddalton/.cargo/bin/tla-mc -compile -workers 16 -fpmem 12000 -queue-mem 8000 "${extra[@]}" -config "cfg/$n.cfg" "$spec" > "logs/$n.log" 2>&1
    rc=$?
    last=$(grep -E 'distinct states found|is violated|^Error|error' "logs/$n.log" | tail -2 | tr '\n' ' ')
    echo "$n rc=$rc wall=$(( $(date +%s) - s )) md5=$(md5 -q "$spec")+$want $last" >> run.out
done
