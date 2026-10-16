#!/bin/bash
# Replaces runner.sh's phase T (user, 21:40Z: "the models should run with full
# 192 cores").  runner.sh is stopped by PID; its proof processes keep running.
# Wait for every tlapm to exit (deadline 23:15Z: a straggler is stopped, its
# log kept), then the 8 worlds one at a time at 192 workers.
BK=$1; W=/work; F=$W/formal; cd $W
export HOME=/root; . /root/.cargo/env
echo "RUNNER2 $(date -u +%FT%TZ) waiting for the proofs" >> out/RESULTS.txt
DL=$(date -u -d '2026-10-15 23:15:00' +%s)
while pgrep -x tlapm > /dev/null; do
  if [ $(date -u +%s) -ge $DL ]; then
    for p in $(pgrep -x tlapm); do echo "DEADLINE stop tlapm $p $(tr '\0' ' ' < /proc/$p/cmdline | grep -o 'LeanP1Proof[A-Z0-9]*')" >> out/RESULTS.txt; kill $p; done
    sleep 20; for p in $(pgrep -x z3); do kill $p; done; break
  fi
  sleep 30
done
for p in $(pgrep -x z3); do kill $p; done
TW=192
echo "PHASE T $(date -u +%FT%TZ) proofs done; TLC workers $TW" >> out/RESULTS.txt
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
echo "DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
