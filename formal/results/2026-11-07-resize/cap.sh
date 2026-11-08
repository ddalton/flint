#!/bin/bash
# usage: cap.sh NAME SECONDS [tla-mc args]: run.sh with a wall cap; on the cap the checker is killed and the log says BOUNDED
n=$1; cap=$2; shift 2
cd "$(dirname "$0")"
./run.sh $n "$@" &
rp=$!
( sleep $cap; pkill -f "flintcatchupsr-.*$n.cfg" && echo "BOUNDED: killed at wall cap ${cap}s" >> logs/$n.log ) &
wait $rp
