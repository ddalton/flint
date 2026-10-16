#!/bin/bash
# Replaces runner3.sh (user 22:04Z: "when the proofs finish, run the remaining
# models with full 192 cores").  runner3 is stopped by its OLDEST pid; the world
# it is running (its child) runs on to its end, then this runs the rest, each
# at 192 workers when no tlapm is left, else 184.
BK=$1; W=/work; F=$W/formal; cd $W
export HOME=/root; . /root/.cargo/env
K=$(pgrep -o -f "[b]ash /work/runner3.sh"); C=$(pgrep -P $K | head -1)
CMD=$(tr '\0' ' ' < /proc/$C/cmdline); kill $K
CUR=$(echo "$CMD" | grep -o -E 'LeanP1[A-Za-z0-9]+|-config [A-Za-z0-9]+' | head -1 | sed -e 's/-config //')
echo "RUNNER4 $(date -u +%FT%TZ) runner3 stopped (pid $K); its world $CUR (pid $C) runs on" >> out/RESULTS.txt
while kill -0 $C 2>/dev/null; do sleep 10; done
case $CUR in
  LeanP1CarrierCitesAny|LeanP1AckRename) echo "GATE $CUR (runner3) $(date -u +%FT%TZ) $(grep "^$CUR " out/gate.log | tail -1)" >> out/RESULTS.txt; rm -rf $W/out/g/meta-* $W/out/g/cache-* ;;
  *) O=$W/out/d/$CUR.out; echo "rc=? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $W/meta-d $W/cache-d
     echo "DIRECT $CUR (runner3) $(date -u +%FT%TZ) $(grep -h -o -E 'is violated[^.]*|No error has been found|[0-9]+ distinct states found. Depth [0-9]+' $O | tr '\n' ' ')" >> out/RESULTS.txt ;;
esac
X="-fpmem 32000 -queue-mem 60000"
tw() { pgrep -x tlapm > /dev/null && echo 184 || echo 192; }
gate() { # <world> <cap>
  local TW=$(tw); echo "START LeanP1$1 $(date -u +%FT%TZ) workers $TW" >> out/RESULTS.txt
  python3 $F/leanp1-gate.py --only "^LeanP1$1\$" --jobs 1 --workers $TW --cap $2 --compile --out $W/out/g --extra "$X" >> out/gate.log 2>&1
  echo "GATE LeanP1$1 rc=$? $(date -u +%FT%TZ) $(grep "^LeanP1$1 " out/gate.log | tail -1)" >> out/RESULTS.txt
  rm -rf $W/out/g/meta-* $W/out/g/cache-*
}
direct() { # <cfg> <module> <cap>
  local TW=$(tw); echo "START $1 $(date -u +%FT%TZ) workers $TW" >> out/RESULTS.txt
  local O=$W/out/d/$1.out; mkdir -p $W/out/d
  ( cd $F && ( time timeout $3 env TLAMC_CACHE=$W/cache-d tla-mc -compile -workers $TW $X -checkpoint 0 \
      -metadir $W/meta-d -config $1.cfg $2 ) ) > $O 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $W/meta-d $W/cache-d
  echo "DIRECT $1 $(date -u +%FT%TZ) $(grep -h -o -E 'is violated[^.]*|No error has been found|[0-9]+ distinct states found. Depth [0-9]+' $O | tr '\n' ' ') $(grep -h -o 'progress: depth [0-9]*, [0-9]* generated, [0-9]* distinct' $O | tail -1)" >> out/RESULTS.txt
}
ORDER="LeanP1CarrierCitesAny LeanP1AckRename LeanP1AckSync LeanP1AckFetch LeanP1AckRescope LeanP1AckRescopeSync M7AckRescopeRename2 M7AckRename"
past=0
for x in $ORDER; do
  if [ $past = 0 ]; then [ "$x" = "$CUR" ] && past=1; continue; fi
  case $x in
    LeanP1CarrierCitesAny) gate CarrierCitesAny 900 ;;
    LeanP1AckRename) gate AckRename 2700 ;;
    LeanP1*) direct $x LeanP1.tla 1500 ;;
    M7*) direct $x MCLeanP1M7.tla 1200 ;;
  esac
done
echo "TLC DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
while pgrep -x tlapm > /dev/null; do sleep 30; done
echo "DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
