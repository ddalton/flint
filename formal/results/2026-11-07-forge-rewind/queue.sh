#!/usr/bin/env bash
# The forge queue (2026-11-09): each named world in turn through drive.sh on
# HOSTS, spec ForgeSync.tla.  A STOP file in this directory ends the queue
# before its next world, so the cluster is handed back at a world's boundary.
# drive.out gets a line naming each world's nodes before its result line
# (drive.sh does not record HOSTS).
#
#   HOSTS="vc0101 ..." CAP=3600 nohup ./queue.sh F_R1S2_Model F_R2S1_Model ... &
set -uo pipefail
cd "$(dirname "$0")"
: "${HOSTS:?set HOSTS to the nodes, node 0 first}"
for n in "$@"; do
    if [ -e STOP ]; then
        echo "$(date '+%F %H:%M') STOP before $n" >> queue.log
        exit 0
    fi
    [ -f "cfg/$n.cfg" ] || { echo "$(date '+%F %H:%M') no cfg/$n.cfg" >> queue.log; exit 2; }
    echo "$(date '+%F %H:%M') $n" >> queue.log
    echo "$n: $(date '+%F %H:%M') on $(echo $HOSTS | wc -w | tr -d ' ') nodes: $HOSTS" >> drive.out
    ./drive.sh ForgeSync.tla "cfg/$n.cfg"
done
echo "$(date '+%F %H:%M') queue done" >> queue.log
