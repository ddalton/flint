#!/usr/bin/env bash
# The three-replica driver of the resize symmetry batch (from
# ../2026-11-07-vc-cluster/drive.sh): each named cfg in turn, one tla-mc
# -cluster search over HOSTS, compiled, capped at CAP seconds of wall.
# A capped run is bounded: its last progress line names the depth reached.
#
#   CAP=14400 nohup ./drive.sh FlintCatchupSRSymV.tla cfg/R3_SymV_lost_r3f1a3.cfg ... &
#
# Writes logs/<cfg>/node*.log, and to drive.out a line naming each run's
# nodes, then its result:
#   <cfg> rc=<rc> capped=<0|1> wall=<s> md5=<spec md5>+<FlintCatchupSR md5> <tla-mc's last lines>
set -uo pipefail
cd "$(dirname "$0")"
spec=${1:?usage: drive.sh SPEC.tla CFG...}
shift
CAP=${CAP:-3600}
# the 37 usable nodes (vc0114 is down; vc0131 and vc0132 left out as on
# 2026-11-07, see ../2026-11-07-vc-cluster/drive.sh); vc0140 is node 0: it
# builds, and has cargo and gcc
HOSTS=${HOSTS:-"vc0140 $(printf 'vc01%02d ' $(seq 1 13) $(seq 15 30) $(seq 33 39))"}
nodes=$(echo $HOSTS | wc -w | tr -d ' ')
HOSTS=$(for h in $HOSTS; do printf '%s.halxg.cloudera.com ' "$h"; done)
# port, remote dir and spill dir apart from the forge and catch-up batches'
export HOSTS CLUSTER_USER=ddalton PORT=27472 REMOTE_DIR=tla-mc-resize
spill=/var/tmp/ddalton-tlamc-resize
runner=/tmp/tla-mc-0.3.0/scripts/cluster-run.sh
[ "$(git -C /tmp/tla-mc-0.3.0 rev-parse HEAD)" = c8fce23493bb78fcd5405faf7b0653855db620ee ] ||
    { echo "drive: /tmp/tla-mc-0.3.0 is not tla-mc 0.3.0 (c8fce23)" >&2; exit 2; }
base=$(md5 -q ../../FlintCatchupSR.tla)
[ "$(md5 -q FlintCatchupSR.tla)" = "$base" ] || { echo "drive: FlintCatchupSR.tla here is not formal's ($base)" >&2; exit 2; }
specmd5=$(md5 -q "$spec")
first=${HOSTS%% *}

for c in "$@"; do
    n=$(basename "$c" .cfg)
    rm -rf run && mkdir -p run "logs/$n"
    cp "$spec" FlintCatchupSR.tla FlintCatchupSRSym.tla "$c" run/ 2>/dev/null
    echo "$n: $(date '+%F %H:%M') on $nodes nodes:$(for h in $HOSTS; do printf ' %s' "${h%%.*}"; done)" >> drive.out
    # a fresh spill dir on /var (592 GB): /home is 37 GB
    preps=()
    for h in $HOSTS; do
        ssh -o BatchMode=yes "ddalton@$h" "rm -rf $spill && mkdir -p $spill" &
        preps+=($!)
    done
    for p in "${preps[@]}"; do wait "$p"; done
    t0=$(date +%s)
    LOGS="logs/$n" "$runner" "$PWD/run" -compile -workers 40 ${MEMOPTS:--queue-mem 8000 -fpmem 8000 -metadir $spill} -config "$(basename "$c")" "$(basename "$spec")" \
        > "logs/$n/driver.log" 2>&1 &
    pid=$!
    capped=0
    finished=""
    while kill -0 $pid 2>/dev/null; do
        # stop early, as capped, before any node's /var falls below 100 GB free
        # or its available memory below 8 GB (shared nodes)
        low=""
        for h in $HOSTS; do
            read -r a v < <(ssh -o BatchMode=yes "ddalton@$h" "echo \$(awk '/MemAvailable/{print int(\$2/1048576)}' /proc/meminfo) \$(df -BG --output=avail /var | tail -1 | tr -dc 0-9)" 2>/dev/null || echo 999 999)
            [ "${a:-999}" -lt 8 ] && low="$low $h:${a}G"
            [ "${v:-999}" -lt 100 ] && low="$low $h:/var ${v}G"
        done
        [ -n "$low" ] && echo "$n: stopped, low on$low" >> drive.out
        if [ $(( $(date +%s) - t0 )) -ge "$CAP" ] || [ -n "$low" ]; then
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
        # checkers, so release them after a minute
        if [ -z "$finished" ] && grep -qE 'states found|is violated|^Error' "logs/$n/node0.log" 2>/dev/null; then
            finished=$(date +%s)
        fi
        if [ -n "$finished" ] && [ $(( $(date +%s) - finished )) -ge 60 ]; then
            pkill -f "ssh -o BatchMode=yes ddalton@vc01.*exec ../checker -cluster ${first%%.*}" || true
        fi
        sleep 30
    done
    wait $pid
    rc=$?
    wall=$(( $(date +%s) - t0 ))
    last=$(grep -E 'progress:|distinct states found|violated|Error|error' "logs/$n/node0.log" 2>/dev/null | tail -2 | tr '\n' ' ')
    echo "$n rc=$rc capped=$capped wall=$wall md5=$specmd5+$base $last" >> drive.out
done
