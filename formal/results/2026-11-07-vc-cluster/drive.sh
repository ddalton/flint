#!/usr/bin/env bash
# One driver for the 2026-11-07 cluster batch: each named cfg in turn, one
# tla-mc -cluster search over HOSTS, compiled, capped at CAP seconds of wall.
# A capped run is bounded: its last progress line names the depth reached.
#
#   CAP=3600 ./drive.sh SPEC.tla CFG...     (from this directory; cfgs in cfg/)
#
# Writes logs/<cfg>/node*.log and one line per run to drive.out:
#   <cfg> rc=<rc> capped=<0|1> wall=<s> md5=<spec md5> <tla-mc's last lines>
set -uo pipefail
cd "$(dirname "$0")"
spec=${1:?usage: drive.sh SPEC.tla CFG...}
shift
CAP=${CAP:-3600}
# vc0131 left out: its other tenants (impala, hdfs) leave ~70 GB, and the
# checker there reached 67 GB with 1 GB available (2026-11-07 09:50); vc0132
# left out too: it had swapped (10 GB) and was the one node still working
# on the zombie arm's level 32 while the other 24 waited (11:29)
HOSTS=${HOSTS:-"vc0140 $(printf 'vc01%02d ' 15 16 17 18 $(seq 19 30) $(seq 33 39))"}
HOSTS=$(for h in $HOSTS; do printf '%s.halxg.cloudera.com ' "$h"; done)
export HOSTS CLUSTER_USER=ddalton
runner=/tmp/tla-mc-0.3.0/scripts/cluster-run.sh
[ "$(git -C /tmp/tla-mc-0.3.0 rev-parse HEAD)" = c8fce23493bb78fcd5405faf7b0653855db620ee ] ||
    { echo "drive: /tmp/tla-mc-0.3.0 is not tla-mc 0.3.0 (c8fce23)" >&2; exit 2; }
specmd5=$(md5 -q "$spec" 2>/dev/null || md5sum "$spec" | cut -d' ' -f1)

for c in "$@"; do
    n=$(basename "$c" .cfg)
    rm -rf run && mkdir -p run "logs/$n"
    cp "$spec" FlintCatchupS.tla FlintCatchupSSym.tla "$c" run/ 2>/dev/null
    # a fresh spill dir on /var (592 GB): /home is 37 GB and filled at depth 26
    preps=()
    for h in $HOSTS; do
        ssh -o BatchMode=yes "ddalton@$h" "rm -rf /var/tmp/ddalton-tlamc && mkdir -p /var/tmp/ddalton-tlamc" &
        preps+=($!)
    done
    for p in "${preps[@]}"; do wait "$p"; done
    t0=$(date +%s)
    LOGS="logs/$n" "$runner" "$PWD/run" -compile -workers 40 ${MEMOPTS:--queue-mem 8000 -fpmem 8000 -metadir /var/tmp/ddalton-tlamc} -config "$(basename "$c")" "$(basename "$spec")" \
        > "logs/$n/driver.log" 2>&1 &
    pid=$!
    capped=0
    first=${HOSTS%% *}
    finished=""
    while kill -0 $pid 2>/dev/null; do
        # stop early, as capped, before /var on node 0 falls below 100 GB free
        varfree=$(ssh -o BatchMode=yes "ddalton@$first" "df -BG --output=avail /var | tail -1 | tr -dc 0-9" 2>/dev/null || echo 999)
        # and before any node's available memory falls below 8 GB (shared nodes)
        low=""
        for h in $HOSTS; do
            a=$(ssh -o BatchMode=yes "ddalton@$h" "awk '/MemAvailable/{print int(\$2/1048576)}' /proc/meminfo" 2>/dev/null || echo 999)
            [ "${a:-999}" -lt 8 ] && low="$low $h:${a}G"
        done
        [ -n "$low" ] && echo "$n: stopped, low memory on$low" >> drive.out
        if [ $(( $(date +%s) - t0 )) -ge "$CAP" ] || [ "${varfree:-999}" -lt 100 ] || [ -n "$low" ]; then
            capped=1
            # stop every node: cluster-run's own trap would wait for node 0's ssh
            stops=()
            for h in $HOSTS; do
                ssh -o BatchMode=yes "ddalton@$h" "pkill -f '[.][.]/checker -cluster' || true" &
                stops+=($!)
            done
            for p in "${stops[@]}"; do wait "$p"; done
            break
        fi
        # node 0 has its verdict: the peers' ssh sessions here can outlive their
        # checkers (seen 12:50, the zombie arm), so release them after a minute
        if [ -z "$finished" ] && grep -qE 'states found|is violated|^Error' "logs/$n/node0.log" 2>/dev/null; then
            finished=$(date +%s)
        fi
        if [ -n "$finished" ] && [ $(( $(date +%s) - finished )) -ge 60 ]; then
            pkill -f "ssh -o BatchMode=yes ddalton@vc01.*exec ../checker -cluster vc0140" || true
        fi
        sleep 30
    done
    wait $pid
    rc=$?
    wall=$(( $(date +%s) - t0 ))
    last=$(grep -E 'progress:|distinct states found|violated|Error|error' "logs/$n/node0.log" 2>/dev/null | tail -2 | tr '\n' ' ')
    echo "$n rc=$rc capped=$capped wall=$wall md5=$specmd5 $last" >> drive.out
done
