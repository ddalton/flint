#!/bin/bash
# Replaces runner2.sh (user 22:02Z: "start TLC now at 184 workers"): the 8 worlds
# start at once at 184 workers; the 6 proof modules still running keep ~8 cores.
# At the end, wait for any tlapm still running, then shut down.
BK=$1; W=/work; F=$W/formal; cd $W
export HOME=/root; . /root/.cargo/env
TW=184
echo "RUNNER3 $(date -u +%FT%TZ) TLC now at $TW workers; $(pgrep -x tlapm | wc -l) tlapm still proving" >> out/RESULTS.txt
X="-fpmem 32000 -queue-mem 60000"
gate() { # <world> <cap>
  python3 $F/leanp1-gate.py --only "^LeanP1$1\$" --jobs 1 --workers $TW --cap $2 --compile --out $W/out/g --extra "$X" >> out/gate.log 2>&1
  echo "GATE LeanP1$1 rc=$? $(date -u +%FT%TZ) $(grep "^LeanP1$1 " out/gate.log | tail -1)" >> out/RESULTS.txt
  rm -rf $W/out/g/meta-* $W/out/g/cache-*
}
direct() { # <cfg> <module> <cap>
  local O=$W/out/d/$1.out; mkdir -p $W/out/d
  ( cd $F && ( time timeout $3 env TLAMC_CACHE=$W/cache-d tla-mc -compile -workers $TW $X -checkpoint 0 \
      -metadir $W/meta-d -config $1.cfg $2 ) ) > $O 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $W/meta-d $W/cache-d
  echo "DIRECT $1 $(date -u +%FT%TZ) $(grep -h -o -E 'is violated[^.]*|No error has been found|[0-9]+ distinct states found. Depth [0-9]+' $O | tr '\n' ' ') $(grep -h -o 'progress: depth [0-9]*, [0-9]* generated, [0-9]* distinct' $O | tail -1)" >> out/RESULTS.txt
}
gate CarrierCitesAny 900
gate AckRename 2700
for c in LeanP1AckSync LeanP1AckFetch LeanP1AckRescope LeanP1AckRescopeSync; do direct $c LeanP1.tla 1500; done
direct M7AckRescopeRename2 MCLeanP1M7.tla 1200
direct M7AckRename MCLeanP1M7.tla 1200
echo "TLC DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
while pgrep -x tlapm > /dev/null; do sleep 30; done
echo "DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
